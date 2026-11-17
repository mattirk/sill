//! Native ACP v2 upserts: source identities and omitted/null/value semantics survive restart.
use super::*;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct State {
    pub enabled: bool,
    pub active_seen: bool,
    pub accepted_message: Option<String>,
    pub last_turn: Option<String>,
    item_order: Vec<String>,
    items: BTreeMap<String, NativeItem>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeItem {
    item: sfp::Item,
    fields: Map<String, Value>,
    #[serde(default)]
    metadata_block: Option<String>,
}
fn blocks(values: &[Value], media: &mut Vec<AgentMedia>) -> Vec<sfp::Content> {
    values
        .iter()
        .map(|value| {
            // Tool content wraps ordinary content; keep other content types intact.
            let value = if value["type"] == "content" { value.get("content").unwrap_or(value) } else { value };
            // Image bytes become a session upload; the conversation record only names them.
            if value["type"] == "image"
                && let Some(data) = value.get("data").and_then(Value::as_str)
            {
                return image_block(value.get("mimeType").and_then(Value::as_str).unwrap_or_default(), data, media);
            }
            let mut extra = value.as_object().cloned().unwrap_or_default();
            let kind = extra.remove("type").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "unknown".into());
            let text = extra.remove("text").and_then(|v| v.as_str().map(str::to_owned));
            sfp::Content { block_id: id("block"), kind, text, extra: extra.into_iter().collect() }
        })
        .collect()
}
fn patch(fields: &mut Map<String, Value>, update: &Map<String, Value>) {
    for (key, value) in update {
        if matches!(key.as_str(), "sessionUpdate" | "messageId" | "toolCallId" | "terminalId") {
            continue;
        }
        if key == "_meta" {
            if value.is_null() {
                fields.remove(key);
            } else if let Some(update) = value.as_object() {
                let target = fields.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
                if !target.is_object() {
                    *target = Value::Object(Map::new());
                }
                let target = target.as_object_mut().unwrap();
                for (key, value) in update {
                    if value.is_null() {
                        target.remove(key);
                    } else {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        } else if value.is_null() {
            fields.remove(key);
        } else {
            fields.insert(key.clone(), value.clone());
        }
    }
}
fn text_field(fields: &Map<String, Value>, key: &str, default: &str) -> String {
    fields.get(key).and_then(Value::as_str).unwrap_or(default).into()
}
impl AdapterState {
    pub fn is_v2(&self) -> bool {
        self.v2.enabled
    }
    pub fn v2_last_turn(&self) -> Option<&str> {
        self.v2.last_turn.as_deref()
    }
    pub fn v2_turn_text(&self) -> Option<String> {
        let turn = self.v2.last_turn.as_ref()?;
        Some(
            self.v2
                .item_order
                .iter()
                .filter_map(|key| self.v2.items.get(key))
                .filter(|item| item.item.kind == "agent" && item.item.turn_id.as_ref() == Some(turn))
                .map(|item| item.item.content.iter().filter(|b| b.kind == "text").filter_map(|b| b.text.as_deref()).collect::<String>())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }
    pub(super) fn project_v2(&mut self, next: &mut sfp::SessionState, update: &Value, out: &mut Vec<sfp::Event>) {
        self.v2.enabled = true;
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        if kind == "state_update" {
            let state = update.get("state").and_then(Value::as_str).unwrap_or("unknown");
            match state {
                "running" | "requires_action" => {
                    self.v2.active_seen = true;
                    let activity = if state == "running" { "working" } else { "waiting" };
                    if self.turn.is_none() {
                        self.turn = Some(self.new_turn("unknown"));
                    }
                    let turn = self.turn.as_mut().unwrap();
                    if turn.activity != activity {
                        turn.activity = activity.into();
                        out.push(sfp::Event::TurnUpdated { turn: turn.clone() });
                    }
                    self.v2.last_turn = Some(turn.turn_id.clone());
                    next.turn_id = Some(turn.turn_id.clone());
                    next.activity = activity.into();
                }
                "idle" => {
                    // A resume/initial idle snapshot is not a completion. Only a native
                    // active-to-idle transition closes foreground work.
                    if self.v2.active_seen {
                        if let Some(mut turn) = self.turn.take() {
                            let reason = update.get("stopReason").and_then(Value::as_str).unwrap_or("not_reported");
                            turn.activity = "idle".into();
                            turn.outcome = match reason {
                                "end_turn" => "completed",
                                "cancelled" => "cancelled",
                                "refusal" | "error" => "failed",
                                _ => "unknown",
                            }
                            .into();
                            turn.reason = Some(sfp::Reason { code: reason.into(), message: "ACP v2 foreground activity became idle".into() });
                            out.push(sfp::Event::TurnUpdated { turn });
                        }
                        self.v2.active_seen = false;
                    }
                    next.turn_id = self.turn.as_ref().map(|t| t.turn_id.clone());
                    next.activity = if self.turn.is_some() { "unknown" } else { "idle" }.into();
                }
                _ => {
                    next.activity = "unknown".into();
                    if let Some(turn) = self.turn.as_mut()
                        && turn.activity != "unknown"
                    {
                        turn.activity = "unknown".into();
                        out.push(sfp::Event::TurnUpdated { turn: turn.clone() });
                    }
                }
            }
            return;
        }
        // Sill creates canonical user items when it accepts a submission. This opt-in
        // adapter resumes without historical replay and must never identify echoes by text.
        if matches!(kind, "user_message" | "user_message_chunk") {
            return;
        }
        // The transport emits normalized Controls separately for this native update.
        if kind == "config_option_update" {
            return;
        }
        if kind == "session_info_update" {
            if let Some(title) = update.get("title") {
                next.agent_title = title.as_str().map(str::to_owned);
            }
            return;
        }
        if kind == "usage_update" {
            if let (Some(used), Some(size)) = (update.get("used").and_then(Value::as_u64), update.get("size").and_then(Value::as_u64)) {
                next.usage = Some(sfp::Usage { used: used.to_string(), size: size.to_string() });
            }
            return;
        }
        if kind == "available_commands_update" {
            if let Some(commands) = update.get("availableCommands").and_then(Value::as_array) {
                next.commands = commands
                    .iter()
                    .filter_map(|c| {
                        Some(sfp::AgentCommand {
                            name: c.get("name")?.as_str()?.into(),
                            description: c.get("description").and_then(Value::as_str).unwrap_or_default().into(),
                            hint: c.pointer("/input/hint").and_then(Value::as_str).map(str::to_owned),
                        })
                    })
                    .collect();
            }
            return;
        }
        if kind == "notice" {
            let title = update.get("title").and_then(Value::as_str).unwrap_or("Agent notice");
            let text =
                update.get("description").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| format!("{title}\n{s}")).unwrap_or_else(|| title.into());
            let mut block = sfp::Content::text(text);
            block.extra.insert("severity".into(), update.get("severity").cloned().unwrap_or(Value::String("info".into())));
            out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "notice", vec![block], None) });
            return;
        }
        if matches!(kind, "terminal_update" | "terminal_output_chunk" | "plan_update") {
            self.v2_display(update, kind, out);
            return;
        }
        let (source, category, chunk) = match kind {
            "agent_message" | "agent_message_chunk" => (update.get("messageId").and_then(Value::as_str), "agent", kind.ends_with("_chunk")),
            "agent_thought" | "agent_thought_chunk" => (update.get("messageId").and_then(Value::as_str), "thought", kind.ends_with("_chunk")),
            "tool_call_update" | "tool_call_content_chunk" => (update.get("toolCallId").and_then(Value::as_str), "tool", kind.ends_with("_chunk")),
            _ => {
                // Unknown display updates remain display data, never state or permission.
                let mut block = sfp::Content::text(update.get("title").and_then(Value::as_str).unwrap_or(kind));
                block.kind = "extension".into();
                block.extra.insert("name".into(), Value::String(kind.into()));
                block.extra.insert("data".into(), update.clone());
                out.push(sfp::Event::ItemCreated { item: self.item(id("item"), "notice", vec![block], None) });
                return;
            }
        };
        let Some(source) = source else {
            return;
        };
        let Some(object) = update.as_object() else {
            return;
        };
        let key = format!("{}:{source}", if category == "tool" { "tool" } else { "message" });
        let new = !self.v2.items.contains_key(&key);
        if new {
            let item = self.item(id("item"), category, vec![], None);
            self.v2.items.insert(key.clone(), NativeItem { item, fields: Map::new(), metadata_block: None });
            self.v2.item_order.push(key.clone());
        }
        let native = self.v2.items.get_mut(&key).unwrap();
        let previous = native.item.clone();
        let metadata_block = native.metadata_block.take();
        native.item.content.retain(|b| Some(&b.block_id) != metadata_block.as_ref());
        if chunk {
            if let Some(content) = object.get("content") {
                native.item.content.extend(blocks(std::slice::from_ref(content), &mut self.media));
                let current = native.fields.entry("content").or_insert_with(|| Value::Array(vec![]));
                if !current.is_array() {
                    *current = Value::Array(vec![]);
                }
                current.as_array_mut().unwrap().push(content.clone());
            }
        } else {
            let old_content = native.fields.get("content").cloned();
            patch(&mut native.fields, object);
            if object.contains_key("content") && old_content != native.fields.get("content").cloned() {
                native.item.content = blocks(native.fields.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default(), &mut self.media);
            }
        }
        if let Some(metadata) = native.fields.get("_meta").filter(|v| v.as_object().is_some_and(|o| !o.is_empty())) {
            let block_id = metadata_block.unwrap_or_else(|| id("block"));
            native.metadata_block = Some(block_id.clone());
            native.item.content.push(sfp::Content {
                block_id,
                kind: "message_metadata".into(),
                text: None,
                extra: BTreeMap::from([("metadata".into(), metadata.clone())]),
            });
        }
        if category == "tool" {
            let tool = sfp::Tool {
                title: text_field(&native.fields, "title", ""),
                kind: text_field(&native.fields, "kind", "other"),
                status: text_field(&native.fields, "status", "pending"),
                detail: serde_json::json!({"input":native.fields.get("rawInput"),"output":native.fields.get("rawOutput")}).to_string(),
                diff: native
                    .fields
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|content| content.iter().filter(|c| c["type"] == "diff").map(Value::to_string).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default(),
                parent_item_id: None,
                command: shell_command(&text_field(&native.fields, "kind", "other"), acp::input_command(native.fields.get("rawInput")).as_deref()),
            };
            self.tools.insert(source.into(), (native.item.item_id.clone(), tool.clone()));
            native.item.tool = Some(tool);
        }
        if new {
            out.push(sfp::Event::ItemCreated { item: native.item.clone() });
        } else if previous != native.item {
            out.push(sfp::Event::ItemPatched {
                item_id: native.item.item_id.clone(),
                set: sfp::ItemPatch { content: Some(native.item.content.clone()), tool: native.item.tool.clone() },
            });
        }
    }
    fn v2_display(&mut self, update: &Value, kind: &str, out: &mut Vec<sfp::Event>) {
        let (source, category, object) = if kind == "plan_update" {
            let Some(plan) = update.get("plan") else {
                return;
            };
            (plan.get("planId").and_then(Value::as_str), "plan", plan.as_object())
        } else {
            (update.get("terminalId").and_then(Value::as_str), "terminal", update.as_object())
        };
        let (Some(source), Some(object)) = (source, object) else {
            return;
        };
        let key = format!("{category}:{source}");
        let chunk_bytes = if kind == "terminal_output_chunk" {
            let Some(data) = object.get("data").and_then(Value::as_str) else {
                return;
            };
            let Ok(bytes) = sill_proto::admin::decode_bytes(data) else {
                return;
            };
            Some(bytes)
        } else {
            None
        };
        let new = !self.v2.items.contains_key(&key);
        if new {
            let item = self.item(id("item"), category, vec![], None);
            self.v2.items.insert(key.clone(), NativeItem { item, fields: Map::new(), metadata_block: None });
            self.v2.item_order.push(key.clone());
        }
        let native = self.v2.items.get_mut(&key).unwrap();
        let before = native.fields.clone();
        if let Some(bytes) = chunk_bytes {
            let mut accumulated = native
                .fields
                .get("output")
                .and_then(|v| v.get("data"))
                .and_then(Value::as_str)
                .and_then(|s| sill_proto::admin::decode_bytes(s).ok())
                .unwrap_or_default();
            accumulated.extend(bytes);
            native.fields.insert("output".into(), serde_json::json!({"data":sill_proto::admin::encode_bytes(&accumulated)}));
        } else {
            patch(&mut native.fields, object);
        }
        if !new && before == native.fields {
            return;
        }
        if category == "plan" {
            native.item.content = native
                .fields
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|e| sfp::Content {
                            block_id: id("block"),
                            kind: "plan_entry".into(),
                            text: e.get("content").and_then(Value::as_str).map(str::to_owned),
                            extra: BTreeMap::from([
                                ("status".into(), e.get("status").cloned().unwrap_or(Value::Null)),
                                ("priority".into(), e.get("priority").cloned().unwrap_or(Value::Null)),
                            ]),
                        })
                        .collect()
                })
                .unwrap_or_default();
        } else {
            let text = native
                .fields
                .get("output")
                .and_then(|v| v.get("data"))
                .and_then(Value::as_str)
                .and_then(|data| sill_proto::admin::decode_bytes(data).ok())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            native.item.content =
                vec![sfp::Content { block_id: id("block"), kind: "terminal_output".into(), text, extra: native.fields.clone().into_iter().collect() }];
        }
        if new {
            out.push(sfp::Event::ItemCreated { item: native.item.clone() });
        } else {
            out.push(sfp::Event::ItemPatched {
                item_id: native.item.item_id.clone(),
                set: sfp::ItemPatch { content: Some(native.item.content.clone()), tool: None },
            });
        }
    }
    #[expect(clippy::too_many_arguments, reason = "the fields of one ACP permission request, beside the state they change")]
    pub(super) fn v2_permission(
        &mut self,
        next: &mut sfp::SessionState,
        request: u64,
        tool_call_id: Option<&str>,
        title: &str,
        subject: Option<&Value>,
        options: &[acp::PermissionOption],
        out: &mut Vec<sfp::Event>,
    ) {
        if self.questions.contains_key(&request) {
            return;
        }
        if let Some(tool) = subject.and_then(|s| s.get("toolCall")).and_then(Value::as_object) {
            let mut update = tool.clone();
            update.insert("sessionUpdate".into(), Value::String("tool_call_update".into()));
            self.project_v2(next, &Value::Object(update), out);
        }
        let question_id = id("question");
        self.questions.insert(request, question_id.clone());
        let subject = subject.map(|value| {
            let mut content = sfp::Content::text(serde_json::to_string_pretty(value).unwrap_or_default());
            content.kind = "permission_subject".into();
            content.extra.insert("data".into(), value.clone());
            content
        });
        out.push(sfp::Event::QuestionUpdated {
            question: sfp::Question {
                question_id: question_id.clone(),
                agent_generation_id: self.generation.clone(),
                kind: "permission".into(),
                title: title.into(),
                description: subject.as_ref().and_then(|s| s.text.clone()),
                subject,
                tool_item_id: tool_call_id.and_then(|source| self.tools.get(source)).map(|(id, _)| id.clone()),
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
            },
        });
        next.question_ids.push(question_id);
        next.activity = "waiting".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn session() -> sfp::SessionState {
        sfp::SessionState {
            session_id: "s".into(),
            agent_generation_id: "g1".into(),
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
    fn feed(a: &mut AdapterState, s: &mut sfp::SessionState, v: Value) -> Vec<sfp::Event> {
        let events = a.project(s, &relay::Event::V2Update { update: v }, None);
        for event in &events {
            if let sfp::Event::SessionUpdated { session } = event {
                *s = session.clone();
            }
        }
        events
    }
    #[test]
    fn native_message_identity_replacement_clear_and_append_survive_restart() {
        let mut a = AdapterState::default();
        let mut s = session();
        let original = json!({"sessionUpdate":"agent_message","messageId":"m","content":[{"type":"text","text":"old"}],"_meta":{"a":1,"b":{"old":true}}});
        let events = feed(&mut a, &mut s, original.clone());
        let sfp::Event::ItemCreated { item } = &events[0] else { panic!("created") };
        let identity = item.item_id.clone();
        assert_eq!(item.content[0].text.as_deref(), Some("old"));
        let mut a: AdapterState = serde_json::from_slice(&serde_json::to_vec(&a).unwrap()).unwrap();
        assert!(feed(&mut a, &mut s, original).is_empty(), "authoritative replay is idempotent");
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message_chunk","messageId":"other","content":{"type":"text","text":"interleaved"}}));
        let events = feed(
            &mut a,
            &mut s,
            json!({"sessionUpdate":"agent_message","messageId":"m","content":[{"type":"text","text":"replacement"}],"_meta":{"a":null,"b":{"new":true}}}),
        );
        let sfp::Event::ItemPatched { item_id, set } = &events[0] else { panic!("patched") };
        assert_eq!(item_id, &identity);
        assert_eq!(set.content.as_ref().unwrap()[0].text.as_deref(), Some("replacement"));
        assert_eq!(a.v2.items["message:m"].fields["_meta"], json!({"b":{"new":true}}));
        assert!(feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"m"})).is_empty(), "omission does not clear");
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"m","content":null,"_meta":null}));
        assert!(a.v2.items["message:m"].item.content.is_empty());
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"text","text":"after clear"}}));
        assert_eq!(a.v2.items["message:m"].item.item_id, identity);
        assert_eq!(a.v2.items["message:m"].item.content[0].text.as_deref(), Some("after clear"));
        s.agent_generation_id = "g2".into();
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"m","content":[]}));
        assert_ne!(a.v2.items["message:m"].item.item_id, identity);
    }
    #[test]
    fn acceptance_is_not_completion_and_unknown_state_never_becomes_idle() {
        let mut a = AdapterState::default();
        let mut s = session();
        a.project(&s, &relay::Event::V2Ready { session_id: "native".into(), resumed: false, controls: Default::default() }, None);
        a.project(&s, &relay::Event::TurnStarted, None);
        let turn = a.turn.as_ref().unwrap().turn_id.clone();
        assert_eq!(a.turn.as_ref().unwrap().activity, "unknown");
        a.project(&s, &relay::Event::PromptAccepted { message_id: "u".into() }, None);
        assert_eq!(a.turn.as_ref().unwrap().outcome, "pending");
        assert!(
            !feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}))
                .iter()
                .any(|e| matches!(e,sfp::Event::TurnUpdated{turn}if turn.outcome=="completed"))
        );
        feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"running"}));
        assert_eq!(s.activity, "working");
        feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"future_state"}));
        assert_eq!(s.activity, "unknown");
        feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"requires_action"}));
        assert_eq!(s.activity, "waiting");
        let ended = feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled"}));
        assert!(ended.iter().any(|e| matches!(e,sfp::Event::TurnUpdated{turn:t}if t.turn_id==turn&&t.outcome=="cancelled")));
        assert!(feed(&mut a, &mut s, json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled"})).is_empty());
    }
    /// As in the v1 adapter: a shell call's command is the `command` its input declares.
    #[test]
    fn a_native_shell_calls_command_comes_from_its_declared_input() {
        let mut a = AdapterState::default();
        let mut s = session();
        let command = |a: &AdapterState, id: &str| a.v2.items[id].item.tool.as_ref().unwrap().command.clone();
        feed(
            &mut a,
            &mut s,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"sh","title":"Terminal","kind":"execute","status":"pending","rawInput":{}}),
        );
        assert_eq!(command(&a, "tool:sh"), None);
        feed(&mut a, &mut s, json!({"sessionUpdate":"tool_call_update","toolCallId":"sh","title":"List the files","rawInput":{"command":"ls; pwd"}}));
        assert_eq!(command(&a, "tool:sh").as_deref(), Some("ls; pwd"));
        feed(&mut a, &mut s, json!({"sessionUpdate":"tool_call_update","toolCallId":"ed","title":"View","kind":"read","rawInput":{"command":"view"}}));
        assert_eq!(command(&a, "tool:ed"), None, "another kind's `command` is not a shell command");
    }
    #[test]
    fn tools_clear_fields_and_permissions_keep_subject_without_tool_identity() {
        let mut a = AdapterState::default();
        let mut s = session();
        feed(&mut a, &mut s, json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","title":"Read","status":"completed","rawInput":{"path":"x"}}));
        feed(&mut a, &mut s, json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","title":null,"status":null,"rawInput":null}));
        let tool = a.v2.items["tool:tool"].item.tool.as_ref().unwrap();
        assert_eq!(tool.title, "");
        assert_eq!(tool.status, "pending");
        let events = a.project(
            &s,
            &relay::Event::V2PermissionRequest {
                request: 3,
                tool_call_id: None,
                title: "Network request".into(),
                subject: Some(json!({"type":"network","host":"example.test"})),
                options: vec![acp::PermissionOption { id: "deny".into(), name: "Reject".into(), kind: acp::PermissionOptionKind::RejectOnce }],
            },
            None,
        );
        let sfp::Event::QuestionUpdated { question } = &events[0] else { panic!("question") };
        assert!(question.tool_item_id.is_none());
        assert!(question.subject.as_ref().unwrap().text.as_ref().unwrap().contains("example.test"));
        assert_eq!(question.options[0].kind, "reject_once");
    }
    #[test]
    fn native_plan_and_terminal_snapshots_reuse_identity_and_replace_output() {
        let mut a = AdapterState::default();
        let mut s = session();
        let plan =
            json!({"sessionUpdate":"plan_update","plan":{"type":"items","planId":"p","entries":[{"content":"Read","status":"pending","priority":"high"}]}});
        feed(&mut a, &mut s, plan.clone());
        let plan_id = a.v2.items["plan:p"].item.item_id.clone();
        assert!(feed(&mut a, &mut s, plan).is_empty());
        feed(&mut a, &mut s, json!({"sessionUpdate":"plan_update","plan":{"type":"items","planId":"p","entries":[]}}));
        assert_eq!(a.v2.items["plan:p"].item.item_id, plan_id);
        assert!(a.v2.items["plan:p"].item.content.is_empty());
        feed(&mut a, &mut s, json!({"sessionUpdate":"terminal_update","terminalId":"t","output":{"data":"b2xk"},"command":"echo"}));
        let terminal_id = a.v2.items["terminal:t"].item.item_id.clone();
        feed(&mut a, &mut s, json!({"sessionUpdate":"terminal_output_chunk","terminalId":"t","data":"IG5ldw=="}));
        assert_eq!(a.v2.items["terminal:t"].item.content[0].text.as_deref(), Some("old new"));
        feed(&mut a, &mut s, json!({"sessionUpdate":"terminal_update","terminalId":"t","output":{"data":"cmVwbGFjZWQ="}}));
        assert_eq!(a.v2.items["terminal:t"].item.item_id, terminal_id);
        assert_eq!(a.v2.items["terminal:t"].item.content[0].text.as_deref(), Some("replaced"));
        feed(&mut a, &mut s, json!({"sessionUpdate":"terminal_update","terminalId":"t","output":null}));
        assert!(a.v2.items["terminal:t"].item.content[0].text.is_none());
    }

    #[test]
    fn final_turn_text_uses_message_arrival_order_and_authoritative_replacements() {
        let mut a = AdapterState::default();
        let mut s = session();
        a.project(&s, &relay::Event::V2Ready { session_id: "native".into(), resumed: false, controls: Default::default() }, None);
        a.project(&s, &relay::Event::TurnStarted, None);
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"z","content":[{"type":"text","text":"old"}]}));
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"a","content":[{"type":"text","text":"second"}]}));
        feed(&mut a, &mut s, json!({"sessionUpdate":"agent_message","messageId":"z","content":[{"type":"text","text":"first"}]}));
        assert_eq!(a.v2_turn_text().as_deref(), Some("first\n\nsecond"));
        let events = a.project(
            &s,
            &relay::Event::V2PermissionRequest {
                request: 4,
                tool_call_id: Some("new-tool".into()),
                title: "Edit file".into(),
                subject: Some(json!({"type":"tool_call","toolCall":{"toolCallId":"new-tool","title":"Edit","status":"pending"}})),
                options: vec![],
            },
            None,
        );
        let item_id = events
            .iter()
            .find_map(|e| match e {
                sfp::Event::ItemCreated { item } => Some(item.item_id.clone()),
                _ => None,
            })
            .unwrap();
        let question = events
            .iter()
            .find_map(|e| match e {
                sfp::Event::QuestionUpdated { question } => Some(question),
                _ => None,
            })
            .unwrap();
        assert_eq!(question.tool_item_id.as_deref(), Some(item_id.as_str()));
    }
}
