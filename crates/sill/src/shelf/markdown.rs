//! Markdown → a message of segments (SPEC §4.5): prose in text views with tags (headings, emphasis,
//! inline code, lists with hanging indents, links, block quotes, hairline rules, task markers, tables
//! as real columns on Pango tab stops measured with the view's own font), and each fenced code block
//! as a GtkSourceView *between* the prose views, highlighted in the theme's scheme. No HTML, no
//! images (shown as their alt text).
//!
//! Code is a sibling, never a child anchored inside the text: an anchored child only ever gets a
//! fixed size, and a fixed size outlives every resize — it held the column wider than the viewport
//! and everything in it ran past the right edge (2026-10-30). Siblings take the column's width like
//! everything else.
//!
//! Re-rendering the whole message on every streamed chunk is fine: messages are small, the parser
//! is fast, and the segment widgets are reused by position. A theme or zoom change renders every live
//! message again; a width change renders the ones holding a table (`relayout`).

use std::cell::RefCell;

use gtk4 as gtk;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sourceview5::prelude::*;

use super::theme;

/// Gap between table columns, logical px at zoom 1.
const COL_GAP: f64 = 22.0;
/// Inner left/right margin of a prose view; code blocks line up with it (CSS `.sill-code`).
const PROSE_MARGIN: i32 = 8;

/// A live message: its box, kind class, source, and the width its tables were laid out for
/// (`None` = no table; `Some(0)` = laid out before the message had a width).
struct Rendered {
    msg: gtk::glib::WeakRef<gtk::Box>,
    class: String,
    md: String,
    table_width: Option<i32>,
}

thread_local! {
    static MESSAGES: RefCell<Vec<Rendered>> = const { RefCell::new(Vec::new()) };
    static SCHEMES_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn live() -> Vec<(gtk::Box, String, String, Option<i32>)> {
    MESSAGES.with(|m| {
        let mut l = m.borrow_mut();
        l.retain(|r| r.msg.upgrade().is_some());
        l.iter().filter_map(|r| r.msg.upgrade().map(|b| (b, r.class.clone(), r.md.clone(), r.table_width))).collect()
    })
}

/// Render every live message again with the current theme and zoom.
pub fn retheme() {
    for (msg, class, md, _) in live() {
        render(&msg, &class, &md);
    }
}

/// The conversation's width changed (shelf resized, a scrollbar came or went): lay tables out again
/// where the width they were laid out for no longer holds. Call it outside an allocation.
pub fn relayout() {
    for (msg, class, md, laid) in live() {
        let now = msg.width();
        if let Some(laid) = laid
            && now > 0
            && (now - laid).abs() > 4
        {
            render(&msg, &class, &md);
        }
    }
}

fn remember(msg: &gtk::Box, class: &str, md: &str, table_width: Option<i32>) {
    MESSAGES.with(|m| {
        let mut l = m.borrow_mut();
        match l.iter_mut().find(|r| r.msg.upgrade().as_ref() == Some(msg)) {
            Some(r) => {
                r.md = md.to_string();
                r.table_width = table_width;
            }
            None => l.push(Rendered { msg: msg.downgrade(), class: class.to_string(), md: md.to_string(), table_width }),
        }
    });
}

fn scale(px: f64) -> i32 {
    (px * theme::text_scale()).round() as i32
}

/// Install the fixed tags (idempotent); colours come from the theme each time.
pub fn install_tags(buffer: &gtk::TextBuffer) {
    let colors = theme::tag_colors();
    let table = buffer.tag_table();
    let tag = |name: &str| -> gtk::TextTag {
        match table.lookup(name) {
            Some(t) => t,
            None => {
                let t = gtk::TextTag::new(Some(name));
                table.add(&t);
                t
            }
        }
    };
    for (name, s, above) in [("h1", 1.35, 14.0), ("h2", 1.2, 12.0), ("h3", 1.1, 10.0), ("h4", 1.0, 8.0)] {
        let t = tag(name);
        t.set_weight(700);
        t.set_scale(s);
        t.set_pixels_above_lines(scale(above));
        t.set_pixels_below_lines(scale(2.0));
    }
    tag("bold").set_weight(700);
    tag("italic").set_style(gtk::pango::Style::Italic);
    tag("thought").set_style(gtk::pango::Style::Italic);
    tag("strike").set_strikethrough(true);
    {
        let t = tag("code");
        t.set_family(Some("monospace"));
        t.set_background(Some(&colors.code_bg));
    }
    {
        let t = tag("quote");
        t.set_left_margin(scale(14.0));
        t.set_style(gtk::pango::Style::Italic);
        t.set_foreground(colors.dim.as_deref());
    }
    {
        let t = tag("link");
        t.set_underline(gtk::pango::Underline::Single);
        t.set_foreground(Some(&colors.link));
    }
    {
        // a hairline: a one-space paragraph in a 1 pt font with the border colour as its background
        // (spacing around it comes from the neighbours: pixels above/below would be painted too)
        for name in ["rule", "thead-rule"] {
            let t = tag(name);
            t.set_size_points(1.0);
            t.set_paragraph_background(Some(&colors.rule));
            t.set_pixels_above_lines(0);
            t.set_pixels_below_lines(0);
        }
    }
    {
        // a table too narrow for columns: each row a record, its first cell as the title
        let t = tag("table-title");
        t.set_weight(700);
        t.set_pixels_above_lines(scale(6.0));
        let t = tag("table-field");
        t.set_left_margin(scale(16.0));
        let t = tag("table-key");
        t.set_foreground(colors.dim.as_deref());
    }
    {
        let t = tag("cell");
        t.set_pixels_above_lines(scale(3.0));
        t.set_pixels_below_lines(0);
        // continuation lines of a wrapped row sit tight under their first line
        let t = tag("cell-cont");
        t.set_pixels_above_lines(0);
        t.set_pixels_below_lines(0);
    }
    // list items: hanging indent per depth, the marker in the overhang
    for depth in 0..4 {
        let t = tag(&format!("li{depth}"));
        t.set_left_margin(scale(20.0 + 20.0 * depth as f64));
        t.set_indent(-scale(16.0));
        t.set_pixels_below_lines(scale(1.0));
        let t = tag(&format!("ol{depth}"));
        t.set_left_margin(scale(26.0 + 20.0 * depth as f64));
        t.set_indent(-scale(22.0));
        t.set_pixels_below_lines(scale(1.0));
    }
}

fn ensure_schemes() {
    SCHEMES_READY.with(|r| {
        if !r.get() {
            if let Some(dir) = theme::write_source_schemes() {
                sourceview5::StyleSchemeManager::default().append_search_path(&dir.to_string_lossy());
            }
            r.set(true);
        }
    });
}

/// A prose segment: wrapped, read-only, with the Markdown tags; `class` is the message kind
/// (`sill-agent`, `sill-thought`).
fn prose_view(class: &str) -> gtk::TextView {
    let tv = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(PROSE_MARGIN)
        .right_margin(PROSE_MARGIN)
        .top_margin(6)
        .bottom_margin(6)
        .pixels_inside_wrap(3) // a little leading: prose, not a terminal
        .pixels_below_lines(2)
        .build();
    tv.add_css_class(class);
    install_tags(&tv.buffer());
    tv
}

/// A code segment: read-only, wrapped at the column, highlighted.
fn code_view() -> sourceview5::View {
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_highlight_syntax(true);
    buffer.set_highlight_matching_brackets(false);
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(scale(10.0));
    view.set_right_margin(scale(10.0));
    view.set_top_margin(scale(6.0));
    view.set_bottom_margin(scale(6.0));
    view.add_css_class("sill-code");
    view
}

/// Fill a code segment: the fence's language, the theme's scheme, the text (left alone when the
/// same, so a streamed message doesn't redraw finished blocks).
fn fill_code_view(view: &sourceview5::View, lang: &str, code: &str) {
    ensure_schemes();
    let Ok(buffer) = view.buffer().downcast::<sourceview5::Buffer>() else { return };
    let lm = sourceview5::LanguageManager::default();
    let lang = lang.split_whitespace().next().unwrap_or("");
    let language = if lang.is_empty() { None } else { lm.language(lang).or_else(|| lm.guess_language(Some(&format!("x.{lang}")), None)) };
    if buffer.language().map(|l| l.id()) != language.as_ref().map(|l| l.id()) {
        buffer.set_language(language.as_ref());
    }
    if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&theme::source_scheme(&theme::current())) {
        buffer.set_style_scheme(Some(&s));
    }
    let code = code.trim_end_matches('\n');
    if buffer.text(&buffer.start_iter(), &buffer.end_iter(), true) != code {
        buffer.set_text(code);
    }
}

fn trim_trailing_newlines(buffer: &gtk::TextBuffer) {
    let mut end = buffer.end_iter();
    while end.offset() > 0 {
        let prev = buffer.iter_at_offset(end.offset() - 1);
        if buffer.text(&prev, &end, true) == "\n" {
            buffer.delete(&mut prev.clone(), &mut end);
            end = buffer.end_iter();
        } else {
            break;
        }
    }
}

/// Where a render writes: the message's segments in order. The widgets already there are reused by
/// position while their kind matches; from the first mismatch on they are replaced.
struct Out<'a> {
    msg: &'a gtk::Box,
    class: &'a str,
    /// The next existing child to reuse.
    next: Option<gtk::Widget>,
    /// The prose view being written, if any (none at the start and right after a code block).
    cur: Option<gtk::TextView>,
}

impl<'a> Out<'a> {
    fn new(msg: &'a gtk::Box, class: &'a str) -> Self {
        Out { msg, class, next: msg.first_child(), cur: None }
    }

    fn drop_rest(&mut self) {
        let mut c = self.next.take();
        while let Some(w) = c {
            c = w.next_sibling();
            self.msg.remove(&w);
        }
    }

    /// The next existing child when it is exactly of type `ty`; otherwise drop it and the rest.
    fn reuse(&mut self, ty: gtk::glib::Type) -> Option<gtk::Widget> {
        let w = self.next.clone()?;
        if w.type_() == ty {
            self.next = w.next_sibling();
            Some(w)
        } else {
            self.drop_rest();
            None
        }
    }

    /// The prose view to write into, starting one when there is none.
    fn text(&mut self) -> gtk::TextView {
        if let Some(tv) = &self.cur {
            return tv.clone();
        }
        let tv = match self.reuse(gtk::TextView::static_type()).and_then(|w| w.downcast::<gtk::TextView>().ok()) {
            Some(tv) => {
                install_tags(&tv.buffer());
                tv.buffer().set_text("");
                tv
            }
            None => {
                let tv = prose_view(self.class);
                self.msg.append(&tv);
                tv
            }
        };
        self.cur = Some(tv.clone());
        tv
    }

    fn close_text(&mut self) {
        if let Some(tv) = self.cur.take() {
            trim_trailing_newlines(&tv.buffer());
        }
    }

    fn code(&mut self, lang: &str, code: &str) {
        self.close_text();
        let view = match self.reuse(sourceview5::View::static_type()).and_then(|w| w.downcast::<sourceview5::View>().ok()) {
            Some(v) => v,
            None => {
                let v = code_view();
                self.msg.append(&v);
                v
            }
        };
        fill_code_view(&view, lang, code);
    }

    fn finish(mut self) {
        self.close_text();
        self.drop_rest();
    }

    fn at_line_start(&self) -> bool {
        match &self.cur {
            None => true,
            Some(tv) => {
                let b = tv.buffer();
                let end = b.end_iter();
                end.offset() == 0 || b.text(&b.iter_at_offset(end.offset() - 1), &end, true) == "\n"
            }
        }
    }

    fn offset(&mut self) -> i32 {
        self.text().buffer().end_iter().offset()
    }

    fn insert(&mut self, text: &str, tags: &[String]) {
        let buffer = self.text().buffer();
        let mut end = buffer.end_iter();
        let start_off = end.offset();
        buffer.insert(&mut end, text);
        if !tags.is_empty() {
            let start = buffer.iter_at_offset(start_off);
            let end = buffer.end_iter();
            for t in tags {
                buffer.apply_tag_by_name(t, &start, &end);
            }
        }
    }

    /// Separate a new block from what came before in the same prose view (a fresh view needs none).
    fn block_start(&mut self, need_newline: &mut bool) {
        if let Some(tv) = self.cur.clone() {
            if !self.at_line_start() {
                self.insert("\n", &[]);
            }
            if *need_newline && tv.buffer().char_count() > 0 {
                self.insert("\n", &[]);
            }
        }
        *need_newline = false;
    }

    fn rule(&mut self, tag: &str) {
        if !self.at_line_start() {
            self.insert("\n", &[]);
        }
        self.insert(" \n", &[tag.to_string()]); // a space: an empty line gets no paragraph background
    }
}

/// Word-wrap `text` into lines no wider than `width` px in the view's font (a single word wider
/// than that stays whole on its own line).
fn wrap_cell(tv: &gtk::TextView, text: &str, width: i32, bold: bool) -> Vec<String> {
    if measure(tv, text, bold) <= width {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if line.is_empty() || measure(tv, &candidate, bold) <= width {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Width in px of `text` in the view's font (bold when asked), for table columns.
fn measure(tv: &gtk::TextView, text: &str, bold: bool) -> i32 {
    let layout = tv.create_pango_layout(Some(text));
    if bold {
        let attrs = gtk::pango::AttrList::new();
        attrs.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
        layout.set_attributes(Some(&attrs));
    }
    layout.pixel_size().0
}

/// Render `markdown` into the message box `msg` (a vertical box in the conversation column; it must
/// be in the window already, so measuring uses the right font). `class` is the message kind.
pub fn render(msg: &gtk::Box, class: &str, markdown: &str) {
    let mut out = Out::new(msg, class);
    let mut table_width: Option<i32> = None;
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, opts);

    let mut tags: Vec<String> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new(); // None = bullet, Some(n) = next number
    let mut need_newline = false; // a block ended; separate the next block with a blank line
    // fenced code: collected, then written as its own segment at the end tag
    let mut code: Option<(String, String)> = None; // (lang, text)
    // tables: collected as rows of cells, laid out with tab stops at the end tag
    let mut table: Option<(Vec<Alignment>, Vec<Vec<String>>)> = None;
    let mut in_table_cell = false;
    let mut table_index = 0usize;
    let mut item_marker: Option<(i32, i32)> = None; // the last list marker's offsets, replaced by a task box

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if list_stack.is_empty() {
                        out.block_start(&mut need_newline);
                    }
                }
                Tag::Heading { level, .. } => {
                    out.block_start(&mut need_newline);
                    tags.push(
                        match level {
                            HeadingLevel::H1 => "h1",
                            HeadingLevel::H2 => "h2",
                            HeadingLevel::H3 => "h3",
                            _ => "h4",
                        }
                        .into(),
                    );
                }
                Tag::BlockQuote(_) => {
                    out.block_start(&mut need_newline);
                    tags.push("quote".into());
                }
                Tag::CodeBlock(kind) => {
                    out.block_start(&mut need_newline);
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    if list_stack.is_empty() {
                        out.block_start(&mut need_newline);
                    }
                    list_stack.push(start);
                }
                Tag::Item => {
                    if !out.at_line_start() {
                        out.insert("\n", &[]);
                    }
                    let depth = list_stack.len().saturating_sub(1).min(3);
                    let (marker, tag) = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}.\u{2009}");
                            *n += 1;
                            (m, format!("ol{depth}"))
                        }
                        _ => ("•\u{2009} ".to_string(), format!("li{depth}")),
                    };
                    tags.push(tag);
                    let start = out.offset();
                    out.insert(&marker, &tags);
                    item_marker = Some((start, out.offset()));
                }
                Tag::Emphasis => tags.push("italic".into()),
                Tag::Strong => tags.push("bold".into()),
                Tag::Strikethrough => tags.push("strike".into()),
                Tag::Link { .. } => tags.push("link".into()),
                Tag::Image { .. } => out.insert("[image: ", &[]),
                Tag::Table(aligns) => {
                    out.block_start(&mut need_newline);
                    table = Some((aligns, Vec::new()));
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some((_, t)) = table.as_mut() {
                        t.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some((_, t)) = table.as_mut()
                        && let Some(row) = t.last_mut()
                    {
                        row.push(String::new());
                    }
                    in_table_cell = true;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    out.insert("\n", &[]);
                    if list_stack.is_empty() {
                        need_newline = true;
                    }
                }
                TagEnd::Heading(_) => {
                    tags.pop();
                    out.insert("\n", &[]);
                }
                TagEnd::BlockQuote(_) => {
                    tags.pop();
                    need_newline = true;
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = code.take() {
                        out.code(&lang, &text);
                    }
                    need_newline = true;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    if list_stack.is_empty() {
                        need_newline = true;
                    }
                }
                TagEnd::Item => {
                    tags.pop();
                    if !out.at_line_start() {
                        out.insert("\n", &[]);
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    tags.pop();
                }
                TagEnd::Image => out.insert("]", &[]),
                TagEnd::TableHead | TagEnd::TableRow => {}
                TagEnd::TableCell => in_table_cell = false,
                TagEnd::Table => {
                    if let Some((aligns, rows)) = table.take() {
                        let tv = out.text();
                        let buffer = tv.buffer();
                        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                        // column widths in the view's font; the header is bold
                        let widths: Vec<i32> = (0..ncols)
                            .map(|i| {
                                rows.iter()
                                    .enumerate()
                                    .map(|(ri, r)| measure(&tv, r.get(i).map(String::as_str).unwrap_or(""), ri == 0))
                                    .max()
                                    .unwrap_or(0)
                            })
                            .collect();
                        let mut gap = scale(COL_GAP);
                        // wider than the view: narrow the gap, then give the wide columns a share
                        // of what is left and wrap their cells (tab stops can't wrap by themselves)
                        // laid out for the message's width; 0 = not allocated yet (a guess),
                        // and `relayout` renders again once the real width is known
                        let laid = match out.msg.width() {
                            w if w > 0 => w,
                            _ => out.msg.parent().map(|p| p.width()).unwrap_or(0),
                        };
                        table_width = Some(laid);
                        let view_w = laid - 2 * PROSE_MARGIN;
                        let avail = if view_w > 100 { view_w - scale(16.0) } else { scale(620.0) };
                        let total = |widths: &[i32], gap: i32| widths.iter().sum::<i32>() + gap * (ncols.saturating_sub(1) as i32);
                        let mut widths = widths;
                        let mut stacked = false;
                        if total(&widths, gap) > avail {
                            gap = scale(12.0);
                        }
                        if total(&widths, gap) > avail && ncols > 0 {
                            // the narrowest a column can be: its longest word (a word can't wrap,
                            // and one that overruns its tab stop throws the rest of the row onto
                            // the next line), plus the slack wrap_cell keeps
                            let mins: Vec<i32> = (0..ncols)
                                .map(|i| {
                                    rows.iter()
                                        .enumerate()
                                        .map(|(ri, r)| {
                                            r.get(i).map(|c| c.split_whitespace().map(|w| measure(&tv, w, ri == 0)).max().unwrap_or(0)).unwrap_or(0)
                                        })
                                        .max()
                                        .unwrap_or(0)
                                        + scale(4.0)
                                })
                                .collect();
                            // even the longest words don't fit side by side: no column layout
                            // works at this width
                            stacked = mins.iter().sum::<i32>() + gap * (ncols.saturating_sub(1) as i32) > avail;
                            // water-filling: a column that fits in the fair share of what is left
                            // keeps its width, one whose longest word doesn't gets that word's
                            // width; the rest split the remainder equally
                            let mut space = avail - gap * (ncols.saturating_sub(1) as i32);
                            let mut fixed = vec![false; ncols];
                            loop {
                                let open = fixed.iter().filter(|f| !**f).count() as i32;
                                if open == 0 {
                                    break;
                                }
                                let fair = space / open;
                                let mut changed = false;
                                for i in 0..ncols {
                                    if fixed[i] {
                                        continue;
                                    }
                                    if widths[i] <= fair {
                                        fixed[i] = true;
                                        space -= widths[i];
                                        changed = true;
                                    } else if mins[i] >= fair {
                                        fixed[i] = true;
                                        widths[i] = mins[i];
                                        space -= mins[i];
                                        changed = true;
                                    }
                                }
                                if !changed {
                                    for i in 0..ncols {
                                        if !fixed[i] {
                                            widths[i] = fair;
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                        if stacked {
                            // one record per row: the first cell as its title, then a
                            // "Header: value" line per non-empty cell; columns come back through
                            // `relayout` once the view is wide enough
                            let head = rows.first().cloned().unwrap_or_default();
                            for r in rows.iter().skip(1) {
                                out.insert(&format!("{}\n", r.first().map(String::as_str).unwrap_or("")), &["table-title".to_string()]);
                                for i in 1..ncols {
                                    let val = r.get(i).map(String::as_str).unwrap_or("");
                                    if val.is_empty() {
                                        continue;
                                    }
                                    let key = head.get(i).map(String::as_str).unwrap_or("");
                                    let field = vec!["table-field".to_string()];
                                    let mut keyed = field.clone();
                                    keyed.push("table-key".into());
                                    out.insert(&format!("{key}: "), &keyed);
                                    out.insert(&format!("{val}\n"), &field);
                                }
                            }
                        } else {
                            // the first column starts at the margin (a tab at x = 0 would skip to the
                            // next stop), the others at their stops
                            let mut tabs = gtk::pango::TabArray::new(ncols.saturating_sub(1) as i32, true);
                            let mut x = widths.first().copied().unwrap_or(0) + gap;
                            for (i, w) in widths.iter().enumerate().skip(1) {
                                let align = match aligns.get(i) {
                                    Some(Alignment::Right) => gtk::pango::TabAlign::Right,
                                    Some(Alignment::Center) => gtk::pango::TabAlign::Center,
                                    _ => gtk::pango::TabAlign::Left,
                                };
                                // a right/centre stop sits at the column's far edge/middle
                                let pos = match align {
                                    gtk::pango::TabAlign::Right => x + w,
                                    gtk::pango::TabAlign::Center => x + w / 2,
                                    _ => x,
                                };
                                tabs.set_tab((i - 1) as i32, align, pos);
                                x += w + gap;
                            }
                            // one tag per table (named, so re-renders reuse it instead of piling up)
                            let name = format!("table{table_index}");
                            table_index += 1;
                            let ttag = match buffer.tag_table().lookup(&name) {
                                Some(t) => t,
                                None => {
                                    let t = gtk::TextTag::new(Some(&name));
                                    buffer.tag_table().add(&t);
                                    t
                                }
                            };
                            ttag.set_tabs(Some(&tabs));
                            ttag.set_left_margin(scale(4.0));
                            for (ri, r) in rows.iter().enumerate() {
                                // a row is one line per wrapped fragment; fragment k of every cell on line k
                                let wrapped: Vec<Vec<String>> = (0..ncols)
                                    .map(|i| wrap_cell(&tv, r.get(i).map(String::as_str).unwrap_or(""), widths[i] - scale(4.0), ri == 0))
                                    .collect();
                                let lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
                                for k in 0..lines {
                                    let cells: Vec<&str> = wrapped.iter().map(|c| c.get(k).map(String::as_str).unwrap_or("")).collect();
                                    let line = format!("{}\n", cells.join("\t"));
                                    let mut t = vec![name.clone(), if k == 0 { "cell".to_string() } else { "cell-cont".to_string() }];
                                    if ri == 0 {
                                        t.push("bold".into());
                                    }
                                    out.insert(&line, &t);
                                }
                                if ri == 0 {
                                    out.rule("thead-rule");
                                }
                            }
                        }
                    }
                    need_newline = true;
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, c)) = code.as_mut() {
                    c.push_str(&t);
                } else if in_table_cell {
                    if let Some(cell) = table.as_mut().and_then(|(_, t)| t.last_mut()).and_then(|r| r.last_mut()) {
                        cell.push_str(&t);
                    }
                } else {
                    out.insert(&t, &tags);
                }
            }
            Event::Code(c) => {
                if in_table_cell {
                    if let Some(cell) = table.as_mut().and_then(|(_, t)| t.last_mut()).and_then(|r| r.last_mut()) {
                        cell.push_str(&c);
                    }
                } else {
                    let mut t = tags.clone();
                    t.push("code".into());
                    out.insert(&c, &t);
                }
            }
            Event::SoftBreak => out.insert(" ", &tags),
            Event::HardBreak => out.insert("\n", &tags),
            Event::Rule => {
                out.block_start(&mut need_newline);
                out.rule("rule");
                out.insert("\n", &[]); // a blank line after, as before it
                need_newline = true;
            }
            Event::TaskListMarker(done) => {
                if let Some((a, b)) = item_marker.take()
                    && b == out.offset()
                {
                    let buffer = out.text().buffer();
                    buffer.delete(&mut buffer.iter_at_offset(a), &mut buffer.iter_at_offset(b));
                }
                out.insert(if done { "☑ " } else { "☐ " }, &tags)
            }
            Event::Html(h) | Event::InlineHtml(h) => out.insert(&h, &["code".to_string()]),
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
    out.finish();
    remember(msg, class, markdown, table_width);
}
