//! A session's conversation as it crosses the daemon socket (docs/daemon.md §3.3): the wire types
//! are `sill_proto::wire` (re-exported here, so `sill_core::wire::Event` is the same type), and
//! this module is the bridge between them and the agent's side as sill keeps it ([`acp`]'s types).
//! The two are separate on purpose (docs/protocol.md §7.1): the daemon's private relay protocol
//! carries `acp`'s, the frontends get these, and the conversion happens at the daemon's boundary,
//! so a frontend protocol change never changes what the relay speaks. A frontend linking only
//! `sill-proto` never sees this module.

use std::collections::BTreeMap;


pub use sill_proto::wire::*;

use crate::acp;

// --- the agent's side out to the frontends -------------------------------------------------------

impl From<acp::PermissionOptionKind> for PermissionOptionKind {
    fn from(k: acp::PermissionOptionKind) -> Self {
        use acp::PermissionOptionKind as A;
        match k {
            A::AllowOnce => Self::AllowOnce,
            A::AllowAlways => Self::AllowAlways,
            A::RejectOnce => Self::RejectOnce,
            A::RejectAlways => Self::RejectAlways,
            A::Other(s) => Self::Other(s),
        }
    }
}

impl From<acp::PermissionOption> for PermissionOption {
    fn from(o: acp::PermissionOption) -> Self { Self { id: o.id, name: o.name, kind: o.kind.into() } }
}

impl From<acp::StopReason> for StopReason {
    fn from(r: acp::StopReason) -> Self {
        use acp::StopReason as A;
        match r {
            A::EndTurn => Self::EndTurn,
            A::MaxTokens => Self::MaxTokens,
            A::MaxTurnRequests => Self::MaxTurnRequests,
            A::Refusal => Self::Refusal,
            A::Cancelled => Self::Cancelled,
            A::Other(s) => Self::Other(s),
        }
    }
}

impl From<acp::ConfigKind> for ConfigKind {
    fn from(k: acp::ConfigKind) -> Self {
        match k {
            acp::ConfigKind::Select(current, values) => Self::Select(current, values),
            acp::ConfigKind::Boolean(b) => Self::Boolean(b),
        }
    }
}

impl From<acp::ConfigOption> for ConfigOption {
    fn from(o: acp::ConfigOption) -> Self { Self { id: o.id, name: o.name, category: o.category, kind: o.kind.into() } }
}

impl From<acp::Controls> for Controls {
    fn from(c: acp::Controls) -> Self { Self { modes: c.modes, options: c.options.into_iter().map(Into::into).collect() } }
}

impl From<acp::FieldKind> for FieldKind {
    fn from(k: acp::FieldKind) -> Self {
        use acp::FieldKind as A;
        match k {
            A::Choice(v) => Self::Choice(v),
            A::Choices(v) => Self::Choices(v),
            A::Text => Self::Text,
            A::Bool => Self::Bool,
            A::Number => Self::Number,
        }
    }
}

impl From<acp::FormField> for FormField {
    fn from(f: acp::FormField) -> Self { Self { key: f.key, title: f.title, description: f.description, kind: f.kind.into() } }
}

impl From<acp::AuthMethodInfo> for AuthMethodInfo {
    fn from(m: acp::AuthMethodInfo) -> Self { Self { id: m.id, name: m.name, description: m.description, terminal: m.terminal, api_key: m.api_key } }
}

// --- the frontends' side back to the agent's -----------------------------------------------------

impl From<PermissionOptionKind> for acp::PermissionOptionKind {
    fn from(k: PermissionOptionKind) -> Self {
        use PermissionOptionKind as W;
        match k {
            W::AllowOnce => Self::AllowOnce,
            W::AllowAlways => Self::AllowAlways,
            W::RejectOnce => Self::RejectOnce,
            W::RejectAlways => Self::RejectAlways,
            W::Other(s) => Self::Other(s),
        }
    }
}

impl From<PermissionOption> for acp::PermissionOption {
    fn from(o: PermissionOption) -> Self { Self { id: o.id, name: o.name, kind: o.kind.into() } }
}

impl From<StopReason> for acp::StopReason {
    fn from(r: StopReason) -> Self {
        use StopReason as W;
        match r {
            W::EndTurn => Self::EndTurn,
            W::MaxTokens => Self::MaxTokens,
            W::MaxTurnRequests => Self::MaxTurnRequests,
            W::Refusal => Self::Refusal,
            W::Cancelled => Self::Cancelled,
            W::Other(s) => Self::Other(s),
        }
    }
}

impl From<ConfigKind> for acp::ConfigKind {
    fn from(k: ConfigKind) -> Self {
        match k {
            ConfigKind::Select(current, values) => Self::Select(current, values),
            ConfigKind::Boolean(b) => Self::Boolean(b),
        }
    }
}

impl From<ConfigOption> for acp::ConfigOption {
    fn from(o: ConfigOption) -> Self { Self { id: o.id, name: o.name, category: o.category, kind: o.kind.into() } }
}

impl From<Controls> for acp::Controls {
    fn from(c: Controls) -> Self { Self { modes: c.modes, options: c.options.into_iter().map(Into::into).collect() } }
}

impl From<ConfigValue> for acp::ConfigValue {
    fn from(v: ConfigValue) -> Self {
        match v {
            ConfigValue::Id(id) => Self::Id(id),
            ConfigValue::Bool(b) => Self::Bool(b),
        }
    }
}

impl From<FieldKind> for acp::FieldKind {
    fn from(k: FieldKind) -> Self {
        use FieldKind as W;
        match k {
            W::Choice(v) => Self::Choice(v),
            W::Choices(v) => Self::Choices(v),
            W::Text => Self::Text,
            W::Bool => Self::Bool,
            W::Number => Self::Number,
        }
    }
}

impl From<FormField> for acp::FormField {
    fn from(f: FormField) -> Self { Self { key: f.key, title: f.title, description: f.description, kind: f.kind.into() } }
}

impl From<AuthMethodInfo> for acp::AuthMethodInfo {
    fn from(m: AuthMethodInfo) -> Self { Self { id: m.id, name: m.name, description: m.description, terminal: m.terminal, api_key: m.api_key } }
}

impl From<AttachmentKind> for acp::AttachmentKind {
    fn from(k: AttachmentKind) -> Self {
        match k {
            AttachmentKind::Image { data_base64 } => Self::Image { data_base64 },
            AttachmentKind::File { jail_path } => Self::File { jail_path },
        }
    }
}

impl From<Attachment> for acp::Attachment {
    fn from(a: Attachment) -> Self { Self { name: a.name, mime: a.mime, kind: a.kind.into() } }
}

/// The frontend side: back into the `acp::Event` a view renders. A question gets a fresh reply
/// channel whose answer `answer` sends to the daemon; `Resolved` has no `acp` form (None).
pub fn event_into_acp(ev: Event, answer: impl FnOnce(u64, futures::channel::oneshot::Receiver<Answer>) + Send + 'static) -> Option<acp::Event> {
    {
        use acp::Event as A;
        Some(match ev {
            Event::Ready { session_id, resumed, controls } => A::Ready { session_id, resumed, controls: controls.into() },
            Event::Capabilities { images } => A::Capabilities { images },
            Event::Controls(c) => A::Controls(c.into()),
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
                A::PermissionRequest { tool_call_id, title, options: options.into_iter().map(Into::into).collect(), reply: tx }
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
                A::Elicitation { message, fields: fields.into_iter().map(Into::into).collect(), reply: tx }
            }
            Event::Resolved { .. } | Event::SessionEnded { .. } | Event::Summarized { .. } | Event::Note(_) => return None,
            Event::Other(t) => A::Other(t),
            Event::Usage { used, size } => A::Usage { used, size },
            Event::AuthRequired { message, methods } => A::AuthRequired { message, methods: methods.into_iter().map(Into::into).collect() },
            Event::Stopped(r) => A::Stopped(r.into()),
            Event::Ended(e) => A::Ended(e),
            Event::UserMessage { text, attachments } => A::UserMessage { text, attachments: attachments.into_iter().map(|a| acp::UserAttachment { name: a.name, mime: a.mime, upload_id: a.upload_id, data_base64: a.data_base64 }).collect() },
            Event::Plan(entries) => A::Plan(entries.into_iter().map(|e| acp::PlanEntry { content: e.content, status: e.status, priority: e.priority }).collect()),
            Event::Commands(commands) => A::Commands(commands.into_iter().map(|c| acp::AgentCommand { name: c.name, description: c.description, hint: c.hint }).collect()),
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
    fn a_question_reaches_the_view_and_its_answer_the_daemon() {
        let option = PermissionOption { id: "allow".into(), name: "Allow".into(), kind: PermissionOptionKind::Other("_custom".into()) };
        let wire = Event::PermissionRequest { request: 7, tool_call_id: "t1".into(), title: "rm".into(), options: vec![option] };
        let wire: Event = serde_json::from_str(&serde_json::to_string(&wire).unwrap()).unwrap();
        let (got_tx, got_rx) = std::sync::mpsc::channel();
        let ev = event_into_acp(wire, move |request, answer| {
                std::thread::spawn(move || {
                    let a = futures::executor::block_on(answer).unwrap();
                    let _ = got_tx.send((request, a));
                });
            })
            .unwrap();
        let acp::Event::PermissionRequest { reply: view_reply, options, .. } = ev else { panic!("not a permission request") };
        // an option kind sill does not know stays itself through the conversion
        assert!(matches!(&options[0].kind, acp::PermissionOptionKind::Other(k) if k == "_custom"));
        view_reply.send(Some("allow".into())).unwrap();
        let (request, answer) = got_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(request, 7);
        assert!(matches!(answer, Answer::Permission(Some(a)) if a == "allow"));
    }
}
