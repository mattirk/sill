//! A unified diff as numbered, coloured rows: the reader's diff view and Changes share it.
use iced::Font;
use iced::widget::{Column, container, row, text};

use crate::{
    Message, selection,
    style::{self, P},
};

/// Unified diff lines have independent old/new counters. Untracked file content is ordinary
/// text: a Markdown '-' at its start is not a deleted line.
pub(crate) fn numbered_diff(source: &str) -> Vec<(Option<usize>, Option<usize>, &str)> {
    let mut old = 0;
    let mut new = 0;
    let mut hunk = false;
    let untracked = source.starts_with("(untracked)");
    source
        .lines()
        .map(|line| {
            if !untracked && line.starts_with("@@ ") {
                let mut words = line.split_whitespace();
                words.next();
                let start = |s: &str| {
                    s.get(1..)
                        .and_then(|s| s.split(',').next())
                        .and_then(|s| s.parse().ok())
                };
                if let (Some(a), Some(b)) =
                    (words.next().and_then(start), words.next().and_then(start))
                {
                    old = a;
                    new = b;
                    hunk = true;
                }
                return (None, None, line);
            }
            if line.starts_with("diff ") {
                hunk = false;
            }
            let numbers = if hunk && line.starts_with('+') {
                let n = new;
                new += 1;
                (None, Some(n))
            } else if hunk && line.starts_with('-') {
                let n = old;
                old += 1;
                (Some(n), None)
            } else if hunk && line.starts_with(' ') {
                let a = old;
                let b = new;
                old += 1;
                new += 1;
                (Some(a), Some(b))
            } else {
                (None, None)
            };
            (numbers.0, numbers.1, line)
        })
        .collect()
}

/// A diff: one column of line numbers down the diff's own text, in the
/// code's face and size so each number sits on its line; additions and deletions in git's
/// colours, hunk headers in the accent, the file headers bold.
pub(crate) fn diff_rows<'a>(source: &'a str, path: &str) -> Column<'a, Message> {
    let mono = Font::with_name(crate::fonts::MONO_FAMILY);
    let bold = Font {
        weight: iced::font::Weight::Bold,
        ..mono
    };
    let highlighted = crate::syntax::diff(source, path);
    let lines = numbered_diff(source);
    let digits = lines.len().max(1).to_string().len();
    let gutter = style::MONO * 0.62 * digits as f32 + 6.0;
    let mut rows = Column::new();
    for (index, (old, new, line)) in lines.into_iter().enumerate() {
        let file_header = ["diff ", "--- ", "+++ "]
            .iter()
            .any(|p| line.starts_with(p))
            && old.is_none()
            && new.is_none();
        let (color, font) = if old.is_none() && new.is_some() {
            (P.success, mono)
        } else if old.is_some() && new.is_none() {
            (P.error, mono)
        } else if line.starts_with("@@ ") {
            (P.accent, mono)
        } else if file_header {
            (P.fg, bold)
        } else if line.starts_with("index ") && old.is_none() && new.is_none() {
            (P.fg_dim, mono)
        } else {
            (P.fg, mono)
        };
        let mut spans: Vec<iced::widget::text::Span<'_, (), Font>> = Vec::new();
        let mut end = 0;
        if let Some(tokens) = highlighted.get(index) {
            for (range, token_color) in tokens {
                if range.start > end {
                    spans.push(iced::widget::text::Span::new(&line[end..range.start]).color(color));
                }
                spans.push(iced::widget::text::Span::new(&line[range.clone()]).color(*token_color));
                end = range.end;
            }
        }
        if end < line.len() || spans.is_empty() {
            spans.push(iced::widget::text::Span::new(&line[end..]).color(color));
        }
        let content = selection::rich_text(spans)
            .size(style::MONO)
            .font(font)
            .style(style::selectable(color));
        let background = if old.is_none() && new.is_some() {
            style::alpha(P.success, 0.08)
        } else if old.is_some() && new.is_none() {
            style::alpha(P.error, 0.08)
        } else {
            iced::Color::TRANSPARENT
        };
        rows = rows.push(
            row![
                text((index + 1).to_string())
                    .size(style::MONO)
                    .font(mono)
                    .color(P.fg_dim)
                    .width(gutter)
                    .align_x(iced::alignment::Horizontal::Right),
                container(content).style(style::fill(background)),
            ]
            .spacing(12),
        );
    }
    rows
}
