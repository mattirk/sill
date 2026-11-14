//! Conversion at the main daemon's frontend boundary.
//!
//! The private relay owns its serialized types independently of the frontend contract. Keep
//! frontend adapters here so public protocol changes never reach the worker's transport.

use sill_core::wire;
use super::relay::{Answer, Command, Event};

/// A frontend's command, once the daemon took what is the daemon's (a prompt's transcript entry).
impl From<wire::Command> for Command {
    fn from(command: wire::Command) -> Self {
        match command {
            wire::Command::Prompt { text, attachments, entry: _ } => Command::Prompt { text, attachments: attachments.into_iter().map(Into::into).collect() },
            wire::Command::Authenticate(m) => Command::Authenticate(m),
            wire::Command::RetrySession => Command::RetrySession,
            wire::Command::SetMode(m) => Command::SetMode(m),
            wire::Command::SetConfig(o, v) => Command::SetConfig(o, v.into()),
            wire::Command::Cancel => Command::Cancel,
            wire::Command::Quit => Command::Quit,
        }
    }
}

impl From<wire::Answer> for Answer {
    fn from(answer: wire::Answer) -> Self {
        match answer {
            wire::Answer::Permission(choice) => Answer::Permission(choice),
            wire::Answer::Form(values) => Answer::Form(values),
        }
    }
}

/// What the frontends get of an event: the daemon's boundary (docs/protocol.md §7.1).
impl From<Event> for wire::Event {
    fn from(event: Event) -> Self {
        match event {
            Event::Ready { session_id, resumed, controls } => wire::Event::Ready { session_id, resumed, controls: controls.into() },
            Event::Capabilities { images } => wire::Event::Capabilities { images },
            Event::Controls(c) => wire::Event::Controls(c.into()),
            Event::TurnStarted => wire::Event::TurnStarted,
            Event::UserText(t) => wire::Event::UserText(t),
            Event::Text(t) => wire::Event::Text(t),
            Event::Thought(t) => wire::Event::Thought(t),
            Event::ToolCall { id, title, kind, status, detail, diff, parent } => wire::Event::ToolCall { id, title, kind, status, detail, diff, parent },
            Event::ToolCallUpdate { id, status, title, detail, diff } => wire::Event::ToolCallUpdate { id, status, title, detail, diff },
            Event::SubagentText { parent, text } => wire::Event::SubagentText { parent, text },
            Event::PermissionRequest { request, tool_call_id, title, options } => {
                wire::Event::PermissionRequest { request, tool_call_id, title, options: options.into_iter().map(Into::into).collect() }
            }
            Event::AutoAnswered { tool_call_id, title, allowed } => wire::Event::AutoAnswered { tool_call_id, title, allowed },
            Event::Elicitation { request, message, fields } => wire::Event::Elicitation { request, message, fields: fields.into_iter().map(Into::into).collect() },
            Event::Resolved { request } => wire::Event::Resolved { request },
            Event::Other(t) => wire::Event::Other(t),
            Event::Usage { used, size } => wire::Event::Usage { used, size },
            Event::AuthRequired { message, methods } => wire::Event::AuthRequired { message, methods: methods.into_iter().map(Into::into).collect() },
            Event::Stopped(r) => wire::Event::Stopped(r.into()),
            Event::Ended(e) => wire::Event::Ended(e),
        }
    }
}

