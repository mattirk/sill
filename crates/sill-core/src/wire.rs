//! A session's conversation as it crosses the daemon socket (docs/daemon.md §3.3): the wire types
//! are `sill_proto::wire` (re-exported here, so `sill_core::wire::Event` is the same type), and
//! this module is the bridge to [`acp::Event`] — a permission request or a form carries a reply
//! channel there and a request number here, so the conversions each way live where the ACP
//! client does, and a frontend linking only `sill-proto` never sees them.

use std::collections::BTreeMap;


pub use sill_proto::wire::*;

use crate::acp;

/// The `acp::Command` the session loop takes, and the entry for the transcript.
pub fn command_into_acp(cmd: Command) -> (acp::Command, Option<Entry>) {
    {
        match cmd {
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

/// The daemon side: an event of the session loop as it goes out; a question gets `request` and
/// its reply channel comes back to be kept until it is answered.
pub fn event_from_acp(ev: acp::Event, request: u64) -> (Event, Option<Reply>) {
    {
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

}

/// The frontend side: back into the `acp::Event` a view renders. A question gets a fresh reply
/// channel whose answer `answer` sends to the daemon; `Resolved` has no `acp` form (None).
pub fn event_into_acp(ev: Event, answer: impl FnOnce(u64, futures::channel::oneshot::Receiver<Answer>) + Send + 'static) -> Option<acp::Event> {
    {
        use acp::Event as A;
        Some(match ev {
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
                    // the view dropped its dialog unanswered (tab closed, question resolved by
                    // another frontend): no answer — the daemon keeps the question for the next attach
                    let Ok(a) = futures::executor::block_on(rx) else { return };
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
                    let Ok(a) = futures::executor::block_on(rx) else { return };
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
        let (wire, reply) = event_from_acp(ev, 7);
        let wire: Event = serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
        // the frontend's side: its view answers through the reply channel it got
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        let ev = event_into_acp(wire, move |request, answer| {
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
