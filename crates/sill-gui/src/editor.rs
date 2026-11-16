//! Bounded text and cursor history shared by editable buffers. Replacing a buffer starts fresh.
use iced::widget::text_editor::{Action, Content, Cursor, Edit};
use std::collections::VecDeque;
use std::ops::Deref;
use std::time::{Duration, Instant};

const GROUP_PAUSE: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Typing,
    Backspace,
    Delete,
}

struct Group {
    kind: Kind,
    cursor: Cursor,
    at: Instant,
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// The text removed by a deletion, with unchanged prefix and suffix stripped.
fn removed_word(before: &str, after: &str) -> bool {
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let before = &before[prefix..];
    let after = &after[prefix..];
    let suffix: usize = before
        .chars()
        .rev()
        .zip(after.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let removed = &before[..before.len() - suffix];
    !removed.is_empty() && removed.chars().all(word)
}

struct Snapshot {
    text: String,
    cursor: Cursor,
}

#[derive(Default)]
pub struct Editor {
    content: Content,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    group: Option<Group>,
}

impl Deref for Editor {
    type Target = Content;
    fn deref(&self) -> &Content {
        &self.content
    }
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_text(text: &str) -> Self {
        Self {
            content: Content::with_text(text),
            ..Self::default()
        }
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.content.text(),
            cursor: self.content.cursor(),
        }
    }
    fn push(history: &mut VecDeque<Snapshot>, snapshot: Snapshot) {
        history.push_back(snapshot);
        // Bound both the number of edits and retained text, including large pastes.
        let mut bytes: usize = history.iter().map(|s| s.text.len()).sum();
        while history.len() > 100 || bytes > 8 * 1024 * 1024 {
            bytes -= history.pop_front().unwrap().text.len();
        }
    }
    pub fn break_group(&mut self) {
        self.group = None;
    }
    pub fn perform(&mut self, action: Action) {
        self.perform_at(action, Instant::now());
    }
    fn perform_at(&mut self, action: Action, now: Instant) {
        let before = action.is_edit().then(|| self.snapshot());
        // Selection replacement and non-word edits are always their own undo step.
        let kind = if self.content.selection().is_none() {
            match &action {
                Action::Edit(Edit::Insert(c)) if word(*c) => Some(Kind::Typing),
                Action::Edit(Edit::Backspace) => Some(Kind::Backspace),
                Action::Edit(Edit::Delete) => Some(Kind::Delete),
                _ => None,
            }
        } else {
            None
        };
        self.content.perform(action);
        if let Some(before) = before {
            let after = self.content.text();
            if before.text != after {
                let kind =
                    kind.filter(|kind| *kind == Kind::Typing || removed_word(&before.text, &after));
                let merge = kind.is_some_and(|kind| {
                    self.group.as_ref().is_some_and(|group| {
                        group.kind == kind
                            && group.cursor == before.cursor
                            && now.saturating_duration_since(group.at) < GROUP_PAUSE
                    })
                });
                if !merge {
                    Self::push(&mut self.undo, before);
                }
                self.redo.clear();
                self.group = kind.map(|kind| Group {
                    kind,
                    cursor: self.content.cursor(),
                    at: now,
                });
                return;
            }
        }
        self.break_group();
    }
    /// A change made directly to the content, as one undo step. Passes on what `change` returns.
    pub fn edit<T>(&mut self, change: impl FnOnce(&mut Content) -> T) -> T {
        let before = self.snapshot();
        let result = change(&mut self.content);
        if self.content.text() != before.text {
            Self::push(&mut self.undo, before);
            self.redo.clear();
        }
        self.break_group();
        result
    }
    /// Puts the cursor at `cursor` (a selection when it has one) without touching the text or
    /// its history: Find's current match in the reader's source.
    pub fn select(&mut self, cursor: Cursor) {
        self.content.move_to(cursor);
        self.break_group();
    }
    pub fn history(&mut self, redo: bool) {
        self.break_group();
        let snapshot = if redo {
            self.redo.pop_back()
        } else {
            self.undo.pop_back()
        };
        if let Some(snapshot) = snapshot {
            let current = self.snapshot();
            Self::push(if redo { &mut self.undo } else { &mut self.redo }, current);
            self.content = Content::with_text(&snapshot.text);
            self.content.move_to(snapshot.cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::text_editor::Edit;
    use std::sync::Arc;
    #[test]
    fn paste_undo_redo_and_branch() {
        let mut prompt = Editor::with_text("hello");
        let cursor = prompt.cursor();
        prompt.perform(Action::Edit(Edit::Paste(Arc::new("world\n".into()))));
        let pasted = prompt.text();
        prompt.history(false);
        assert_eq!(prompt.text(), "hello");
        assert_eq!(prompt.cursor(), cursor);
        prompt.history(true);
        assert_eq!(prompt.text(), pasted);
        prompt.history(false);
        prompt.perform(Action::Edit(Edit::Insert('!')));
        let branched = prompt.text();
        prompt.history(true);
        assert_eq!(prompt.text(), branched);
        assert_eq!(Editor::new().undo.len(), 0);
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;
    use iced::widget::text_editor::Motion;
    #[test]
    fn a_list_edit_is_one_undo_step() {
        let mut prompt = Editor::with_text("- one\n- ");
        prompt.perform(Action::Move(Motion::DocumentEnd));
        prompt.edit(crate::lists::newline);
        assert_eq!(prompt.text(), "- one\n");
        prompt.history(false);
        assert_eq!(prompt.text(), "- one\n- ");
        assert!(prompt.edit(|content| crate::lists::indent(content, false)));
        assert_eq!(prompt.text(), "- one\n  - ");
        prompt.history(false);
        assert_eq!(prompt.text(), "- one\n- ");
        // nothing changed, nothing to undo
        assert!(prompt.edit(|content| crate::lists::indent(content, true)));
        prompt.history(false);
        assert_eq!(prompt.text(), "- one\n- ");
        prompt.history(true);
        assert_eq!(prompt.text(), "- one\n  - ");
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;
    use iced::widget::text_editor::Motion;
    fn type_text(editor: &mut Editor, text: &str, at: Instant) {
        for c in text.chars() {
            editor.perform_at(Action::Edit(Edit::Insert(c)), at);
        }
    }
    #[test]
    fn words_and_boundaries_round_trip_in_both_directions() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello café!", Instant::now());
        for expected in ["hello café", "hello ", "hello", ""] {
            editor.history(false);
            assert_eq!(editor.text(), expected);
        }
        for expected in ["hello", "hello ", "hello café", "hello café!"] {
            editor.history(true);
            assert_eq!(editor.text(), expected);
        }
    }
    #[test]
    fn pause_movement_replacement_and_branch_break_groups() {
        let mut editor = Editor::new();
        let now = Instant::now();
        type_text(&mut editor, "one", now);
        type_text(&mut editor, "two", now + GROUP_PAUSE);
        editor.history(false);
        assert_eq!(editor.text(), "one");
        type_text(&mut editor, "new", now + GROUP_PAUSE);
        editor.history(true);
        assert_eq!(editor.text(), "onenew");
        editor.history(false);
        assert_eq!(editor.text(), "one");
        editor.perform(Action::Move(Motion::Left));
        editor.perform(Action::Move(Motion::Right));
        type_text(&mut editor, "x", now + GROUP_PAUSE);
        editor.history(false);
        assert_eq!(editor.text(), "one");
        editor.perform(Action::SelectAll);
        let selection = editor.cursor();
        type_text(&mut editor, "xy", now + GROUP_PAUSE);
        editor.history(false);
        assert_eq!(editor.text(), "x");
        editor.history(false);
        assert_eq!(editor.text(), "one");
        assert_eq!(editor.cursor(), selection);
    }
    #[test]
    fn cursor_movement_and_save_boundary_split_continuous_typing() {
        for save in [false, true] {
            let mut editor = Editor::new();
            let now = Instant::now();
            type_text(&mut editor, "first", now);
            if save {
                editor.break_group();
            } else {
                editor.perform(Action::Move(Motion::Left));
                editor.perform(Action::Move(Motion::Right));
            }
            type_text(&mut editor, "second", now);
            editor.history(false);
            assert_eq!(editor.text(), "first");
            editor.history(false);
            assert_eq!(editor.text(), "");
            editor.history(true);
            assert_eq!(editor.text(), "first");
            editor.history(true);
            assert_eq!(editor.text(), "firstsecond");
        }
    }
    #[test]
    fn deletion_groups_stop_at_word_boundaries_and_redo_together() {
        for backwards in [true, false] {
            let mut editor = Editor::with_text("one two");
            if backwards {
                editor.perform(Action::Move(Motion::DocumentEnd));
            }
            let now = Instant::now();
            let edit = if backwards {
                Edit::Backspace
            } else {
                Edit::Delete
            };
            for _ in 0..3 {
                editor.perform_at(Action::Edit(edit.clone()), now);
            }
            assert_eq!(editor.text(), if backwards { "one " } else { " two" });
            editor.perform_at(Action::Edit(edit.clone()), now);
            editor.perform_at(Action::Edit(edit), now);
            editor.history(false);
            assert_eq!(editor.text(), if backwards { "one" } else { "two" });
            editor.history(false);
            assert_eq!(editor.text(), if backwards { "one " } else { " two" });
            editor.history(false);
            assert_eq!(editor.text(), "one two");
            editor.history(true);
            assert_eq!(editor.text(), if backwards { "one " } else { " two" });
        }
    }
}
