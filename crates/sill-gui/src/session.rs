//! What the person does in one session's own view: the prompt, the conversation's rows and
//! questions, the outbox, the agent's controls and the shell. Each event carries its session's
//! id from the view that made it: the selection can change between a press and its handling,
//! and the press must not land on another session.
use std::collections::BTreeSet;

use iced::Task;
use iced::widget::{operation, text_editor};
use serde_json::json;
use sill_client::actions::Outcome;
use sill_client::lane;
use sill_proto::sfp::{Answer, AnswerQuestion, CancelTurn, ConfigValue, Control, SessionControl, SubmissionKey};

use crate::{Act, App, Message, Pick, blocking, demo, editor, forms, keys, lists, media, terminal};

#[derive(Debug, Clone)]
pub enum Event {
    /// The conversation scrolled: how far it is from its end, its offset, and the height in view.
    Scrolled(f32, f32, f32),
    Latest,
    Prompt(text_editor::Action),
    PromptHistory(bool),
    /// Shift+Enter in the prompt: a line break that continues a list.
    PromptNewline,
    /// Tab (false) or Shift+Tab in the prompt: indentation in a list, focus elsewhere.
    PromptIndent(bool),
    Send,
    Stop,
    Answer {
        question_id: String,
        option_id: String,
    },
    Unqueue(String),
    /// A queued prompt goes now: the agent's turn stops and it is sent first.
    SendNow(String),
    ToggleTool(String),
    /// A group of tool calls in a run (`tool_runs`), by its first call's item.
    ToggleGroup(String),
    ToggleAnswered(String),
    SetMode(Pick),
    SetOption(String, Pick),
    /// A prompt that did not arrive: back into the composer, sent again, checked, or dropped.
    EditSend(String),
    RetrySend(String),
    CheckSend(String),
    DiscardSend(String),
    /// A form question: a field edited, the form sent, or declined.
    FormEdit(String, forms::Edit),
    SubmitForm(String),
    DeclineForm(String),
    /// A held prompt: send it after all, or drop it.
    ResumeHeld(String),
    DiscardHeld(String),
    /// `>_`: the session's jail shell in place of the prompt, or the prompt again.
    ToggleTerminal,
    ToggleHostTerminal,
    TermAction(terminal::Action),
    /// End the shell itself (hiding the pane only detaches from it).
    EndTerminal,
    ReopenTerminal,
}

/// Scrolling down to within this of the conversation's end takes up its tail again.
const NEAR_END: f32 = 24.0;

/// Whether the conversation follows its tail after a scroll from `before` to `offset`, which
/// leaves it `from_end` short of its end. Distances are in pixels, whatever the conversation's
/// length: any move up from the end lets go of the tail, a move down to near the end takes it up
/// again, and anything else (new text below, a resize) changes nothing.
pub fn follows(following: bool, before: f32, offset: f32, from_end: f32) -> bool {
    if from_end <= 1.0 {
        true
    } else if offset < before - 0.5 {
        false
    } else if offset > before + 0.5 && from_end <= NEAR_END {
        true
    } else {
        following
    }
}

/// `event` as the message `session`'s view sends.
pub fn message(session: &str, event: Event) -> Message {
    Message::Session(session.to_string(), event)
}

impl App {
    /// An event of the session `id`, whichever session is selected by now; nothing when its
    /// view has gone.
    pub fn session_event(&mut self, id: String, event: Event) -> Task<Message> {
        match event {
            Event::Scrolled(from_end, offset, viewport) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.follow_tail = follows(view.follow_tail, view.scroll_offset, offset, from_end);
                    view.scroll_offset = offset;
                    view.viewport = viewport;
                }
            }
            Event::Latest => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.follow_tail = true;
                    return operation::snap_to_end("conversation");
                }
            }
            Event::Prompt(action) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view.annotation.is_none()
                {
                    view.draft.perform(action);
                }
            }
            Event::PromptHistory(redo) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view.annotation.is_none()
                {
                    view.draft.history(redo);
                }
            }
            Event::PromptNewline => {
                if let Some(view) = self.views.get_mut(&id).filter(|view| view.annotation.is_none()) {
                    view.draft.edit(lists::newline);
                }
            }
            Event::PromptIndent(outdent) => {
                let indented = self.views.get_mut(&id).is_some_and(|view| view.annotation.is_none() && view.draft.edit(|draft| lists::indent(draft, outdent)));
                if !indented {
                    return keys::step(!outdent);
                }
            }
            Event::Send => return self.send(&id),
            Event::Stop => return self.stop(&id),
            Event::Answer { question_id, option_id } => return self.answer(&id, question_id, option_id),
            Event::Unqueue(submission_id) => {
                let Some(view) = self.views.get(&id) else {
                    return Task::none();
                };
                let params = SubmissionKey { session_id: view.id.clone(), submission_id };
                return self.act(&view.id, Act::Unqueue, "_sill/submissions.cancel", json!(params));
            }
            Event::SendNow(submission_id) => {
                let (Some(view), Some(actions)) = (self.views.get(&id), self.actions.clone()) else {
                    return Task::none();
                };
                let session = view.id.clone();
                let params = json!(SubmissionKey { session_id: session.clone(), submission_id });
                return blocking(
                    move || actions.call_needing("sill.send_now", "_sill/submissions.send_now", params),
                    move |outcome| Message::Done(session, Act::SendNow, outcome),
                );
            }
            Event::ToggleTool(item) => {
                if let Some(view) = self.views.get_mut(&id) {
                    toggle(&mut view.open_tools, item);
                }
            }
            Event::ToggleGroup(item) => {
                if let Some(view) = self.views.get_mut(&id) {
                    toggle(&mut view.open_groups, item);
                }
            }
            Event::ToggleAnswered(question) => {
                if let Some(view) = self.views.get_mut(&id) {
                    toggle(&mut view.open_answers, question);
                }
            }
            Event::SetMode(pick) => {
                return self.control(&id, Control::SetMode { mode_id: pick.value });
            }
            Event::SetOption(option_id, pick) => {
                let value = match pick.boolean {
                    Some(value) => ConfigValue::Bool { value },
                    None => ConfigValue::Id { value: pick.value },
                };
                return self.control(&id, Control::SetConfig { option_id, value });
            }
            Event::EditSend(submission) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view.annotation.is_none()
                    && let Some(pending) = view.outbox.take(&submission)
                {
                    let mut text = view.draft.text().trim_end().to_string();
                    if !text.trim().is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(pending.text());
                    let restored =
                        view.pending_attachments.remove(&submission).unwrap_or_else(|| pending.submit.attachments.into_iter().map(Into::into).collect());
                    view.attachments.extend(restored);
                    view.draft = editor::Editor::with_text(&text);
                    return operation::focus("prompt");
                }
            }
            Event::RetrySend(submission) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let Some(submit) = view.outbox.retry(&submission) else {
                    return Task::none();
                };
                return self.act(&id, Act::Send(submission), "_sill/sessions.submit", json!(submit));
            }
            Event::CheckSend(submission) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let Some(key) = view.outbox.check(&submission) else {
                    return Task::none();
                };
                return self.act(&id, Act::Check(submission), "_sill/submissions.get", json!(key));
            }
            Event::DiscardSend(submission) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.outbox.take(&submission);
                }
            }
            Event::FormEdit(question_id, edit) => {
                if let Some(view) = self.views.get_mut(&id)
                    && let Some(question) = view.conversation.questions.get(&question_id)
                {
                    let form = view.forms.entry(question_id).or_default();
                    forms::apply(form, question, edit);
                }
            }
            Event::SubmitForm(question_id) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let Some(question) = view.conversation.questions.get(&question_id) else {
                    return Task::none();
                };
                let form = view.forms.get(&question_id).cloned().unwrap_or_default();
                match forms::values(&form, question) {
                    Ok(values) => {
                        return self.answer_with(&id, question_id, Answer::Form { values });
                    }
                    Err(problem) => view.failed = Some(format!("The form cannot be sent: {problem}")),
                }
            }
            Event::DeclineForm(question_id) => {
                return self.answer_with(&id, question_id, Answer::Cancel);
            }
            Event::ResumeHeld(_) | Event::DiscardHeld(_) if self.demo => {}
            Event::ResumeHeld(submission_id) => {
                let Some(view) = self.views.get(&id) else {
                    return Task::none();
                };
                let key = SubmissionKey { session_id: view.id.clone(), submission_id };
                return self.act(&id, Act::Resume, "_sill/submissions.resume", json!(key));
            }
            Event::DiscardHeld(submission_id) => {
                let Some(view) = self.views.get(&id) else {
                    return Task::none();
                };
                let key = SubmissionKey { session_id: view.id.clone(), submission_id };
                return self.act(&id, Act::Unqueue, "_sill/submissions.cancel", json!(key));
            }
            Event::ToggleTerminal | Event::ToggleHostTerminal => {
                if self.views.get(&id).is_some_and(|v| v.annotation.is_some()) {
                    return Task::none();
                }
                return self.toggle_shell(&id, matches!(event, Event::ToggleHostTerminal));
            }
            Event::ReopenTerminal => return self.open_terminal(&id),
            Event::EndTerminal => {
                if let Some(pane) = self.views.get(&id).and_then(|v| v.terminal.as_ref()) {
                    pane.close();
                }
            }
            Event::TermAction(action) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let generation = view.terminal_generation;
                let Some(pane) = view.terminal.as_mut() else {
                    return Task::none();
                };
                match action {
                    terminal::Action::Copy => {
                        if let Some(selected) = pane.selection() {
                            return iced::clipboard::write(selected);
                        }
                    }
                    terminal::Action::Paste => {
                        return iced::clipboard::read().map(move |text| Message::TermPaste(id.clone(), generation, text));
                    }
                    terminal::Action::PastePrimary => {
                        return iced::clipboard::read_primary().map(move |text| Message::TermPaste(id.clone(), generation, text));
                    }
                    terminal::Action::Open(url) => terminal::open_link(url),
                    action => pane.perform(action),
                }
            }
        }
        Task::none()
    }

    /// The prompt goes to the agent (the daemon queues it while a turn runs). It leaves the
    /// composer, but stays in the outbox until the daemon has it.
    fn send(&mut self, session: &str) -> Task<Message> {
        let demo = self.demo;
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        let text = view.draft.text().trim_end().to_string();
        if view.annotation.is_some() || view.attachments_busy {
            return Task::none();
        }
        if text.trim().is_empty() && view.attachments.is_empty() {
            let next = view
                .conversation
                .submissions
                .values()
                .filter(|s| s.delivery == "queued" && s.insertion == "pending" && s.outcome == "pending" && !s.held)
                .min_by_key(|s| s.accepted_seq.parse::<u64>().unwrap_or(u64::MAX))
                .map(|s| s.submission_id.clone());
            return match next {
                Some(id) => self.update(message(session, Event::SendNow(id))),
                None => Task::none(),
            };
        }
        view.draft = editor::Editor::new();
        view.follow_tail = true;
        view.failed = None;
        let logged_in = view.conversation.session.as_ref().is_some_and(|s| s.authentication.is_none());
        view.login.settle(logged_in);
        if demo {
            for event in demo::prompt(view.conversation.seq + 1, &text) {
                view.accept(event);
            }
            return operation::snap_to_end("conversation").chain(self.focus_prompt());
        }
        let attachments = std::mem::take(&mut view.attachments);
        self.submit(session, text, attachments)
    }

    /// `text` goes to `session`'s agent as a prompt of the person's: the composer's, or one a
    /// button writes for them (the shelf's Resolve). The draft is not touched.
    pub(crate) fn submit(&mut self, session: &str, text: String, attachments: Vec<media::DraftAttachment>) -> Task<Message> {
        self.submit_for(session, text, attachments, None)
    }

    /// [`App::submit`] with the prompt's `purpose` (`Submit::purpose`): one a button wrote for
    /// the person whose turn the conversation folds (Apply's commit).
    pub(crate) fn submit_for(&mut self, session: &str, text: String, attachments: Vec<media::DraftAttachment>, purpose: Option<String>) -> Task<Message> {
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        view.follow_tail = true;
        view.failed = None;
        let submit = view.outbox.send_for(session, text, attachments.iter().map(|a| a.attachment.clone()).collect(), purpose);
        view.pending_attachments.insert(submit.submission_id.clone(), attachments);
        let reopen = view.lane.is_none();
        if reopen {
            self.open_lane(session, lane::Mode::Open);
        }
        let id = submit.submission_id.clone();
        Task::batch([self.act(session, Act::Send(id), "_sill/sessions.submit", json!(submit)), operation::snap_to_end("conversation"), self.focus_prompt()])
    }

    /// Stop the turn. Prompts still queued are removed first: stopping first
    /// could let the turn's end dispatch the next one.
    fn stop(&mut self, session: &str) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let (Some(view), Some(actions)) = (self.views.get(session), self.actions.clone()) else {
            return Task::none();
        };
        let Some(turn_id) = view.conversation.session.as_ref().and_then(|s| s.turn_id.clone()) else {
            return Task::none();
        };
        let queued = view.conversation.queued_submission_ids();
        let session = view.id.clone();
        let session_id = session.clone();
        blocking(
            move || {
                let mut kept = vec![];
                for submission_id in queued {
                    let key = SubmissionKey { session_id: session_id.clone(), submission_id };
                    if let Some(problem) = actions.call("_sill/submissions.cancel", json!(key)).problem() {
                        kept.push(problem.to_string());
                    }
                }
                let cancel = CancelTurn { session_id, turn_id };
                let outcome = actions.call("_sill/sessions.cancel", json!(cancel));
                if kept.is_empty() || !outcome.is_done() {
                    outcome
                } else {
                    Outcome::Refused { code: None, message: format!("stop requested, but some queued prompts could not be removed: {}", kept.join("; ")) }
                }
            },
            move |outcome| Message::Done(session, Act::Stop, outcome),
        )
    }

    fn answer(&mut self, session: &str, question_id: String, option_id: String) -> Task<Message> {
        let demo = self.demo;
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        view.follow_tail = view.follow_tail || view.conversation_shown();
        if demo {
            for event in demo::answered(view.conversation.seq + 1, &option_id) {
                view.accept(event);
            }
            return operation::snap_to_end("conversation");
        }
        let _ = view;
        self.answer_with(session, question_id, Answer::Permission { option_id })
    }

    /// One answer to a question of `session`; a second press while it is on its way sends
    /// nothing.
    fn answer_with(&mut self, session: &str, question_id: String, answer: Answer) -> Task<Message> {
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        if !view.answering.insert(question_id.clone()) {
            return Task::none();
        }
        view.failed = None;
        let params = AnswerQuestion { session_id: view.id.clone(), question_id: question_id.clone(), answer_id: sill_client::request_id("answer"), answer };
        let session = view.id.clone();
        self.act(&session, Act::Answer(question_id), "_sill/questions.answer", json!(params))
    }

    fn control(&mut self, session: &str, control: Control) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        view.failed = None;
        let params = SessionControl { session_id: view.id.clone(), control };
        let session = view.id.clone();
        self.act(&session, Act::Control, "_sill/sessions.control", json!(params))
    }

    fn toggle_shell(&mut self, session: &str, host: bool) -> Task<Message> {
        if let Some(view) = self.views.get_mut(session) {
            view.select_shell(host);
        }
        self.toggle_terminal(session)
    }

    fn toggle_terminal(&mut self, session: &str) -> Task<Message> {
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        if view.terminal_shown {
            view.terminal_shown = false;
            return operation::focus("prompt");
        }
        view.terminal_shown = true;
        let reusable = view.terminal.as_ref().is_some_and(|t| matches!(t.status, terminal::Status::Opening | terminal::Status::Running));
        if let Some(pane) = view.terminal.as_mut().filter(|_| reusable) {
            pane.focused = true;
            return Task::none();
        }
        self.open_terminal(session)
    }

    /// A pane on the session's jail shell (the pretend one in `--demo`); its events come tagged
    /// so a replaced pane's late ones are dropped. The replaced pane detaches as it goes.
    pub(crate) fn open_terminal(&mut self, session: &str) -> Task<Message> {
        let connect = self.connect.clone();
        let demo = self.demo;
        self.next_terminal += 1;
        let generation = self.next_terminal;
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        view.terminal_generation = generation;
        let (cols, rows) = view.terminal.as_ref().map_or((80, 16), |t| (t.cols, t.rows));
        // the old pane goes first, so its stream is closed before another attaches
        view.terminal = None;
        let (pane, events) = match (demo, connect) {
            (false, Some(connect)) => terminal::Pane::open(cols, rows, {
                let id = view.id.clone();
                let host = view.terminal_host;
                let cwd = view
                    .files
                    .roots
                    .iter()
                    .find(|r| r.metadata.as_ref().is_some_and(|m| m.directory))
                    .map(|r| r.path.clone())
                    .or_else(|| view.files.home.clone());
                move |cols, rows| {
                    if host {
                        sill_client::terminal::host_shell(connect, id, cwd)(cols, rows)
                    } else {
                        sill_client::terminal::jail_shell(connect, id)(cols, rows)
                    }
                }
            }),
            _ => terminal::Pane::open(cols, rows, terminal::demo_shell),
        };
        view.terminal = Some(pane);
        let session = view.id.clone();
        let stream = iced::futures::stream::unfold(events, |events| async move { events.recv().await.ok().map(|event| (event, events)) });
        Task::run(stream, move |event| Message::Terminal(session.clone(), generation, event))
    }
}

fn toggle(set: &mut BTreeSet<String>, id: String) {
    if !set.remove(&id) {
        set.insert(id);
    }
}
