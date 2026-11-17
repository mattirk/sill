//! The reader's source editor: its rendering, editing and the caret, saving, and the guard that
//! keeps unsaved edits from being dropped by navigation, closing or a reply.
use iced::Task;
use iced::widget::{markdown, text_editor};
use sill_client::files::{self, Request};
use sill_proto::method;

use super::Action;
use crate::{App, Message, SessionView};

/// The open file as the window draws it, rebuilt when the file's text changes.
pub struct ReaderView {
    pub key: String,
    pub revision: u64,
    pub content: crate::editor::Editor,
    pub source_columns: usize,
    pub markdown: Option<markdown::Content>,
    pub saved: String,
    pub dirty: bool,
    pub saving: Option<u64>,
    pub notice: Option<String>,
    /// Counts the person's edits to the source, so Find knows to look again.
    pub edits: u64,
}

/// Source Code Pro uses fixed-width cells; tabs advance to eight-column stops.
pub(crate) fn source_columns(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    text.lines()
        .map(|line| {
            line.split('\t').enumerate().fold(0, |columns, (index, part)| {
                let columns = if index == 0 { columns } else { (columns / 8 + 1) * 8 };
                columns + part.width()
            })
        })
        .max()
        .unwrap_or(0)
}

/// Keep the caret visible inside the source's horizontal viewport during keyboard editing.
struct RevealSourceCaret {
    id: iced::advanced::widget::Id,
    x: f32,
}

impl iced::advanced::widget::operation::Operation for RevealSourceCaret {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::operation::Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        _content: iced::Rectangle,
        translation: iced::Vector,
        state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id != Some(&self.id) {
            return;
        }
        let margin = 12.0;
        let offset = if self.x < translation.x + margin {
            self.x - margin
        } else if self.x > translation.x + bounds.width - margin {
            self.x - bounds.width + margin
        } else {
            return;
        };
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset { x: Some(offset.max(0.0)), y: None });
    }
}

impl SessionView {
    /// The reader's rendering for the file now open; parsed Markdown only when it is shown.
    pub fn sync_reader(&mut self) {
        let Some(file) = &self.files.reader else {
            self.reader = None;
            return;
        };
        let key = files::key(&file.path);
        if self.reader.as_ref().is_none_or(|r| r.key != key || r.revision != file.revision) {
            self.reader = Some(ReaderView {
                key: key.into(),
                revision: file.revision,
                content: crate::editor::Editor::with_text(&file.text),
                source_columns: source_columns(&file.text),
                markdown: None,
                saved: file.text.clone(),
                dirty: false,
                saving: None,
                notice: None,
                edits: 0,
            });
        }
        let reader = self.reader.as_mut().unwrap();
        if !file.source && file.repo.is_none() && file.renderable() && reader.markdown.is_none() && file.pending.is_none() && file.error.is_none() {
            reader.markdown = Some(markdown::Content::parse(&reader.content.text()));
        }
    }
}

/// Apply a text-editor action or undo/redo to the open source; a caret reveal is returned as a
/// task, which skips the caller's follow-up as it always did.
pub(super) fn edit_source(view: &mut SessionView, action: Action) -> Option<Task<Message>> {
    let editing = matches!(action, Action::Text(text_editor::Action::Edit(_)) | Action::History(_));
    let reveal =
        matches!(action, Action::Text(text_editor::Action::Edit(_) | text_editor::Action::Move(_) | text_editor::Action::Select(_)) | Action::History(_));
    let editable = view.files.reader.as_ref().is_some_and(|f| f.source && f.repo.is_none() && f.pending.is_none() && f.error.is_none());
    if (!editing || editable)
        && let Some(reader) = &mut view.reader
    {
        match action {
            Action::Text(action) => reader.content.perform(action),
            Action::History(redo) => reader.content.history(redo),
            _ => unreachable!(),
        }
        if editing {
            let text = reader.content.text();
            reader.source_columns = source_columns(&text);
            reader.dirty = text != reader.saved;
            reader.markdown = None;
            reader.notice = None;
            reader.edits += 1;
        }
        if reveal {
            return Some(reveal_caret(reader));
        }
    }
    None
}

/// Scrolls the source's sideways scrollbar to the caret (the editor brings it into view
/// vertically itself).
pub(crate) fn reveal_caret(reader: &ReaderView) -> Task<Message> {
    let cursor = reader.content.cursor().position;
    let columns = reader.content.line(cursor.line).map_or(0, |line| source_columns(line.text.get(..cursor.column).unwrap_or(&line.text)));
    let digits = reader.content.line_count().max(1).ilog10() + 1;
    iced_runtime::task::effect(iced_runtime::Action::widget(RevealSourceCaret {
        id: iced::advanced::widget::Id::from(format!("source-horizontal-{}", reader.key)),
        x: 12.0 + (columns as f32 + digits.max(2) as f32 + 2.0) * crate::style::MONO * 0.6,
    }))
}

impl App {
    /// Unsaved buffers survive navigation; ask only before an action can discard them.
    pub fn guard_file_edits(&mut self, message: &Message) -> bool {
        let target = match message {
            Message::Quit | Message::CloseRequested | Message::Control(crate::control::Command::Quit) => {
                self.views.iter().find(|(_, v)| v.reader.as_ref().is_some_and(|r| r.dirty || r.saving.is_some())).map(|(id, _)| id.clone())
            }
            Message::CloseTab(id) => Some(id.clone()),
            Message::Files(Action::Navigate { session, path, .. })
                if self.views.get(session).and_then(|v| v.files.reader.as_ref()).is_none_or(|f| files::key(&f.path) != files::key(path)) =>
            {
                Some(session.clone())
            }
            Message::Files(Action::Back(_)) => self.selected.clone(),
            Message::Files(
                Action::Open { .. } | Action::OpenAndHide { .. } | Action::CloseViewer | Action::RefreshFile | Action::BrowsePath | Action::PinPath,
            ) => self.selected.clone(),
            Message::Files(Action::Reply(reply)) if matches!(reply.request, Request::Inspect(_)) => Some(reply.session.clone()),
            _ => None,
        };
        let Some(id) = target else {
            return false;
        };
        let Some(reader) = self.views.get_mut(&id).and_then(|v| v.reader.as_mut()) else {
            return false;
        };
        if !reader.dirty && reader.saving.is_none() {
            return false;
        }
        reader.notice = Some(
            if reader.saving.is_some() {
                "Saving… wait before closing or replacing this file."
            } else {
                "Unsaved changes. Save before continuing, or discard them."
            }
            .into(),
        );
        if reader.saving.is_none() {
            self.drawer.pending_discard = Some((id.clone(), Box::new(message.clone())));
        }
        if !matches!(message, Message::Files(Action::Reply(_))) {
            self.selected = Some(id);
            self.changes = None;
            self.settings_page = None;
            self.agents_page.shown = false;
        }
        true
    }

    pub(super) fn save_file(&mut self) -> Task<Message> {
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(&id) else {
            return Task::none();
        };
        let (Some(file), Some(reader)) = (&view.files.reader, &mut view.reader) else {
            return Task::none();
        };
        if file.repo.is_some() || file.pending.is_some() || file.error.is_some() || !reader.dirty || reader.saving.is_some() {
            return Task::none();
        }
        reader.content.break_group();
        let text = reader.content.text();
        if text.len() > sill_proto::admin::FILE_CHUNK_BYTES {
            reader.notice = Some("Save supports files up to 1 MiB; your edits are still here.".into());
            return Task::none();
        }
        let Some(actions) = self.actions.clone() else {
            reader.notice = Some("The demo cannot save files; your edits are still here.".into());
            return Task::none();
        };
        self.drawer.serial += 1;
        let serial = self.drawer.serial;
        reader.saving = Some(serial);
        reader.notice = None;
        let key = reader.key.clone();
        let params = serde_json::to_value(sill_proto::admin::FileWrite {
            // where the text was read from: the session's workspace copy, not the person's checkout
            path: self.drawer.workspace_path(&id, &file.path),
            bytes_base64: sill_proto::admin::encode_bytes(text.as_bytes()),
        })
        .unwrap();
        crate::blocking(
            move || (text, actions.call(method::FILES_WRITE, params)),
            move |(text, outcome)| Message::Files(Action::Saved { session: id.clone(), key: key.clone(), serial, text, outcome }),
        )
    }

    pub(super) fn file_saved(&mut self, session: String, key: String, serial: u64, text: String, outcome: sill_client::actions::Outcome) -> Task<Message> {
        let Some(view) = self.views.get_mut(&session) else {
            return Task::none();
        };
        let Some(reader) = view.reader.as_mut().filter(|r| r.key == key && r.saving == Some(serial)) else {
            return Task::none();
        };
        reader.saving = None;
        if outcome.is_done() {
            reader.content.break_group();
            reader.saved = text.clone();
            reader.dirty = reader.content.text() != text;
            reader.markdown = None;
            reader.notice = Some("Saved".into());
            if let Some(file) = &mut view.files.reader {
                file.text = text;
            }
            if self.drawer.pending_discard.as_ref().is_some_and(|(id, _)| id == &session) {
                self.drawer.pending_discard = None;
            }
            return self.files_refresh(&session);
        }
        reader.notice = Some(match outcome {
            sill_client::actions::Outcome::Uncertain(why) => format!("Save outcome unknown: {why}. Edits kept; check the file before retrying."),
            _ => format!("Save failed: {}. Edits kept.", outcome.problem().unwrap_or("unknown error")),
        });
        Task::none()
    }

    pub(super) fn cancel_discard(&mut self) -> Task<Message> {
        self.drawer.pending_discard = None;
        if let Some(reader) = self.current_mut().and_then(|v| v.reader.as_mut()) {
            reader.notice = None;
        }
        Task::none()
    }

    pub(super) fn discard_edits(&mut self) -> Task<Message> {
        let Some((id, next)) = self.drawer.pending_discard.take() else {
            return Task::none();
        };
        if let Some(reader) = self.views.get_mut(&id).and_then(|v| v.reader.as_mut()) {
            if reader.saving.is_some() {
                return Task::none();
            }
            reader.content = crate::editor::Editor::with_text(&reader.saved);
            reader.source_columns = source_columns(&reader.saved);
            reader.dirty = false;
            reader.notice = None;
            reader.markdown = None;
        }
        self.update(*next)
    }
}
