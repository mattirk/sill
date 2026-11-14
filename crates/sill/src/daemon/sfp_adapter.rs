//! ACP v1 events projected into durable, frontend-independent conversation records.
//!
//! This state is saved with the runner cursor. Replaying a relay event must be deduplicated
//! before calling `project`, and the result committed with this state before acknowledging it.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use sill_core::acp;
use sill_proto::sfp;
use super::relay;
#[path="sfp_adapter_v2.rs"]
mod v2;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AdapterState {
    generation: String,
    v2: v2::State,
    turn: Option<sfp::Turn>,
    text_item: Option<String>,
    thought_item: Option<String>,
    text_bytes: usize,
    thought_bytes: usize,
    tools: BTreeMap<String, (String, sfp::Tool)>,
    subagent_items: BTreeMap<String, String>,
    questions: BTreeMap<u64, String>,
    replaying: bool,
    /// The turn's plan item: each ACP plan replaces the last, in place.
    plan_item: Option<String>,
    /// Update kinds the ACP loop dumped instead of rendering, logged once each.
    unhandled: std::collections::BTreeSet<String>,
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}{}{}{}", sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id())
}

impl AdapterState {
    pub fn question_id(&self, request: u64) -> Option<&str> { self.questions.get(&request).map(String::as_str) }
    pub fn request_for_question(&self, question: &str) -> Option<u64> {
        self.questions.iter().find_map(|(request, public)| (public == question).then_some(*request))
    }
    pub fn turn_id(&self) -> Option<&str> { self.turn.as_ref().map(|t| t.turn_id.as_str()) }

    /// `existing_question` is the durable current record for `Resolved`, preserving the winner.
    /// The journal stamps new items' creation sequence when it appends each event.
    pub fn project(&mut self, session: &sfp::SessionState, event: &relay::Event, existing_question: Option<&sfp::Question>) -> Vec<sfp::Event> {
        if self.generation != session.agent_generation_id {
            *self = Self { generation: session.agent_generation_id.clone(), ..Default::default() };
        }
        let mut out = Vec::new();
        let mut next = session.clone();
        match event {
            relay::Event::V2Ready { resumed, controls, .. } => {
                self.v2.enabled = true;
                self.replaying = *resumed;
                next.controls = Some(session_controls(controls));
                next.authentication = None;
                next.activity = if *resumed { "unknown" } else { "idle" }.into();
                next.lifecycle = "open".into();
                next.capabilities.insert("steering".into(), false);
            }
            relay::Event::V2Update { update } => self.project_v2(&mut next, update, &mut out),
            relay::Event::V2PermissionsCancelled => {}, // Runner reconciles durable winning answers.

            relay::Event::PromptAccepted { message_id } => { self.v2.accepted_message = Some(message_id.clone()); }
            relay::Event::V2PromptFailed { code, message } => {
                if let Some(mut turn) = self.turn.take() {
                    turn.activity = "unknown".into(); turn.outcome = "unknown".into();
                    turn.reason = Some(sfp::Reason { code: code.clone(), message: message.clone() });
                    out.push(sfp::Event::TurnUpdated { turn });
                }
                next.turn_id = None; next.activity = "unknown".into();
            }
            relay::Event::V2PermissionRequest { request, tool_call_id, title, subject, options } => {
                self.v2_permission(&mut next, *request, tool_call_id.as_deref(), title, subject.as_ref(), options, &mut out);
            }
            relay::Event::Ready { resumed, controls, .. } => {
                next.controls = Some(session_controls(controls));
                next.authentication = None;
                self.replaying = *resumed;
                next.activity = "idle".into();
                next.lifecycle = "open".into();
                next.capabilities.insert("steering".into(), false);
            }
            relay::Event::Capabilities { images } => { next.capabilities.insert("images".into(), *images); }
            relay::Event::TurnStarted => {
                self.replaying = false;
                self.v2.active_seen = false;
                self.v2.accepted_message = None;
                next.authentication = None;
                self.text_item = None;
                self.thought_item = None;
                self.plan_item = None;
                self.subagent_items.clear();
                let turn = sfp::Turn { turn_id: id("turn"), agent_generation_id: self.generation.clone(), activity: if self.is_v2() { "unknown" } else { "working" }.into(), outcome: "pending".into(), reason: None };
                next.turn_id = Some(turn.turn_id.clone());
                next.activity = turn.activity.clone();
                self.turn = Some(turn.clone());
                if self.is_v2() { self.v2.last_turn = Some(turn.turn_id.clone()); }
                out.push(sfp::Event::TurnUpdated { turn });
            }
            relay::Event::UserText(_) => {
                // ACP v1 echoes/replays user text without identities. The submit path creates
                // user items; never guess an identity by comparing echoed text.
            }
            relay::Event::Text(_) | relay::Event::Thought(_) | relay::Event::ToolCall { .. }
                | relay::Event::ToolCallUpdate { .. } | relay::Event::SubagentText { .. } if self.replaying => {
                // ACP v1 session/load has no stable message identities. Existing stored items
                // remain authoritative; replay cannot be merged by text or source call IDs.
            }
            relay::Event::Text(text) => self.append_text("agent", text, &mut out),
            relay::Event::Thought(text) => self.append_text("thought", text, &mut out),
            relay::Event::ToolCall { id: source, title, kind, status, detail, diff, parent } => {
                self.text_item = None;
                self.thought_item = None;
                let tool = sfp::Tool { title: title.clone(), kind: kind.clone(), status: status.clone(), detail: detail.clone().unwrap_or_default(), diff: diff.clone().unwrap_or_default(), parent_item_id: parent.as_ref().and_then(|p| self.tools.get(p)).map(|(id, _)| id.clone()) };
                if let Some((item_id, current)) = self.tools.get_mut(source) {
                    *current = tool.clone();
                    out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { tool: Some(tool), ..Default::default() } });
                } else {
                    let item_id = id("item");
                    self.tools.insert(source.clone(), (item_id.clone(), tool.clone()));
                    out.push(sfp::Event::ItemCreated { item: self.item(item_id, "tool", vec![], Some(tool)) });
                }
            }
            relay::Event::ToolCallUpdate { id: source, status, title, detail, diff } => {
                if let Some((item_id, tool)) = self.tools.get_mut(source) {
                    if let Some(v) = status { tool.status = v.clone(); }
                    if let Some(v) = title { tool.title = v.clone(); }
                    if let Some(v) = detail { tool.detail = v.clone(); }
                    if let Some(v) = diff { tool.diff = v.clone(); }
                    out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { tool: Some(tool.clone()), ..Default::default() } });
                } else {
                    // Some agents first mention a call through an update. Establish an item
                    // before any patch and preserve only fields the agent actually supplied.
                    let tool = sfp::Tool { title: title.clone().unwrap_or_default(), kind: "unknown".into(), status: status.clone().unwrap_or_else(|| "unknown".into()), detail: detail.clone().unwrap_or_default(), diff: diff.clone().unwrap_or_default(), parent_item_id: None };
                    let item_id = id("item");
                    self.tools.insert(source.clone(), (item_id.clone(), tool.clone()));
                    out.push(sfp::Event::ItemCreated { item: self.item(item_id, "tool", vec![], Some(tool)) });
                }
            }
            relay::Event::SubagentText { parent, text } => {
                if let Some(item_id) = self.subagent_items.get(parent) {
                    out.push(sfp::Event::ItemTextAppended { item_id: item_id.clone(), block_id: "text".into(), text: text.clone() });
                } else {
                    let item_id = id("item");
                    self.subagent_items.insert(parent.clone(), item_id.clone());
                    let mut content = sfp::Content::text(text);
                    if let Some((parent_id, _)) = self.tools.get(parent) { content.extra.insert("parent_item_id".into(), serde_json::json!(parent_id)); }
                    out.push(sfp::Event::ItemCreated { item: self.item(item_id, "agent", vec![content], None) });
                }
            }
            relay::Event::PermissionRequest { request, tool_call_id, title, options } => {
                if !self.questions.contains_key(request) {
                    let question_id = id("question");
                    self.questions.insert(*request, question_id.clone());
                    let question = sfp::Question { question_id: question_id.clone(), agent_generation_id: self.generation.clone(), kind: "permission".into(), title: title.clone(), description: None, subject: None,
                        tool_item_id: self.tools.get(tool_call_id).map(|(id, _)| id.clone()), options: options.iter().map(|o| sfp::PermissionOption { option_id: o.id.clone(), title: o.name.clone(), kind: permission_kind(&o.kind) }).collect(), fields: vec![],
                        state: "open".into(), answer_id: None, answer: None, delivery: None, reason: None };
                    out.push(sfp::Event::QuestionUpdated { question });
                    next.question_ids.push(question_id);
                    next.activity = "waiting".into();
                }
            }
            relay::Event::Elicitation { request, message, fields } => {
                if !self.questions.contains_key(request) {
                    let question_id = id("question");
                    self.questions.insert(*request, question_id.clone());
                    let question = sfp::Question { question_id: question_id.clone(), agent_generation_id: self.generation.clone(), kind: "form".into(), title: message.clone(), description: None, subject: None, tool_item_id: None, options: vec![], fields: fields.iter().map(form_field).collect(), state: "open".into(), answer_id: None, answer: None, delivery: None, reason: None };
                    out.push(sfp::Event::QuestionUpdated { question });
                    next.question_ids.push(question_id);
                    next.activity = "waiting".into();
                }
            }
            relay::Event::Resolved { request } => {
                if let Some(question_id) = self.questions.get(request) {
                    if let Some(current) = existing_question.filter(|q| &q.question_id == question_id) {
                        let mut question = current.clone();
                        if question.state == "answered" { question.delivery = Some("confirmed".into()); }
                        else if question.state == "open" {
                            question.state = "cancelled".into();
                            question.reason = Some(sfp::Reason { code: "resolved_without_recorded_answer".into(), message: "The agent question was resolved outside the recorded answer path".into() });
                        }
                        out.push(sfp::Event::QuestionUpdated { question });
                    }
                    next.question_ids.retain(|id| id != question_id);
                    if next.question_ids.is_empty() { next.activity = if self.is_v2() { "unknown" } else if self.turn.is_some() { "working" } else { "idle" }.into(); }
                }
            }
            relay::Event::Stopped(reason) => {
                self.text_item = None;
                self.thought_item = None;
                self.subagent_items.clear();
                if let Some(mut turn) = self.turn.take() {
                    turn.activity = "idle".into();
                    turn.outcome = match reason {
                        acp::StopReason::EndTurn => "completed", acp::StopReason::Cancelled => "cancelled",
                        acp::StopReason::Refusal => "failed", _ => "unknown",
                    }.into();
                    turn.reason = Some(sfp::Reason { code: stop_reason(reason), message: "ACP v1 prompt returned this stop reason".into() });
                    out.push(sfp::Event::TurnUpdated { turn });
                }
                next.turn_id = None;
                next.activity = if next.question_ids.is_empty() { "idle" } else { "waiting" }.into();
            }
            relay::Event::Ended(error) => {
                if let Some(mut turn) = self.turn.take() {
                    turn.activity = "unknown".into();
                    turn.outcome = "unknown".into();
                    turn.reason = Some(sfp::Reason { code: "agent_lost".into(), message: error.clone().unwrap_or_else(|| "Agent connection ended".into()) });
                    out.push(sfp::Event::TurnUpdated { turn });
                }
                next.activity = "unknown".into();
                next.turn_id = None;
                // Root reconciles all open/answered questions against their durable records.
            }
            // Private markers and the ACP loop's debug dump of updates it does not handle
            // are diagnostics, not conversation.
            relay::Event::Other(text) if acp::is_diagnostic(text) => {
                let kind: String = text.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                if !text.starts_with("_sill") && self.unhandled.insert(kind.clone()) {
                    eprintln!("sill daemon: {}: ACP update {kind} is not rendered", session.session_id);
                }
            }
            relay::Event::Plan(entries) if !self.replaying => {
                let content: Vec<sfp::Content> = entries.iter().enumerate().map(|(i, e)| sfp::Content {
                    block_id: format!("entry-{i}"), kind: "plan_entry".into(), text: Some(e.content.clone()),
                    extra: BTreeMap::from([("status".into(), serde_json::json!(e.status)), ("priority".into(), serde_json::json!(e.priority))]),
                }).collect();
                match &self.plan_item {
                    Some(item_id) => out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { content: Some(content), ..Default::default() } }),
                    None => {
                        let item_id = id("item");
                        self.plan_item = Some(item_id.clone());
                        out.push(sfp::Event::ItemCreated { item: self.item(item_id, "plan", content, None) });
                    }
                }
            }
            relay::Event::Plan(_) => {}
            relay::Event::Notice { .. } if self.replaying => {}
            relay::Event::Notice { severity, title, description } => {
                let mut content = sfp::Content::text(match description { Some(d) if !d.is_empty() => format!("{title}\n{d}"), _ => title.clone() });
                content.extra.insert("severity".into(), serde_json::json!(severity));
                out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "notice", vec![content], None) });
            }
            relay::Event::Commands(commands) => {
                next.commands = commands.iter().map(|c| sfp::AgentCommand { name: c.name.clone(), description: c.description.clone(), hint: c.hint.clone() }).collect();
            }
            relay::Event::AgentTitle(title) => { next.agent_title = Some(title.clone()); }
            relay::Event::Other(text) => out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "note", vec![sfp::Content::text(text)], None) }),
            relay::Event::AutoAnswered { title, allowed, .. } => out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "note", vec![sfp::Content::text(format!("Policy {}: {title}", if *allowed { "allowed" } else { "denied" }))], None) }),
            relay::Event::AuthRequired { message, methods } => {
                next.activity = "waiting".into();
                next.authentication = Some(sfp::Authentication { message: message.clone(), methods: methods.iter().map(|method| sfp::AuthMethod {
                    method_id: method.id.clone(), title: method.name.clone(), description: method.description.clone(),
                    terminal: method.terminal.as_ref().map(|(args, env)| sfp::AuthTerminal { args: args.clone(), env: env.clone() }),
                    api_key: method.api_key.clone(),
                }).collect() });
            }
            relay::Event::Controls(controls) => { next.controls = Some(session_controls(controls)); }
            relay::Event::Usage { used, size } => { next.usage = Some(sfp::Usage { used: used.to_string(), size: size.to_string() }); }
        }
        if &next != session { out.push(sfp::Event::SessionUpdated { session: next }); }
        out
    }

    fn item(&self, item_id: String, kind: &str, content: Vec<sfp::Content>, tool: Option<sfp::Tool>) -> sfp::Item {
        sfp::Item { item_id, created_seq: String::new(), turn_id: self.turn.as_ref().map(|t| t.turn_id.clone()), submission_id: None, agent_generation_id: self.generation.clone(), kind: kind.into(), content, tool }
    }

    fn append_text(&mut self, kind: &str, mut text: &str, out: &mut Vec<sfp::Event>) {
        // Bound the complete record, not just individual deltas. At worst JSON escaping
        // expands text sixfold, so 256 KiB leaves room within a 4 MiB snapshot page.
        const ITEM_TEXT_BYTES: usize = 256 * 1024;
        while !text.is_empty() {
            let (target, bytes) = if kind == "thought" {
                (&mut self.thought_item, &mut self.thought_bytes)
            } else {
                (&mut self.text_item, &mut self.text_bytes)
            };
            if target.is_none() { *bytes = 0; }
            if *bytes >= ITEM_TEXT_BYTES { *target = None; *bytes = 0; }
            let mut end = text.len().min(ITEM_TEXT_BYTES - *bytes);
            while !text.is_char_boundary(end) { end -= 1; }
            if end == 0 {
                // The next UTF-8 character does not fit; start a new item before it.
                *target = None;
                *bytes = 0;
                continue;
            }
            let chunk = &text[..end];
            *bytes += end;
            if let Some(item_id) = target {
                out.push(sfp::Event::ItemTextAppended { item_id: item_id.clone(), block_id: "text".into(), text: chunk.into() });
            } else {
                let item_id = id("item");
                *target = Some(item_id.clone());
                let mut content = sfp::Content::text(chunk);
                if kind == "thought" { content.kind = "thought".into(); }
                out.push(sfp::Event::ItemCreated { item: self.item(item_id, kind, vec![content], None) });
            }
            text = &text[end..];
        }
    }

}

fn permission_kind(kind: &acp::PermissionOptionKind) -> String {
    match kind {
        acp::PermissionOptionKind::AllowOnce => "allow_once", acp::PermissionOptionKind::AllowAlways => "allow_always",
        acp::PermissionOptionKind::RejectOnce => "reject_once", acp::PermissionOptionKind::RejectAlways => "reject_always",
        acp::PermissionOptionKind::Other(value) => value,
    }.into()
}
fn stop_reason(reason: &acp::StopReason) -> String {
    match reason {
        acp::StopReason::EndTurn => "end_turn", acp::StopReason::MaxTokens => "max_tokens", acp::StopReason::MaxTurnRequests => "max_turn_requests",
        acp::StopReason::Refusal => "refusal", acp::StopReason::Cancelled => "cancelled", acp::StopReason::Other(value) => value,
    }.into()
}
fn form_field(field: &acp::FormField) -> sfp::FormField {
    let (kind, choices) = match &field.kind {
        acp::FieldKind::Choice(choices) => ("choice", choices.clone()), acp::FieldKind::Choices(choices) => ("choices", choices.clone()),
        acp::FieldKind::Text => ("text", vec![]), acp::FieldKind::Bool => ("boolean", vec![]), acp::FieldKind::Number => ("number", vec![]),
    };
    sfp::FormField { key: field.key.clone(), title: field.title.clone(), description: field.description.clone(), kind: kind.into(), choices: choices.into_iter().map(|(value, title, description)| sfp::Choice { value, title, description }).collect() }
}

pub(crate) fn session_controls(controls: &acp::Controls) -> sfp::SessionControls {
    let choice = |(value, title): &(String, String)| sfp::ControlChoice { value: value.clone(), title: title.clone() };
    sfp::SessionControls {
        current_mode: controls.modes.as_ref().map(|(current, _)| current.clone()),
        modes: controls.modes.as_ref().map(|(_, modes)| modes.iter().map(choice).collect()).unwrap_or_default(),
        options: controls.options.iter().map(|option| {
            let (value, choices) = match &option.kind {
                acp::ConfigKind::Select(value, choices) => (sfp::ControlValue::Id { value: value.clone() }, choices.iter().map(choice).collect()),
                acp::ConfigKind::Boolean(value) => (sfp::ControlValue::Boolean { value: *value }, vec![]),
            };
            sfp::ControlOption { option_id: option.id.clone(), title: option.name.clone(), category: option.category.clone(), value, choices }
        }).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> sfp::SessionState {
        sfp::SessionState { session_id: "s".into(), agent_generation_id: "generation-one".into(), lifecycle: "open".into(), title:None, summary:None, end_reason:None, deleted:false, activity: "idle".into(), turn_id: None, question_ids: vec![], background: None, capabilities: BTreeMap::new(), controls: None, authentication: None, usage: None, agent_title: None, commands: vec![], active_ms:0, active_since_ms:None }
    }
    #[test]
    fn restart_retains_item_and_question_identity_but_replacement_does_not() {
        let session = session();
        let mut adapter = AdapterState::default();
        adapter.project(&session, &relay::Event::TurnStarted, None);
        let first = adapter.project(&session, &relay::Event::Text("hello".into()), None);
        let sfp::Event::ItemCreated { item } = &first[0] else { panic!("expected item") };
        let question = relay::Event::PermissionRequest { request: 7, tool_call_id: "tool".into(), title: "Run".into(), options: vec![] };
        adapter.project(&session, &question, None);
        let question_id = adapter.question_id(7).unwrap().to_owned();
        let mut resumed: AdapterState = serde_json::from_str(&serde_json::to_string(&adapter).unwrap()).unwrap();
        let appended = resumed.project(&session, &relay::Event::Text(" world".into()), None);
        assert!(matches!(&appended[0], sfp::Event::ItemTextAppended { item_id, text, .. } if item_id == &item.item_id && text == " world"));
        assert!(resumed.project(&session, &question, None).is_empty(), "an ACKed pending question replay cannot reopen or duplicate it");
        assert_eq!(resumed.request_for_question(&question_id), Some(7));
        let mut replacement = session.clone();
        replacement.agent_generation_id = "generation-two".into();
        resumed.project(&replacement, &question, None);
        assert_ne!(resumed.question_id(7).unwrap(), question_id);
        assert_eq!(resumed.request_for_question(&question_id), None);
    }
    #[test]
    fn permission_resolution_preserves_the_durable_answer_and_confirms_delivery() {
        let mut adapter = AdapterState::default();
        let session = session();
        let question = relay::Event::PermissionRequest { request: 1, tool_call_id: String::new(), title: "Unknown operation".into(), options: vec![acp::PermissionOption { id: "option".into(), name: "Custom".into(), kind: acp::PermissionOptionKind::Other("_future".into()) }] };
        let events = adapter.project(&session, &question, None);
        let sfp::Event::QuestionUpdated { mut question } = events[0].clone() else { panic!("expected question") };
        assert_eq!(question.options[0].kind, "_future");
        assert!(question.tool_item_id.is_none());
        question.state = "answered".into();
        question.answer_id = Some("answer-1".into());
        question.answer = Some(sfp::Answer::Cancel);
        question.delivery = Some("pending".into());
        let events = adapter.project(&session, &relay::Event::Resolved { request: 1 }, Some(&question));
        let sfp::Event::QuestionUpdated { question: resolved } = &events[0] else { panic!("expected resolution") };
        assert_eq!(resolved.answer_id, question.answer_id);
        assert_eq!(resolved.answer, question.answer);
        assert_eq!(resolved.delivery.as_deref(), Some("confirmed"));
    }
    #[test]
    fn growing_text_and_single_large_chunks_remain_snapshot_sized() {
        let mut adapter = AdapterState::default();
        let session = session();
        let mut events = adapter.project(&session, &relay::Event::Text("prefix".into()), None);
        // Escaped controls exercise JSON's worst expansion; a multibyte suffix checks UTF-8
        // boundaries when an already open item has fewer bytes remaining than one character.
        let huge = format!("{}{}", "\u{0001}".repeat(256 * 1024 - 7), "é".repeat(256 * 1024));
        events.extend(adapter.project(&session, &relay::Event::Text(huge.clone()), None));
        let mut items = BTreeMap::<String, sfp::Item>::new();
        let mut order = Vec::new();
        for event in events {
            match event {
                sfp::Event::ItemCreated { item } => { order.push(item.item_id.clone()); items.insert(item.item_id.clone(), item); }
                sfp::Event::ItemTextAppended { item_id, text, .. } => { items.get_mut(&item_id).unwrap().content[0].text.as_mut().unwrap().push_str(&text); }
                _ => {}
            }
        }
        assert!(items.len() >= 3);
        for item in items.values() {
            assert!(item.content[0].text.as_ref().unwrap().len() <= 256 * 1024);
            assert!(serde_json::to_vec(&sfp::Record::Item { item:item.clone() }).unwrap().len() < 4 * 1024 * 1024);
        }
        let restored = order.iter().map(|id|items[id].content[0].text.as_deref().unwrap()).collect::<String>();
        assert_eq!(restored, format!("prefix{huge}"));
        let thoughts = adapter.project(&session, &relay::Event::Thought("é".repeat(256 * 1024)), None);
        assert_eq!(thoughts.iter().filter(|e|matches!(e,sfp::Event::ItemCreated { .. })).count(),2);
    }

    #[test]
    fn a_turns_plan_is_one_item_revised_in_place_and_commands_belong_to_the_session() {
        let mut adapter = AdapterState::default();
        let session = session();
        adapter.project(&session, &relay::Event::TurnStarted, None);
        let step = |content: &str, status: &str| acp::PlanEntry { content: content.into(), status: status.into(), priority: "medium".into() };
        let first = adapter.project(&session, &relay::Event::Plan(vec![step("read", "in_progress"), step("fix", "pending")]), None);
        let sfp::Event::ItemCreated { item } = &first[0] else { panic!("expected a plan item: {first:?}") };
        assert_eq!(item.kind, "plan");
        assert_eq!(item.content[1].text.as_deref(), Some("fix"));
        assert_eq!(item.content[0].extra["status"], "in_progress");
        let revised = adapter.project(&session, &relay::Event::Plan(vec![step("read", "completed"), step("fix", "in_progress")]), None);
        assert!(matches!(&revised[0], sfp::Event::ItemPatched { item_id, set } if item_id == &item.item_id && set.content.as_ref().unwrap()[0].extra["status"] == "completed"));
        adapter.project(&session, &relay::Event::Stopped(acp::StopReason::EndTurn), None);
        adapter.project(&session, &relay::Event::TurnStarted, None);
        assert!(matches!(adapter.project(&session, &relay::Event::Plan(vec![step("next", "pending")]), None)[0], sfp::Event::ItemCreated { .. }), "a new turn has its own plan");

        let commands = adapter.project(&session, &relay::Event::Commands(vec![acp::AgentCommand { name: "review".into(), description: "Review".into(), hint: None }]), None);
        assert!(matches!(&commands[..], [sfp::Event::SessionUpdated { session }] if session.commands[0].name == "review"));
        let titled = adapter.project(&session, &relay::Event::AgentTitle("Fix the shelf".into()), None);
        assert!(matches!(&titled[..], [sfp::Event::SessionUpdated { session: s }] if s.agent_title.as_deref() == Some("Fix the shelf") && s.title == session.title));
        let notice = adapter.project(&session, &relay::Event::Notice { severity: "warning".into(), title: "Rate limited".into(), description: Some("retrying".into()) }, None);
        assert!(matches!(&notice[0], sfp::Event::ItemCreated { item } if item.kind == "notice" && item.content[0].text.as_deref() == Some("Rate limited\nretrying") && item.content[0].extra["severity"] == "warning"));
    }

    #[test]
    fn replay_without_durable_identities_never_duplicates_stored_text() {
        let mut adapter = AdapterState::default();
        let session = session();
        adapter.project(&session, &relay::Event::Ready { session_id: "acp".into(), resumed: true, controls: acp::Controls::default() }, None);
        assert!(adapter.project(&session, &relay::Event::Text("stored history".into()), None).is_empty());
        adapter.project(&session, &relay::Event::TurnStarted, None);
        assert!(matches!(adapter.project(&session, &relay::Event::Text("new text".into()), None)[0], sfp::Event::ItemCreated { .. }));
    }
}
