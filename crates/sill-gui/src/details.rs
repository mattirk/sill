//! Session details occupies the main pane; its drafts survive navigation.
use crate::{App, Message, blocking, style};
use iced::widget::{
    button, column, container, markdown, row, scrollable, space, text, text_editor, text_input,
};
use iced::{Element, Fill, Task};
use serde_json::json;
use sill_proto::{memory::Note, sessions::Session};

#[derive(Debug, Clone)]
pub enum Event {
    Open(String),
    Back,
    Loaded(String, Result<(Session, Vec<Note>, Option<usize>), String>),
    Edit,
    Cancel,
    Input(text_editor::Action),
    Save,
    Note(String),
    Add,
    Remove(String, String),
    Summarize,
    Done(String, Result<(), String>),
}
pub struct Page {
    pub(crate) id: String,
    token: String,
    saving_summary: bool,
    adding_note: bool,
    session: Option<Session>,
    notes: Vec<Note>,
    editor: text_editor::Content,
    rendered: Vec<markdown::Item>,
    editing: bool,
    note: String,
    pub(crate) pending: bool,
    kept: Option<usize>,
    error: Option<String>,
}
impl App {
    pub(crate) fn stash_details(&mut self) {
        if let Some(p) = &self.details {
            if p.editing {
                self.details_drafts.insert(p.id.clone(), p.editor.text());
            }
        }
    }

    pub(crate) fn details_load(&self, id: String) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let key = self
            .details
            .as_ref()
            .filter(|p| p.id == id)
            .map(|p| p.token.clone())
            .unwrap_or_else(|| id.clone());
        blocking(
            move || {
                actions.reading(|c| {
            let session = c.call("_sill/sessions.get", json!({"session_id":id}))?;
            let mut notes = vec![];
            let mut after: Option<String> = None;
            loop {
                let page: sill_proto::memory::Notes = c.call("_sill/memory.notes.list", json!({"session_id":id,"after":after,"limit":128}))?;
                notes.extend(page.notes);
                after = page.next;
                if after.is_none() { break; }
            }
            let kept = sill_client::changes::list(c).ok().map(|entries| entries.iter().filter(|entry| matches!(entry, sill_proto::changes::Entry::Kept { session_id, .. } if session_id == &id)).count());
            Ok((session, notes, kept))
        }).map_err(|e| format!("{e:#}"))
            },
            move |r| Message::Details(Event::Loaded(key, r)),
        )
    }
    pub fn details_event(&mut self, event: Event) -> Task<Message> {
        if let Event::Open(id) = event {
            self.stash_details();
            let draft = self.details_drafts.get(&id).cloned();
            let task = self.choose(id.clone());
            self.details = Some(Page {
                id: id.clone(),
                token: sill_client::request_id("details"),
                saving_summary: false,
                adding_note: false,
                session: None,
                notes: vec![],
                editor: text_editor::Content::new(),
                rendered: vec![],
                editing: false,
                note: String::new(),
                pending: false,
                kept: None,
                error: None,
            });
            if let Some(draft) = draft {
                let p = self.details.as_mut().unwrap();
                p.editor = text_editor::Content::with_text(&draft);
                p.editing = true;
            }
            return Task::batch([task, self.details_load(id)]);
        }
        if matches!(event, Event::Back) {
            self.stash_details();
            self.details = None;
            return self.restore_scroll();
        }
        let Some(page) = &mut self.details else {
            return Task::none();
        };
        let id = page.id.clone();
        let saving_summary = matches!(event, Event::Save);
        let adding_note = matches!(event, Event::Add);
        let request = match event {
            Event::Loaded(key, result) if key == page.token => {
                match result {
                    Ok((session, notes, kept)) => {
                        if !page.editing {
                            page.rendered = markdown::parse(&session.summary).collect();
                            page.editor = text_editor::Content::with_text(&session.summary);
                        }
                        page.session = Some(session);
                        page.notes = notes;
                        page.kept = kept;
                    }
                    Err(e) => page.error = Some(e),
                }
                return Task::none();
            }
            Event::Edit => {
                page.editing = true;
                return Task::none();
            }
            Event::Cancel => {
                self.details_drafts.remove(&id);
                page.editing = false;
                if let Some(s) = &page.session {
                    page.editor = text_editor::Content::with_text(&s.summary);
                }
                return Task::none();
            }
            Event::Input(a) if !page.pending => {
                page.editor.perform(a);
                return Task::none();
            }
            Event::Note(s) if !page.pending => {
                page.note = s;
                return Task::none();
            }
            Event::Save => (
                "_sill/sessions.update",
                json!({"session_id":id,"change":{"type":"summary","text":page.editor.text(),"kind":"person"}}),
            ),
            Event::Add if !page.note.trim().is_empty() => (
                "_sill/memory.notes.add",
                json!({"session_id":id,"text":page.note.trim()}),
            ),
            Event::Remove(timestamp, text) => (
                "_sill/memory.notes.remove",
                json!({"session_id":id,"timestamp":timestamp,"text":text}),
            ),
            Event::Summarize => ("_sill/sessions.checkpoint", json!({"session_id":id})),
            Event::Done(key, result) if key == page.token => {
                page.pending = false;
                match result {
                    Ok(()) => {
                        if page.saving_summary {
                            self.details_drafts.remove(&id);
                            page.editing = false;
                        }
                        if page.adding_note {
                            page.note.clear();
                        }
                    }
                    Err(e) => page.error = Some(e),
                }
                return self.details_load(id);
            }
            _ => return Task::none(),
        };
        if page.pending {
            return Task::none();
        }
        page.pending = true;
        page.error = None;
        page.saving_summary = saving_summary;
        page.adding_note = adding_note;
        let token = page.token.clone();
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        blocking(
            move || {
                let outcome = actions.call(request.0, request.1);
                if outcome.is_done() {
                    Ok(())
                } else {
                    Err(outcome
                        .problem()
                        .unwrap_or("Outcome unknown; reload before retrying")
                        .to_string())
                }
            },
            move |r| Message::Details(Event::Done(token, r)),
        )
    }
    pub fn details_view<'a>(&'a self, page: &'a Page) -> Element<'a, Message> {
        let msg = |e| Message::Details(e);
        let mut body = column![].spacing(20);
        if let Some(s) = &page.session {
            body = body.push(text(sill_client::sessions::display_title(s)).size(24));
            body = body.push(
                text(format!(
                    "{} · {}",
                    s.agent,
                    if s.ended_at.is_some() {
                        "Ended"
                    } else {
                        "Open"
                    }
                ))
                .size(style::SMALL),
            );
            if s.ghost {
                body = body.push(text("Ghost sessions do not store summaries or notes."));
            } else {
                let mut actions = row![text("Summary").size(18), space().width(Fill)].spacing(10);
                if !page.editing {
                    actions = actions.push(crate::keys::press_maybe(
                        button("Edit").style(style::quiet),
                        (!page.pending).then(|| msg(Event::Edit)),
                    ));
                    let idle = self
                        .views
                        .get(&page.id)
                        .is_some_and(|v| !v.conversation.busy())
                        && s.running;
                    actions = actions.push(crate::keys::press_maybe(
                        button("Summarize now").style(style::quiet),
                        (idle && !page.pending).then(|| msg(Event::Summarize)),
                    ));
                }
                body = body.push(actions);
                body = body.push(
                    text(format!(
                        "Summary source: {}",
                        if s.summary_kind.is_empty() {
                            "none yet"
                        } else {
                            &s.summary_kind
                        }
                    ))
                    .size(style::SMALL),
                );
                if page.editing {
                    body = body.push(
                        text_editor(&page.editor)
                            .on_action(move |a| msg(Event::Input(a)))
                            .height(240),
                    );
                    body = body.push(
                        row![
                            crate::keys::press_maybe(
                                button("Save").style(style::quiet),
                                (!page.pending).then(|| msg(Event::Save))
                            ),
                            crate::keys::press_maybe(
                                button("Cancel").style(style::quiet),
                                (!page.pending).then(|| msg(Event::Cancel))
                            )
                        ]
                        .spacing(8),
                    );
                } else if page.rendered.is_empty() {
                    body = body.push(text("No summary yet."));
                } else {
                    body = body.push(
                        markdown::view(&page.rendered, crate::ui::markdown_settings())
                            .map(Message::Link),
                    );
                }
                body = body.push(text("Your notes").size(18));
                for n in &page.notes {
                    body = body.push(
                        row![
                            text(&n.text).width(Fill),
                            crate::keys::press_maybe(
                                button("Remove").style(style::quiet),
                                (!page.pending).then(|| msg(Event::Remove(
                                    n.timestamp.clone(),
                                    n.text.clone()
                                )))
                            )
                        ]
                        .spacing(10),
                    );
                }
                body = body.push(
                    row![
                        text_input("Add a note", &page.note).on_input(move |s| msg(Event::Note(s))),
                        crate::keys::press_maybe(
                            button("Add").style(style::quiet),
                            (!page.pending && !page.note.trim().is_empty())
                                .then(|| msg(Event::Add))
                        )
                    ]
                    .spacing(8),
                );
            }
            body = body.push(text("Session facts").size(18));
            body = body.push(text(format!(
                "Started: {}\nEnded: {}\nPolicy: {}\nTranscript compactions: {}",
                timestamp(&s.created_at),
                s.ended_at
                    .as_deref()
                    .map(timestamp)
                    .unwrap_or_else(|| "Still open".into()),
                s.policy,
                s.compacted
            )));
            let start = s.created_at.parse::<u64>().unwrap_or(0);
            let end = s
                .ended_at
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
            body = body.push(text(format!(
                "Duration: {} min",
                end.saturating_sub(start) / 60
            )));
            if let Some(kept) = page.kept {
                body = body.push(text(format!("Kept changes: {kept}")));
            }
            for pin in &s.pins {
                body = body.push(text(&pin.display));
            }
        } else {
            body = body.push(text("Loading…"));
        }
        if page.pending {
            body = body.push(text("Saving…"));
        }
        if let Some(e) = &page.error {
            body = body.push(text(e));
        }
        scrollable(container(body).padding(24).width(Fill).max_width(800))
            .height(Fill)
            .into()
    }
}

fn timestamp(value: &str) -> String {
    let Ok(seconds) = value.parse::<i64>() else {
        return value.to_owned();
    };
    format!(
        "{} {:02}:{:02} UTC",
        crate::menu::date(seconds),
        seconds.rem_euclid(86400) / 3600,
        seconds.rem_euclid(3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_draft_survives_back_and_reopening_and_cancel_discards_it() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let _ = app.details_event(Event::Input(text_editor::Action::Edit(
            text_editor::Edit::Paste(std::sync::Arc::new("personal draft".into())),
        )));
        let _ = app.details_event(Event::Back);
        assert_eq!(
            app.details_drafts.get("demo").map(String::as_str),
            Some("personal draft")
        );
        let _ = app.details_event(Event::Open("demo".into()));
        let p = app.details.as_ref().unwrap();
        assert!(p.editing);
        assert_eq!(p.editor.text(), "personal draft");
        let _ = app.details_event(Event::Cancel);
        assert!(!app.details_drafts.contains_key("demo"));
    }
    #[test]
    fn a_completion_from_an_earlier_page_cannot_clear_a_new_draft() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let old_token = app.details.as_ref().unwrap().token.clone();
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let _ = app.details_event(Event::Done(old_token, Ok(())));
        assert!(app.details.as_ref().unwrap().editing);
    }
}
