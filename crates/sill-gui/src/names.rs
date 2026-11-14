//! Inline session-name editing; the summarizer's title is a separate value.
use crate::{App, Message, blocking};
use iced::{Task, widget::operation};
use serde_json::json;

#[derive(Debug, Clone)]
pub enum Event {
    Begin(String),
    Input(String),
    Save,
    Done(String, String, String, Result<(), String>),
}
pub struct Editor {
    pub id: String,
    pub token: String,
    pub text: String,
    pub pending: bool,
    pub error: Option<String>,
}
impl App {
    pub(crate) fn session_name<'a>(&'a self, id: &str, fallback: &'a str) -> &'a str {
        self.all_sessions
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.name.as_deref())
            .unwrap_or(fallback)
    }
    pub(crate) fn name_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Begin(id) => {
                if self.name_editor.as_ref().is_some_and(|e| e.pending) {
                    return Task::none();
                }
                let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
                    return Task::none();
                };
                let text = self.session_name(&id, &tab.title).to_owned();
                self.name_editor = Some(Editor {
                    id,
                    text,
                    token: sill_client::request_id("name"),
                    pending: false,
                    error: None,
                });
                operation::focus("session-name").chain(operation::select_all("session-name"))
            }
            Event::Input(text) => {
                if let Some(e) = self.name_editor.as_mut().filter(|e| !e.pending) {
                    e.text = text;
                    e.error = None;
                }
                Task::none()
            }
            Event::Save => {
                let Some(e) = self.name_editor.as_mut().filter(|e| !e.pending) else {
                    return Task::none();
                };
                let text = e.text.trim().to_owned();
                if text.is_empty()
                    || text.chars().count() > 80
                    || text.chars().any(char::is_control)
                {
                    e.error = Some("Use 1–80 characters without control characters".into());
                    return Task::none();
                }
                let Some(actions) = self.actions.clone() else {
                    return Task::none();
                };
                e.pending = true;
                let id = e.id.clone();
                let token = e.token.clone();
                blocking(
                    move || {
                        let outcome = actions.call(
                            "_sill/sessions.update",
                            json!({"session_id":id,"change":{"type":"name","text":text}}),
                        );
                        let result = if outcome.is_done() {
                            Ok(())
                        } else {
                            Err(outcome
                                .problem()
                                .unwrap_or(
                                    "Outcome unknown; check the session name before retrying",
                                )
                                .to_owned())
                        };
                        (token, id, text, result)
                    },
                    |(token, id, text, r)| Message::Name(Event::Done(token, id, text, r)),
                )
            }
            Event::Done(token, id, text, result) => {
                // A successful write stays relevant even if its editor was closed meanwhile.
                if result.is_ok() {
                    if let Some(s) = self.all_sessions.iter_mut().find(|s| s.session_id == id) {
                        s.name = Some(text);
                    }
                }
                if self.name_editor.as_ref().is_some_and(|e| e.token == token) {
                    match result {
                        Ok(()) => self.name_editor = None,
                        Err(error) => {
                            let e = self.name_editor.as_mut().unwrap();
                            e.pending = false;
                            e.error = Some(error);
                        }
                    }
                }
                let list = self.list();
                if self.details.as_ref().is_some_and(|p| p.id == id) {
                    Task::batch([list, self.details_load(id)])
                } else {
                    list
                }
            }
        }
    }
}
