//! Markdown → GtkTextBuffer with tags (SPEC §4.5): headings, emphasis, inline code, fenced code
//! blocks, lists, links, block quotes. No HTML, no images (shown as their alt text). Re-rendering the
//! whole message on every streamed chunk is fine: messages are small and the parser is fast.

use gtk4 as gtk;
use gtk::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Tag names installed by [`install_tags`].
const TAGS: &[(&str, &[(&str, &str)])] = &[
    ("h1", &[("weight", "bold"), ("scale", "1.5")]),
    ("h2", &[("weight", "bold"), ("scale", "1.3")]),
    ("h3", &[("weight", "bold"), ("scale", "1.15")]),
    ("h4", &[("weight", "bold")]),
    ("bold", &[("weight", "bold")]),
    ("italic", &[("style", "italic")]),
    ("strike", &[("strikethrough", "true")]),
    ("code", &[("family", "monospace")]),
    ("codeblock", &[("family", "monospace"), ("left-margin", "12"), ("right-margin", "12")]),
    ("quote", &[("left-margin", "16"), ("style", "italic")]),
    ("link", &[("underline", "single")]),
    ("thought", &[("style", "italic")]),
];

thread_local! {
    /// Every buffer with our tags, so a theme change can recolour text already on screen.
    static BUFFERS: std::cell::RefCell<Vec<gtk::glib::WeakRef<gtk::TextBuffer>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Recolour the theme-dependent tags of all live buffers.
pub fn retheme() {
    let colors = super::theme::tag_colors();
    BUFFERS.with(|b| {
        let mut list = b.borrow_mut();
        list.retain(|w| w.upgrade().is_some());
        for w in list.iter() {
            let Some(buffer) = w.upgrade() else { continue };
            let table = buffer.tag_table();
            if let Some(t) = table.lookup("code") {
                t.set_background(Some(&colors.code_bg));
            }
            if let Some(t) = table.lookup("codeblock") {
                t.set_paragraph_background(Some(&colors.code_bg));
            }
            if let Some(t) = table.lookup("link") {
                t.set_foreground(Some(&colors.link));
            }
        }
    });
}

pub fn install_tags(buffer: &gtk::TextBuffer) {
    let colors = super::theme::tag_colors();
    let table = buffer.tag_table();
    if table.lookup("code").is_none() {
        BUFFERS.with(|b| b.borrow_mut().push(buffer.downgrade()));
    }
    for (name, _) in TAGS {
        if table.lookup(name).is_some() {
            continue;
        }
        let tag = gtk::TextTag::new(Some(name));
        match *name {
            "h1" => { tag.set_weight(700); tag.set_scale(1.5); }
            "h2" => { tag.set_weight(700); tag.set_scale(1.3); }
            "h3" => { tag.set_weight(700); tag.set_scale(1.15); }
            "h4" | "bold" => tag.set_weight(700),
            "italic" | "thought" => tag.set_style(gtk::pango::Style::Italic),
            "strike" => tag.set_strikethrough(true),
            "code" => { tag.set_family(Some("monospace")); tag.set_background(Some(&colors.code_bg)); }
            "codeblock" => {
                tag.set_family(Some("monospace"));
                tag.set_left_margin(12);
                tag.set_right_margin(12);
                tag.set_paragraph_background(Some(&colors.code_bg));
            }
            "quote" => { tag.set_left_margin(16); tag.set_style(gtk::pango::Style::Italic); }
            "link" => { tag.set_underline(gtk::pango::Underline::Single); tag.set_foreground(Some(&colors.link)); }
            _ => {}
        }
        table.add(&tag);
    }
}

/// Replace the buffer's content with the rendered markdown.
pub fn render(buffer: &gtk::TextBuffer, markdown: &str) {
    install_tags(buffer);
    buffer.set_text("");
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(markdown, opts);

    let mut tags: Vec<&'static str> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new(); // None = bullet, Some(n) = next number
    let mut in_code_block = false;
    let mut need_newline = false; // a block ended; separate the next block with a blank line
    // tables: collected as rows of cells, rendered aligned in monospace at the end tag
    let mut table: Option<Vec<Vec<String>>> = None;
    let mut in_table_cell = false;

    let insert = |buffer: &gtk::TextBuffer, text: &str, tags: &[&str]| {
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
    let block_start = |buffer: &gtk::TextBuffer, need_newline: &mut bool| {
        if *need_newline && buffer.char_count() > 0 {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, "\n");
        }
        *need_newline = false;
    };

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => block_start(buffer, &mut need_newline),
                Tag::Heading { level, .. } => {
                    block_start(buffer, &mut need_newline);
                    tags.push(match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        HeadingLevel::H3 => "h3",
                        _ => "h4",
                    });
                }
                Tag::BlockQuote(_) => {
                    block_start(buffer, &mut need_newline);
                    tags.push("quote");
                }
                Tag::CodeBlock(kind) => {
                    block_start(buffer, &mut need_newline);
                    if let CodeBlockKind::Fenced(lang) = kind
                        && !lang.is_empty()
                    {
                        insert(buffer, &format!("{lang}\n"), &["codeblock", "italic"]);
                    }
                    in_code_block = true;
                    tags.push("codeblock");
                }
                Tag::List(start) => {
                    if list_stack.is_empty() {
                        block_start(buffer, &mut need_newline);
                    }
                    list_stack.push(start);
                }
                Tag::Item => {
                    let depth = list_stack.len().saturating_sub(1);
                    let marker = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => "• ".to_string(),
                    };
                    insert(buffer, &format!("{}{marker}", "    ".repeat(depth)), &[]);
                }
                Tag::Emphasis => tags.push("italic"),
                Tag::Strong => tags.push("bold"),
                Tag::Strikethrough => tags.push("strike"),
                Tag::Link { .. } => tags.push("link"),
                Tag::Image { .. } => insert(buffer, "[image: ", &[]),
                Tag::Table(_) => {
                    block_start(buffer, &mut need_newline);
                    table = Some(Vec::new());
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = table.as_mut() {
                        t.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = table.as_mut()
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
                    if list_stack.is_empty() {
                        insert(buffer, "\n", &[]);
                        need_newline = true;
                    } else {
                        insert(buffer, "\n", &[]);
                    }
                }
                TagEnd::Heading(_) => {
                    tags.pop();
                    insert(buffer, "\n", &[]);
                    need_newline = true;
                }
                TagEnd::BlockQuote(_) => {
                    tags.pop();
                    need_newline = true;
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    tags.pop();
                    need_newline = true;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    if list_stack.is_empty() {
                        need_newline = true;
                    }
                }
                TagEnd::Item => {
                    // items with no paragraph end need their own newline
                    let end = buffer.end_iter();
                    if end.offset() > 0 && !buffer.text(&buffer.iter_at_offset(end.offset() - 1), &end, true).ends_with('\n') {
                        insert(buffer, "\n", &[]);
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    tags.pop();
                }
                TagEnd::Image => insert(buffer, "]", &[]),
                TagEnd::TableHead | TagEnd::TableRow => {}
                TagEnd::TableCell => in_table_cell = false,
                TagEnd::Table => {
                    if let Some(rows) = table.take() {
                        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                        let widths: Vec<usize> = (0..ncols)
                            .map(|i| rows.iter().map(|r| r.get(i).map(|c| c.chars().count()).unwrap_or(0)).max().unwrap_or(0))
                            .collect();
                        for (ri, r) in rows.iter().enumerate() {
                            let line: Vec<String> = (0..ncols)
                                .map(|i| {
                                    let c = r.get(i).map(String::as_str).unwrap_or("");
                                    format!("{c:<w$}", w = widths[i])
                                })
                                .collect();
                            let t: &[&str] = if ri == 0 { &["codeblock", "bold"] } else { &["codeblock"] };
                            insert(buffer, &format!("{}\n", line.join("  ").trim_end()), t);
                            if ri == 0 {
                                let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                                insert(buffer, &format!("{}\n", sep.join("  ")), &["codeblock"]);
                            }
                        }
                    }
                    need_newline = true;
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_table_cell {
                    if let Some(row) = table.as_mut().and_then(|t| t.last_mut()).and_then(|r| r.last_mut()) {
                        row.push_str(&t);
                    }
                } else {
                    let _ = in_code_block;
                    insert(buffer, &t, &tags);
                }
            }
            Event::Code(c) => {
                if in_table_cell {
                    if let Some(row) = table.as_mut().and_then(|t| t.last_mut()).and_then(|r| r.last_mut()) {
                        row.push_str(&c);
                    }
                } else {
                    let mut t = tags.clone();
                    t.push("code");
                    insert(buffer, &c, &t);
                }
            }
            Event::SoftBreak => insert(buffer, " ", &tags),
            Event::HardBreak => insert(buffer, "\n", &tags),
            Event::Rule => {
                block_start(buffer, &mut need_newline);
                insert(buffer, "――――――――――\n", &[]);
                need_newline = true;
            }
            Event::Html(h) | Event::InlineHtml(h) => insert(buffer, &h, &["code"]),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
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
