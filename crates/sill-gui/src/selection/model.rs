//! What one selectable leaf knows about its own text, in a form the selection [`Area`] can use
//! without knowing the leaf's widget type: the laid-out paragraph, the text flattened to one
//! string (lines joined by their own endings) and the byte range the area asks it to highlight.
//!
//! Offsets are byte offsets into the flattened text. Every entry point clamps them to the text
//! and to a character boundary first, so an offset that went stale when the text changed cannot
//! index out of range or split a character.
//!
//! [`Area`]: super::Area
use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::graphics::text::cosmic_text::{Affinity, Cursor, LayoutRun};
use iced::advanced::text::Paragraph as _;
use iced::{Point, Rectangle, Size};
use unicode_segmentation::UnicodeSegmentation;

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// A laid-out paragraph and its flattened text. Cloning is three `Arc` clones.
#[derive(Debug, Clone)]
pub struct TextLayout {
    paragraph: Paragraph,
    text: Arc<str>,
    line_starts: Arc<[usize]>,
}

impl Default for TextLayout {
    fn default() -> Self {
        Self {
            paragraph: Paragraph::default(),
            text: Arc::from(""),
            line_starts: Arc::from([]),
        }
    }
}

impl TextLayout {
    fn new(paragraph: Paragraph) -> Self {
        let mut text = String::new();
        let mut starts = Vec::new();
        for line in &paragraph.buffer().lines {
            starts.push(text.len());
            text.push_str(line.text());
            text.push_str(line.ending().as_str());
        }
        Self {
            paragraph,
            text: Arc::from(text),
            line_starts: Arc::from(starts),
        }
    }

    /// The text as it is laid out: every line followed by its own line ending.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn paragraph(&self) -> &Paragraph {
        &self.paragraph
    }

    /// Where the paragraph is drawn inside the widget's `bounds` (iced's text anchor).
    pub fn origin(&self, bounds: Rectangle) -> Point {
        bounds.anchor(
            self.paragraph.min_bounds(),
            self.paragraph.align_x(),
            self.paragraph.align_y(),
        )
    }

    /// `offset` clamped to the text and moved back to a character boundary.
    pub fn floor(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(line) => line.saturating_sub(1),
        }
    }

    fn line_len(&self, line: usize) -> usize {
        self.paragraph
            .buffer()
            .lines
            .get(line)
            .map_or(0, |line| line.text().len())
    }

    fn cursor(&self, offset: usize, affinity: Affinity) -> Cursor {
        let offset = self.floor(offset);
        let line = self.line_of(offset);
        let start = self.line_starts.get(line).copied().unwrap_or(0);
        Cursor::new_with_affinity(
            line,
            offset.saturating_sub(start).min(self.line_len(line)),
            affinity,
        )
    }

    /// The logical line (without its ending) that holds `offset`.
    pub fn line_at(&self, offset: usize) -> (usize, usize) {
        let line = self.line_of(self.floor(offset));
        let start = self.line_starts.get(line).copied().unwrap_or(0);
        (start, start + self.line_len(line))
    }

    /// The word (a UAX #29 segment) under the pointer; `affinity` tells on which side of `offset`
    /// the pointer was.
    pub fn word_at(&self, offset: usize, affinity: Affinity) -> (usize, usize) {
        let (start, end) = self.line_at(offset);
        let offset = self.floor(offset).clamp(start, end);
        let probe = if affinity == Affinity::Before && offset > start {
            self.text[..offset]
                .char_indices()
                .next_back()
                .map_or(start, |(i, _)| i)
        } else {
            offset
        };
        for (i, word) in self.text[start..end].split_word_bound_indices() {
            let (a, b) = (start + i, start + i + word.len());
            if probe >= a && probe < b {
                return (a, b);
            }
        }
        (offset, offset)
    }

    /// The caret position nearest to `point` (paragraph coordinates) as a byte offset. Points
    /// above, below or beside the text snap to the nearest visual line and its ends.
    pub fn hit(&self, point: Point) -> (usize, Affinity) {
        let buffer = self.paragraph.buffer();
        let mut chosen = None;
        for run in buffer.layout_runs() {
            chosen = Some((run.line_i, run.line_top, run.line_height, extent(&run)));
            if point.y < run.line_top + run.line_height {
                break;
            }
        }
        let Some((line, top, height, extent)) = chosen else {
            return (0, Affinity::After);
        };
        let line_start = self.line_starts.get(line).copied().unwrap_or(0);
        let Some((x0, x1)) = extent else {
            return (self.floor(line_start), Affinity::After);
        };
        // cosmic-text maps an x left of a line that does not start at 0 to the line's end; clamp
        // into the glyphs so the result is always the nearest caret on this visual line
        let x = point.x.clamp(x0, x1);
        match buffer.hit(x, top + height / 2.0) {
            Some(cursor) => {
                let start = self.line_starts.get(cursor.line).copied().unwrap_or(0);
                (self.floor(start + cursor.index), cursor.affinity)
            }
            None => (self.floor(line_start), Affinity::After),
        }
    }

    /// A point (paragraph coordinates) that hit-tests to exactly `offset`: a quarter into the
    /// glyph that starts there, or three quarters into the glyph that ends there (the end of a
    /// visual line). For scripted drags in tests.
    #[cfg(test)]
    pub fn caret_point(&self, offset: usize) -> Option<Point> {
        let cursor = self.cursor(offset, Affinity::After);
        let mut end_of_line = None;
        for run in self.paragraph.buffer().layout_runs() {
            if run.line_i != cursor.line {
                continue;
            }
            let middle = run.line_top + run.line_height / 2.0;
            for glyph in run.glyphs {
                if glyph.start == cursor.index {
                    return Some(Point::new(glyph.x + glyph.w * 0.25, middle));
                }
                if glyph.end == cursor.index {
                    end_of_line = Some(Point::new(glyph.x + glyph.w * 0.75, middle));
                }
            }
        }
        end_of_line
    }

    /// The rectangles behind `start..end`, in paragraph coordinates; wrapped lines give one per
    /// visual line.
    pub fn highlight_rects(&self, (start, end): (usize, usize)) -> Vec<Rectangle> {
        let (start, end) = (self.floor(start), self.floor(end));
        if start >= end {
            return Vec::new();
        }
        let from = self.cursor(start, Affinity::Before);
        let to = self.cursor(end, Affinity::After);
        let mut rects = Vec::new();
        for run in self.paragraph.buffer().layout_runs() {
            if run.line_i < from.line || run.line_i > to.line {
                continue;
            }
            if run.glyphs.is_empty() {
                // an empty line inside the selection: show that its newline is selected
                let at = self.line_starts.get(run.line_i).copied().unwrap_or(0);
                if start <= at && at < end {
                    rects.push(Rectangle::new(
                        Point::new(0.0, run.line_top),
                        Size::new(run.line_height * 0.3, run.line_height),
                    ));
                }
                continue;
            }
            if let Some((x, width)) = run.highlight(from, to)
                && width > 0.0
            {
                rects.push(Rectangle::new(
                    Point::new(x, run.line_top),
                    Size::new(width, run.line_height),
                ));
            }
        }
        rects
    }
}

/// The selection-relevant state of one leaf. The leaf widget owns it in its tree state and hands
/// it to the area through `Operation::custom`.
#[derive(Debug, Clone)]
pub struct Selectable {
    layout: TextLayout,
    /// Identifies the tree slot this state lives in (a new state gets a new uid).
    uid: u64,
    /// Bumped whenever the text changes.
    generation: u64,
    /// Bumped whenever the paragraph is laid out again (text, shape or bounds changed).
    layout_version: u64,
    highlight: Option<(usize, usize)>,
    rects: Vec<Rectangle>,
    rects_key: Option<((usize, usize), u64)>,
}

impl Default for Selectable {
    fn default() -> Self {
        Self {
            layout: TextLayout::default(),
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            generation: 0,
            layout_version: 0,
            highlight: None,
            rects: Vec::new(),
            rects_key: None,
        }
    }
}

impl Selectable {
    pub fn layout(&self) -> &TextLayout {
        &self.layout
    }

    pub fn text(&self) -> &str {
        self.layout.text()
    }

    pub fn paragraph(&self) -> &Paragraph {
        self.layout.paragraph()
    }

    pub fn origin(&self, bounds: Rectangle) -> Point {
        self.layout.origin(bounds)
    }

    /// The highlighted part of the text, if any.
    #[cfg(test)]
    pub fn highlighted_text(&self) -> Option<&str> {
        let (start, end) = self.highlight?;
        self.layout.text().get(start..end)
    }

    /// Whether a highlight is set.
    pub fn is_highlighted(&self) -> bool {
        self.highlight.is_some()
    }

    /// The rectangles to paint behind the highlighted text, in paragraph coordinates.
    pub fn highlight_rects(&self) -> Cow<'_, [Rectangle]> {
        match self.highlight {
            None => Cow::Borrowed(&[]),
            Some(range) if self.rects_key == Some((range, self.layout_version)) => {
                Cow::Borrowed(&self.rects)
            }
            Some(range) => Cow::Owned(self.layout.highlight_rects(range)),
        }
    }

    pub fn uid(&self) -> u64 {
        self.uid
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Replaces the paragraph after the leaf rebuilt it.
    pub fn set_paragraph(&mut self, paragraph: Paragraph) {
        let layout = TextLayout::new(paragraph);
        if layout.text != self.layout.text {
            self.generation += 1;
        }
        self.layout = layout;
        self.layout_version += 1;
    }

    pub fn resize(&mut self, bounds: Size) {
        self.layout.paragraph.resize(bounds);
        self.layout_version += 1;
    }

    /// Sets the highlighted byte range (clamped) and lays out its rectangles once, so drawing
    /// does not redo it every frame.
    pub fn set_highlight(&mut self, range: Option<(usize, usize)>) {
        let range = range
            .map(|(start, end)| (self.layout.floor(start), self.layout.floor(end)))
            .filter(|(start, end)| start < end);
        self.highlight = range;
        if let Some(range) = range
            && self.rects_key != Some((range, self.layout_version))
        {
            self.rects = self.layout.highlight_rects(range);
            self.rects_key = Some((range, self.layout_version));
        }
    }
}

fn extent(run: &LayoutRun<'_>) -> Option<(f32, f32)> {
    let x0 = run.glyphs.iter().map(|g| g.x).reduce(f32::min)?;
    let x1 = run.glyphs.iter().map(|g| g.x + g.w).reduce(f32::max)?;
    Some((x0, x1))
}

/// A hash of `text[..len]`; the caller guarantees `len` is a boundary in `text`.
pub fn fingerprint(text: &str, len: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    text[..len].hash(&mut hasher);
    hasher.finish()
}
