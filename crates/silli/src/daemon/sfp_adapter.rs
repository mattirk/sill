//! ACP v1 events projected into durable, frontend-independent conversation records.
//!
//! This state is saved with the runner cursor. Replaying a relay event must be deduplicated
//! before calling `project`, and the result committed with this state before acknowledging it.
use super::relay;
use serde::{Deserialize, Serialize};
use silli_core::acp;
use silli_proto::sfp;
use std::collections::BTreeMap;
#[path = "sfp_adapter_v2.rs"]
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
    /// The purpose of the turn the next prompt starts (`sfp::Turn::purpose`), set when the runner
    /// queues that prompt and taken by the turn it starts.
    next_purpose: Option<String>,
    /// `turn` is one the agent started itself (`relay::Event::AgentTurn`).
    agent_turn: bool,
    /// Update kinds the ACP loop dumped instead of rendering, logged once each.
    unhandled: std::collections::BTreeSet<String>,
    /// Image bytes the last `project` referenced; the runner keeps them in the same transaction.
    #[serde(skip)]
    media: Vec<AgentMedia>,
}

/// Agent image bytes named by content, waiting to become a session upload.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentMedia {
    pub upload_id: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// An agent image as an SFP `image` block: `mime` always, then `upload_id` (bytes fetched with
/// `_silli/uploads.get`) or `omitted` with the reason it was not kept. Never the bytes themselves.
pub(crate) fn image_block(mime: &str, data_base64: &str, media: &mut Vec<AgentMedia>) -> sfp::Content {
    let mut extra = BTreeMap::new();
    let valid_mime = !mime.is_empty() && mime.len() <= 256 && !mime.chars().any(char::is_control);
    extra.insert("mime".into(), serde_json::json!(if valid_mime { mime } else { "application/octet-stream" }));
    let omitted = if !valid_mime {
        Some("invalid")
    } else {
        match super::uploads::decode_limited(data_base64, super::uploads::AGENT_IMAGE_BYTES) {
            Ok(bytes) => {
                let upload_id = format!("{}{}", super::uploads::AGENT_PREFIX, silli_core::sfp::create_fingerprint(&format!("{mime}\n{data_base64}")));
                extra.insert("upload_id".into(), serde_json::json!(upload_id));
                if !media.iter().any(|m| m.upload_id == upload_id) {
                    media.push(AgentMedia { upload_id, mime: mime.into(), bytes });
                }
                None
            }
            Err(e) if e.downcast_ref::<silli_proto::rpc::Error>().is_some_and(|e| e.data.as_ref().is_some_and(|d| d["code"] == "limit_exceeded")) => {
                Some("too_large")
            }
            Err(_) => Some("invalid"),
        }
    };
    if let Some(reason) = omitted {
        extra.insert("omitted".into(), serde_json::json!(reason));
    }
    sfp::Content { block_id: id("block"), kind: "image".into(), text: None, extra }
}

/// Image blocks whose bytes the upload store refused (its agent budget is full) say so.
pub(crate) fn mark_omitted(events: &mut [sfp::Event], refused: &std::collections::BTreeSet<String>) {
    if refused.is_empty() {
        return;
    }
    let blocks = events
        .iter_mut()
        .flat_map(|event| match event {
            sfp::Event::ItemCreated { item } => Some(&mut item.content),
            sfp::Event::ItemPatched { set, .. } => set.content.as_mut(),
            _ => None,
        })
        .flatten();
    for block in blocks {
        if block.kind == "image" && block.extra.get("upload_id").and_then(|v| v.as_str()).is_some_and(|id| refused.contains(id)) {
            block.extra.remove("upload_id");
            block.extra.insert("omitted".into(), serde_json::json!("capacity"));
        }
    }
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}{}{}{}", silli_core::store::new_id(), silli_core::store::new_id(), silli_core::store::new_id(), silli_core::store::new_id())
}

impl AdapterState {
    pub fn question_id(&self, request: u64) -> Option<&str> {
        self.questions.get(&request).map(String::as_str)
    }
    pub fn request_for_question(&self, question: &str) -> Option<u64> {
        self.questions.iter().find_map(|(request, public)| (public == question).then_some(*request))
    }
    pub fn turn_id(&self) -> Option<&str> {
        self.turn.as_ref().map(|t| t.turn_id.as_str())
    }
    /// The open turn is one the agent started itself, which the runner is not busy with.
    pub fn agent_turn(&self) -> bool {
        self.agent_turn && self.turn.is_some()
    }
    pub fn take_media(&mut self) -> Vec<AgentMedia> {
        std::mem::take(&mut self.media)
    }
    /// The prompt being queued starts a turn for `purpose` (null: the person's own).
    pub fn prompt_purpose(&mut self, purpose: Option<String>) {
        self.next_purpose = purpose;
    }
    /// A new pending turn, carrying the queued prompt's purpose.
    fn new_turn(&mut self, activity: &str) -> sfp::Turn {
        sfp::Turn {
            turn_id: id("turn"),
            agent_generation_id: self.generation.clone(),
            activity: activity.into(),
            outcome: "pending".into(),
            reason: None,
            purpose: self.next_purpose.take(),
        }
    }

    /// `existing_question` is the durable current record for `Resolved`, preserving the winner.
    /// The journal stamps new items' creation sequence when it appends each event.
    pub fn project(&mut self, session: &sfp::SessionState, event: &relay::Event, existing_question: Option<&sfp::Question>) -> Vec<sfp::Event> {
        if self.generation != session.agent_generation_id {
            // a prompt queued before the agent changed is still the next one it is sent
            *self = Self { generation: session.agent_generation_id.clone(), next_purpose: self.next_purpose.take(), ..Default::default() };
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
            relay::Event::V2PermissionsCancelled => {} // Runner reconciles durable winning answers.

            relay::Event::PromptAccepted { message_id } => {
                self.v2.accepted_message = Some(message_id.clone());
            }
            relay::Event::V2PromptFailed { code, message } => {
                if let Some(mut turn) = self.turn.take() {
                    turn.activity = "unknown".into();
                    turn.outcome = "unknown".into();
                    turn.reason = Some(sfp::Reason { code: code.clone(), message: message.clone() });
                    out.push(sfp::Event::TurnUpdated { turn });
                }
                next.turn_id = None;
                next.activity = "unknown".into();
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
            relay::Event::Capabilities { images } => {
                next.capabilities.insert("images".into(), *images);
            }
            relay::Event::TurnStarted => {
                self.replaying = false;
                self.v2.active_seen = false;
                self.v2.accepted_message = None;
                next.authentication = None;
                self.text_item = None;
                self.thought_item = None;
                self.plan_item = None;
                self.subagent_items.clear();
                self.agent_turn = false;
                let turn = self.new_turn(if self.is_v2() { "unknown" } else { "working" });
                next.turn_id = Some(turn.turn_id.clone());
                next.activity = turn.activity.clone();
                self.turn = Some(turn.clone());
                if self.is_v2() {
                    self.v2.last_turn = Some(turn.turn_id.clone());
                }
                out.push(sfp::Event::TurnUpdated { turn });
            }
            // A turn the agent started itself, with no prompt open: working until it ends.
            relay::Event::AgentTurn(acp::AgentTurn::Started) => {
                if self.turn.is_none() {
                    self.replaying = false;
                    self.text_item = None;
                    self.thought_item = None;
                    self.plan_item = None;
                    self.subagent_items.clear();
                    // neither the person's nor silli's: a queued prompt keeps its purpose for its own turn
                    let queued = self.next_purpose.take();
                    let turn = self.new_turn("working");
                    self.next_purpose = queued;
                    next.turn_id = Some(turn.turn_id.clone());
                    next.activity = turn.activity.clone();
                    self.turn = Some(turn.clone());
                    self.agent_turn = true;
                    out.push(sfp::Event::TurnUpdated { turn });
                }
            }
            // Only the agent's own turn: a prompt's turn ends at its stop reason.
            relay::Event::AgentTurn(end) => {
                if std::mem::take(&mut self.agent_turn)
                    && let Some(mut turn) = self.turn.take()
                {
                    self.text_item = None;
                    self.thought_item = None;
                    self.subagent_items.clear();
                    let (outcome, code, message) = if *end == acp::AgentTurn::Cancelled {
                        ("cancelled", "cancelled", "The person stopped a turn the agent started itself")
                    } else {
                        ("completed", "agent_idle", "The agent became idle after a turn it started itself")
                    };
                    turn.activity = "idle".into();
                    turn.outcome = outcome.into();
                    turn.reason = Some(sfp::Reason { code: code.into(), message: message.into() });
                    out.push(sfp::Event::TurnUpdated { turn });
                    next.turn_id = None;
                    next.activity = if next.question_ids.is_empty() { "idle" } else { "waiting" }.into();
                }
            }
            relay::Event::UserText(_) => {
                // ACP v1 echoes/replays user text without identities. The submit path creates
                // user items; never guess an identity by comparing echoed text.
            }
            relay::Event::Text(_)
            | relay::Event::Thought(_)
            | relay::Event::ToolCall { .. }
            | relay::Event::ToolCallUpdate { .. }
            | relay::Event::SubagentText { .. }
            | relay::Event::Image { .. }
                if self.replaying =>
            {
                // ACP v1 session/load has no stable message identities. Existing stored items
                // remain authoritative; replay cannot be merged by text or source call IDs.
            }
            relay::Event::Text(text) => self.append_text("agent", text, &mut out),
            relay::Event::Thought(text) => self.append_text("thought", text, &mut out),
            relay::Event::Image { mime, data_base64, parent } => {
                // An image is its own item between text items: text after it starts a new one.
                self.text_item = None;
                self.thought_item = None;
                let mut block = image_block(mime, data_base64, &mut self.media);
                if let Some(parent) = parent {
                    self.subagent_items.remove(parent);
                    if let Some((parent_id, _)) = self.tools.get(parent) {
                        block.extra.insert("parent_item_id".into(), serde_json::json!(parent_id));
                    }
                }
                out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "agent", vec![block], None) });
            }
            relay::Event::ToolCall { id: source, title, kind, status, detail, diff, parent, command } => {
                self.text_item = None;
                self.thought_item = None;
                let tool = sfp::Tool {
                    title: title.clone(),
                    kind: kind.clone(),
                    status: status.clone(),
                    detail: detail.clone().unwrap_or_default(),
                    diff: diff.clone().unwrap_or_default(),
                    parent_item_id: parent.as_ref().and_then(|p| self.tools.get(p)).map(|(id, _)| id.clone()),
                    command: shell_command(kind, command.as_deref()),
                };
                if let Some((item_id, current)) = self.tools.get_mut(source) {
                    *current = tool.clone();
                    out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { tool: Some(tool), ..Default::default() } });
                } else {
                    let item_id = id("item");
                    self.tools.insert(source.clone(), (item_id.clone(), tool.clone()));
                    out.push(sfp::Event::ItemCreated { item: self.item(item_id, "tool", vec![], Some(tool)) });
                }
            }
            relay::Event::ToolCallUpdate { id: source, status, title, detail, diff, command } => {
                if let Some((item_id, tool)) = self.tools.get_mut(source) {
                    if let Some(v) = status {
                        tool.status = v.clone();
                    }
                    if let Some(v) = title {
                        tool.title = v.clone();
                    }
                    if let Some(v) = detail {
                        tool.detail = v.clone();
                    }
                    if let Some(v) = diff {
                        tool.diff = v.clone();
                    }
                    // an update that declares no command leaves the one already known
                    if let Some(v) = shell_command(&tool.kind, command.as_deref()) {
                        tool.command = Some(v);
                    }
                    out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { tool: Some(tool.clone()), ..Default::default() } });
                } else {
                    // Some agents first mention a call through an update. Establish an item
                    // before any patch and preserve only fields the agent actually supplied.
                    let tool = sfp::Tool {
                        title: title.clone().unwrap_or_default(),
                        kind: "unknown".into(),
                        status: status.clone().unwrap_or_else(|| "unknown".into()),
                        detail: detail.clone().unwrap_or_default(),
                        diff: diff.clone().unwrap_or_default(),
                        parent_item_id: None,
                        command: None,
                    };
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
                    if let Some((parent_id, _)) = self.tools.get(parent) {
                        content.extra.insert("parent_item_id".into(), serde_json::json!(parent_id));
                    }
                    out.push(sfp::Event::ItemCreated { item: self.item(item_id, "agent", vec![content], None) });
                }
            }
            relay::Event::PermissionRequest { request, tool_call_id, title, options } => {
                if !self.questions.contains_key(request) {
                    let question_id = id("question");
                    self.questions.insert(*request, question_id.clone());
                    let question = sfp::Question {
                        question_id: question_id.clone(),
                        agent_generation_id: self.generation.clone(),
                        kind: "permission".into(),
                        title: title.clone(),
                        description: None,
                        subject: None,
                        tool_item_id: self.tools.get(tool_call_id).map(|(id, _)| id.clone()),
                        options: options
                            .iter()
                            .map(|o| sfp::PermissionOption { option_id: o.id.clone(), title: o.name.clone(), kind: permission_kind(&o.kind) })
                            .collect(),
                        fields: vec![],
                        state: "open".into(),
                        answer_id: None,
                        answer: None,
                        delivery: None,
                        reason: None,
                    };
                    out.push(sfp::Event::QuestionUpdated { question });
                    next.question_ids.push(question_id);
                    next.activity = "waiting".into();
                }
            }
            relay::Event::Elicitation { request, message, fields } => {
                if !self.questions.contains_key(request) {
                    let question_id = id("question");
                    self.questions.insert(*request, question_id.clone());
                    let question = sfp::Question {
                        question_id: question_id.clone(),
                        agent_generation_id: self.generation.clone(),
                        kind: "form".into(),
                        title: message.clone(),
                        description: None,
                        subject: None,
                        tool_item_id: None,
                        options: vec![],
                        fields: fields.iter().map(form_field).collect(),
                        state: "open".into(),
                        answer_id: None,
                        answer: None,
                        delivery: None,
                        reason: None,
                    };
                    out.push(sfp::Event::QuestionUpdated { question });
                    next.question_ids.push(question_id);
                    next.activity = "waiting".into();
                }
            }
            relay::Event::Resolved { request } => {
                if let Some(question_id) = self.questions.get(request) {
                    if let Some(current) = existing_question.filter(|q| &q.question_id == question_id) {
                        let mut question = current.clone();
                        if question.state == "answered" {
                            question.delivery = Some("confirmed".into());
                        } else if question.state == "open" {
                            question.state = "cancelled".into();
                            question.reason = Some(sfp::Reason {
                                code: "resolved_without_recorded_answer".into(),
                                message: "The agent question was resolved outside the recorded answer path".into(),
                            });
                        }
                        out.push(sfp::Event::QuestionUpdated { question });
                    }
                    next.question_ids.retain(|id| id != question_id);
                    if next.question_ids.is_empty() {
                        next.activity = if self.is_v2() {
                            "unknown"
                        } else if self.turn.is_some() {
                            "working"
                        } else {
                            "idle"
                        }
                        .into();
                    }
                }
            }
            relay::Event::Stopped(reason) => {
                self.text_item = None;
                self.thought_item = None;
                self.subagent_items.clear();
                if let Some(mut turn) = self.turn.take() {
                    turn.activity = "idle".into();
                    turn.outcome = match reason {
                        acp::StopReason::EndTurn => "completed",
                        acp::StopReason::Cancelled => "cancelled",
                        acp::StopReason::Refusal => "failed",
                        _ => "unknown",
                    }
                    .into();
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
                // an agent that failed, at its start above all, says why in the conversation:
                // without it the session only seemed to hang (the person, 2026-11-13: Kimi Code
                // needed a newer Node and exited before it answered)
                if let Some(error) = error {
                    let mut said = format!("The agent stopped: {error}");
                    let tail = agent_stderr_tail(&session.session_id);
                    if !tail.is_empty() {
                        said.push('\n');
                        said.push_str(&tail);
                    }
                    let mut content = sfp::Content::text(said);
                    content.extra.insert("severity".into(), serde_json::json!("error"));
                    out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "notice", vec![content], None) });
                }
                // Root reconciles all open/answered questions against their durable records.
            }
            // Private markers and the ACP loop's debug dump of updates it does not handle
            // are diagnostics, not conversation.
            relay::Event::Other(text) if acp::is_diagnostic(text) => {
                let kind: String = text.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                if !text.starts_with("_silli") && self.unhandled.insert(kind.clone()) {
                    eprintln!("silli daemon: {}: ACP update {kind} is not rendered", session.session_id);
                }
            }
            relay::Event::Plan(entries) if !self.replaying => {
                let content: Vec<sfp::Content> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| sfp::Content {
                        block_id: format!("entry-{i}"),
                        kind: "plan_entry".into(),
                        text: Some(e.content.clone()),
                        extra: BTreeMap::from([("status".into(), serde_json::json!(e.status)), ("priority".into(), serde_json::json!(e.priority))]),
                    })
                    .collect();
                match &self.plan_item {
                    Some(item_id) => {
                        out.push(sfp::Event::ItemPatched { item_id: item_id.clone(), set: sfp::ItemPatch { content: Some(content), ..Default::default() } })
                    }
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
                let mut content = sfp::Content::text(match description {
                    Some(d) if !d.is_empty() => format!("{title}\n{d}"),
                    _ => title.clone(),
                });
                content.extra.insert("severity".into(), serde_json::json!(severity));
                out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "notice", vec![content], None) });
            }
            relay::Event::Commands(commands) => {
                next.commands =
                    commands.iter().map(|c| sfp::AgentCommand { name: c.name.clone(), description: c.description.clone(), hint: c.hint.clone() }).collect();
            }
            relay::Event::AgentTitle(title) => {
                next.agent_title = Some(title.clone());
            }
            relay::Event::Other(text) => out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "note", vec![sfp::Content::text(text)], None) }),
            relay::Event::AutoAnswered { title, allowed, .. } => out.push(sfp::Event::ItemCreated {
                item: self.item(id("item"), "note", vec![sfp::Content::text(format!("Policy {}: {title}", if *allowed { "allowed" } else { "denied" }))], None),
            }),
            relay::Event::AuthRequired { message, methods } => {
                next.activity = "waiting".into();
                next.authentication = Some(sfp::Authentication {
                    message: message.clone(),
                    methods: methods
                        .iter()
                        .map(|method| sfp::AuthMethod {
                            method_id: method.id.clone(),
                            title: method.name.clone(),
                            description: method.description.clone(),
                            terminal: method.terminal.as_ref().map(|(args, env)| sfp::AuthTerminal { args: args.clone(), env: env.clone() }),
                            api_key: method.api_key.clone(),
                        })
                        .collect(),
                });
            }
            relay::Event::ControlFailed { .. } => {}
            relay::Event::Controls(controls) => {
                next.controls = Some(session_controls(controls));
            }
            relay::Event::Usage { used, size } => {
                next.usage = Some(sfp::Usage { used: used.to_string(), size: size.to_string() });
            }
        }
        if &next != session {
            out.push(sfp::Event::SessionUpdated { session: next });
        }
        out
    }

    fn item(&self, item_id: String, kind: &str, content: Vec<sfp::Content>, tool: Option<sfp::Tool>) -> sfp::Item {
        sfp::Item {
            item_id,
            created_seq: String::new(),
            turn_id: self.turn.as_ref().map(|t| t.turn_id.clone()),
            submission_id: None,
            agent_generation_id: self.generation.clone(),
            kind: kind.into(),
            content,
            tool,
        }
    }

    fn append_text(&mut self, kind: &str, mut text: &str, out: &mut Vec<sfp::Event>) {
        // Bound the complete record, not just individual deltas. At worst JSON escaping
        // expands text sixfold, so 256 KiB leaves room within a 4 MiB snapshot page.
        const ITEM_TEXT_BYTES: usize = 256 * 1024;
        while !text.is_empty() {
            let (target, bytes) =
                if kind == "thought" { (&mut self.thought_item, &mut self.thought_bytes) } else { (&mut self.text_item, &mut self.text_bytes) };
            if target.is_none() {
                *bytes = 0;
            }
            if *bytes >= ITEM_TEXT_BYTES {
                *target = None;
                *bytes = 0;
            }
            let mut end = text.len().min(ITEM_TEXT_BYTES - *bytes);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
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
                if kind == "thought" {
                    content.kind = "thought".into();
                }
                out.push(sfp::Event::ItemCreated { item: self.item(item_id, kind, vec![content], None) });
            }
            text = &text[end..];
        }
    }
}

/// A shell call's command (`sfp::Tool::command`): what the agent declared as the call's command
/// (`acp::input_command`), for a call it reported as kind `execute`. Both are the agent's own
/// statements about the call. The title is never read for it: that is display text, and an
/// agent that words it differently tomorrow changes nothing here. Other kinds have inputs with
/// a `command` of their own meaning (an editor's `view`), so the kind decides.
pub(crate) fn shell_command(kind: &str, declared: Option<&str>) -> Option<String> {
    kind.eq_ignore_ascii_case("execute").then(|| declared.map(str::to_owned)).flatten()
}

fn permission_kind(kind: &acp::PermissionOptionKind) -> String {
    match kind {
        acp::PermissionOptionKind::AllowOnce => "allow_once",
        acp::PermissionOptionKind::AllowAlways => "allow_always",
        acp::PermissionOptionKind::RejectOnce => "reject_once",
        acp::PermissionOptionKind::RejectAlways => "reject_always",
        acp::PermissionOptionKind::Other(value) => value,
    }
    .into()
}
fn stop_reason(reason: &acp::StopReason) -> String {
    match reason {
        acp::StopReason::EndTurn => "end_turn",
        acp::StopReason::MaxTokens => "max_tokens",
        acp::StopReason::MaxTurnRequests => "max_turn_requests",
        acp::StopReason::Refusal => "refusal",
        acp::StopReason::Cancelled => "cancelled",
        acp::StopReason::Other(value) => value,
    }
    .into()
}
fn form_field(field: &acp::FormField) -> sfp::FormField {
    let (kind, choices) = match &field.kind {
        acp::FieldKind::Choice(choices) => ("choice", choices.clone()),
        acp::FieldKind::Choices(choices) => ("choices", choices.clone()),
        acp::FieldKind::Text => ("text", vec![]),
        acp::FieldKind::Bool => ("boolean", vec![]),
        acp::FieldKind::Number => ("number", vec![]),
    };
    sfp::FormField {
        key: field.key.clone(),
        title: field.title.clone(),
        description: field.description.clone(),
        kind: kind.into(),
        choices: choices.into_iter().map(|(value, title, description)| sfp::Choice { value, title, description }).collect(),
    }
}

pub(crate) fn session_controls(controls: &acp::Controls) -> sfp::SessionControls {
    let choice = |(value, title): &(String, String)| sfp::ControlChoice { value: value.clone(), title: title.clone() };
    sfp::SessionControls {
        status: "ready".into(),
        error: None,
        current_mode: controls.modes.as_ref().map(|(current, _)| current.clone()),
        modes: controls.modes.as_ref().map(|(_, modes)| modes.iter().map(choice).collect()).unwrap_or_default(),
        options: controls
            .options
            .iter()
            .map(|option| {
                let (value, choices) = match &option.kind {
                    acp::ConfigKind::Select(value, choices) => (sfp::ControlValue::Id { value: value.clone() }, choices.iter().map(choice).collect()),
                    acp::ConfigKind::Boolean(value) => (sfp::ControlValue::Boolean { value: *value }, vec![]),
                };
                sfp::ControlOption { option_id: option.id.clone(), title: option.name.clone(), category: option.category.clone(), value, choices }
            })
            .collect(),
    }
}

/// The last lines the session's agent wrote to its stderr: the shim puts them in the session's
/// journal as `agent[PID]: …` (silli-shim). Empty where there is no journal to read.
fn agent_stderr_tail(session_id: &str) -> String {
    const LINES: usize = 8;
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return String::new();
    }
    let Ok(out) = std::process::Command::new("journalctl")
        .args(["--user", "--no-pager", "-q", "-o", "cat", "-n", "200", "-t"])
        .arg(format!("silli-{session_id}"))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("agent[").and_then(|r| r.split_once("]: ")).map(|(_, rest)| rest))
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(LINES)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> sfp::SessionState {
        sfp::SessionState {
            session_id: "s".into(),
            agent_generation_id: "generation-one".into(),
            lifecycle: "open".into(),
            title: None,
            summary: None,
            end_reason: None,
            deleted: false,
            activity: "idle".into(),
            turn_id: None,
            question_ids: vec![],
            background: None,
            capabilities: BTreeMap::new(),
            controls: None,
            authentication: None,
            usage: None,
            agent_title: None,
            commands: vec![],
            active_ms: 0,
            active_since_ms: None,
            summarizing: None,
        }
    }
    fn created(events: &[sfp::Event]) -> &sfp::Item {
        events
            .iter()
            .find_map(|e| match e {
                sfp::Event::ItemCreated { item } => Some(item),
                _ => None,
            })
            .expect("an item")
    }
    #[test]
    fn agent_images_are_named_by_content_and_their_bytes_leave_the_record() {
        let session = session();
        let mut adapter = AdapterState::default();
        adapter.project(&session, &relay::Event::TurnStarted, None);
        let text = adapter.project(&session, &relay::Event::Text("before".into()), None);
        let image = adapter.project(&session, &relay::Event::Image { mime: "image/png".into(), data_base64: acp::base64(b"png"), parent: None }, None);
        let item = created(&image);
        assert_ne!(item.item_id, created(&text).item_id, "an image is its own item");
        let block = &item.content[0];
        assert_eq!((block.kind.as_str(), block.text.as_deref()), ("image", None));
        let upload_id = block.extra["upload_id"].as_str().unwrap().to_owned();
        assert!(upload_id.starts_with("silli-agent-") && upload_id.len() <= 128);
        assert!(!serde_json::to_string(&image).unwrap().contains(&acp::base64(b"png")), "no bytes in conversation events");
        assert_eq!(adapter.take_media(), vec![AgentMedia { upload_id: upload_id.clone(), mime: "image/png".into(), bytes: b"png".to_vec() }]);
        assert!(adapter.take_media().is_empty());
        let after = adapter.project(&session, &relay::Event::Text("after".into()), None);
        assert!(matches!(&after[0], sfp::Event::ItemCreated { .. }), "text after an image starts a new item");
        // same bytes, same name: a repeat is deduplicated by the store
        let again = adapter.project(&session, &relay::Event::Image { mime: "image/png".into(), data_base64: acp::base64(b"png"), parent: None }, None);
        assert_eq!(created(&again).content[0].extra["upload_id"], upload_id.as_str());
    }
    /// `Tool::command` is what the agent declared for a call it reported as `execute`, and
    /// nothing else: not a title that reads like a command, not another kind's `command`.
    #[test]
    fn a_shell_calls_command_is_what_the_agent_declared_never_its_title() {
        let session = session();
        let mut adapter = AdapterState::default();
        adapter.project(&session, &relay::Event::TurnStarted, None);
        let call = |id: &str, kind: &str, title: &str, command: Option<&str>| relay::Event::ToolCall {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            status: "Pending".into(),
            detail: None,
            diff: None,
            parent: None,
            command: command.map(Into::into),
        };
        let update = |id: &str, status: Option<&str>, title: Option<&str>, command: Option<&str>| relay::Event::ToolCallUpdate {
            id: id.into(),
            status: status.map(Into::into),
            title: title.map(Into::into),
            detail: None,
            diff: None,
            command: command.map(Into::into),
        };
        let patched = |events: &[sfp::Event]| {
            events
                .iter()
                .find_map(|e| match e {
                    sfp::Event::ItemPatched { set, .. } => set.tool.clone(),
                    _ => None,
                })
                .expect("a patched tool")
        };
        // Claude: the call appears while its input streams, then the command arrives
        let started = adapter.project(&session, &call("bash", "Execute", "Terminal", None), None);
        assert_eq!(created(&started).tool.as_ref().unwrap().command, None);
        let refined = adapter.project(&session, &update("bash", None, Some("ls; pwd"), Some("ls; pwd")), None);
        assert_eq!(patched(&refined).command.as_deref(), Some("ls; pwd"));
        // an update that declares nothing leaves it, and a reworded title does not touch it
        let done = adapter.project(&session, &update("bash", Some("Completed"), Some("List the files"), None), None);
        assert_eq!((patched(&done).command.as_deref(), patched(&done).title.as_str()), (Some("ls; pwd"), "List the files"));
        // Codex: declared with the call
        let codex = adapter.project(&session, &call("exec", "Execute", "git status", Some("git status")), None);
        assert_eq!(created(&codex).tool.as_ref().unwrap().command.as_deref(), Some("git status"));
        // a title that reads like a command is not one
        let titled = adapter.project(&session, &call("titled", "Execute", "git status; ls", None), None);
        assert_eq!(created(&titled).tool.as_ref().unwrap().command, None);
        // another kind's `command` means something else (an editor's "view")
        let editor = adapter.project(&session, &call("editor", "Read", "View a.rs", Some("view")), None);
        assert_eq!(created(&editor).tool.as_ref().unwrap().command, None);
    }
    #[test]
    fn oversized_or_invalid_agent_images_are_omitted_with_a_reason() {
        let mut media = vec![];
        let big = image_block("image/png", &acp::base64(&vec![0; super::super::uploads::AGENT_IMAGE_BYTES + 1]), &mut media);
        assert_eq!(big.extra["omitted"], "too_large");
        assert_eq!(image_block("image/png", "not base64!", &mut media).extra["omitted"], "invalid");
        assert!(media.is_empty() && !big.extra.contains_key("upload_id"));
        let mut events =
            vec![sfp::Event::ItemCreated { item: AdapterState::default().item("i".into(), "agent", vec![image_block("image/png", "YQ==", &mut media)], None) }];
        mark_omitted(&mut events, &media.iter().map(|m| m.upload_id.clone()).collect());
        let block = &created(&events).content[0];
        assert_eq!(block.extra["omitted"], "capacity");
        assert!(!block.extra.contains_key("upload_id"));
    }
    #[test]
    fn native_image_content_becomes_an_image_block_without_bytes() {
        let session = session();
        let mut adapter = AdapterState::default();
        let update =
            serde_json::json!({"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"image","mimeType":"image/webp","data":"YWJj"}});
        let events = adapter.project(&session, &relay::Event::V2Update { update }, None);
        let block = &created(&events).content[0];
        assert_eq!(block.kind, "image");
        assert_eq!(block.extra["mime"], "image/webp");
        assert!(!block.extra.contains_key("data") && block.extra.contains_key("upload_id"));
        assert_eq!(adapter.take_media()[0].bytes, b"abc");
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
        let question = relay::Event::PermissionRequest {
            request: 1,
            tool_call_id: String::new(),
            title: "Unknown operation".into(),
            options: vec![acp::PermissionOption { id: "option".into(), name: "Custom".into(), kind: acp::PermissionOptionKind::Other("_future".into()) }],
        };
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
                sfp::Event::ItemCreated { item } => {
                    order.push(item.item_id.clone());
                    items.insert(item.item_id.clone(), item);
                }
                sfp::Event::ItemTextAppended { item_id, text, .. } => {
                    items.get_mut(&item_id).unwrap().content[0].text.as_mut().unwrap().push_str(&text);
                }
                _ => {}
            }
        }
        assert!(items.len() >= 3);
        for item in items.values() {
            assert!(item.content[0].text.as_ref().unwrap().len() <= 256 * 1024);
            assert!(serde_json::to_vec(&sfp::Record::Item { item: item.clone() }).unwrap().len() < 4 * 1024 * 1024);
        }
        let restored = order.iter().map(|id| items[id].content[0].text.as_deref().unwrap()).collect::<String>();
        assert_eq!(restored, format!("prefix{huge}"));
        let thoughts = adapter.project(&session, &relay::Event::Thought("é".repeat(256 * 1024)), None);
        assert_eq!(thoughts.iter().filter(|e| matches!(e, sfp::Event::ItemCreated { .. })).count(), 2);
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
        assert!(
            matches!(&revised[0], sfp::Event::ItemPatched { item_id, set } if item_id == &item.item_id && set.content.as_ref().unwrap()[0].extra["status"] == "completed")
        );
        adapter.project(&session, &relay::Event::Stopped(acp::StopReason::EndTurn), None);
        adapter.project(&session, &relay::Event::TurnStarted, None);
        assert!(
            matches!(adapter.project(&session, &relay::Event::Plan(vec![step("next", "pending")]), None)[0], sfp::Event::ItemCreated { .. }),
            "a new turn has its own plan"
        );

        let commands = adapter.project(
            &session,
            &relay::Event::Commands(vec![acp::AgentCommand { name: "review".into(), description: "Review".into(), hint: None }]),
            None,
        );
        assert!(matches!(&commands[..], [sfp::Event::SessionUpdated { session }] if session.commands[0].name == "review"));
        let titled = adapter.project(&session, &relay::Event::AgentTitle("Fix the window".into()), None);
        assert!(
            matches!(&titled[..], [sfp::Event::SessionUpdated { session: s }] if s.agent_title.as_deref() == Some("Fix the window") && s.title == session.title)
        );
        let notice = adapter.project(
            &session,
            &relay::Event::Notice { severity: "warning".into(), title: "Rate limited".into(), description: Some("retrying".into()) },
            None,
        );
        assert!(
            matches!(&notice[0], sfp::Event::ItemCreated { item } if item.kind == "notice" && item.content[0].text.as_deref() == Some("Rate limited\nretrying") && item.content[0].extra["severity"] == "warning")
        );
        // an agent that fails says so in the conversation, not only in the journal
        let ended = adapter.project(&session, &relay::Event::Ended(Some("connection closed".into())), None);
        assert!(ended.iter().any(|e| matches!(e, sfp::Event::ItemCreated { item } if item.kind == "notice"
            && item.content[0].text.as_deref().is_some_and(|t| t.starts_with("The agent stopped: connection closed"))
            && item.content[0].extra["severity"] == "error")));
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
