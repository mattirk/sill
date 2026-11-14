//! SFP state reconstruction and local adaptation for the existing Rust renderers.
//! No daemon store is read: snapshots and sequenced public events are the only state source.
use std::collections::BTreeMap;
use anyhow::{Result, bail, Context};
use sill_proto::{sfp::*, Client, wire};

#[derive(Default)]
pub struct View {
    pub subscription_id: String,
    pub session_id: String,
    pub seq: u64,
    pub session: Option<SessionState>,
    items: BTreeMap<String, Item>,
    questions: BTreeMap<String, Question>,
    submissions: BTreeMap<String, Submission>,
    requests: BTreeMap<String, u64>,
    next_request: u64,
}
impl View {
    pub fn attach(&mut self, client: &Client, session_id: &str) -> Result<Vec<wire::Event>> {
        let attached: Attached = client.call("_sill/sessions.attach", Attach { session_id: session_id.into(), since: None })?;
        let Attached::Snapshot { subscription_id, snapshot } = attached else { bail!("snapshot attach unexpectedly returned replay"); };
        let mut replacement = Self { subscription_id, session_id: session_id.into(), seq: snapshot.at.seq.parse()?, ..Default::default() };
        let mut page = None;
        loop {
            let part: SnapshotPage = client.call("_sill/sessions.snapshot", SnapshotRequest { token: snapshot.token.clone(), page })?;
            if part.at != snapshot.at { bail!("snapshot cursor changed between pages"); }
            for record in part.records {
                match record {
                    Record::Session { session } => replacement.session = Some(session),
                    Record::Item { item } => { replacement.items.insert(item.item_id.clone(), item); }
                    Record::Question { question } => { replacement.questions.insert(question.question_id.clone(), question); }
                    Record::Submission { submission } => { replacement.submissions.insert(submission.submission_id.clone(),submission); }
                    Record::Turn { .. } => {}
                }
            }
            page = part.next_page;
            if page.is_none() { break; }
        }
        let events = replacement.render_snapshot();
        *self = replacement;
        Ok(events)
    }
    pub fn detach(&self, client: &Client) -> Result<()> {
        client.call::<serde_json::Value>("_sill/sessions.detach", Detach { session_id: self.session_id.clone(), subscription_id: self.subscription_id.clone() })?;
        Ok(())
    }
    pub fn question_id(&self, request: u64) -> Option<&str> {
        self.requests.iter().find_map(|(id, n)| (*n == request).then_some(id.as_str()))
    }
    pub fn accept(&mut self, notification: SessionEvent) -> Result<Vec<wire::Event>> {
        if notification.session_id != self.session_id || notification.subscription_id != self.subscription_id { return Ok(vec![]); }
        let seq: u64 = notification.seq.parse()?;
        if seq <= self.seq { return Ok(vec![]); }
        if seq != self.seq + 1 { bail!("SFP event gap: expected {}, received {seq}", self.seq + 1); }
        let mut events = vec![];
        match notification.event {
            Event::ItemCreated { item } => {
                if self.items.contains_key(&item.item_id) { bail!("duplicate SFP item identity"); }
                events.extend(render_item(&item, &self.attachments_of(&item)));
                self.items.insert(item.item_id.clone(), item);
            }
            Event::ItemTextAppended { item_id, block_id, text } => {
                // The existing renderer appends text at its visual tail. Queued user items and
                // background output can put a newer item after this target: reconstruct then
                // so stable SFP order survives without appending to the wrong visual message.
                let at_tail = self.items.values().max_by_key(|item|item.created_seq.parse::<u64>().unwrap_or_default()).is_some_and(|item|item.item_id == item_id);
                let item = self.items.get_mut(&item_id).context("SFP append target missing")?;
                let content = item.content.iter_mut().find(|b| b.block_id == block_id).context("SFP text block missing")?;
                content.text.as_mut().context("SFP append target is not text")?.push_str(&text);
                events.push(match item.kind.as_str() {
                    "thought" => wire::Event::Thought(text),
                    "agent" => match content.extra.get("parent_item_id").and_then(|v|v.as_str()) {
                        Some(parent) => wire::Event::SubagentText { parent:parent.into(),text },
                        None => wire::Event::Text(text),
                    },
                    _ => bail!("unsupported text append target"),
                });
                if !at_tail { events = self.render_snapshot(); }
            }
            Event::ItemPatched { item_id, set } => {
                let item = self.items.get_mut(&item_id).context("SFP patch target missing")?;
                let content_changed = set.content.is_some();
                let content_changed = content_changed && item.kind != "plan";
                if let Some(content) = set.content {
                    item.content = content;
                    // A plan replaces itself in place wherever it stands; nothing else moves.
                    if item.kind == "plan" { events.push(wire::Event::Plan(plan_entries(item))); }
                }
                if let Some(tool) = set.tool {
                    events.push(wire::Event::ToolCallUpdate { id: item_id, status: Some(tool.status.clone()), title: Some(tool.title.clone()), detail: Some(tool.detail.clone()), diff: Some(tool.diff.clone()) });
                    item.tool = Some(tool);
                }
                if content_changed { events = self.render_snapshot(); }
            }
            Event::ItemRemoved { item_id, .. } => { self.items.remove(&item_id).context("SFP removal target missing")?; events = self.render_snapshot(); }
            Event::SessionUpdated { session } => {
                events.extend(render_session(&session, self.session.as_ref()));
                self.session = Some(session);
            }
            Event::TurnUpdated { turn } => {
                if turn.outcome != "pending" {
                    let reason = turn.reason.map(|r| r.code).unwrap_or_else(|| turn.outcome.clone());
                    events.push(wire::Event::Stopped(match reason.as_str() { "end_turn" => wire::StopReason::EndTurn, "cancelled" => wire::StopReason::Cancelled, "refusal" => wire::StopReason::Refusal, "max_tokens" => wire::StopReason::MaxTokens, "max_turn_requests" => wire::StopReason::MaxTurnRequests, _ => wire::StopReason::Other(reason) }));
                }
            }
            Event::QuestionUpdated { question } => {
                let should_show = question.state == "open" && !self.questions.get(&question.question_id).is_some_and(|q| q.state == "open");
                if should_show { events.extend(self.render_question(&question)); }
                else if question.state != "open" && let Some(request) = self.requests.get(&question.question_id) { events.push(wire::Event::Resolved { request: *request }); }
                self.questions.insert(question.question_id.clone(), question);
            }
            Event::SubmissionUpdated { submission } => {
                if !self.submissions.contains_key(&submission.submission_id) && submission.delivery == "queued" && self.session.as_ref().is_some_and(|s|s.activity != "idle") {
                    events.push(wire::Event::Note("Prompt queued until the agent is ready".into()));
                }
                if submission.held { events.push(wire::Event::Note("Queued prompt is held until explicitly resumed".into())); }
                if submission.insertion == "unknown" || submission.outcome == "unknown" { events.push(wire::Event::Note("Prompt outcome is uncertain; it will not be resent automatically".into())); }
                self.submissions.insert(submission.submission_id.clone(),submission);
            }
        }
        self.seq = seq;
        Ok(events)
    }
    fn render_snapshot(&mut self) -> Vec<wire::Event> {
        let mut items: Vec<&Item> = self.items.values().collect();
        items.sort_by_key(|item| item.created_seq.parse::<u64>().unwrap_or_default());
        // Leave trailing blocks in the active turn open in the renderer, so the next delta
        // continues that block rather than creating a second visual message after reconnect.
        let active_turn = self.session.as_ref().filter(|s|matches!(s.activity.as_str(),"working"|"waiting")).and_then(|s|s.turn_id.as_ref());
        let mut live = Vec::new();
        for item in items.iter().rev() {
            if active_turn.is_some() && item.turn_id.as_ref() == active_turn && matches!(item.kind.as_str(),"agent"|"thought") {
                live.push((*item).clone());
            } else { break; }
        }
        live.reverse();
        let entries: Vec<crate::store::Entry> = items.iter().filter(|item|!live.iter().any(|open|open.item_id == item.item_id) && !is_diagnostic_note(item)).map(|item| crate::store::Entry {
            seq: item.created_seq.parse().unwrap_or_default(), ts: 0,
            kind: match item.kind.as_str() { "agent" if content_parent(item).is_some() => "subagent".into(), "notice" => "note".into(), kind => kind.into() },
            text: if item.kind == "plan" { plan_text(&plan_entries(item)) } else { item.tool.as_ref().map(|t| t.title.clone()).unwrap_or_else(|| item.content.iter().filter_map(|b| b.text.as_deref()).collect::<Vec<_>>().join("")) },
            attachments: self.attachments_of(item).into_iter().map(|a| crate::acp::UserAttachment { name: a.name, mime: a.mime, upload_id: a.upload_id, data_base64: a.data_base64 }).collect(),
            extra: item.tool.as_ref().map(|t| t.status.clone()).unwrap_or_default(), tool_kind: item.tool.as_ref().map(|t| t.kind.clone()).unwrap_or_default(),
            detail: item.tool.as_ref().map(|t| t.detail.clone()).unwrap_or_default(), diff: item.tool.as_ref().map(|t| t.diff.clone()).unwrap_or_default(),
            call_id: if item.tool.is_some() { item.item_id.clone() } else { String::new() }, parent: item.tool.as_ref().and_then(|t| t.parent_item_id.clone()).or_else(||content_parent(item).map(str::to_owned)).unwrap_or_default(),
        }).collect();
        let mut out = vec![wire::Event::Other(format!("sill:replay:{}", serde_json::json!({"key":self.session_id,"total":entries.len(),"entries":entries})))];
        if let Some(session) = &self.session { out.extend(render_session(session, None)); }
        for item in live { out.extend(render_item(&item, &[])); }
        let questions: Vec<Question> = self.questions.values().filter(|q| q.state == "open").cloned().collect();
        for question in questions { out.extend(self.render_question(&question)); }
        for submission in self.submissions.values() {
            if submission.held { out.push(wire::Event::Note("Queued prompt is held until explicitly resumed".into())); }
            else if submission.delivery == "queued" && submission.outcome == "pending" { out.push(wire::Event::Note("Prompt queued until the agent is ready".into())); }
        }
        out
    }
    /// What went with a user item: its submission's attachments. Data appears only in records
    /// from before attachments became uploads; otherwise a frontend fetches by upload ID.
    fn attachments_of(&self, item: &Item) -> Vec<wire::UserAttachment> {
        if item.kind != "user" { return vec![]; }
        let Some(submission) = item.submission_id.as_ref().and_then(|id| self.submissions.get(id)) else { return vec![] };
        submission.attachments.iter().map(|a| match a {
            Attachment::Inline { name, mime, data_base64 } => wire::UserAttachment { name: name.clone(), mime: mime.clone(), upload_id: None, data_base64: Some(data_base64.clone()) },
            Attachment::Uploaded { upload_id, name, mime } => wire::UserAttachment { name: name.clone(), mime: mime.clone(), upload_id: Some(upload_id.clone()), data_base64: None },
        }).collect()
    }
    fn render_question(&mut self, question: &Question) -> Vec<wire::Event> {
        let request = *self.requests.entry(question.question_id.clone()).or_insert_with(|| { self.next_request += 1; self.next_request });
        match question.kind.as_str() {
            "permission" => vec![wire::Event::PermissionRequest { request, tool_call_id: question.tool_item_id.clone().unwrap_or_else(|| question.question_id.clone()), title: question.title.clone(), options: question.options.iter().map(|o| wire::PermissionOption { id: o.option_id.clone(), name: o.title.clone(), kind: match o.kind.as_str() { "allow_once" => wire::PermissionOptionKind::AllowOnce, "allow_always" => wire::PermissionOptionKind::AllowAlways, "reject_once" => wire::PermissionOptionKind::RejectOnce, "reject_always" => wire::PermissionOptionKind::RejectAlways, other => wire::PermissionOptionKind::Other(other.into()) } }).collect() }],
            "form" => {
                let mut fields = Vec::new();
                for f in &question.fields {
                    let choices = || f.choices.iter().map(|c| (c.value.clone(), c.title.clone(), c.description.clone())).collect();
                    let kind = match f.kind.as_str() { "choice" => wire::FieldKind::Choice(choices()), "choices" => wire::FieldKind::Choices(choices()), "text" => wire::FieldKind::Text, "boolean" => wire::FieldKind::Bool, "number" => wire::FieldKind::Number, _ => return vec![wire::Event::Note(format!("Unsupported form field {}; question remains pending", f.kind))] };
                    fields.push(wire::FormField { key: f.key.clone(), title: f.title.clone(), description: f.description.clone(), kind });
                }
                vec![wire::Event::Elicitation { request, message: question.title.clone(), fields }]
            }
            _ => vec![wire::Event::Note(format!("Unsupported question {}; remains pending", question.kind))],
        }
    }
}
fn content_parent(item: &Item) -> Option<&str> {
    item.content.iter().find_map(|block|block.extra.get("parent_item_id").and_then(|v|v.as_str()))
}
/// Notes daemons before this fix stored from the ACP loop's diagnostics (`SessionInfoUpdate(…)`).
fn is_diagnostic_note(item: &Item) -> bool {
    item.kind == "note" && item.content.iter().filter_map(|b| b.text.as_deref()).any(crate::acp::is_diagnostic)
}
fn plan_entries(item: &Item) -> Vec<wire::PlanEntry> {
    let field = |block: &Content, key: &str| block.extra.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    item.content.iter().filter(|b| b.kind == "plan_entry").map(|b| wire::PlanEntry { content: b.text.clone().unwrap_or_default(), status: field(b, "status"), priority: field(b, "priority") }).collect()
}
/// A plan as one line per step, for transcript replay (the live view draws the same marks).
pub fn plan_text(entries: &[wire::PlanEntry]) -> String {
    entries.iter().map(|e| format!("{} {}", match e.status.as_str() { "completed" => "✓", "in_progress" => "◐", _ => "○" }, e.content)).collect::<Vec<_>>().join("\n")
}
fn render_item(item: &Item, attachments: &[wire::UserAttachment]) -> Vec<wire::Event> {
    if is_diagnostic_note(item) { return vec![]; }
    if item.kind == "plan" { return vec![wire::Event::Plan(plan_entries(item))]; }
    if item.kind == "user" && !attachments.is_empty() {
        let text = item.content.iter().filter_map(|b| b.text.as_deref()).collect::<Vec<_>>().join("");
        return vec![wire::Event::UserMessage { text, attachments: attachments.to_vec() }];
    }
    if let Some(tool) = &item.tool { return vec![wire::Event::ToolCall { id: item.item_id.clone(), title: tool.title.clone(), kind: tool.kind.clone(), status: tool.status.clone(), detail: Some(tool.detail.clone()), diff: Some(tool.diff.clone()), parent: tool.parent_item_id.clone() }]; }
    item.content.iter().filter_map(|block| block.text.as_ref().map(|text|(block,text))).map(|(block,text)| match item.kind.as_str() {
        "user" => wire::Event::UserText(text.clone()),
        "agent" => match block.extra.get("parent_item_id").and_then(|v|v.as_str()) { Some(parent)=>wire::Event::SubagentText {parent:parent.into(),text:text.clone()},None=>wire::Event::Text(text.clone()) },
        "thought" => wire::Event::Thought(text.clone()),
        _ => wire::Event::Note(text.clone()),
    }).collect()
}
fn render_session(session: &SessionState, previous: Option<&SessionState>) -> Vec<wire::Event> {
    let mut out = Vec::new();
    out.push(wire::Event::Other(format!("sill:active-time:{}", serde_json::json!([session.active_ms, session.active_since_ms]))));
    if session.lifecycle == "ended" && !previous.is_some_and(|old| old.lifecycle == "ended") {
        out.push(wire::Event::SessionEnded { deleted:session.deleted, reason:session.end_reason.clone().unwrap_or_else(||"ended".into()) });
    }
    if session.summary.is_some() && previous.is_some_and(|old|old.summary != session.summary || old.title != session.title) {
        out.push(wire::Event::Summarized { text:session.summary.clone(),title:session.title.clone() });
    }
    if previous.is_none() || previous.is_some_and(|old| old.controls != session.controls) {
        if let Some(c) = &session.controls {
            let controls = wire::Controls { modes: c.current_mode.as_ref().map(|current| (current.clone(), c.modes.iter().map(|m| (m.value.clone(),m.title.clone())).collect())), options: c.options.iter().map(|o| wire::ConfigOption { id:o.option_id.clone(),name:o.title.clone(),category:o.category.clone(),kind:match &o.value { ControlValue::Id { value } => wire::ConfigKind::Select(value.clone(),o.choices.iter().map(|c|(c.value.clone(),c.title.clone())).collect()), ControlValue::Boolean { value } => wire::ConfigKind::Boolean(*value) } }).collect() };
            if previous.is_none() || previous.is_some_and(|old| old.controls.is_none()) { out.push(wire::Event::Ready { session_id:session.agent_generation_id.clone(),resumed:true,controls }); }
            else { out.push(wire::Event::Controls(controls)); }
        }
    }
    if previous.is_none() || previous.is_some_and(|old| old.capabilities != session.capabilities) { out.push(wire::Event::Capabilities { images:session.capabilities.get("images").copied().unwrap_or(false) }); }
    if (previous.is_none() || previous.is_some_and(|old| old.authentication != session.authentication)) && let Some(a)=&session.authentication {
        out.push(wire::Event::AuthRequired { message:a.message.clone(),methods:a.methods.iter().map(|m|wire::AuthMethodInfo { id:m.method_id.clone(),name:m.title.clone(),description:m.description.clone(),terminal:m.terminal.as_ref().map(|t|(t.args.clone(),t.env.clone())) }).collect() });
    }
    if previous.map_or(!session.commands.is_empty(), |old| old.commands != session.commands) {
        out.push(wire::Event::Commands(session.commands.iter().map(|c| wire::AgentCommand { name: c.name.clone(), description: c.description.clone(), hint: c.hint.clone() }).collect()));
    }
    if (previous.is_none() || previous.is_some_and(|old| old.usage != session.usage)) && let Some(u)=&session.usage { out.push(wire::Event::Usage { used:u.used.parse().unwrap_or_default(),size:u.size.parse().unwrap_or_default() }); }
    if (previous.is_none() || previous.is_some_and(|old| old.activity != session.activity || old.turn_id != session.turn_id)) && matches!(session.activity.as_str(),"working"|"waiting") && session.turn_id.is_some() { out.push(wire::Event::TurnStarted); }
    out
}
pub fn new_id() -> String { format!("{}{}{}{}",crate::store::new_id(),crate::store::new_id(),crate::store::new_id(),crate::store::new_id()) }
pub fn answer(question_id: String, session_id: String, answer: wire::Answer) -> AnswerQuestion {
    AnswerQuestion { session_id, question_id, answer_id:new_id(), answer:match answer { wire::Answer::Permission(Some(option_id))=>Answer::Permission { option_id },wire::Answer::Form(Some(values))=>Answer::Form { values },_=>Answer::Cancel } }
}

pub fn command_request(session_id: &str, command: &wire::Command, view: Option<&View>) -> Result<(&'static str, serde_json::Value)> {
    use serde_json::json;
    let control = match command {
        wire::Command::Prompt { text, attachments, .. } => {
            let attachments: Vec<serde_json::Value> = attachments.iter().map(|a| match &a.kind {
                wire::AttachmentKind::Image { data_base64 } => json!({"type":"inline","name":a.name,"mime":a.mime,"data_base64":data_base64}),
                // The shelf stores an immutable uploaded-object ID in its local attachment
                // carrier. It is never interpreted as a path by the SFP client or server.
                wire::AttachmentKind::File { jail_path } => json!({"type":"uploaded","name":a.name,"mime":a.mime,"upload_id":jail_path.to_string_lossy()}),
            }).collect();
            return Ok(("_sill/sessions.submit", json!({"session_id":session_id,"submission_id":new_id(),"text":text,"attachments":attachments})));
        }
        wire::Command::Cancel => {
            let turn_id = view.and_then(|v|v.session.as_ref()).and_then(|s|s.turn_id.as_ref()).context("no active turn to cancel")?;
            return Ok(("_sill/sessions.cancel",json!({"session_id":session_id,"turn_id":turn_id})));
        }
        wire::Command::Authenticate(method_id) => json!({"type":"authenticate","method_id":method_id}),
        wire::Command::RetrySession => json!({"type":"retry_session"}),
        wire::Command::SetMode(mode_id) => json!({"type":"set_mode","mode_id":mode_id}),
        wire::Command::SetConfig(option_id,value) => json!({"type":"set_config","option_id":option_id,"value":match value { wire::ConfigValue::Id(value)=>json!({"type":"id","value":value}),wire::ConfigValue::Bool(value)=>json!({"type":"bool","value":value}) }}),
        wire::Command::Quit => bail!("detach using the subscription ID"),
    };
    Ok(("_sill/sessions.control",json!({"session_id":session_id,"control":control})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item() -> Item { Item { item_id:"item".into(),created_seq:"1".into(),turn_id:None,submission_id:None,agent_generation_id:"g".into(),kind:"agent".into(),content:vec![Content::text("hello")],tool:None } }
    fn notification(seq:&str,event:Event)->SessionEvent { SessionEvent {session_id:"s".into(),subscription_id:"sub".into(),seq:seq.into(),event} }
    #[test]
    fn replay_duplicates_are_ignored_and_gaps_do_not_advance_cursor() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let first=notification("1",Event::ItemCreated {item:item()});
        assert_eq!(view.accept(first.clone()).unwrap().len(),1);
        assert!(view.accept(first).unwrap().is_empty());
        let append=Event::ItemTextAppended {item_id:"item".into(),block_id:"text".into(),text:" world".into()};
        assert!(view.accept(notification("3",append.clone())).is_err());
        assert_eq!(view.seq,1);
        view.accept(notification("2",append)).unwrap();
        assert_eq!(view.items["item"].content[0].text.as_deref(),Some("hello world"));
        let mut old=notification("3",Event::ItemRemoved {item_id:"item".into(),reason:Reason{code:"removed".into(),message:String::new()}});
        old.subscription_id="old-subscription".into();
        assert!(view.accept(old).unwrap().is_empty());
        assert_eq!(view.seq,2);
    }
    #[test]
    fn appending_before_a_queued_user_item_rebuilds_in_durable_order() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        view.accept(notification("1",Event::ItemCreated {item:item()})).unwrap();
        let mut user=item();
        user.item_id="queued-user".into(); user.created_seq="2".into(); user.kind="user".into(); user.content=vec![Content::text("next prompt")];
        view.accept(notification("2",Event::ItemCreated {item:user})).unwrap();
        let update=notification("3",Event::ItemTextAppended {item_id:"item".into(),block_id:"text".into(),text:" world".into()});
        let rendered=view.accept(update.clone()).unwrap();
        assert!(!rendered.iter().any(|event|matches!(event,wire::Event::Text(_))), "an old item delta cannot append after the queued user");
        let wire::Event::Other(replay)=&rendered[0] else {panic!("expected replacement history")};
        let replay:serde_json::Value=serde_json::from_str(replay.strip_prefix("sill:replay:").unwrap()).unwrap();
        let entries=replay["entries"].as_array().unwrap();
        assert_eq!(entries.len(),2);
        assert_eq!(entries[0]["kind"],"agent");
        assert_eq!(entries[0]["text"],"hello world");
        assert_eq!(entries[1]["kind"],"user");
        assert_eq!(entries[1]["text"],"next prompt");
        assert!(view.accept(update).unwrap().is_empty());
    }
    #[test]
    fn subagent_identity_survives_live_output_and_snapshot_reconstruction() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let mut child=item();
        child.content[0].extra.insert("parent_item_id".into(),serde_json::json!("parent-tool"));
        let output=view.accept(notification("1",Event::ItemCreated {item:child})).unwrap();
        assert!(matches!(&output[0],wire::Event::SubagentText {parent,text} if parent=="parent-tool" && text=="hello"));
        let output=view.accept(notification("2",Event::ItemTextAppended {item_id:"item".into(),block_id:"text".into(),text:" world".into()})).unwrap();
        assert!(matches!(&output[0],wire::Event::SubagentText {parent,text} if parent=="parent-tool" && text==" world"));
        let output=view.render_snapshot();
        let wire::Event::Other(replay)=&output[0] else {panic!("expected replacement history")};
        let replay:serde_json::Value=serde_json::from_str(replay.strip_prefix("sill:replay:").unwrap()).unwrap();
        assert_eq!(replay["entries"][0]["kind"],"subagent");
        assert_eq!(replay["entries"][0]["parent"],"parent-tool");
        assert_eq!(replay["entries"][0]["text"],"hello world");
    }

    #[test]
    fn stored_diagnostic_notes_stay_out_of_the_conversation() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let mut dump=item(); dump.kind="note".into(); dump.content=vec![Content::text("SessionInfoUpdate(SessionInfoUpdate { title: Undefined")];
        assert!(view.accept(notification("1",Event::ItemCreated {item:dump})).unwrap().is_empty());
        let mut note=item(); note.item_id="note".into(); note.created_seq="2".into(); note.kind="note".into(); note.content=vec![Content::text("set mode failed: no")];
        assert_eq!(view.accept(notification("2",Event::ItemCreated {item:note})).unwrap().len(),1);
        let wire::Event::Other(replay)=&view.render_snapshot()[0] else {panic!("expected replacement history")};
        let replay:serde_json::Value=serde_json::from_str(replay.strip_prefix("sill:replay:").unwrap()).unwrap();
        assert_eq!(replay["total"],1);
        assert!(crate::acp::is_diagnostic("_sill_authenticated") && crate::acp::is_diagnostic("Plan(Plan { entries: [] })"));
        assert!(!crate::acp::is_diagnostic("prompt failed: Internal(x)") && !crate::acp::is_diagnostic("Policy allowed: Run (ls)"));
    }
    fn replay_of(events: &[wire::Event]) -> serde_json::Value {
        let wire::Event::Other(replay)=&events[0] else {panic!("expected replacement history: {events:?}")};
        serde_json::from_str(replay.strip_prefix("sill:replay:").unwrap()).unwrap()
    }
    #[test]
    fn plans_update_in_place_without_rebuilding_the_conversation() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let step=|text:&str,status:&str| { let mut c=Content::text(text); c.block_id=format!("entry-{text}"); c.kind="plan_entry".into(); c.extra.insert("status".into(),serde_json::json!(status)); c.extra.insert("priority".into(),serde_json::json!("high")); c };
        let mut plan=item(); plan.kind="plan".into(); plan.content=vec![step("read","in_progress"),step("fix","pending")];
        let created=view.accept(notification("1",Event::ItemCreated {item:plan})).unwrap();
        assert!(matches!(&created[..], [wire::Event::Plan(e)] if e[0].status=="in_progress" && e[1].content=="fix"));
        let mut later=item(); later.item_id="after".into(); later.created_seq="2".into();
        view.accept(notification("2",Event::ItemCreated {item:later})).unwrap();
        let patched=view.accept(notification("3",Event::ItemPatched {item_id:"item".into(),set:ItemPatch {content:Some(vec![step("read","completed"),step("fix","in_progress")]),..Default::default()}})).unwrap();
        assert!(matches!(&patched[..], [wire::Event::Plan(e)] if e[0].status=="completed"), "{patched:?}");
        let replay=replay_of(&view.render_snapshot());
        assert_eq!(replay["entries"][0]["kind"],"plan");
        assert_eq!(replay["entries"][0]["text"],"✓ read\n◐ fix");
    }
    #[test]
    fn a_prompt_shows_its_attachments_live_and_in_history() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let submission=Submission { submission_id:"sub-1".into(),session_id:"s".into(),accepted_seq:"1".into(),agent_generation_id:"g".into(),text:"look".into(),
            attachments:vec![Attachment::Uploaded{upload_id:"inline-0-sub-1".into(),name:"shot.png".into(),mime:"image/png".into()},Attachment::Inline{name:"old.png".into(),mime:"image/png".into(),data_base64:"AAAA".into()}],
            acceptance:"accepted".into(),delivery:"direct".into(),held:false,hold_reason:None,insertion:"pending".into(),outcome:"pending".into(),reason:None };
        view.accept(notification("1",Event::SubmissionUpdated {submission})).unwrap();
        let mut user=item(); user.kind="user".into(); user.submission_id=Some("sub-1".into()); user.content=vec![Content::text("look")];
        let shown=view.accept(notification("2",Event::ItemCreated {item:user})).unwrap();
        let [wire::Event::UserMessage {text,attachments}]=&shown[..] else {panic!("expected a user message: {shown:?}")};
        assert_eq!(text,"look");
        assert_eq!(attachments[0].upload_id.as_deref(),Some("inline-0-sub-1"));
        assert!(attachments[0].data_base64.is_none(),"uploaded bytes are fetched, not carried");
        assert_eq!(attachments[1].data_base64.as_deref(),Some("AAAA"));
        let replay=replay_of(&view.render_snapshot());
        assert_eq!(replay["entries"][0]["attachments"][0]["upload_id"],"inline-0-sub-1");
    }
    #[test]
    fn offered_commands_reach_the_view_when_they_change() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        let mut session=SessionState { session_id:"s".into(),agent_generation_id:"g".into(),lifecycle:"open".into(),title:None,summary:None,end_reason:None,deleted:false,activity:"idle".into(),turn_id:None,question_ids:vec![],background:None,capabilities:BTreeMap::new(),controls:None,authentication:None,usage:None,agent_title:None,commands:vec![], active_ms:0, active_since_ms:None };
        let quiet=view.accept(notification("1",Event::SessionUpdated {session:session.clone()})).unwrap();
        assert!(!quiet.iter().any(|e|matches!(e,wire::Event::Commands(_))));
        session.commands=vec![AgentCommand {name:"review".into(),description:"Review".into(),hint:Some("what".into())}];
        let offered=view.accept(notification("2",Event::SessionUpdated {session:session.clone()})).unwrap();
        assert!(offered.iter().any(|e|matches!(e,wire::Event::Commands(c) if c[0].name=="review" && c[0].hint.as_deref()==Some("what"))));
        assert!(!view.accept(notification("3",Event::SessionUpdated {session})).unwrap().iter().any(|e|matches!(e,wire::Event::Commands(_))));
    }
    #[test]
    fn missing_patch_target_requires_resynchronization() {
        let mut view=View {session_id:"s".into(),subscription_id:"sub".into(),..Default::default()};
        assert!(view.accept(notification("1",Event::ItemPatched {item_id:"missing".into(),set:ItemPatch::default()})).is_err());
        assert_eq!(view.seq,0);
    }
}
