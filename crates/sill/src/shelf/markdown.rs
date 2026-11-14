//! Markdown → GtkTextView with tags (SPEC §4.5): headings, emphasis, inline code, lists with hanging
//! indents, links, block quotes, hairline rules, task markers; tables as real columns (Pango tab stops
//! measured with the view's own font); fenced code blocks as embedded GtkSourceView widgets with
//! syntax highlighting in the theme's scheme. No HTML, no images (shown as their alt text).
//!
//! Re-rendering the whole message on every streamed chunk is fine: messages are small and the parser
//! is fast. A theme or zoom change re-renders every live view too, so colours, schemes and tab stops
//! follow (the source is kept per view for that).

use gtk4 as gtk;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sourceview5::prelude::*;

use super::theme;

/// Gap between table columns, logical px at zoom 1.
const COL_GAP: f64 = 22.0;

thread_local! {
    /// Every live view and the source it shows, so a theme or zoom change can render it again.
    static VIEWS: std::cell::RefCell<Vec<(gtk::glib::WeakRef<gtk::TextView>, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Embedded code views per text view: an anchored child only gets its natural width, so they
    /// are sized to the text view by hand once it has one (and again whenever it is shown).
    static CODE_VIEWS: std::cell::RefCell<Vec<(gtk::glib::WeakRef<gtk::TextView>, Vec<gtk::Widget>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Views holding a table and the width their columns were laid out for (0 = a guess, the
    /// view had no width yet); re-rendered when the width turns out different (see `size_code_views`).
    static TABLE_WIDTHS: std::cell::RefCell<Vec<(gtk::glib::WeakRef<gtk::TextView>, i32)>> = const { std::cell::RefCell::new(Vec::new()) };
    static SCHEMES_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Render every live view again with the current theme and zoom.
pub fn retheme() {
    let list: Vec<(gtk::TextView, String)> = VIEWS.with(|v| {
        let mut l = v.borrow_mut();
        l.retain(|(w, _)| w.upgrade().is_some());
        l.iter().filter_map(|(w, md)| w.upgrade().map(|tv| (tv, md.clone()))).collect()
    });
    for (tv, md) in list {
        render(&tv, &md);
    }
}

fn remember(tv: &gtk::TextView, md: &str) {
    VIEWS.with(|v| {
        let mut l = v.borrow_mut();
        if let Some(e) = l.iter_mut().find(|(w, _)| w.upgrade().as_ref() == Some(tv)) {
            e.1 = md.to_string();
        } else {
            l.push((tv.downgrade(), md.to_string()));
        }
    });
}

/// Give the code views of `tv` its full width. Runs on the frame clock until the width is known,
/// then stops; `map` re-arms it (tab switch, shelf shown again). A message rendered before the view
/// had a width (replay, first chunk) and holding a table is rendered again then: its columns were
/// laid out for a guessed width.
fn size_code_views(tv: &gtk::TextView) {
    let weak = tv.downgrade();
    let frames = std::cell::Cell::new(0);
    tv.add_tick_callback(move |_, _| {
        let Some(tv) = weak.upgrade() else { return gtk::glib::ControlFlow::Break };
        let w = content_width(&tv);
        if w > 40 {
            CODE_VIEWS.with(|c| {
                if let Some((_, views)) = c.borrow().iter().find(|(t, _)| t.upgrade().as_ref() == Some(&tv)) {
                    for v in views {
                        v.set_size_request(w, -1);
                    }
                }
            });
            let stale_table = TABLE_WIDTHS.with(|u| {
                let mut l = u.borrow_mut();
                l.retain(|(t, _)| t.upgrade().is_some());
                l.iter().any(|(t, laid)| t.upgrade().as_ref() == Some(&tv) && (w - laid).abs() > 4)
            });
            if stale_table {
                let md = VIEWS.with(|v| v.borrow().iter().find(|(t, _)| t.upgrade().as_ref() == Some(&tv)).map(|(_, md)| md.clone()));
                if let Some(md) = md {
                    render(&tv, &md);
                }
            }
            return gtk::glib::ControlFlow::Break;
        }
        frames.set(frames.get() + 1);
        if frames.get() > 120 { gtk::glib::ControlFlow::Break } else { gtk::glib::ControlFlow::Continue }
    });
}

/// The width the view's content may use: the text view's own width, capped by the scrolled window
/// it sits in. The cap matters: an embedded child's minimum width would otherwise inflate the text
/// view, the column and finally the window, and nothing could ever shrink back.
fn content_width(tv: &gtk::TextView) -> i32 {
    let own = tv.width();
    let cap = tv
        .ancestor(gtk::ScrolledWindow::static_type())
        .map(|sw| sw.width() - 24) // the column's margins
        .unwrap_or(own);
    let w = if own > 0 { own.min(cap) } else { cap };
    w - tv.left_margin() - tv.right_margin()
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

/// A highlighted, read-only code view for a fenced block.
fn code_view(lang: &str, code: &str) -> gtk::Widget {
    ensure_schemes();
    let buffer = sourceview5::Buffer::new(None);
    let lm = sourceview5::LanguageManager::default();
    let lang = lang.split_whitespace().next().unwrap_or("");
    let language = if lang.is_empty() {
        None
    } else {
        lm.language(lang).or_else(|| lm.guess_language(Some(&format!("x.{lang}")), None))
    };
    buffer.set_language(language.as_ref());
    buffer.set_highlight_syntax(true);
    buffer.set_highlight_matching_brackets(false);
    if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&theme::source_scheme(&theme::current())) {
        buffer.set_style_scheme(Some(&s));
    }
    buffer.set_text(code.trim_end_matches('\n'));
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
    view.set_hexpand(true);
    view.upcast()
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

/// Replace the view's content with the rendered markdown.
pub fn render(tv: &gtk::TextView, markdown: &str) {
    let first = VIEWS.with(|v| !v.borrow().iter().any(|(w, _)| w.upgrade().as_ref() == Some(tv)));
    remember(tv, markdown);
    if first {
        tv.connect_map(size_code_views);
    }
    let mut code_views: Vec<gtk::Widget> = Vec::new();
    let buffer = tv.buffer();
    install_tags(&buffer);
    buffer.set_text("");
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, opts);

    let mut tags: Vec<String> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new(); // None = bullet, Some(n) = next number
    let mut need_newline = false; // a block ended; separate the next block with a blank line
    // fenced code: collected, then embedded as a widget at the end tag
    let mut code: Option<(String, String)> = None; // (lang, text)
    // tables: collected as rows of cells, laid out with tab stops at the end tag
    let mut table: Option<(Vec<Alignment>, Vec<Vec<String>>)> = None;
    let mut in_table_cell = false;
    let mut table_index = 0usize;
    let mut item_marker: Option<(i32, i32)> = None; // the last list marker's offsets, replaced by a task box

    let insert = |text: &str, tags: &[String]| {
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
    };
    let at_line_start = || {
        let end = buffer.end_iter();
        end.offset() == 0 || buffer.text(&buffer.iter_at_offset(end.offset() - 1), &end, true) == "\n"
    };
    let block_start = |need_newline: &mut bool| {
        if !at_line_start() {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
        }
        if *need_newline && buffer.char_count() > 0 {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
        }
        *need_newline = false;
    };
    let rule = |tag: &str| {
        if !at_line_start() {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
        }
        insert(" \n", &[tag.to_string()]); // a space: an empty line gets no paragraph background
    };

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if list_stack.is_empty() {
                        block_start(&mut need_newline);
                    }
                }
                Tag::Heading { level, .. } => {
                    block_start(&mut need_newline);
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
                    block_start(&mut need_newline);
                    tags.push("quote".into());
                }
                Tag::CodeBlock(kind) => {
                    block_start(&mut need_newline);
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    if list_stack.is_empty() {
                        block_start(&mut need_newline);
                    }
                    list_stack.push(start);
                }
                Tag::Item => {
                    if !at_line_start() {
                        insert("\n", &[]);
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
                    let start = buffer.end_iter().offset();
                    insert(&marker, &tags);
                    item_marker = Some((start, buffer.end_iter().offset()));
                }
                Tag::Emphasis => tags.push("italic".into()),
                Tag::Strong => tags.push("bold".into()),
                Tag::Strikethrough => tags.push("strike".into()),
                Tag::Link { .. } => tags.push("link".into()),
                Tag::Image { .. } => insert("[image: ", &[]),
                Tag::Table(aligns) => {
                    block_start(&mut need_newline);
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
                    insert("\n", &[]);
                    if list_stack.is_empty() {
                        need_newline = true;
                    }
                }
                TagEnd::Heading(_) => {
                    tags.pop();
                    insert("\n", &[]);
                }
                TagEnd::BlockQuote(_) => {
                    tags.pop();
                    need_newline = true;
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = code.take() {
                        let mut end = buffer.end_iter();
                        let anchor = buffer.create_child_anchor(&mut end);
                        let view = code_view(&lang, &text);
                        tv.add_child_at_anchor(&view, &anchor);
                        code_views.push(view);
                        insert("\n", &[]);
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
                    if !at_line_start() {
                        insert("\n", &[]);
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    tags.pop();
                }
                TagEnd::Image => insert("]", &[]),
                TagEnd::TableHead | TagEnd::TableRow => {}
                TagEnd::TableCell => in_table_cell = false,
                TagEnd::Table => {
                    if let Some((aligns, rows)) = table.take() {
                        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                        // column widths in the view's font; the header is bold
                        let widths: Vec<i32> = (0..ncols)
                            .map(|i| {
                                rows.iter()
                                    .enumerate()
                                    .map(|(ri, r)| measure(tv, r.get(i).map(String::as_str).unwrap_or(""), ri == 0))
                                    .max()
                                    .unwrap_or(0)
                            })
                            .collect();
                        let mut gap = scale(COL_GAP);
                        // wider than the view: narrow the gap, then give the wide columns a share
                        // of what is left and wrap their cells (tab stops can't wrap by themselves)
                        let view_w = content_width(tv);
                        // laid out for this width; a guess (0) when the view isn't allocated yet,
                        // and the sizing tick renders again as soon as the real width differs
                        TABLE_WIDTHS.with(|u| {
                            let mut l = u.borrow_mut();
                            match l.iter_mut().find(|(t, _)| t.upgrade().as_ref() == Some(tv)) {
                                Some(e) => e.1 = view_w.max(0),
                                None => l.push((tv.downgrade(), view_w.max(0))),
                            }
                        });
                        let avail = if view_w > 100 { view_w - scale(16.0) } else { scale(620.0) };
                        let total = |widths: &[i32], gap: i32| widths.iter().sum::<i32>() + gap * (ncols.saturating_sub(1) as i32);
                        let mut widths = widths;
                        if total(&widths, gap) > avail {
                            gap = scale(12.0);
                        }
                        if total(&widths, gap) > avail && ncols > 0 {
                            // water-filling: a column narrower than the fair share of what is left
                            // keeps its width; the rest split the remainder equally. Never overshoots.
                            let mut space = avail - gap * (ncols.saturating_sub(1) as i32);
                            let mut fixed = vec![false; ncols];
                            loop {
                                let open = fixed.iter().filter(|f| !**f).count() as i32;
                                if open == 0 {
                                    break;
                                }
                                let fair = (space / open).max(scale(40.0));
                                let mut changed = false;
                                for i in 0..ncols {
                                    if !fixed[i] && widths[i] <= fair {
                                        fixed[i] = true;
                                        space -= widths[i];
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
                                .map(|i| wrap_cell(tv, r.get(i).map(String::as_str).unwrap_or(""), widths[i] - scale(4.0), ri == 0))
                                .collect();
                            let lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
                            for k in 0..lines {
                                let cells: Vec<&str> = wrapped.iter().map(|c| c.get(k).map(String::as_str).unwrap_or("")).collect();
                                let line = format!("{}\n", cells.join("\t"));
                                let mut t = vec![name.clone(), if k == 0 { "cell".to_string() } else { "cell-cont".to_string() }];
                                if ri == 0 {
                                    t.push("bold".into());
                                }
                                insert(&line, &t);
                            }
                            if ri == 0 {
                                rule("thead-rule");
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
                    insert(&t, &tags);
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
                    insert(&c, &t);
                }
            }
            Event::SoftBreak => insert(" ", &tags),
            Event::HardBreak => insert("\n", &tags),
            Event::Rule => {
                block_start(&mut need_newline);
                rule("rule");
                insert("\n", &[]); // a blank line after, as before it
                need_newline = true;
            }
            Event::TaskListMarker(done) => {
                if let Some((a, b)) = item_marker.take()
                    && b == buffer.end_iter().offset()
                {
                    buffer.delete(&mut buffer.iter_at_offset(a), &mut buffer.iter_at_offset(b));
                }
                insert(if done { "☑ " } else { "☐ " }, &tags)
            }
            Event::Html(h) | Event::InlineHtml(h) => insert(&h, &["code".to_string()]),
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
    CODE_VIEWS.with(|c| {
        let mut l = c.borrow_mut();
        l.retain(|(w, _)| w.upgrade().is_some());
        match l.iter_mut().find(|(w, _)| w.upgrade().as_ref() == Some(tv)) {
            Some(e) => e.1 = code_views,
            None => l.push((tv.downgrade(), code_views)),
        }
    });
    size_code_views(tv);
    // trim trailing newlines
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
