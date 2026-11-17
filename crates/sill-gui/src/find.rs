//! Find (`Ctrl+F`) bar, the conversation's text index and the reader's (`docs/find.md`).
//!
//! The bar searches one [`Surface`]: the conversation, or the file open in the reader (its
//! Markdown preview, its diff rows or its source). The reader builds all of its text, so its index
//! is its leaves (preview, diff) or its lines (source) in order, all in one row.
//!
//! The window owns one [`State`]. While the conversation is shown, `index_rows` walks every
//! conversation row (including folded turns, grouped tool calls, closed tool outputs, long
//! thoughts, long notices and answered questions) in the exact leaf order that [`crate::ui`]
//! and [`crate::viewer::SillViewer`] render [`crate::selection::Selectable`] leaves.
//!
//! When a match inside a folded item becomes the current match, its [`OpenKey`]s are inserted into
//! the session view's open sets (`open_groups`, `open_tools`, `open_answers`) so the row unfolds,
//! and the conversation scrolls to bring the match into view.
use std::any::Any;
use std::cell::RefCell;
use std::ops::Range;

use iced::advanced::widget::operation::scrollable::{self as scroll, AbsoluteOffset, Scrollable};
use iced::advanced::widget::operation::{Operation, Outcome};
use iced::advanced::widget::Id;
use iced::widget::{container, mouse_area, right, row, text, text_editor, text_input};
use iced::{Alignment, Element, Padding, Rectangle, Task, Vector};
use sill_proto::sfp::{Answer, Item, Question, Tool};

use crate::selection::{FindQuery, Selectable, VisibleLeaf};
use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, icons, keys};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Open,
    Close,
    Input(String),
    Next,
    Prev,
    ToggleCase,
}

/// Which open set in [`SessionView`] must contain a key for a leaf to be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenKey {
    Group(String),
    Tool(String),
    Answer(String),
}

impl OpenKey {
    pub fn is_open(&self, view: &SessionView) -> bool {
        match self {
            OpenKey::Group(key) => view.open_groups.contains(key),
            OpenKey::Tool(key) => view.open_tools.contains(key),
            OpenKey::Answer(key) => view.open_answers.contains(key),
        }
    }

    pub fn open(&self, view: &mut SessionView) {
        match self {
            OpenKey::Group(key) => {
                view.open_groups.insert(key.clone());
            }
            OpenKey::Tool(key) => {
                view.open_tools.insert(key.clone());
            }
            OpenKey::Answer(key) => {
                view.open_answers.insert(key.clone());
            }
        }
    }
}

/// What the bar searches, decided when it opens; the bar closes when the window shows another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    Conversation,
    /// The open file, by its key; `revision` and `edits` say when its text changed.
    Reader {
        key: String,
        mode: ReaderMode,
        revision: u64,
        edits: u64,
    },
}

/// How the reader shows its file, as `files::reader_ui` decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderMode {
    /// The rendered Markdown (`markdown-reader`).
    Preview,
    /// The numbered diff rows (`diff-reader`).
    Diff,
    /// The source editor: the current match is its selection.
    Source,
    /// Being read, or unreadable: nothing to search yet.
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub row: usize,
    pub leaf_in_row: usize,
    pub range: (usize, usize),
    pub open_keys: Vec<OpenKey>,
}

#[derive(Default)]
pub struct State {
    pub shown: bool,
    /// The surface the bar was opened on.
    pub surface: Option<Surface>,
    pub query: String,
    pub case_sensitive: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    /// The conversation's selected text, mirrored by the selection area for Ctrl+F's query.
    pub selected_text: RefCell<Option<String>>,
    /// The same for the reader's preview and diff, and the file it was selected in: an area
    /// mirrors only while it holds a selection, so another file's would outlive it.
    pub reader_selected: RefCell<Option<String>>,
    reader_file: Option<String>,
    /// The conversation's laid-out leaves, recorded by the selection area on each layout.
    pub visible_leaves: RefCell<Vec<VisibleLeaf>>,
}

impl State {
    /// Closes the Find bar and clears highlights while keeping the query and case toggle for the
    /// next `Ctrl+F` in this window.
    pub fn close(&mut self) {
        self.shown = false;
        self.surface = None;
        self.matches.clear();
        self.current = None;
    }

    /// The row of the current match, so [`crate::rows::plan_with_pinned`] always builds it.
    pub fn pinned_row(&self) -> Option<usize> {
        self.current.and_then(|idx| self.matches.get(idx)).map(|m| m.row)
    }

    /// The query and current match for the reader's preview or diff, whose text is all one row.
    pub fn reader_query(&self) -> Option<FindQuery> {
        if !self.shown || self.query.is_empty() {
            return None;
        }
        Some(FindQuery {
            needle: self.query.clone(),
            case_sensitive: self.case_sensitive,
            current: self.current.map(|i| (0, i)),
        })
    }

    /// The matches in the reader's source as `(line, byte range, current)`, for the editor's
    /// marks.
    pub fn source_marks(&self) -> Vec<(usize, Range<usize>, bool)> {
        if !self.shown
            || !matches!(
                self.surface,
                Some(Surface::Reader {
                    mode: ReaderMode::Source,
                    ..
                })
            )
        {
            return Vec::new();
        }
        self.matches
            .iter()
            .enumerate()
            .map(|(i, m)| (m.leaf_in_row, m.range.0..m.range.1, self.current == Some(i)))
            .collect()
    }

    /// The query and current visible match to hand to [`crate::selection::Area::find`].
    pub fn area_query(&self, view: &SessionView) -> Option<FindQuery> {
        if self.query.is_empty() {
            return None;
        }
        let current = self.current.and_then(|idx| {
            let m = self.matches.get(idx)?;
            let visible_occ = self.matches[..idx]
                .iter()
                .filter(|prev| {
                    prev.row == m.row && prev.open_keys.iter().all(|k| k.is_open(view))
                })
                .count();
            Some((m.row, visible_occ))
        });
        Some(FindQuery {
            needle: self.query.clone(),
            case_sensitive: self.case_sensitive,
            current,
        })
    }

    /// `(label, has_matches)` for the Find bar's count display, or `None` when the query is empty.
    pub fn count_label(&self) -> Option<(String, bool)> {
        if self.query.is_empty() {
            None
        } else if self.matches.is_empty() {
            Some(("No matches".to_string(), false))
        } else {
            let idx = self.current.map_or(1, |i| i + 1);
            Some((format!("{idx} of {}", self.matches.len()), true))
        }
    }

    /// The floating Find bar card in the top-right corner of the active content pane.
    pub fn bar<'a>(&'a self) -> Element<'a, Message> {
        let input = keys::find_input(
            text_input("Find…", &self.query)
                .id("find-query")
                .on_input(|s| Message::Find(Event::Input(s)))
                .size(style::SMALL)
                .padding([4, 8])
                .width(180)
                .style(style::find_input),
        );
        let mut bar_row = row![input].spacing(4).align_y(Alignment::Center);
        if let Some((label, _)) = self.count_label() {
            bar_row = bar_row.push(
                container(text(label).size(style::TINY).color(P.fg_dim)).padding([0, 4]),
            );
        }
        let case_sensitive = self.case_sensitive;
        let case_button = crate::ui::bar_tip(
            keys::press(
                crate::ui::bar_button(
                    text("Aa")
                        .size(style::TINY)
                        .color(if case_sensitive { P.accent } else { P.fg_dim }),
                )
                .style(move |theme, status| {
                    if case_sensitive {
                        style::tinted(P.accent)(theme, status)
                    } else {
                        style::quiet(theme, status)
                    }
                }),
                Message::Find(Event::ToggleCase),
            ),
            "Match case (Alt+C)",
        );
        let has_matches = !self.matches.is_empty();
        let prev_button = crate::ui::bar_tip(
            keys::press_maybe(
                crate::ui::bar_button(icons::glyph(
                    "chevron-up",
                    crate::ui::BAR_ICON,
                    if has_matches {
                        P.fg_dim
                    } else {
                        alpha(P.fg_dim, 0.35)
                    },
                ))
                .style(style::quiet),
                has_matches.then_some(Message::Find(Event::Prev)),
            ),
            "Previous match (Shift+Enter)",
        );
        let next_button = crate::ui::bar_tip(
            keys::press_maybe(
                crate::ui::bar_button(icons::glyph(
                    "chevron-down",
                    crate::ui::BAR_ICON,
                    if has_matches {
                        P.fg_dim
                    } else {
                        alpha(P.fg_dim, 0.35)
                    },
                ))
                .style(style::quiet),
                has_matches.then_some(Message::Find(Event::Next)),
            ),
            "Next match (Enter)",
        );
        let close_button = crate::ui::bar_tip(
            keys::press(
                crate::ui::bar_button(icons::glyph("x", 12.0, P.fg_dim)).style(style::quiet),
                Message::Find(Event::Close),
            ),
            "Close (Escape)",
        );
        bar_row = bar_row
            .push(case_button)
            .push(prev_button)
            .push(next_button)
            .push(close_button);
        let card = mouse_area(
            container(bar_row)
                .id("find-bar")
                .padding([4, 6])
                .style(style::find_bar),
        )
        .on_press(Message::Noop);
        right(card)
            .padding(Padding {
                top: 8.0,
                right: 18.0,
                bottom: 0.0,
                left: 0.0,
            })
            .into()
    }
}

/// Non-overlapping `(start_byte, end_byte)` matches of `needle` in `haystack`.
///
/// Offsets are always valid UTF-8 character boundaries in `haystack`, even when Unicode
/// lowercasing changes the byte length of characters.
pub fn match_ranges(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() || haystack.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        return haystack
            .match_indices(needle)
            .map(|(i, s)| (i, i + s.len()))
            .collect();
    }
    if haystack.is_ascii() && needle.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        if n.len() > h.len() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i + n.len() <= h.len() {
            if h[i..i + n.len()].eq_ignore_ascii_case(n) {
                out.push((i, i + n.len()));
                i += n.len();
            } else {
                i += 1;
            }
        }
        return out;
    }
    let needle_lower = needle.to_lowercase();
    if needle_lower.is_empty() {
        return Vec::new();
    }
    let mut hay_lower = String::with_capacity(haystack.len());
    let mut start_map = Vec::with_capacity(haystack.len());
    let mut end_map = Vec::with_capacity(haystack.len());
    for (orig_start, ch) in haystack.char_indices() {
        let orig_end = orig_start + ch.len_utf8();
        start_map.push((hay_lower.len(), orig_start));
        for lc in ch.to_lowercase() {
            hay_lower.push(lc);
        }
        end_map.push((hay_lower.len(), orig_end));
    }
    let mut out = Vec::new();
    let mut min_orig_start = 0;
    for &(low_start, orig_start) in &start_map {
        if orig_start < min_orig_start {
            continue;
        }
        if low_start + needle_lower.len() > hay_lower.len() {
            break;
        }
        if hay_lower[low_start..].starts_with(&needle_lower) {
            let low_end = low_start + needle_lower.len();
            if let Ok(end_idx) = end_map.binary_search_by_key(&low_end, |&(le, _)| le) {
                let orig_end = end_map[end_idx].1;
                out.push((orig_start, orig_end));
                min_orig_start = orig_end;
            }
        }
    }
    out
}

/// One leaf in a conversation row, with the [`OpenKey`]s required for it to be visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedLeaf {
    pub text: String,
    pub open_keys: Vec<OpenKey>,
}

/// Extracts all [`IndexedLeaf`] entries per row for `view`, in the exact order [`crate::ui`]
/// builds rows and [`crate::selection::Area`] visits their [`Selectable`] leaves.
fn index_rows(view: &SessionView) -> Vec<Vec<IndexedLeaf>> {
    let conversation = &view.conversation;
    let items = conversation.visible_items();
    let places = conversation.question_places();
    let below = |item: &Item| -> Vec<&Question> {
        places
            .get(item.item_id.as_str())
            .cloned()
            .unwrap_or_default()
    };
    let turn_end = |index: usize, item: &Item| -> Option<String> {
        let turn_id = item.turn_id.as_ref()?;
        if items.get(index + 1).and_then(|next| next.turn_id.as_ref()) == Some(turn_id) {
            return None;
        }
        conversation.turn_end(turn_id).map(|end| end.to_string())
    };
    let folds = crate::folds::rows(
        &items,
        |item| conversation.purpose(item),
        |item| places.contains_key(item.item_id.as_str()),
    );
    let entries: Vec<Range<usize>> = folds.iter().map(|r| r.range().clone()).collect();

    let mut rows = Vec::with_capacity(entries.len() + 4);
    let mut placed = std::collections::BTreeSet::new();
    for question in places.values().flatten() {
        placed.insert(question.question_id.as_str());
    }

    for (index, entry) in entries.iter().cloned().enumerate() {
        let (item, last) = (items[entry.start], entry.end - 1);
        let mut leaves = Vec::new();
        if matches!(&folds[index], crate::folds::Row::Fold(_, _)) {
            let inside = items[entry].to_vec();
            let base_keys = vec![OpenKey::Group(crate::ui::fold_key(inside[0]))];
            for sub_entry in crate::tool_runs::entries(&inside, |_| false) {
                if inside[sub_entry.clone()]
                    .iter()
                    .all(|i| crate::host_runs::host_text(i).is_some())
                {
                    index_host_run(&inside[sub_entry], &base_keys, &mut leaves);
                } else if sub_entry.len() > 1 {
                    index_tool_run(&inside[sub_entry], &base_keys, &mut leaves);
                } else {
                    index_item(view, inside[sub_entry.start], &base_keys, &mut leaves);
                }
            }
        } else if items[entry.clone()]
            .iter()
            .all(|i| crate::host_runs::host_text(i).is_some())
        {
            index_host_run(&items[entry], &[], &mut leaves);
        } else if entry.len() > 1 {
            index_tool_run(&items[entry], &[], &mut leaves);
        } else {
            index_item(view, item, &[], &mut leaves);
            for question in below(item) {
                index_question(question, &[], &mut leaves);
            }
        }
        if let Some(end) = turn_end(last, items[last]) {
            leaves.push(IndexedLeaf {
                text: format!("· {end}"),
                open_keys: Vec::new(),
            });
        }
        rows.push(leaves);
    }

    for question in conversation
        .questions
        .values()
        .filter(|q| !placed.contains(q.question_id.as_str()))
    {
        let mut leaves = Vec::new();
        index_question(question, &[], &mut leaves);
        rows.push(leaves);
    }
    if let Some(error) = view.status.problem() {
        rows.push(vec![IndexedLeaf {
            text: error.to_string(),
            open_keys: Vec::new(),
        }]);
    }
    if let Some(failed) = &view.failed {
        rows.push(vec![IndexedLeaf {
            text: failed.clone(),
            open_keys: Vec::new(),
        }]);
    }

    rows
}

fn push_leaf(out: &mut Vec<IndexedLeaf>, text: impl Into<String>, keys: &[OpenKey]) {
    out.push(IndexedLeaf {
        text: text.into(),
        open_keys: keys.to_vec(),
    });
}

fn index_item(
    view: &SessionView,
    item: &Item,
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let empty_user = item.kind == "user"
        && view
            .rendered
            .get(&item.item_id)
            .is_none_or(|r| r.source.trim().is_empty());
    if empty_user {
        return;
    }
    if let Some(tool) = &item.tool {
        index_tool_call(item, tool, keys, out);
        return;
    }
    let source = view
        .rendered
        .get(&item.item_id)
        .map_or("", |r| r.source.as_str());
    match item.kind.as_str() {
        "user" => push_leaf(out, source, keys),
        "thought" => {
            if let Some(rendered) = view.rendered.get(&item.item_id) {
                let mut thought_keys = keys.to_vec();
                if crate::ui::is_long_thought(source) {
                    thought_keys.push(OpenKey::Tool(item.item_id.clone()));
                }
                for leaf in rendered.leaves() {
                    push_leaf(out, leaf.as_str(), &thought_keys);
                }
            }
        }
        "note" => push_leaf(out, format!("· {source}"), keys),
        "notice" => {
            let mut notice_keys = keys.to_vec();
            if source.starts_with("HOST action ")
                || source.starts_with("HOST process ")
                || crate::ui::is_long_notice(source)
            {
                notice_keys.push(OpenKey::Tool(item.item_id.clone()));
            }
            push_leaf(out, source, &notice_keys);
        }
        "plan" => {
            for block in &item.content {
                push_leaf(out, block.text.as_deref().unwrap_or(""), keys);
            }
        }
        _ => {
            if let Some(rendered) = view.rendered.get(&item.item_id) {
                for leaf in rendered.leaves() {
                    push_leaf(out, leaf.as_str(), keys);
                }
            }
        }
    }
}

fn index_step_line(
    step: &crate::shell::Step<'_>,
    arguments_quiet: bool,
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let shown = crate::ui::one_line(step.text, 140);
    match shown.split_once(' ') {
        Some((word, arguments)) if arguments_quiet => {
            push_leaf(out, word, keys);
            push_leaf(out, arguments.trim_start(), keys);
        }
        _ => push_leaf(out, shown, keys),
    }
    let operator = step.operator.trim_start();
    if !operator.is_empty() {
        push_leaf(out, operator, keys);
    }
}

fn index_tool_output(
    item_id: &str,
    tool: &Tool,
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let mut out_keys = keys.to_vec();
    out_keys.push(OpenKey::Tool(item_id.to_string()));
    if !tool.detail.trim().is_empty() {
        push_leaf(out, tool.detail.trim_end(), &out_keys);
    }
    if !tool.diff.trim().is_empty() {
        for line in tool.diff.lines().take(400) {
            push_leaf(out, line, &out_keys);
        }
    }
}

fn index_tool_call(
    item: &Item,
    tool: &Tool,
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    for step in crate::tool_runs::lines(tool) {
        index_step_line(&step, false, keys, out);
    }
    index_tool_output(&item.item_id, tool, keys, out);
}

fn index_tool_run(
    run: &[&Item],
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let groups = crate::tool_runs::groups(run);
    for group in &groups {
        let name = crate::ui::one_line(&group.name, 24);
        let (first, first_tool) = group.members[0];
        let single = group.members.len() == 1;
        if single {
            let lines = crate::tool_runs::lines(first_tool);
            if let [head, more @ ..] = lines.as_slice()
                && !more.is_empty()
            {
                index_step_line(head, true, keys, out);
                for step in more {
                    index_step_line(step, true, keys, out);
                }
            } else {
                let latest = crate::ui::one_line(&group.latest(), 140);
                push_leaf(out, name, keys);
                push_leaf(out, latest, keys);
            }
            index_tool_output(&first.item_id, first_tool, keys, out);
        } else {
            let mut group_keys = keys.to_vec();
            group_keys.push(OpenKey::Group(first.item_id.clone()));
            for (item, tool) in &group.members {
                index_tool_call(item, tool, &group_keys, out);
            }
        }
    }
}

fn index_host_details(
    action: &crate::host_runs::Action<'_>,
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let mut detail_keys = keys.to_vec();
    detail_keys.push(OpenKey::Tool(action.key.clone()));
    if let Some(request) = action.request {
        push_leaf(out, format!("{} {request}", action.op), &detail_keys);
    }
    match action.outcome {
        Some(crate::host_runs::Outcome::Done(result)) => push_leaf(out, result, &detail_keys),
        Some(crate::host_runs::Outcome::Failed(error)) => push_leaf(out, error, &detail_keys),
        None => {}
    }
}

fn index_host_run(
    notices: &[&Item],
    keys: &[OpenKey],
    out: &mut Vec<IndexedLeaf>,
) {
    let texts: Vec<(&str, &str)> = notices
        .iter()
        .filter_map(|item| Some((item.item_id.as_str(), crate::host_runs::host_text(item)?)))
        .collect();
    let groups = crate::host_runs::groups(crate::host_runs::actions(&texts));
    for group in &groups {
        let single = group.actions.len() == 1;
        let first = &group.actions[0];
        if single {
            index_host_details(first, keys, out);
        } else {
            let mut group_keys = keys.to_vec();
            group_keys.push(OpenKey::Group(first.key.clone()));
            for action in &group.actions {
                index_host_details(action, &group_keys, out);
            }
        }
    }
}

fn index_asked(question: &Question, keys: &[OpenKey], out: &mut Vec<IndexedLeaf>) {
    if let Some(description) = &question.description {
        push_leaf(out, description, keys);
    }
    if let Some(subject) = &question.subject
        && let Some(text) = &subject.text
    {
        push_leaf(out, text, keys);
    }
}

fn index_question(question: &Question, keys: &[OpenKey], out: &mut Vec<IndexedLeaf>) {
    if question.state == "open" {
        push_leaf(out, &question.title, keys);
        index_asked(question, keys, out);
        for field in &question.fields {
            if !field.description.is_empty() {
                push_leaf(out, &field.description, keys);
            }
        }
    } else {
        let outcome = match &question.answer {
            Some(Answer::Permission { option_id }) => question
                .options
                .iter()
                .find(|o| &o.option_id == option_id)
                .map_or(option_id.as_str(), |o| o.title.as_str())
                .to_string(),
            Some(Answer::Cancel) => "cancelled".into(),
            Some(Answer::Form { .. }) => "answered".into(),
            None => question.state.clone(),
        };
        push_leaf(out, format!("{} → {outcome}", question.title), keys);
        let mut ans_keys = keys.to_vec();
        ans_keys.push(OpenKey::Answer(question.question_id.clone()));
        index_asked(question, &ans_keys, out);
    }
}

/// All matches of `(query, case_sensitive)` in the indexed `rows`.
fn collect_matches(rows: &[Vec<IndexedLeaf>], query: &str, case_sensitive: bool) -> Vec<Match> {
    let mut matches = Vec::new();
    for (row_idx, leaves) in rows.iter().enumerate() {
        for (leaf_idx, leaf) in leaves.iter().enumerate() {
            for range in match_ranges(&leaf.text, query, case_sensitive) {
                matches.push(Match {
                    row: row_idx,
                    leaf_in_row: leaf_idx,
                    range,
                    open_keys: leaf.open_keys.clone(),
                });
            }
        }
    }
    matches
}

/// Estimates the conversation's scroll offset and each entry row's `(y, height)` from the
/// measured heights, as [`crate::rows::plan`] lays them out in a view `viewport` high.
fn row_positions(view: &SessionView, viewport: f32) -> (f32, Vec<(f32, f32)>) {
    let conversation = &view.conversation;
    let items = conversation.visible_items();
    let places = conversation.question_places();
    let folds = crate::folds::rows(
        &items,
        |item| conversation.purpose(item),
        |item| places.contains_key(item.item_id.as_str()),
    );
    let heights = view.heights.borrow();
    let mut positions = Vec::with_capacity(folds.len());
    let mut y = 0.0_f32;
    for row in &folds {
        let h = heights
            .get(&items[row.range().start].item_id)
            .map_or(0.0, |(_, h)| *h);
        positions.push((y, h));
        y += h + crate::rows::SPACING;
    }
    let top_y = if view.follow_tail {
        let total = (y - crate::rows::SPACING).max(0.0);
        (total - viewport).max(0.0)
    } else {
        view.scroll_offset
    };
    (top_y, positions)
}

/// Picks the first match at or below the top of the viewport. Within the top row, uses the
/// laid-out `visible_leaves` when there are any, so matches scrolled above the top of a long
/// message are skipped in favour of the first visible one.
fn first_match_at_or_below_top(
    view: &SessionView,
    viewport: f32,
    rows: &[Vec<IndexedLeaf>],
    matches: &[Match],
    visible_leaves: &[VisibleLeaf],
) -> usize {
    let (top_y, positions) = row_positions(view, viewport);
    let top_row = positions
        .iter()
        .position(|&(ry, rh)| ry + rh > top_y + 1.0)
        .unwrap_or(0);
    let top_row_leaves: Vec<&VisibleLeaf> = visible_leaves
        .iter()
        .filter(|l| l.row == Some(top_row))
        .collect();
    let is_open = |keys: &[OpenKey]| keys.iter().all(|k| k.is_open(view));
    for (idx, m) in matches.iter().enumerate() {
        if m.row < top_row {
            continue;
        }
        if m.row > top_row || !is_open(&m.open_keys) {
            return idx;
        }
        let shown_before = rows[top_row][..m.leaf_in_row]
            .iter()
            .filter(|leaf| is_open(&leaf.open_keys))
            .count();
        let Some(rect) = top_row_leaves
            .get(shown_before)
            .and_then(|leaf| Some((leaf, leaf.layout.highlight_rects(m.range).into_iter().next()?)))
            .map(|(leaf, rect)| leaf.origin_y + rect.y + rect.height)
        else {
            return idx;
        };
        if rect > top_y + 1.0 {
            return idx;
        }
    }
    0
}

/// The conversation's height: until it has scrolled, the window's stands in (as in
/// [`crate::ui`]).
fn viewport(view: &SessionView, window_height: f32) -> f32 {
    if view.viewport > 0.0 {
        view.viewport
    } else {
        window_height
    }
}

/// The reader's text in the order its leaves are built: the preview's Markdown leaves, the diff's
/// lines, or the source's lines (the editor's own, so a match's line is the editor's).
fn reader_leaves(view: &SessionView, mode: ReaderMode) -> Vec<String> {
    let (Some(file), Some(reader)) = (&view.files.reader, &view.reader) else {
        return Vec::new();
    };
    match mode {
        ReaderMode::Preview => {
            let mut out = Vec::new();
            if let Some(markdown) = &reader.markdown {
                crate::viewer::markdown_leaves(markdown.items(), &mut out);
            }
            out
        }
        ReaderMode::Diff => crate::diff_ui::numbered_diff(&file.text)
            .into_iter()
            .map(|(_, _, line)| line.to_string())
            .collect(),
        ReaderMode::Source => (0..reader.content.line_count())
            .map(|i| {
                reader
                    .content
                    .line(i)
                    .map_or_else(String::new, |line| line.text.into_owned())
            })
            .collect(),
        ReaderMode::Waiting => Vec::new(),
    }
}

/// The reader's first match at or below where it is: in the source, at or after the cursor (the
/// start of its selection); in the preview or diff, the first at or below the top of the view,
/// from the leaves laid out last.
fn reader_first_match(
    view: &SessionView,
    mode: ReaderMode,
    leaves: usize,
    matches: &[Match],
    visible_leaves: &[VisibleLeaf],
) -> usize {
    let found = match mode {
        ReaderMode::Source => view.reader.as_ref().and_then(|reader| {
            let cursor = reader.content.cursor();
            let key = |p: text_editor::Position| (p.line, p.column);
            let at = cursor
                .selection
                .map_or(key(cursor.position), |s| key(s).min(key(cursor.position)));
            matches
                .iter()
                .position(|m| (m.leaf_in_row, m.range.0) >= at)
        }),
        ReaderMode::Preview | ReaderMode::Diff if visible_leaves.len() == leaves => {
            matches.iter().position(|m| {
                let leaf = &visible_leaves[m.leaf_in_row];
                leaf.layout
                    .highlight_rects(m.range)
                    .into_iter()
                    .next()
                    .is_none_or(|rect| leaf.origin_y + rect.y + rect.height > leaf.view_top + 1.0)
            })
        }
        _ => None,
    };
    found.unwrap_or(0)
}

/// The rectangles of `marks` (`(line, byte range, current)`, in line order) in the lines the
/// source editor has laid out, in its text's own coordinates.
pub fn mark_rects(
    buffer: &iced::advanced::graphics::text::cosmic_text::Buffer,
    marks: &[(usize, Range<usize>, bool)],
) -> Vec<(Rectangle, iced::Color)> {
    use iced::advanced::graphics::text::cosmic_text::Cursor;
    let mut out = Vec::new();
    if marks.is_empty() {
        return out;
    }
    for run in buffer.layout_runs() {
        let first = marks.partition_point(|m| m.0 < run.line_i);
        for (line, range, current) in marks[first..].iter().take_while(|m| m.0 == run.line_i) {
            if let Some((x, width)) =
                run.highlight(Cursor::new(*line, range.start), Cursor::new(*line, range.end))
                && width > 0.0
            {
                out.push((
                    Rectangle {
                        x,
                        y: run.line_top,
                        width,
                        height: run.line_height,
                    },
                    style::find_match(*current),
                ));
            }
        }
    }
    out
}

impl App {
    /// Whether a running session terminal or host shell currently has keyboard focus.
    pub fn terminal_focused(&self) -> bool {
        self.find_available()
            && self.menu.is_none()
            && self.current().is_some_and(|v| {
                v.terminal_shown
                    && v.terminal
                        .as_ref()
                        .is_some_and(|t| t.focused && t.status == crate::terminal::Status::Running)
            })
    }

    /// Whether the window shows something Find searches.
    pub fn find_available(&self) -> bool {
        self.find_surface().is_some()
    }

    /// What Find would search now: the reader when a file is open, else the conversation. Pages,
    /// session details, an annotation and the test desktop are not searched (yet).
    pub fn find_surface(&self) -> Option<Surface> {
        if self.backend_shown() || self.details.is_some() {
            return None;
        }
        let view = self.current()?;
        if view.annotation.is_some() {
            return None;
        }
        if let Some(file) = &view.files.reader {
            let reader = view.reader.as_ref();
            let mode = if file.error.is_some() || file.pending.is_some() {
                ReaderMode::Waiting
            } else if file.source {
                ReaderMode::Source
            } else if file.repo.is_some() {
                ReaderMode::Diff
            } else if reader.is_some_and(|r| r.markdown.is_some()) {
                ReaderMode::Preview
            } else {
                ReaderMode::Waiting
            };
            return Some(Surface::Reader {
                key: sill_client::files::key(&file.path).into(),
                mode,
                revision: file.revision,
                edits: reader.map_or(0, |r| r.edits),
            });
        }
        (!view.desktop.shown).then_some(Surface::Conversation)
    }

    /// After every message: the bar closes when the window shows another surface (another tab,
    /// another file, a page; docs/find.md, decision 3), and looks again when the same file is
    /// shown another way or its text changed.
    pub fn find_follow(&mut self, prior_session: &Option<String>) -> Task<Message> {
        let file = self
            .current()
            .and_then(|v| v.files.reader.as_ref())
            .map(|f| sill_client::files::key(&f.path).to_string());
        if file != self.find.reader_file {
            self.find.reader_file = file;
            self.find.reader_selected.take();
        }
        if !self.find.shown {
            return Task::none();
        }
        let now = self.find_surface();
        if *prior_session != self.selected || now.is_none() {
            self.find.close();
            return Task::none();
        }
        if now == self.find.surface {
            return Task::none();
        }
        match (&self.find.surface, &now) {
            (
                Some(Surface::Reader {
                    key: was,
                    mode: was_mode,
                    revision: was_revision,
                    ..
                }),
                Some(Surface::Reader {
                    key,
                    mode,
                    revision,
                    ..
                }),
            ) if was == key => {
                let keep = was_mode == mode;
                // the file read again is a new editor, its cursor at the start: the kept match is
                // its selection again (an edit keeps the editor, and the cursor where it is typed)
                let replaced = keep && *mode == ReaderMode::Source && was_revision != revision;
                self.find.surface = now;
                let task = self.recompute_find(keep);
                if replaced {
                    Task::batch([task, self.activate_current_match()])
                } else {
                    task
                }
            }
            _ => {
                self.find.close();
                Task::none()
            }
        }
    }

    /// Text streamed into the conversation: its matches again, the current one kept.
    pub fn find_streamed(&mut self) -> Task<Message> {
        if self.find.surface == Some(Surface::Conversation) {
            self.recompute_find(true)
        } else {
            Task::none()
        }
    }

    pub fn find_event(&mut self, event: Event) -> Task<Message> {
        if !self.find.shown && event != Event::Open {
            return Task::none();
        }
        match event {
            Event::Open => {
                let Some(surface) = self.find_surface() else {
                    return Task::none();
                };
                if self.terminal_focused() {
                    return Task::none();
                }
                if let Some(pane) = self.current_mut().and_then(|v| v.terminal.as_mut()) {
                    pane.focused = false;
                }
                let focus = keys::focus_and_select_all("find-query");
                if self.find.shown {
                    return focus;
                }
                let selected = match &surface {
                    Surface::Reader {
                        mode: ReaderMode::Source,
                        ..
                    } => self
                        .current()
                        .and_then(|v| v.reader.as_ref())
                        .and_then(|r| r.content.selection()),
                    Surface::Reader { .. } => self.find.reader_selected.borrow().clone(),
                    Surface::Conversation => self.find.selected_text.borrow().clone(),
                };
                if let Some(selected) = selected
                    && !selected.is_empty()
                    && selected.chars().count() <= 120
                    && !selected.contains(['\n', '\r'])
                {
                    self.find.query = selected;
                }
                self.find.shown = true;
                self.find.surface = Some(surface);
                Task::batch([focus, self.recompute_find(false)])
            }
            Event::Close => {
                let surface = self.find.surface.clone();
                self.find.close();
                match surface {
                    Some(Surface::Conversation) => iced::widget::operation::focus("prompt"),
                    // the cursor stays on the last match (docs/find.md, the reader)
                    Some(Surface::Reader {
                        mode: ReaderMode::Source,
                        ..
                    }) => iced::widget::operation::focus("file-reader"),
                    _ => Task::none(),
                }
            }
            Event::Input(query) => {
                self.find.query = query;
                self.recompute_find(false)
            }
            Event::ToggleCase => {
                self.find.case_sensitive = !self.find.case_sensitive;
                self.recompute_find(false)
            }
            Event::Next | Event::Prev => {
                let len = self.find.matches.len();
                if len == 0 {
                    return Task::none();
                }
                self.find.current = Some(match (self.find.current, event == Event::Next) {
                    (Some(i), true) => (i + 1) % len,
                    (None, true) => 0,
                    (Some(0) | None, false) => len - 1,
                    (Some(i), false) => i - 1,
                });
                self.activate_current_match()
            }
        }
    }

    /// Recomputes the matches for the query. With `keep_current` (text streamed in, a file read
    /// again), the current match stays where it is; otherwise the first match from the top of the
    /// view (in the source, from the cursor) becomes current and is revealed.
    pub fn recompute_find(&mut self, keep_current: bool) -> Task<Message> {
        if !self.find.shown {
            return Task::none();
        }
        let Some(surface) = self.find.surface.clone() else {
            return Task::none();
        };
        let Some(view) = self.current() else {
            return Task::none();
        };
        let rows = match (&surface, self.find.query.is_empty()) {
            (_, true) => Vec::new(),
            (Surface::Conversation, false) => index_rows(view),
            (Surface::Reader { mode, .. }, false) => vec![
                reader_leaves(view, *mode)
                    .into_iter()
                    .map(|text| IndexedLeaf {
                        text,
                        open_keys: Vec::new(),
                    })
                    .collect(),
            ],
        };
        let matches = collect_matches(&rows, &self.find.query, self.find.case_sensitive);
        if matches.is_empty() {
            self.find.matches.clear();
            self.find.current = None;
            return Task::none();
        }
        let kept = keep_current
            .then(|| self.find.current.and_then(|i| self.find.matches.get(i)))
            .flatten()
            .map(|prev| {
                matches
                    .iter()
                    .position(|m| {
                        (m.row, m.leaf_in_row, m.range) == (prev.row, prev.leaf_in_row, prev.range)
                    })
                    .unwrap_or(self.find.current.unwrap_or(0).min(matches.len() - 1))
            });
        if let Some(current) = kept {
            self.find.matches = matches;
            self.find.current = Some(current);
            return Task::none();
        }
        let initial = match &surface {
            Surface::Conversation => first_match_at_or_below_top(
                view,
                viewport(view, self.height),
                &rows,
                &matches,
                &self.find.visible_leaves.borrow(),
            ),
            Surface::Reader { mode, .. } => reader_first_match(
                view,
                *mode,
                rows[0].len(),
                &matches,
                &self.find.visible_leaves.borrow(),
            ),
        };
        self.find.matches = matches;
        self.find.current = Some(initial);
        self.activate_current_match()
    }

    /// Brings the current match into view: in the conversation, opens what hides it and scrolls
    /// its row into the built range; in the reader's source, makes it the editor's selection.
    fn activate_current_match(&mut self) -> Task<Message> {
        let Some(m) = self.find.current.and_then(|i| self.find.matches.get(i)).cloned() else {
            return Task::none();
        };
        match self.find.surface {
            Some(Surface::Conversation) => {}
            Some(Surface::Reader { mode, .. }) => {
                return match mode {
                    ReaderMode::Preview => reveal_match("markdown-reader"),
                    ReaderMode::Diff => reveal_match("diff-reader"),
                    ReaderMode::Source => {
                        let Some(reader) = self.current_mut().and_then(|v| v.reader.as_mut())
                        else {
                            return Task::none();
                        };
                        let at = |column| text_editor::Position {
                            line: m.leaf_in_row,
                            column,
                        };
                        reader.content.select(text_editor::Cursor {
                            position: at(m.range.1),
                            selection: Some(at(m.range.0)),
                        });
                        crate::files::reveal_caret(reader)
                    }
                    ReaderMode::Waiting => Task::none(),
                };
            }
            None => return Task::none(),
        }
        let window_height = self.height;
        let Some(view) = self.current_mut() else {
            return Task::none();
        };
        for key in &m.open_keys {
            key.open(view);
        }
        let viewport = viewport(view, window_height);
        let (top_y, positions) = row_positions(view, viewport);
        view.follow_tail = false;
        view.scroll_offset = top_y;
        if let Some(&(row_y, row_h)) = positions.get(m.row) {
            let margin = (viewport * 0.1).min(48.0);
            if row_y + row_h < top_y + margin || row_y > top_y + viewport - margin {
                view.scroll_offset = (row_y - viewport / 3.0).max(0.0);
            }
        }
        reveal_match("conversation")
    }
}

#[derive(Clone)]
struct ScrollFrame {
    id: Option<Id>,
    bounds: Rectangle,
    content: iced::Size,
    translation: Vector,
}

/// Where a scrollable should be scrolled along one axis for `[start, end]` (content coordinates)
/// to show: as it is when the match is inside the view, away from its edges; otherwise with the
/// match a third of the way in.
fn reveal_axis(start: f32, end: f32, scroll: f32, view: f32, content: f32) -> f32 {
    let margin = (view * 0.1).min(48.0);
    let max_scroll = (content - view).max(0.0);
    if start < scroll + margin || end > scroll + view - margin {
        (start - view / 3.0).clamp(0.0, max_scroll)
    } else {
        scroll.clamp(0.0, max_scroll)
    }
}

struct RevealMatch {
    id: Id,
    entered: Option<ScrollFrame>,
    frames: Vec<ScrollFrame>,
    /// The scrollable and its offset; `x` only when it is wider than its view (the diff).
    target: Option<(Id, Option<f32>, f32)>,
}

impl Operation for RevealMatch {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        match self.entered.take() {
            Some(frame) => {
                self.frames.push(frame);
                operate(self);
                self.frames.pop();
            }
            None => operate(self),
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        self.entered = Some(ScrollFrame {
            id: id.cloned(),
            bounds,
            content: content_bounds.size(),
            translation,
        });
    }

    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        if self.target.is_some() {
            return;
        }
        let Some(selectable) = state.downcast_ref::<Selectable>() else {
            return;
        };
        let Some(rect) = selectable.current_match_rect() else {
            return;
        };
        let Some(frame) = self
            .frames
            .iter()
            .rev()
            .find(|f| f.id.as_ref() == Some(&self.id))
        else {
            return;
        };
        let origin = selectable.origin(bounds);
        let top = origin.y + rect.y - frame.bounds.y;
        let y = reveal_axis(
            top,
            top + rect.height,
            frame.translation.y,
            frame.bounds.height,
            frame.content.height,
        );
        let x = (frame.content.width > frame.bounds.width + 0.5).then(|| {
            let left = origin.x + rect.x - frame.bounds.x;
            reveal_axis(
                left,
                left + rect.width,
                frame.translation.x,
                frame.bounds.width,
                frame.content.width,
            )
        });
        self.target = Some((self.id.clone(), x, y));
    }

    fn finish(&self) -> Outcome<()> {
        match &self.target {
            Some((id, x, y)) => Outcome::Chain(Box::new(scroll::scroll_to(
                id.clone(),
                AbsoluteOffset {
                    x: *x,
                    y: Some(*y),
                },
            ))),
            None => Outcome::None,
        }
    }
}

fn reveal_match(id: &'static str) -> Task<Message> {
    keys::effect(RevealMatch {
        id: Id::new(id),
        entered: None,
        frames: Vec::new(),
        target: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_ranges_handles_case_unicode_and_non_overlapping_occurrences() {
        assert_eq!(match_ranges("hello", "", false), Vec::<(usize, usize)>::new());
        assert_eq!(match_ranges("ababa", "aba", false), vec![(0, 3)]);
        assert_eq!(
            match_ranges("Send send SEND", "send", false),
            vec![(0, 4), (5, 9), (10, 14)]
        );
        assert_eq!(
            match_ranges("Send send SEND", "send", true),
            vec![(5, 9)]
        );
        // Multi-byte UTF-8: cafés and Café
        let hay = "Deux cafés au Café Central";
        let ranges = match_ranges(hay, "café", false);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&hay[ranges[0].0..ranges[0].1], "café");
        assert_eq!(&hay[ranges[1].0..ranges[1].1], "Café");
        assert_eq!(
            match_ranges(hay, "Café", true),
            vec![ranges[1]]
        );
    }
}
