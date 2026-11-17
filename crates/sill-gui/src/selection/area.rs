//! One text selection across every selectable leaf inside an [`Area`].
//!
//! The area owns the selection: an *anchor* where the drag started and a *focus* where the
//! pointer is, each a [`Position`] — a leaf's ordinal in tree (reading) order and a byte offset
//! into that leaf's text. Leaves are passive: each reports its laid-out paragraph through
//! `Operation::custom` and draws whatever byte range the area hands back.
//!
//! The area runs three operations over its content:
//!
//! - **collect** — every leaf's paragraph snapshot and its bounds *in the area's coordinates*:
//!   `scrollable` hooks are tracked the way `iced_selector`'s `Finder` does (translation and clip
//!   per nesting level), so a point under the pointer maps through the conversation's vertical
//!   scroll and a code block's horizontal scroll alike.
//! - **paint** — the per-leaf byte range of the current selection, into each leaf's state.
//! - **scroll** — scrolls the outermost vertical scrollable while a drag is held past its edge.
//!
//! Collect and paint run on press, on every pointer move during a drag, on copy, and after every
//! layout while a selection exists (layout is when text streams in or rewraps), so highlights are
//! never drawn from stale offsets.
use std::any::Any;
use std::cell::RefCell;

use iced::advanced::graphics::text::cosmic_text::Affinity;
use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, Scrollable};
use iced::advanced::widget::{Id, Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, clipboard, overlay};
use iced::keyboard;
use iced::time::{Duration, Instant};
use iced::window;
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

use super::model::{Selectable, TextLayout, fingerprint};

/// A place in the area's text: leaf ordinal (reading order) and byte offset into its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub leaf: usize,
    pub offset: usize,
}

/// What [`Area`] highlights for Find during layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindQuery {
    pub needle: String,
    pub case_sensitive: bool,
    /// The current match as `(row, occurrence among the row's laid-out leaves)`.
    pub current: Option<(usize, usize)>,
}

/// A rendered leaf's layout and vertical position within the scrollable content, recorded during
/// layout so Find can locate the first match at or below the top of the viewport even inside a
/// partially scrolled long message.
#[derive(Debug, Clone)]
pub struct VisibleLeaf {
    pub row: Option<usize>,
    pub origin_y: f32,
    /// How far the outer scrollable was scrolled down when the leaf was recorded.
    pub view_top: f32,
    pub layout: TextLayout,
}

/// A selection area around some content (the conversation's or a reader's scrollable).
pub struct Area<'a, Message> {
    content: Element<'a, Message>,
    on_copy: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_drag: Option<Box<dyn Fn(bool) -> Message + 'a>>,
    find: Option<FindQuery>,
    selection: Option<&'a RefCell<Option<String>>>,
    visible_leaves: Option<&'a RefCell<Vec<VisibleLeaf>>>,
    on_text_change: Option<Box<dyn Fn() -> Message + 'a>>,
}

pub fn area<'a, Message>(content: impl Into<Element<'a, Message>>) -> Area<'a, Message> {
    Area { content: content.into(), on_copy: None, on_drag: None, find: None, selection: None, visible_leaves: None, on_text_change: None }
}

impl<'a, Message> Area<'a, Message> {
    /// Copy (Ctrl+C) publishes this message with the text instead of writing the clipboard
    /// itself, so the application decides (and tests can see what was copied).
    pub fn on_copy(mut self, on_copy: impl Fn(String) -> Message + 'a) -> Self {
        self.on_copy = Some(Box::new(on_copy));
        self
    }

    /// Told `true` when a selection drag starts and `false` when it ends, however it ends.
    pub fn on_drag(mut self, on_drag: impl Fn(bool) -> Message + 'a) -> Self {
        self.on_drag = Some(Box::new(on_drag));
        self
    }

    /// Find query and current match to highlight inside this area's leaves.
    pub fn find(mut self, find: Option<FindQuery>) -> Self {
        self.find = find;
        self
    }

    /// Keeps the currently selected text mirrored here so Ctrl+F can pre-populate its query.
    pub fn selection(mut self, selection: &'a RefCell<Option<String>>) -> Self {
        self.selection = Some(selection);
        self
    }

    /// Records the rendered leaves with their scrollable content y-offsets on each layout pass.
    pub fn visible_leaves(mut self, visible_leaves: &'a RefCell<Vec<VisibleLeaf>>) -> Self {
        self.visible_leaves = Some(visible_leaves);
        self
    }

    /// Published on the first event after a layout whose recorded leaves (`visible_leaves`) hold
    /// other text than the layout before: Find on a page counts what the page laid out, so it
    /// counts again once a page changed (another category, a section folded).
    pub fn on_text_change(mut self, message: impl Fn() -> Message + 'a) -> Self {
        self.on_text_change = Some(Box::new(message));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Char,
    Word,
    Line,
}

/// The extent of what a press or drag point selects: a caret, a word or a line.
type Unit = (Position, Position);

/// Recognises a referenced leaf again after the content changed.
#[derive(Debug, Clone, Copy)]
struct Check {
    leaf: usize,
    uid: u64,
    generation: u64,
    len: usize,
    hash: u64,
}

impl Check {
    /// At least this much of a leaf's text (or all of it, if shorter) must be unchanged for a
    /// position in it to survive; it tells apart leaves whose texts merely start alike.
    const CONTEXT: usize = 64;

    /// How much of `text` to check for a position at `offset`: up to the position (at least
    /// the context), but never the trailing line ending — Markdown paragraphs end with one, and
    /// streamed text lands after the last character, replacing it.
    fn length(layout: &TextLayout, offset: usize) -> usize {
        let content = layout.text().trim_end_matches(['\n', '\r']).len();
        layout.floor(offset.max(Self::CONTEXT).min(content))
    }

    fn matches(&self, text: &str) -> bool {
        text.len() >= self.len && text.is_char_boundary(self.len) && fingerprint(text, self.len) == self.hash
    }
}

#[derive(Debug, Clone, Copy)]
struct Outer {
    /// Which `scrollable` hook (in traversal order) it is.
    index: usize,
    /// Its visible viewport, in the area's coordinates.
    viewport: Rectangle,
}

#[derive(Default)]
struct State {
    anchor: Option<Unit>,
    focus: Option<Unit>,
    checks: Vec<Check>,
    drag: Option<Granularity>,
    /// The drag state last reported through `on_drag`.
    reported: bool,
    last_click: Option<mouse::Click>,
    /// Where the pointer was last seen during a drag (it may be outside the area).
    cursor: Option<Point>,
    /// Pixels per second to scroll the outer scrollable while dragging beyond its edge.
    autoscroll: f32,
    outer: Option<Outer>,
    last_frame: Option<Instant>,
    modifiers: keyboard::Modifiers,
    /// Whether any leaf may still hold a highlight.
    painted: bool,
    /// Whether any leaf may still hold a find highlight.
    find_painted: bool,
    /// A hash of the recorded leaves' text, and whether it changed since `on_text_change` was
    /// last published.
    text_signature: Option<u64>,
    text_changed: bool,
}

impl State {
    fn range(&self) -> Option<(Position, Position)> {
        let (anchor, focus) = (self.anchor?, self.focus?);
        Some((anchor.0.min(focus.0), anchor.1.max(focus.1)))
    }

    fn has_selection(&self) -> bool {
        self.range().is_some_and(|(start, end)| start != end)
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.checks.clear();
        self.drag = None;
        self.autoscroll = 0.0;
        self.last_frame = None;
    }

    fn positions(&self) -> impl Iterator<Item = Position> + '_ {
        self.anchor.into_iter().chain(self.focus).flat_map(|(a, b)| [a, b])
    }

    /// Remembers enough of every referenced leaf to recognise it after the content changes.
    fn record(&mut self, leaves: &[Leaf]) {
        let positions: Vec<Position> = self.positions().collect();
        self.checks.clear();
        for position in positions {
            let Some(leaf) = leaves.get(position.leaf) else {
                continue;
            };
            let text = leaf.layout.text();
            let len = Check::length(&leaf.layout, position.offset);
            match self.checks.iter_mut().find(|c| c.leaf == position.leaf) {
                Some(check) if len > check.len => {
                    check.len = len;
                    check.hash = fingerprint(text, len);
                }
                Some(_) => {}
                None => self.checks.push(Check { leaf: position.leaf, uid: leaf.uid, generation: leaf.generation, len, hash: fingerprint(text, len) }),
            }
        }
    }

    /// Re-finds the referenced leaves in freshly collected content. A leaf that only grew at its
    /// end (streaming) keeps the selection; one that moved (content inserted or removed before
    /// it) is found again nearby, by its tree slot or by its text; anything else is gone and the
    /// selection must be cleared (returns `false`).
    fn revalidate(&mut self, leaves: &[Leaf]) -> bool {
        if self.anchor.is_none() {
            return true;
        }
        let mut moves = Vec::new();
        for check in &self.checks {
            let here = leaves.get(check.leaf);
            if here.is_some_and(|l| (l.uid == check.uid && l.generation == check.generation) || check.matches(l.layout.text())) {
                continue;
            }
            let found = nearest(leaves, check.leaf, |l| l.uid == check.uid && check.matches(l.layout.text()))
                .or_else(|| nearest(leaves, check.leaf, |l| check.matches(l.layout.text())));
            match found {
                Some(leaf) => moves.push((check.leaf, leaf)),
                None => return false,
            }
        }
        let remap = |position: Position| -> Option<Position> {
            let leaf = moves.iter().find(|(from, _)| *from == position.leaf).map_or(position.leaf, |(_, to)| *to);
            let layout = &leaves.get(leaf)?.layout;
            Some(Position { leaf, offset: layout.floor(position.offset) })
        };
        let unit = |(a, b): Unit| Some((remap(a)?, remap(b)?));
        match (self.anchor.and_then(unit), self.focus.and_then(unit)) {
            (Some(anchor), Some(focus)) => {
                self.anchor = Some(anchor);
                self.focus = Some(focus);
                self.record(leaves);
                true
            }
            _ => false,
        }
    }
}

fn nearest(leaves: &[Leaf], around: usize, matches: impl Fn(&Leaf) -> bool) -> Option<usize> {
    (0..leaves.len())
        .find_map(|distance| [around.checked_add(distance), around.checked_sub(distance)].into_iter().flatten().find(|&k| leaves.get(k).is_some_and(&matches)))
}

/// One leaf as collected: its text layout and where it is, in the area's coordinates.
#[derive(Debug, Clone)]
struct Leaf {
    layout: TextLayout,
    uid: u64,
    generation: u64,
    /// The leaf's layout bounds, translated by every scrollable between it and the area.
    bounds: Rectangle,
    /// The visible part of the scrollables between it and the area, if any.
    clip: Option<Rectangle>,
}

impl Leaf {
    fn visible(&self) -> Option<Rectangle> {
        match self.clip {
            Some(clip) => self.bounds.intersection(&clip),
            None => Some(self.bounds),
        }
    }

    fn hit(&self, ordinal: usize, point: Point) -> (Position, Affinity) {
        let origin = self.layout.origin(self.bounds);
        let (offset, affinity) = self.layout.hit(point - (origin - Point::ORIGIN));
        (Position { leaf: ordinal, offset }, affinity)
    }
}

#[derive(Debug, Default)]
struct Collected {
    leaves: Vec<Leaf>,
    outer: Option<Outer>,
}

/// Maps a point (area coordinates) to a text position: inside a leaf, beside one on the same
/// row, or — in a gap — the boundary before the first leaf below. The flag says whether the
/// point was on (or beside) text, so word and line units apply.
fn locate(leaves: &[Leaf], point: Point) -> Option<(Position, Affinity, bool)> {
    let last = leaves.len().checked_sub(1)?;
    for (k, leaf) in leaves.iter().enumerate() {
        if leaf.visible().is_some_and(|v| v.contains(point)) {
            let (position, affinity) = leaf.hit(k, point);
            return Some((position, affinity, true));
        }
    }
    let mut beside: Option<(f32, usize, Rectangle)> = None;
    for (k, leaf) in leaves.iter().enumerate() {
        let Some(v) = leaf.visible() else { continue };
        if point.y >= v.y && point.y < v.y + v.height {
            let distance = if point.x < v.x { v.x - point.x } else { point.x - (v.x + v.width) };
            if beside.is_none_or(|(d, ..)| distance < d) {
                beside = Some((distance, k, v));
            }
        }
    }
    if let Some((_, k, v)) = beside {
        let x = point.x.clamp(v.x, v.x + v.width);
        let (position, affinity) = leaves[k].hit(k, Point::new(x, point.y));
        return Some((position, affinity, true));
    }
    let position = leaves
        .iter()
        .position(|leaf| leaf.bounds.y > point.y)
        .map_or(Position { leaf: last, offset: leaves[last].layout.text().len() }, |leaf| Position { leaf, offset: 0 });
    Some((position, Affinity::After, false))
}

fn unit(leaves: &[Leaf], granularity: Granularity, at: (Position, Affinity, bool)) -> Unit {
    let (position, affinity, on_text) = at;
    let layout = &leaves[position.leaf].layout;
    let (start, end) = match granularity {
        _ if !on_text => return (position, position),
        Granularity::Char => return (position, position),
        Granularity::Word => layout.word_at(position.offset, affinity),
        Granularity::Line => layout.line_at(position.offset),
    };
    let leaf = position.leaf;
    (Position { leaf, offset: start }, Position { leaf, offset: end })
}

/// Two leaves side by side on one row (a bullet and its item, two table cells) are joined by a
/// space; a leaf below the previous one starts a new line.
fn separator(previous: &Leaf, next: &Leaf) -> &'static str {
    let (a, b) = (previous.bounds, next.bounds);
    let same_row = b.y < a.y + a.height - 0.5 && b.y + b.height > a.y + 0.5;
    if same_row && b.x >= a.x + a.width - 0.5 { " " } else { "\n" }
}

/// The selected text in reading order. Empty slices at either end (a range that starts at the
/// end of one leaf or stops at the start of the next) add nothing, not even a separator.
fn extract(leaves: &[Leaf], (start, end): (Position, Position)) -> String {
    let mut parts: Vec<(usize, &str)> = Vec::new();
    let last = end.leaf.min(leaves.len().saturating_sub(1));
    for k in start.leaf..=last {
        let Some(leaf) = leaves.get(k) else { break };
        let text = leaf.layout.text();
        let from = if k == start.leaf { leaf.layout.floor(start.offset) } else { 0 };
        let to = if k == end.leaf { leaf.layout.floor(end.offset) } else { text.len() };
        parts.push((k, text.get(from.min(to)..to).unwrap_or("")));
    }
    while parts.first().is_some_and(|(_, part)| part.is_empty()) {
        parts.remove(0);
    }
    while parts.last().is_some_and(|(_, part)| part.is_empty()) {
        parts.pop();
    }
    let mut out = String::new();
    for (i, (k, part)) in parts.iter().enumerate() {
        if i > 0 {
            match separator(&leaves[parts[i - 1].0], &leaves[*k]) {
                // a Markdown paragraph ends with its own newline: no second one
                "\n" if out.ends_with('\n') => {}
                // nor a newline inside a row (a list item's or a cell's paragraph)
                " " => {
                    out.truncate(out.trim_end_matches(['\n', '\r']).len());
                    out.push(' ');
                }
                separator => out.push_str(separator),
            }
        }
        out.push_str(part);
    }
    out
}

fn autoscroll_speed(beyond: f32) -> f32 {
    beyond.signum() * (150.0 + beyond.abs() * 12.0)
}

impl<Message> Area<'_, Message> {
    fn collect(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> Collected {
        let mut collected = Collected::default();
        let mut operation = Collect { collected: &mut collected, scrollables: 0, frame: Frame::default(), stack: vec![Frame::default()] };
        self.content.as_widget_mut().operate(tree, layout, renderer, &mut operation);
        collected
    }

    /// Hands every leaf its part of `range`; returns whether any leaf now shows a highlight.
    fn paint(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, range: Option<(Position, Position)>) -> bool {
        let mut operation = Paint { range, ordinal: 0, painted: false };
        self.content.as_widget_mut().operate(tree, layout, renderer, &mut operation);
        operation.painted
    }

    fn paint_find(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> bool {
        let mut recorded = Vec::new();
        let mut operation = PaintFind {
            query: self.find.as_ref(),
            current_row: None,
            last_row: None,
            row_seen: 0,
            painted: false,
            entered_scrollable: None,
            scrollable_stack: Vec::new(),
            visible_leaves: self.visible_leaves.is_some().then_some(&mut recorded),
        };
        self.content.as_widget_mut().operate(tree, layout, renderer, &mut operation);
        let painted = operation.painted;
        if let Some(slot) = self.visible_leaves {
            *slot.borrow_mut() = recorded;
        }
        painted
    }

    fn sync_selection(&self, state: &State, leaves: &[Leaf]) {
        if let Some(slot) = self.selection {
            *slot.borrow_mut() = state.range().filter(|(a, b)| a != b).map(|r| extract(leaves, r)).filter(|s| !s.is_empty());
        }
    }

    fn clear(&mut self, state: &mut State, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> bool {
        let had = state.anchor.is_some() || state.painted;
        state.reset();
        if state.painted {
            state.painted = self.paint(tree, layout, renderer, None);
        }
        if let Some(slot) = self.selection {
            *slot.borrow_mut() = None;
        }
        had
    }

    fn press(&mut self, state: &mut State, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, point: Point) {
        let collected = self.collect(tree, layout, renderer);
        let click = mouse::Click::new(point, mouse::Button::Left, state.last_click);
        state.last_click = Some(click);
        let Some(at) = locate(&collected.leaves, point) else {
            let _ = self.clear(state, tree, layout, renderer);
            return;
        };
        let granularity = match click.kind() {
            mouse::click::Kind::Single => Granularity::Char,
            mouse::click::Kind::Double => Granularity::Word,
            mouse::click::Kind::Triple => Granularity::Line,
        };
        let unit = unit(&collected.leaves, granularity, at);
        let extend = granularity == Granularity::Char && state.modifiers.shift() && state.anchor.is_some() && state.revalidate(&collected.leaves);
        if !extend {
            state.anchor = Some(unit);
        }
        state.focus = Some(unit);
        state.drag = Some(granularity);
        state.cursor = Some(point);
        state.outer = collected.outer;
        state.autoscroll = 0.0;
        state.record(&collected.leaves);
        state.painted = self.paint(tree, layout, renderer, state.range());
        self.sync_selection(state, &collected.leaves);
    }

    /// Moves the focus to where the pointer is now. Returns whether the selection changed.
    fn drag(&mut self, state: &mut State, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, viewport: &Rectangle) -> bool {
        let (Some(granularity), Some(point)) = (state.drag, state.cursor) else {
            return false;
        };
        let collected = self.collect(tree, layout, renderer);
        if !state.revalidate(&collected.leaves) {
            return self.clear(state, tree, layout, renderer);
        }
        // select what is visible; past the conversation's top or bottom edge, scroll it
        let mut target = Point::new(point.x.clamp(viewport.x, viewport.x + viewport.width), point.y.clamp(viewport.y, viewport.y + viewport.height));
        state.outer = collected.outer;
        state.autoscroll = 0.0;
        if let Some(outer) = collected.outer {
            let (top, bottom) = (outer.viewport.y, outer.viewport.y + outer.viewport.height);
            if point.y < top {
                state.autoscroll = autoscroll_speed(point.y - top);
            } else if point.y >= bottom {
                state.autoscroll = autoscroll_speed(point.y - bottom + 1.0);
            }
            target.y = target.y.clamp(top, (bottom - 1.0).max(top));
        }
        let Some(at) = locate(&collected.leaves, target) else {
            return false;
        };
        let focus = unit(&collected.leaves, granularity, at);
        if state.focus == Some(focus) {
            return false;
        }
        state.focus = Some(focus);
        state.record(&collected.leaves);
        state.painted = self.paint(tree, layout, renderer, state.range());
        self.sync_selection(state, &collected.leaves);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn autoscroll(
        &mut self,
        state: &mut State,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        now: Instant,
        viewport: &Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        let (Some(_), Some(outer)) = (state.drag, state.outer) else {
            state.last_frame = None;
            return;
        };
        if state.autoscroll == 0.0 {
            state.last_frame = None;
            return;
        }
        let elapsed = match state.last_frame {
            Some(last) if now > last => (now - last).min(Duration::from_millis(100)),
            Some(_) => Duration::ZERO,
            None => Duration::from_millis(16),
        };
        state.last_frame = Some(now);
        let mut operation = ScrollBy { target: outer.index, seen: 0, dy: state.autoscroll * elapsed.as_secs_f32() };
        self.content.as_widget_mut().operate(tree, layout, renderer, &mut operation);
        let _ = self.drag(state, tree, layout, renderer, viewport);
        shell.request_redraw();
    }

    fn selected_text(&mut self, state: &mut State, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> Option<String> {
        let collected = self.collect(tree, layout, renderer);
        if !state.revalidate(&collected.leaves) {
            let _ = self.clear(state, tree, layout, renderer);
            return None;
        }
        Some(extract(&collected.leaves, state.range()?)).filter(|text| !text.is_empty())
    }

    /// The pointer and keyboard, after the content has had the event.
    #[allow(clippy::too_many_arguments)]
    fn handle(
        &mut self,
        state: &mut State,
        child: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        content: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let captured = shell.is_event_captured();
        match event {
            // a press the content took (a button, a link, a scrollbar) is not a selection's
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if !captured => match cursor.position_over(layout.bounds()) {
                Some(point) => {
                    self.press(state, child, content, renderer, point);
                    shell.capture_event();
                    shell.request_redraw();
                }
                None => {
                    if self.clear(state, child, content, renderer) {
                        shell.request_redraw();
                    }
                }
            },
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.drag.is_some() => {
                // a scrollable hands its content a levitating cursor once the pointer leaves
                // it; the drag goes on wherever the pointer is
                if let Some(point) = cursor.land().position() {
                    state.cursor = Some(point);
                    if self.drag(state, child, content, renderer, viewport) || state.autoscroll != 0.0 {
                        shell.request_redraw();
                    }
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { .. }) if state.drag.is_some() => {
                // the text moved under a still pointer
                if self.drag(state, child, content, renderer, viewport) {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some() {
                    state.autoscroll = 0.0;
                    state.last_frame = None;
                    // the X11 / Wayland primary selection, as toolkits set it
                    if state.has_selection()
                        && let Some(text) = self.selected_text(state, child, content, renderer)
                    {
                        clipboard.write(clipboard::Kind::Primary, text);
                    }
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) if !captured => {
                let command = modifiers.command() || state.modifiers.command();
                match key.as_ref() {
                    keyboard::Key::Character("c") if command && state.has_selection() => {
                        if let Some(text) = self.selected_text(state, child, content, renderer) {
                            match &self.on_copy {
                                Some(on_copy) => shell.publish(on_copy(text)),
                                None => clipboard.write(clipboard::Kind::Standard, text),
                            }
                            shell.capture_event();
                        }
                    }
                    keyboard::Key::Character("a") if command && state.has_selection() => {
                        let collected = self.collect(child, content, renderer);
                        if let Some(last) = collected.leaves.len().checked_sub(1) {
                            let start = Position::default();
                            let end = Position { leaf: last, offset: collected.leaves[last].layout.text().len() };
                            state.anchor = Some((start, start));
                            state.focus = Some((end, end));
                            state.record(&collected.leaves);
                            state.painted = self.paint(child, content, renderer, state.range());
                            self.sync_selection(state, &collected.leaves);
                            shell.capture_event();
                            shell.request_redraw();
                        }
                    }
                    keyboard::Key::Named(keyboard::key::Named::Escape) if state.anchor.is_some() => {
                        let _ = self.clear(state, child, content, renderer);
                        shell.capture_event();
                        shell.request_redraw();
                    }
                    _ => {}
                }
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            _ => {}
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Area<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let Tree { state, children, .. } = tree;
        let state = state.downcast_mut::<State>();
        let child = &mut children[0];
        let node = self.content.as_widget_mut().layout(child, renderer, limits);
        let layout = Layout::new(&node);
        // text may have streamed in, been replaced or rewrapped: re-find the selection and
        // repaint it before anything is drawn from stale offsets
        if state.anchor.is_some() || state.painted {
            let collected = self.collect(child, layout, renderer);
            if !state.revalidate(&collected.leaves) {
                state.reset();
            }
            state.painted = self.paint(child, layout, renderer, state.range());
            self.sync_selection(state, &collected.leaves);
        }
        if self.find.is_some() || state.find_painted || self.visible_leaves.is_some() {
            state.find_painted = self.paint_find(child, layout, renderer);
        }
        if self.on_text_change.is_some()
            && let Some(slot) = self.visible_leaves
        {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for leaf in slot.borrow().iter() {
                leaf.layout.text().hash(&mut hasher);
            }
            let signature = hasher.finish();
            if state.text_signature != Some(signature) {
                state.text_changed |= state.text_signature.is_some();
                state.text_signature = Some(signature);
            }
        }
        layout::Node::with_children(node.size(), vec![node])
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout.children().next().expect("area content"), cursor, viewport);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let content = layout.children().next().expect("area content");
        let Tree { state, children, .. } = tree;
        let state = state.downcast_mut::<State>();
        let child = &mut children[0];

        if std::mem::take(&mut state.text_changed)
            && let Some(on_text_change) = &self.on_text_change
        {
            shell.publish(on_text_change());
        }
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            self.autoscroll(state, child, content, renderer, *now, viewport, shell);
        }
        // the content first: buttons, links and scrollbars keep their presses
        self.content.as_widget_mut().update(child, event, content, cursor, renderer, clipboard, shell, viewport);
        self.handle(state, child, event, layout, content, cursor, renderer, clipboard, shell, viewport);
        // a drag can also end in layout (its text went away); report whatever changed
        let dragging = state.drag.is_some();
        if dragging != state.reported {
            state.reported = dragging;
            if let Some(on_drag) = &self.on_drag {
                shell.publish(on_drag(dragging));
            }
        }
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &Renderer) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().drag.is_some() {
            return mouse::Interaction::Text;
        }
        self.content.as_widget().mouse_interaction(&tree.children[0], layout.children().next().expect("area content"), cursor, viewport, renderer)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout.children().next().expect("area content"), renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout.children().next().expect("area content"), renderer, viewport, translation)
    }
}

impl<'a, Message: 'a> From<Area<'a, Message>> for Element<'a, Message> {
    fn from(area: Area<'a, Message>) -> Self {
        Element::new(area)
    }
}

// --- operations ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct Frame {
    clip: Option<Rectangle>,
    translation: Vector,
    depth: usize,
}

/// Collects every leaf with its bounds in the area's coordinates. A `scrollable` hook changes
/// the frame for the `traverse` that follows it; `traverse` restores the enclosing frame after
/// (the scheme of `iced_selector::Finder`).
struct Collect<'a> {
    collected: &'a mut Collected,
    scrollables: usize,
    frame: Frame,
    stack: Vec<Frame>,
}

impl Operation for Collect<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        self.stack.push(self.frame);
        operate(self);
        let _ = self.stack.pop();
        self.frame = self.stack.last().copied().unwrap_or_default();
    }

    fn scrollable(&mut self, _id: Option<&Id>, bounds: Rectangle, content_bounds: Rectangle, translation: Vector, _state: &mut dyn Scrollable) {
        let viewport = bounds + self.frame.translation;
        let visible = match self.frame.clip {
            Some(clip) => clip.intersection(&viewport).unwrap_or(Rectangle::new(viewport.position(), Size::ZERO)),
            None => viewport,
        };
        if self.frame.depth == 0 && self.collected.outer.is_none() && content_bounds.height > bounds.height + 0.5 {
            self.collected.outer = Some(Outer { index: self.scrollables, viewport: visible });
        }
        self.scrollables += 1;
        self.frame = Frame { clip: Some(visible), translation: self.frame.translation - translation, depth: self.frame.depth + 1 };
    }

    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        if let Some(selectable) = state.downcast_ref::<Selectable>() {
            if !selectable.is_mouse_selectable() {
                return;
            }
            self.collected.leaves.push(Leaf {
                layout: selectable.layout().clone(),
                uid: selectable.uid(),
                generation: selectable.generation(),
                bounds: bounds + self.frame.translation,
                clip: self.frame.clip,
            });
        }
    }
}

/// Gives every leaf, in the same order as [`Collect`], its byte range of the selection.
struct Paint {
    range: Option<(Position, Position)>,
    ordinal: usize,
    painted: bool,
}

impl Operation for Paint {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn custom(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Any) {
        if let Some(selectable) = state.downcast_mut::<Selectable>() {
            if !selectable.is_mouse_selectable() {
                return;
            }
            let k = self.ordinal;
            self.ordinal += 1;
            let part = self.range.and_then(|(start, end)| {
                (start.leaf <= k && k <= end.leaf)
                    .then_some((if k == start.leaf { start.offset } else { 0 }, if k == end.leaf { end.offset } else { usize::MAX }))
            });
            selectable.set_highlight(part);
            self.painted |= selectable.is_highlighted();
        }
    }
}

/// Gives every leaf (including `find_only` leaves) its find match byte ranges and current-match
/// index within the leaf, and optionally records each rendered leaf's vertical position in the
/// outer scrollable.
struct PaintFind<'a> {
    query: Option<&'a FindQuery>,
    current_row: Option<usize>,
    last_row: Option<usize>,
    row_seen: usize,
    painted: bool,
    /// A scrollable just met: its top and how far it is scrolled, for the traversal of its
    /// content; the stack holds those entered, the outermost first.
    entered_scrollable: Option<(f32, f32)>,
    scrollable_stack: Vec<(f32, f32)>,
    visible_leaves: Option<&'a mut Vec<VisibleLeaf>>,
}

impl Operation for PaintFind<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        match self.entered_scrollable.take() {
            Some(frame) => {
                self.scrollable_stack.push(frame);
                operate(self);
                self.scrollable_stack.pop();
            }
            None => operate(self),
        }
    }

    fn scrollable(&mut self, _id: Option<&Id>, bounds: Rectangle, _content_bounds: Rectangle, translation: Vector, _state: &mut dyn Scrollable) {
        self.entered_scrollable = Some((bounds.y, translation.y));
    }

    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        if let Some(scope) = state.downcast_ref::<crate::rows::RowScope>() {
            self.current_row = scope.0;
            if let Some(row) = scope.0
                && self.last_row != Some(row)
            {
                self.last_row = Some(row);
                self.row_seen = 0;
            }
            return;
        }
        let Some(selectable) = state.downcast_mut::<Selectable>() else {
            return;
        };
        if let Some(out) = self.visible_leaves.as_deref_mut() {
            let origin = selectable.origin(bounds);
            let (scroll_y, view_top) = self.scrollable_stack.first().copied().unwrap_or_default();
            out.push(VisibleLeaf { row: self.current_row, origin_y: origin.y - scroll_y, view_top, layout: selectable.layout().clone() });
        }
        let Some(query) = self.query.filter(|q| !q.needle.is_empty()) else {
            selectable.set_find(Vec::new(), None);
            return;
        };
        let ranges = crate::find::match_ranges(selectable.text(), &query.needle, query.case_sensitive);
        let count = ranges.len();
        let current_in_leaf = match (query.current, self.current_row) {
            (Some((target_row, occ)), Some(row)) if row == target_row && (self.row_seen..self.row_seen + count).contains(&occ) => Some(occ - self.row_seen),
            _ => None,
        };
        if self.current_row.is_some() {
            self.row_seen += count;
        }
        selectable.set_find(ranges, current_in_leaf);
        self.painted |= selectable.has_find();
    }
}

/// Scrolls the `target`-th scrollable (traversal order, as counted by [`Collect`]).
struct ScrollBy {
    target: usize,
    seen: usize,
    dy: f32,
}

impl Operation for ScrollBy {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.seen <= self.target {
            operate(self);
        }
    }

    fn scrollable(&mut self, _id: Option<&Id>, bounds: Rectangle, content_bounds: Rectangle, _translation: Vector, state: &mut dyn Scrollable) {
        if self.seen == self.target {
            state.scroll_by(AbsoluteOffset { x: 0.0, y: self.dy }, bounds, content_bounds);
        }
        self.seen += 1;
    }
}
