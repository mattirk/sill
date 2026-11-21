//! Session details occupies the main pane; its drafts survive navigation.
use crate::{App, Message, blocking, style};
use iced::widget::{button, column, container, markdown, row, scrollable, space, text, text_editor, text_input};
use iced::{Element, Fill, Task};
use serde_json::json;
use silli_proto::method;
use silli_proto::{memory::Note, sessions::Session};

#[expect(clippy::large_enum_variant, reason = "a window message: made once, handled once, never kept in bulk")]
#[derive(Debug, Clone)]
pub enum Event {
    Open(String),
    Back,
    Loaded(String, Result<(Session, Vec<Note>), String>),
    /// The session's kept changes, counted apart: the page does not wait for the Changes listing.
    Kept(String, Option<usize>),
    Edit,
    Cancel,
    Input(text_editor::Action),
    Save,
    Note(String),
    Add,
    Remove(String, String),
    Summarize,
    Done(String, String, Result<(), String>),
}
#[derive(Clone, Default)]
pub(crate) struct Draft {
    summary: Option<String>,
    note: String,
    mutation: Option<Mutation>,
    error: Option<String>,
}
#[derive(Clone)]
struct Mutation {
    token: String,
    summary: bool,
    note: bool,
}
pub struct Page {
    pub(crate) id: String,
    token: String,
    load_pending: bool,
    refresh_after_load: bool,
    mutation: Option<Mutation>,
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
            self.details_drafts.insert(
                p.id.clone(),
                Draft { summary: p.editing.then(|| p.editor.text()), note: p.note.clone(), mutation: p.mutation.clone(), error: p.error.clone() },
            );
        }
    }

    /// The worked time of a session this window follows, as its timer shows it.
    fn worked_ms(&self, id: &str) -> Option<u64> {
        self.views.get(id).filter(|view| view.conversation.session.is_some()).and_then(|view| view.attention().worked_ms)
    }

    pub(crate) fn details_load(&mut self, id: String) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let Some(page) = self.details.as_mut().filter(|p| p.id == id && !p.load_pending) else {
            return Task::none();
        };
        let key = silli_client::request_id("details-load");
        page.token = key.clone();
        page.load_pending = true;
        let (counting, counted, listed) = (actions.clone(), id.clone(), key.clone());
        let load = blocking(
            move || {
                actions
                    .reading(|c| {
                        let session = c.call(method::SESSIONS_GET, json!({"session_id":id}))?;
                        let mut notes = vec![];
                        let mut after: Option<String> = None;
                        loop {
                            let page: silli_proto::memory::Notes = c.call(method::MEMORY_NOTES_LIST, json!({"session_id":id,"after":after,"limit":128}))?;
                            notes.extend(page.notes);
                            after = page.next;
                            if after.is_none() {
                                break;
                            }
                        }
                        Ok((session, notes))
                    })
                    .map_err(|e| format!("{e:#}"))
            },
            move |r| Message::Details(Event::Loaded(key, r)),
        );
        // asked after the page's own read: the daemon answers one connection's requests in order
        let kept = blocking(
            move || {
                counting.reading(silli_client::changes::list).ok().map(|entries| {
                    entries.iter().filter(|entry| matches!(entry, silli_proto::changes::Entry::Kept { session_id, .. } if session_id == &counted)).count()
                })
            },
            move |kept| Message::Details(Event::Kept(listed, kept)),
        );
        Task::batch([load, kept])
    }
    pub fn details_event(&mut self, event: Event) -> Task<Message> {
        if let Event::Open(id) = event {
            self.stash_details();
            let draft = self.details_drafts.get(&id).cloned().unwrap_or_default();
            let task = self.choose(id.clone());
            self.details = Some(Page {
                id: id.clone(),
                token: silli_client::request_id("details"),
                load_pending: false,
                refresh_after_load: false,
                mutation: draft.mutation.clone(),
                session: None,
                notes: vec![],
                editor: text_editor::Content::new(),
                rendered: vec![],
                editing: false,
                note: draft.note,
                pending: draft.mutation.is_some(),
                kept: None,
                error: draft.error,
            });
            if let Some(draft) = draft.summary {
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
        if let Event::Done(token, id, result) = event {
            // Mutations belong to the draft, even while its page is hidden or reopened.
            let active = self.details.as_mut().filter(|p| p.id == id);
            if let Some(page) = active.filter(|p| p.mutation.as_ref().is_some_and(|m| m.token == token)) {
                let mutation = page.mutation.take().unwrap();
                page.pending = false;
                // Finish an older read before refreshing, without applying its stale state.
                // This also bounds reads when several writes finish before a slow read does.
                page.refresh_after_load = page.load_pending;
                match result {
                    Ok(()) => {
                        if mutation.summary {
                            page.editing = false;
                        }
                        if mutation.note {
                            page.note.clear();
                        }
                        page.error = None;
                    }
                    Err(error) => page.error = Some(error),
                }
                self.stash_details();
                return self.details_load(id);
            }
            if let Some(draft) = self.details_drafts.get_mut(&id).filter(|d| d.mutation.as_ref().is_some_and(|m| m.token == token)) {
                let mutation = draft.mutation.take().unwrap();
                match result {
                    Ok(()) => {
                        if mutation.summary {
                            draft.summary = None;
                        }
                        if mutation.note {
                            draft.note.clear();
                        }
                        draft.error = None;
                    }
                    Err(error) => draft.error = Some(error),
                }
            }
            return Task::none();
        }
        let Some(page) = &mut self.details else {
            return Task::none();
        };
        let id = page.id.clone();
        let saving_summary = matches!(event, Event::Save);
        let adding_note = matches!(event, Event::Add);
        let request = match event {
            Event::Loaded(key, result) if key == page.token => {
                page.load_pending = false;
                if std::mem::take(&mut page.refresh_after_load) {
                    return self.details_load(id);
                }
                match result {
                    Ok((session, notes)) => {
                        page.rendered = markdown::parse(&session.summary).collect();
                        if !page.editing {
                            page.editor = text_editor::Content::with_text(&session.summary);
                        }
                        page.session = Some(session);
                        page.notes = notes;
                    }
                    Err(e) => page.error = Some(e),
                }
                return Task::none();
            }
            Event::Kept(key, kept) if key == page.token => {
                page.kept = kept;
                return Task::none();
            }
            Event::Edit if !page.pending => {
                page.editing = true;
                return Task::none();
            }
            Event::Cancel if !page.pending => {
                if let Some(draft) = self.details_drafts.get_mut(&id) {
                    draft.summary = None;
                }
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
            Event::Save => (method::SESSIONS_UPDATE, json!({"session_id":id,"change":{"type":"summary","text":page.editor.text(),"kind":"person"}})),
            Event::Add if !page.note.trim().is_empty() => (method::MEMORY_NOTES_ADD, json!({"session_id":id,"text":page.note.trim()})),
            Event::Remove(timestamp, text) => (method::MEMORY_NOTES_REMOVE, json!({"session_id":id,"timestamp":timestamp,"text":text})),
            Event::Summarize => (method::SESSIONS_CHECKPOINT, json!({"session_id":id})),
            _ => return Task::none(),
        };
        if page.pending {
            return Task::none();
        }
        let Some(actions) = self.actions.clone() else {
            page.error = Some("No daemon connected; your draft is still here.".into());
            return Task::none();
        };
        let token = silli_client::request_id("details-write");
        page.pending = true;
        page.error = None;
        page.mutation = Some(Mutation { token: token.clone(), summary: saving_summary, note: adding_note });
        blocking(
            move || {
                let outcome = actions.call(request.0, request.1);
                match outcome {
                    silli_client::actions::Outcome::Done(_) => Ok(()),
                    silli_client::actions::Outcome::Uncertain(problem) => {
                        Err(format!("The request may have taken effect: {problem}. Check the refreshed details before retrying."))
                    }
                    other => Err(other.problem().unwrap_or("Unexpected response").to_owned()),
                }
            },
            move |r| Message::Details(Event::Done(token, id, r)),
        )
    }
    pub fn details_view<'a>(&'a self, page: &'a Page) -> Element<'a, Message> {
        let msg = |e| Message::Details(e);
        let mut body = column![].spacing(20);
        if let Some(s) = &page.session {
            body = body.push(text(silli_client::sessions::display_title(s)).size(24));
            body = body.push(text(format!("{} · {}", s.agent, if s.ended_at.is_some() { "Ended" } else { "Open" })).size(style::SMALL));
            if s.ghost {
                body = body.push(text("Ghost sessions do not store summaries or notes."));
            } else {
                let mut actions = row![text("Summary").size(18), space().width(Fill)].spacing(10);
                if !page.editing {
                    actions = actions.push(crate::keys::press_maybe(button("Edit").style(style::quiet), (!page.pending).then(|| msg(Event::Edit))));
                    let idle = self.views.get(&page.id).is_some_and(|v| !v.conversation.busy()) && s.running;
                    actions = actions
                        .push(crate::keys::press_maybe(button("Summarize now").style(style::quiet), (idle && !page.pending).then(|| msg(Event::Summarize))));
                }
                body = body.push(actions);
                body = body.push(text(format!("Summary source: {}", if s.summary_kind.is_empty() { "none yet" } else { &s.summary_kind })).size(style::SMALL));
                if page.editing {
                    body = body.push(text_editor(&page.editor).on_action(move |a| msg(Event::Input(a))).height(240));
                    body = body.push(
                        row![
                            crate::keys::press_maybe(button("Save").style(style::quiet), (!page.pending).then(|| msg(Event::Save))),
                            crate::keys::press_maybe(button("Cancel").style(style::quiet), (!page.pending).then(|| msg(Event::Cancel)))
                        ]
                        .spacing(8),
                    );
                } else if page.rendered.is_empty() {
                    body = body.push(text("No summary yet."));
                } else {
                    body = body.push(markdown::view(&page.rendered, crate::ui::markdown_settings()).map(Message::Link));
                }
                body = body.push(text("Your notes").size(18));
                for n in &page.notes {
                    body = body.push(
                        row![
                            text(&n.text).width(Fill),
                            crate::keys::press_maybe(
                                button("Remove").style(style::quiet),
                                (!page.pending).then(|| msg(Event::Remove(n.timestamp.clone(), n.text.clone())))
                            )
                        ]
                        .spacing(10),
                    );
                }
                body = body.push(
                    row![
                        text_input("Add a note", &page.note).on_input(move |s| msg(Event::Note(s))),
                        crate::keys::press_maybe(button("Add").style(style::quiet), (!page.pending && !page.note.trim().is_empty()).then(|| msg(Event::Add)))
                    ]
                    .spacing(8),
                );
            }
            body = body.push(text("Session facts").size(18));
            body = body.push(text(format!(
                "Started: {}\nEnded: {}\nPolicy: {}\nTranscript compactions: {}",
                timestamp(&s.created_at),
                s.ended_at.as_deref().map(timestamp).unwrap_or_else(|| "Still open".into()),
                s.policy,
                s.compacted
            )));
            // the time the agent worked, as the top bar's timer shows it
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            body = body.push(text(format!("Worked: {}", silli_client::supervision::clock(worked_ms(s, self.worked_ms(&s.session_id), now)))));
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
        scrollable(container(body).padding(24).width(Fill).max_width(800)).height(Fill).into()
    }
}

/// The time the agent worked in a session: the live conversation's when this window follows
/// it (what the timer shows), else the session record's, counted on while it works.
fn worked_ms(session: &Session, live: Option<u64>, now_ms: u64) -> u64 {
    live.unwrap_or_else(|| session.active_ms + session.active_since_ms.map_or(0, |since| now_ms.saturating_sub(since)))
}

fn timestamp(value: &str) -> String {
    let Ok(seconds) = value.parse::<i64>() else {
        return value.to_owned();
    };
    format!("{} {:02}:{:02} UTC", crate::menu::date(seconds), seconds.rem_euclid(86400) / 3600, seconds.rem_euclid(3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_draft_survives_back_and_reopening_and_cancel_discards_it() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let _ = app.details_event(Event::Input(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("personal draft".into())))));
        let _ = app.details_event(Event::Back);
        assert_eq!(app.details_drafts.get("demo").and_then(|d| d.summary.as_deref()), Some("personal draft"));
        let _ = app.details_event(Event::Open("demo".into()));
        let p = app.details.as_ref().unwrap();
        assert!(p.editing);
        assert_eq!(p.editor.text(), "personal draft");
        let _ = app.details_event(Event::Cancel);
        assert!(app.details_drafts.get("demo").is_none_or(|d| d.summary.is_none()));
    }
    #[test]
    fn cancel_restored_draft_shows_the_loaded_summary() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let token = app.details.as_ref().unwrap().token.clone();
        let session = serde_json::from_value(json!({
            "session_id":"demo", "agent":"claude", "title":"Summary title", "title_kind":"",
            "created_at":"1", "last_used":"1", "ended_at":null, "policy":"default",
            "auto_allow":false, "memory_scope":"", "summary":"Saved summary",
            "summary_kind":"person", "ghost":false, "running":true, "pins":[],
            "settings":{}, "follows":null, "compacted":"0"
        }))
        .unwrap();
        let _ = app.details_event(Event::Loaded(token, Ok((session, vec![]))));
        let _ = app.details_event(Event::Cancel);
        let page = app.details.as_ref().unwrap();
        assert!(!page.editing);
        assert_eq!(page.editor.text(), "Saved summary");
        assert!(!page.rendered.is_empty(), "the saved Markdown remains visible after Cancel");
    }

    #[test]
    fn note_input_and_pending_add_survive_navigation_until_completion() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Note("remember this".into()));
        // The write has left; navigate away before its reply arrives.
        app.details.as_mut().unwrap().pending = true;
        app.details.as_mut().unwrap().mutation = Some(Mutation { token: "write".into(), summary: false, note: true });
        let _ = app.details_event(Event::Back);
        let _ = app.details_event(Event::Open("demo".into()));
        let page = app.details.as_ref().unwrap();
        assert_eq!(page.note, "remember this");
        assert!(page.pending, "a reopened page cannot submit the note again");
        let _ = app.details_event(Event::Note("overwrite".into()));
        assert_eq!(app.details.as_ref().unwrap().note, "remember this");
        let _ = app.details_event(Event::Done("write".into(), "demo".into(), Ok(())));
        let page = app.details.as_ref().unwrap();
        assert!(!page.pending);
        assert!(page.note.is_empty());
    }

    #[test]
    fn hidden_summary_completion_clears_a_saved_draft_but_failure_keeps_it() {
        for result in [Ok(()), Err("refused".into())] {
            let success = result.is_ok();
            let (mut app, _) = App::new(true, None);
            let _ = app.details_event(Event::Open("demo".into()));
            let _ = app.details_event(Event::Edit);
            let _ = app.details_event(Event::Input(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("my summary".into())))));
            app.details.as_mut().unwrap().pending = true;
            app.details.as_mut().unwrap().mutation = Some(Mutation { token: "write".into(), summary: true, note: false });
            let _ = app.details_event(Event::Back);
            let _ = app.details_event(Event::Done("write".into(), "demo".into(), result));
            let _ = app.details_event(Event::Open("demo".into()));
            let page = app.details.as_ref().unwrap();
            assert!(!page.pending);
            assert_eq!(page.editing, !success);
            if !success {
                assert_eq!(page.editor.text(), "my summary");
                assert_eq!(page.error.as_deref(), Some("refused"));
            }
        }
    }

    #[test]
    fn disconnected_save_keeps_draft_editable_instead_of_sticking_pending() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let _ = app.details_event(Event::Save);
        let page = app.details.as_ref().unwrap();
        assert!(!page.pending);
        assert!(page.editing);
        assert!(page.error.as_ref().unwrap().contains("No daemon"));
    }

    #[test]
    fn load_before_write_cannot_replace_the_refresh_after_completion() {
        let (mut app, _) = App::new(true, None);
        app.actions = Some(silli_client::actions::Actions::new(crate::tests::connector(vec![])));
        let _ = app.details_event(Event::Open("demo".into()));
        let old_token = app.details.as_ref().unwrap().token.clone();
        let _ = app.details_load("demo".into());
        assert_eq!(app.details.as_ref().unwrap().token, old_token, "only one background read per page");
        app.details.as_mut().unwrap().pending = true;
        app.details.as_mut().unwrap().mutation = Some(Mutation { token: "write".into(), summary: false, note: true });
        let _ = app.details_event(Event::Done("write".into(), "demo".into(), Ok(())));
        assert_eq!(app.details.as_ref().unwrap().token, old_token);
        assert!(app.details.as_ref().unwrap().refresh_after_load);
        let _ = app.details_event(Event::Loaded(old_token.clone(), Err("stale failure".into())));
        assert_ne!(app.details.as_ref().unwrap().token, old_token);
        assert!(app.details.as_ref().unwrap().error.is_none());
        assert!(app.details.as_ref().unwrap().load_pending);
    }

    #[test]
    fn a_completion_from_an_earlier_page_cannot_clear_a_new_draft() {
        let (mut app, _) = App::new(true, None);
        let _ = app.details_event(Event::Open("demo".into()));
        let old_token = app.details.as_ref().unwrap().token.clone();
        let _ = app.details_event(Event::Open("demo".into()));
        let _ = app.details_event(Event::Edit);
        let _ = app.details_event(Event::Done(old_token, "demo".into(), Ok(())));
        assert!(app.details.as_ref().unwrap().editing);
    }
}
