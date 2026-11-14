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
            // the internal path keeps no keys: the agent refuses the login and asks again
            wire::Command::AuthenticateWithKey { method_id, .. } => Command::Authenticate(method_id),
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
            Event::V2PermissionsCancelled | Event::V2Update { .. } | Event::PromptAccepted { .. } => wire::Event::Other("Native ACP v2 update; use the SFP conversation subscription".into()),
            Event::V2PromptFailed { message, .. } => wire::Event::Other(message),
            Event::V2PermissionRequest { request, tool_call_id, title, options, .. } => wire::Event::PermissionRequest { request, tool_call_id:tool_call_id.unwrap_or_default(), title, options:options.into_iter().map(Into::into).collect() },
            Event::Ready { session_id, resumed, controls } | Event::V2Ready { session_id, resumed, controls } => wire::Event::Ready { session_id, resumed, controls: controls.into() },
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
            Event::Plan(entries) => wire::Event::Plan(entries.into_iter().map(|e| wire::PlanEntry { content: e.content, status: e.status, priority: e.priority }).collect()),
            Event::Commands(commands) => wire::Event::Commands(commands.into_iter().map(|c| wire::AgentCommand { name: c.name, description: c.description, hint: c.hint }).collect()),
            Event::AgentTitle(title) => wire::Event::Other(format!("_sill_agent_title {title}")),
            Event::Notice { title, description, .. } => wire::Event::Note(match description { Some(d) if !d.is_empty() => format!("{title}\n{d}"), _ => title }),
        }
    }
}

