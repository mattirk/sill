//! A session's conversation as it crosses the daemon socket (docs/daemon.md §3.3): what the daemon,
//! the ACP client of every running session, tells the frontends attached to it, and what they ask
//! it to do. [`acp::Event`] without its reply channels — a permission request or a form carries a
//! request number instead, and the answer comes back as `_sill/sessions.answer` — and a command that
//! says what the transcript gets for a prompt, since the daemon is the one that writes it.
//!
//! Serialized as JSON inside the `_sill/session.event` notification and the `_sill/sessions.command`
//! request (`sill-proto` carries them as plain values). The types live here, next to `acp`, until
//! the protocol is frozen (M3) and they move to the protocol crate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::acp::{self, Attachment, AuthMethodInfo, ConfigValue, Controls, FormField, PermissionOption, StopReason};

/// What the transcript gets when a prompt is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Entry {
    /// The person's prompt as they typed it (without sill's prefix) and the attachments' names.
    User { text: String, names: Vec<String> },
    /// A turn sill asks for itself (the summary): a note says what it was.
    Note(String),
}

/// What a frontend asks of a running session (`acp::Command`, plus the transcript entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// `text` is what the agent gets (standing block and pin prefix included).
    Prompt { text: String, attachments: Vec<Attachment>, entry: Option<Entry> },
    Authenticate(String),
    RetrySession,
    SetMode(String),
    SetConfig(String, ConfigValue),
    Cancel,
    /// The frontend is done with the session (its tab closed): it detaches; the agent stays the
    /// daemon's until the session is stopped.
    Quit,
}

impl Command {
    /// The `acp::Command` the session loop takes, and the entry for the transcript.
    pub fn into_acp(self) -> (acp::Command, Option<Entry>) {
        match self {
            Command::Prompt { text, attachments, entry } => (acp::Command::Prompt(text, attachments), entry),
            Command::Authenticate(m) => (acp::Command::Authenticate(m), None),
            Command::RetrySession => (acp::Command::RetrySession, None),
            Command::SetMode(m) => (acp::Command::SetMode(m), None),
            Command::SetConfig(o, v) => (acp::Command::SetConfig(o, v), None),
            Command::Cancel => (acp::Command::Cancel, None),
            Command::Quit => (acp::Command::Quit, None),
        }
    }
}

/// `acp::Event` on the wire. `request` numbers a question the agent asked; it is answered with
/// [`Answer`] and, once any frontend answered, every attached one gets [`Event::Resolved`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Ready { session_id: String, resumed: bool, controls: Controls },
    Capabilities { images: bool },
    Controls(Controls),
    TurnStarted,
    UserText(String),
    Text(String),
    Thought(String),
    ToolCall { id: String, title: String, kind: String, status: String, detail: Option<String>, diff: Option<String>, parent: Option<String> },
    ToolCallUpdate { id: String, status: Option<String>, title: Option<String>, detail: Option<String>, diff: Option<String> },
    SubagentText { parent: String, text: String },
    PermissionRequest { request: u64, tool_call_id: String, title: String, options: Vec<PermissionOption> },
    AutoAnswered { tool_call_id: String, title: String, allowed: bool },
    Elicitation { request: u64, message: String, fields: Vec<FormField> },
    /// A question was answered (by this frontend or another): drop its dialog.
    Resolved { request: u64 },
    Other(String),
    Usage { used: u64, size: u64 },
    AuthRequired { message: String, methods: Vec<AuthMethodInfo> },
    Stopped(StopReason),
    Ended(Option<String>),
    /// The daemon ended the session (`sessions.end`): its row is written — or deleted, when
    /// nothing was asked in it — and its jail is stopping. Frontends drop the session.
    SessionEnded { deleted: bool, reason: String },
    /// A line for the transcript's reader that sill wrote itself (the summary turn's mark).
    Note(String),
    /// A checkpoint summary (`sessions.checkpoint`) is in: the text as stored (None when the agent
    /// wrote nothing), and the name it gave the session if the first line was one.
    Summarized { text: Option<String>, title: Option<String> },
}

/// One change to a session's row (`_sill/sessions.set`): what the shelf wrote to the store itself
/// before the daemon was its only writer. `kind` as in `Session::title_kind` / `summary_kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSet {
    Title { title: String, kind: String },
    AutoAllow(bool),
    Desktop(String),
    MemoryScope(String),
    Policy(String),
    Summary { text: String, kind: String },
    /// The session's own agent settings (mode, model, options — SPEC §3.2).
    Settings(crate::store::AgentSettings),
    /// An ended session comes back: `ended_at` cleared.
    Reopen,
}

/// The answer to a question: the chosen permission option (None = cancel), or the form's values
/// (None = cancel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Answer {
    Permission(Option<String>),
    Form(Option<BTreeMap<String, serde_json::Value>>),
}

/// Where an agent's question waits for its answer.
pub enum Reply {
    Permission(futures::channel::oneshot::Sender<Option<String>>),
    Form(futures::channel::oneshot::Sender<Option<BTreeMap<String, serde_json::Value>>>),
}

impl Reply {
    /// Deliver `answer`; false when it is of the other kind (the reply is dropped: the agent sees a
    /// cancel).
    pub fn send(self, answer: Answer) -> bool {
        match (self, answer) {
            (Reply::Permission(tx), Answer::Permission(a)) => {
                let _ = tx.send(a);
                true
            }
            (Reply::Form(tx), Answer::Form(a)) => {
                let _ = tx.send(a);
                true
            }
            _ => false,
        }
    }
}

impl Event {
    /// The daemon side: an event of the session loop as it goes out; a question gets `request`
    /// and its reply channel comes back to be kept until it is answered.
    pub fn from_acp(ev: acp::Event, request: u64) -> (Event, Option<Reply>) {
        use acp::Event as A;
        let e = match ev {
            A::Ready { session_id, resumed, controls } => Event::Ready { session_id, resumed, controls },
            A::Capabilities { images } => Event::Capabilities { images },
            A::Controls(c) => Event::Controls(c),
            A::TurnStarted => Event::TurnStarted,
            A::UserText(t) => Event::UserText(t),
            A::Text(t) => Event::Text(t),
            A::Thought(t) => Event::Thought(t),
            A::ToolCall { id, title, kind, status, detail, diff, parent } => Event::ToolCall { id, title, kind, status, detail, diff, parent },
            A::ToolCallUpdate { id, status, title, detail, diff } => Event::ToolCallUpdate { id, status, title, detail, diff },
            A::SubagentText { parent, text } => Event::SubagentText { parent, text },
            A::PermissionRequest { tool_call_id, title, options, reply } => {
                return (Event::PermissionRequest { request, tool_call_id, title, options }, Some(Reply::Permission(reply)));
            }
            A::AutoAnswered { tool_call_id, title, allowed } => Event::AutoAnswered { tool_call_id, title, allowed },
            A::Elicitation { message, fields, reply } => return (Event::Elicitation { request, message, fields }, Some(Reply::Form(reply))),
            A::Other(t) => Event::Other(t),
            A::Usage { used, size } => Event::Usage { used, size },
            A::AuthRequired { message, methods } => Event::AuthRequired { message, methods },
            A::Stopped(r) => Event::Stopped(r),
            A::Ended(e) => Event::Ended(e),
        };
        (e, None)
    }

    /// The frontend side: back into the `acp::Event` a view renders. A question gets a fresh reply
    /// channel whose answer `answer` sends to the daemon; `Resolved` has no `acp` form (None).
    pub fn into_acp(self, answer: impl FnOnce(u64, futures::channel::oneshot::Receiver<Answer>) + Send + 'static) -> Option<acp::Event> {
        use acp::Event as A;
        Some(match self {
            Event::Ready { session_id, resumed, controls } => A::Ready { session_id, resumed, controls },
            Event::Capabilities { images } => A::Capabilities { images },
            Event::Controls(c) => A::Controls(c),
            Event::TurnStarted => A::TurnStarted,
            Event::UserText(t) => A::UserText(t),
            Event::Text(t) => A::Text(t),
            Event::Thought(t) => A::Thought(t),
            Event::ToolCall { id, title, kind, status, detail, diff, parent } => A::ToolCall { id, title, kind, status, detail, diff, parent },
            Event::ToolCallUpdate { id, status, title, detail, diff } => A::ToolCallUpdate { id, status, title, detail, diff },
            Event::SubagentText { parent, text } => A::SubagentText { parent, text },
            Event::PermissionRequest { request, tool_call_id, title, options } => {
                let (tx, rx) = futures::channel::oneshot::channel::<Option<String>>();
                let (atx, arx) = futures::channel::oneshot::channel::<Answer>();
                std::thread::spawn(move || {
                    let a = futures::executor::block_on(rx).unwrap_or(None);
                    let _ = atx.send(Answer::Permission(a));
                });
                answer(request, arx);
                A::PermissionRequest { tool_call_id, title, options, reply: tx }
            }
            Event::AutoAnswered { tool_call_id, title, allowed } => A::AutoAnswered { tool_call_id, title, allowed },
            Event::Elicitation { request, message, fields } => {
                let (tx, rx) = futures::channel::oneshot::channel::<Option<BTreeMap<String, serde_json::Value>>>();
                let (atx, arx) = futures::channel::oneshot::channel::<Answer>();
                std::thread::spawn(move || {
                    let a = futures::executor::block_on(rx).unwrap_or(None);
                    let _ = atx.send(Answer::Form(a));
                });
                answer(request, arx);
                A::Elicitation { message, fields, reply: tx }
            }
            Event::Resolved { .. } | Event::SessionEnded { .. } | Event::Summarized { .. } | Event::Note(_) => return None,
            Event::Other(t) => A::Other(t),
            Event::Usage { used, size } => A::Usage { used, size },
            Event::AuthRequired { message, methods } => A::AuthRequired { message, methods },
            Event::Stopped(r) => A::Stopped(r),
            Event::Ended(e) => A::Ended(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_and_commands_round_trip() {
        let c = Command::Prompt {
            text: "hi".into(),
            attachments: vec![],
            entry: Some(Entry::User { text: "hi".into(), names: vec!["a.png".into()] }),
        };
        let back: Command = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert!(matches!(back, Command::Prompt { entry: Some(Entry::User { .. }), .. }));
        let set = Command::SetConfig("model".into(), ConfigValue::Id("fable".into()));
        let back: Command = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
        assert!(matches!(back, Command::SetConfig(o, ConfigValue::Id(v)) if o == "model" && v == "fable"));
        for e in [Event::TurnStarted, Event::Text("x".into()), Event::Stopped(StopReason::EndTurn), Event::Ended(None), Event::Resolved { request: 3 }] {
            let s = serde_json::to_string(&e).unwrap();
            let _: Event = serde_json::from_str(&s).unwrap();
        }
    }

    #[test]
    fn a_question_round_trips_with_its_answer() {
        let (tx, rx) = futures::channel::oneshot::channel::<Option<String>>();
        let ev = acp::Event::PermissionRequest { tool_call_id: "t1".into(), title: "rm".into(), options: vec![], reply: tx };
        let (wire, reply) = Event::from_acp(ev, 7);
        let wire: Event = serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
        // the frontend's side: its view answers through the reply channel it got
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        let ev = wire
            .into_acp(move |request, answer| {
                std::thread::spawn(move || {
                    let a = futures::executor::block_on(answer).unwrap();
                    let _ = got_tx.send((request, a));
                });
            })
            .unwrap();
        let acp::Event::PermissionRequest { reply: view_reply, .. } = ev else { panic!("not a permission request") };
        view_reply.send(Some("allow".into())).unwrap();
        let (request, answer) = got_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(request, 7);
        // the daemon's side: the answer reaches the agent's channel
        assert!(reply.unwrap().send(answer));
        assert_eq!(futures::executor::block_on(rx).unwrap(), Some("allow".to_string()));
    }
}
