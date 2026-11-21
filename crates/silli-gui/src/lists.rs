//! Markdown list conveniences for prose editors (the prompt, the instructions): a new line
//! continues the list, Tab and Shift+Tab indent its items, and markers and code are coloured.
use crate::style::P;
use iced::advanced::text::highlighter::{Format, Highlighter};
use iced::widget::text_editor::{Action, Content, Cursor, Edit, Position};
use iced::{Color, Font};
use std::ops::Range;
use std::sync::Arc;

const INDENT: &str = "  ";

/// A list item's start: `indent`, then a bullet or number, then perhaps a checkbox.
struct Item<'a> {
    indent: &'a str,
    /// Where the marker ends (before a checkbox) and where the item's own text starts.
    marker: usize,
    text: usize,
    /// The marker a following item gets.
    next: String,
}

fn item(line: &str) -> Option<Item<'_>> {
    let body = line.trim_start_matches([' ', '\t']);
    let indent = &line[..line.len() - body.len()];
    let (marker, next) = if let Some(rest) = body.strip_prefix(['-', '*', '+']) {
        rest.starts_with(' ').then(|| (2, body[..2].to_string()))?
    } else {
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        let rest = &body[digits..];
        if !(1..=9).contains(&digits) || !(rest.starts_with(". ") || rest.starts_with(") ")) {
            return None;
        }
        let number: u64 = body[..digits].parse().ok()?;
        (digits + 2, format!("{}{}", number + 1, &rest[..2]))
    };
    let rest = &body[marker..];
    let checkbox = ["[ ] ", "[x] ", "[X] "].iter().any(|b| rest.starts_with(b)) || matches!(rest, "[ ]" | "[x]" | "[X]");
    let (text, next) = if checkbox { ((marker + 4).min(body.len()), next + "[ ] ") } else { (marker, next) };
    Some(Item { indent, marker: indent.len() + marker, text: indent.len() + text, next })
}

fn line(content: &Content, index: usize) -> String {
    content.line(index).map(|line| line.text.to_string()).unwrap_or_default()
}

fn replace_lines(content: &mut Content, first: usize, last: usize, text: String) {
    content.move_to(Cursor { position: Position { line: first, column: 0 }, selection: Some(Position { line: last, column: line(content, last).len() }) });
    if text.is_empty() {
        content.perform(Action::Edit(Edit::Backspace));
    } else {
        content.perform(Action::Edit(Edit::Paste(Arc::new(text))));
    }
}

/// A line break that knows lists: the next item's marker follows it, and on an item with no
/// text yet the marker goes instead, which ends the list.
pub fn newline(content: &mut Content) {
    let cursor = content.cursor();
    let text = line(content, cursor.position.line);
    let Some(item) = item(&text).filter(|item| cursor.selection.is_none() && cursor.position.column >= item.text) else {
        content.perform(Action::Edit(Edit::Enter));
        return;
    };
    if text[item.text..].trim().is_empty() {
        let row = cursor.position.line;
        replace_lines(content, row, row, String::new());
    } else {
        let marker = format!("\n{}{}", item.indent, item.next);
        content.perform(Action::Edit(Edit::Paste(Arc::new(marker))));
    }
}

/// Tab (or Shift+Tab, `outdent`) where it means indentation: on a list item, or with a selection
/// over several lines. False, and nothing done, anywhere else: there Tab moves the keyboard on.
pub fn indent(content: &mut Content, outdent: bool) -> bool {
    let cursor = content.cursor();
    let anchor = cursor.selection.unwrap_or(cursor.position);
    let (first, last) = (cursor.position.line.min(anchor.line), cursor.position.line.max(anchor.line));
    if first == last && item(&line(content, first)).is_none() {
        return false;
    }
    // how far each line's start moved, for putting the cursor back
    let mut shifts = Vec::new();
    let lines: Vec<String> = (first..=last)
        .map(|index| {
            let text = line(content, index);
            let (text, shift) = if !outdent {
                if text.is_empty() { (text, 0) } else { (format!("{INDENT}{text}"), 2) }
            } else if let Some(rest) = text.strip_prefix('\t') {
                (rest.to_string(), -1)
            } else {
                let spaces = text.bytes().take(INDENT.len()).take_while(|b| *b == b' ').count();
                (text[spaces..].to_string(), -(spaces as isize))
            };
            shifts.push(shift);
            text
        })
        .collect();
    if shifts.iter().all(|shift| *shift == 0) {
        return true;
    }
    replace_lines(content, first, last, lines.join("\n"));
    let moved = |position: Position| Position { line: position.line, column: position.column.saturating_add_signed(shifts[position.line - first]) };
    // a selection is put back from its anchor, as `move_to` leaves the cursor at its end
    match cursor.selection {
        Some(selection) => content.move_to(Cursor { position: moved(selection), selection: Some(moved(cursor.position)) }),
        None => content.move_to(Cursor { position: moved(cursor.position), selection: None }),
    }
    true
}

/// The colours of [`Marks`]: a theme change is a change of these.
#[derive(Clone, Copy, PartialEq)]
pub struct Colours {
    marker: Color,
    dim: Color,
    code: Color,
}

pub fn colours() -> Colours {
    Colours { marker: P.accent, dim: P.fg_dim, code: P.link }
}

pub fn format(colour: &Color, _: &iced::Theme) -> Format<Font> {
    Format { color: Some(*colour), font: None }
}

/// Colours list markers, checkboxes, finished items and code; all other text is left alone.
pub struct Marks {
    colours: Colours,
    /// Per highlighted line: whether a code fence is open after it.
    fenced: Vec<bool>,
}

fn fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// Inline code: a run of backticks up to the next run of the same length.
fn code_spans(line: &str, from: usize) -> Vec<Range<usize>> {
    let bytes = line.as_bytes();
    let run = |at: usize| bytes[at..].iter().take_while(|b| **b == b'`').count();
    let mut spans = Vec::new();
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let open = run(at);
        let mut end = at + open;
        let mut closed = None;
        while end < bytes.len() {
            if bytes[end] == b'`' {
                let close = run(end);
                if close == open {
                    closed = Some(end + close);
                    break;
                }
                end += close;
            } else {
                end += 1;
            }
        }
        match closed {
            Some(end) => {
                spans.push(at..end);
                at = end;
            }
            None => at += open,
        }
    }
    spans
}

impl Highlighter for Marks {
    type Settings = Colours;
    type Highlight = Color;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Color)>;

    fn new(settings: &Colours) -> Self {
        Self { colours: *settings, fenced: Vec::new() }
    }
    fn update(&mut self, settings: &Colours) {
        self.colours = *settings;
        self.fenced.clear();
    }
    fn change_line(&mut self, line: usize) {
        self.fenced.truncate(line);
    }
    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let Colours { marker, dim, code } = self.colours;
        let inside = self.fenced.last().copied().unwrap_or(false);
        let fence = fence(line);
        self.fenced.push(inside != fence);
        let mut spans = Vec::new();
        if inside || fence {
            if !line.is_empty() {
                spans.push((0..line.len(), code));
            }
            return spans.into_iter();
        }
        let mut from = 0;
        if let Some(item) = item(line) {
            spans.push((item.indent.len()..item.marker - 1, marker));
            from = item.text;
            if item.text > item.marker {
                spans.push((item.marker..item.marker + 3, dim));
                // a ticked item is done with: all of it steps back
                if line[item.marker..].starts_with(['[']) && !line[item.marker..].starts_with("[ ]") {
                    if item.text < line.len() {
                        spans.push((item.text..line.len(), dim));
                    }
                    return spans.into_iter();
                }
            }
        }
        spans.extend(code_spans(line, from).into_iter().map(|span| (span, code)));
        spans.into_iter()
    }
    fn current_line(&self) -> usize {
        self.fenced.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::text_editor::Motion;

    fn at_end(text: &str) -> Content {
        let mut content = Content::with_text(text);
        content.perform(Action::Move(Motion::DocumentEnd));
        content
    }
    fn after_newline(text: &str) -> String {
        let mut content = at_end(text);
        newline(&mut content);
        content.text()
    }

    #[test]
    fn a_new_line_continues_each_kind_of_list() {
        for (before, after) in [
            ("- one", "- one\n- "),
            ("  * one", "  * one\n  * "),
            ("+ one", "+ one\n+ "),
            ("1. one", "1. one\n2. "),
            ("9) one", "9) one\n10) "),
            ("- [x] done", "- [x] done\n- [ ] "),
            ("\t3. [ ] open", "\t3. [ ] open\n\t4. [ ] "),
            ("plain", "plain\n"),
            ("-not a list", "-not a list\n"),
            ("2024 was a year", "2024 was a year\n"),
            ("- one\n\ntext", "- one\n\ntext\n"),
        ] {
            assert_eq!(after_newline(before), after, "{before:?}");
        }
    }

    #[test]
    fn a_new_line_on_an_empty_item_ends_the_list() {
        for (before, after) in [("- one\n- ", "- one\n"), ("1. one\n2. ", "1. one\n"), ("- [ ] one\n- [ ] ", "- [ ] one\n"), ("- ", "")] {
            let mut content = at_end(before);
            newline(&mut content);
            assert_eq!(content.text(), after, "{before:?}");
            assert_eq!(content.cursor().position.column, 0);
            // and typing goes on where the marker was
            content.perform(Action::Edit(Edit::Insert('x')));
            assert_eq!(content.text(), format!("{after}x"));
        }
    }

    #[test]
    fn a_new_line_splits_an_item_and_leaves_markers_and_selections_alone() {
        let mut content = Content::with_text("- one two");
        content.move_to(Cursor { position: Position { line: 0, column: 5 }, selection: None });
        newline(&mut content);
        assert_eq!(content.text(), "- one\n-  two");
        // inside the marker it is only a line break
        let mut content = Content::with_text("- one");
        newline(&mut content);
        assert_eq!(content.text(), "\n- one");
        let mut content = Content::with_text("- one\n- two");
        content.perform(Action::SelectAll);
        newline(&mut content);
        assert_eq!(content.text(), "\n");
    }

    #[test]
    fn tab_indents_items_and_selected_lines_only() {
        let mut content = at_end("plain");
        assert!(!indent(&mut content, false));
        assert_eq!(content.text(), "plain");

        let mut content = at_end("- one");
        assert!(indent(&mut content, false));
        assert_eq!(content.text(), "  - one");
        assert_eq!(content.cursor().position, Position { line: 0, column: 7 });
        content.perform(Action::Edit(Edit::Insert('!')));
        assert_eq!(content.text(), "  - one!");
        assert!(indent(&mut content, true));
        assert!(indent(&mut content, true));
        assert_eq!(content.text(), "- one!");
        assert_eq!(content.cursor().position, Position { line: 0, column: 6 });

        let mut content = Content::with_text("a\n\n b\n\tc\nlast");
        content.move_to(Cursor { position: Position { line: 0, column: 1 }, selection: Some(Position { line: 3, column: 2 }) });
        assert!(indent(&mut content, false));
        assert_eq!(content.text(), "  a\n\n   b\n  \tc\nlast");
        assert_eq!(content.selection().as_deref(), Some("\n\n   b\n  \tc"));
        assert!(indent(&mut content, true));
        assert!(indent(&mut content, true));
        assert_eq!(content.text(), "a\n\nb\nc\nlast");
        assert_eq!(content.selection().as_deref(), Some("\n\nb\nc"));
    }

    fn marks(text: &str) -> Vec<Vec<(Range<usize>, Color)>> {
        let mut marks = Marks::new(&colours());
        text.lines().map(|line| marks.highlight_line(line).collect()).collect()
    }

    #[test]
    fn markers_checkboxes_and_code_are_coloured() {
        let Colours { marker, dim, code } = colours();
        assert_eq!(
            marks("- one `code` two\n  12. [x] done `x`\n* [ ] open\nplain ``a ` b`` and `open"),
            vec![vec![(0..1, marker), (6..12, code)], vec![(2..5, marker), (6..9, dim), (10..18, dim)], vec![(0..1, marker), (2..5, dim)], vec![(6..15, code)],]
        );
    }

    #[test]
    fn fences_colour_their_lines_and_follow_edits() {
        let code = colours().code;
        let text = "- a\n```\n- b\n```\n- c";
        let lines = marks(text);
        assert_eq!(lines[1], vec![(0..3, code)]);
        assert_eq!(lines[2], vec![(0..3, code)]);
        assert_eq!(lines[3], vec![(0..3, code)]);
        assert_eq!(lines[4].len(), 1);
        assert_ne!(lines[4][0].1, code);
        // the fence's first line is edited away: what follows is a list again
        let mut marks = Marks::new(&colours());
        for line in text.lines() {
            let _ = marks.highlight_line(line);
        }
        marks.change_line(1);
        assert_eq!(marks.current_line(), 1);
        assert_ne!(marks.highlight_line("- b").next().unwrap().1, code);
    }
}
