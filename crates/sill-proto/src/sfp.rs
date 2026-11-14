//! SFP conversation records. These are independent of ACP and of the stored transcript format.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cursor { pub session_id: String, pub seq: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionState {
    pub session_id: String,
    pub agent_generation_id: String,
    pub lifecycle: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub end_reason: Option<String>,
    pub deleted: bool,
    /// idle, working, waiting, unknown. Unknown must never be interpreted as idle.
    pub activity: String,
    /// Completed working intervals, in milliseconds. Waiting and idle time are excluded.
    #[serde(default)]
    pub active_ms: u64,
    /// Unix milliseconds at the start of the current working interval.
    #[serde(default)]
    pub active_since_ms: Option<u64>,
    pub turn_id: Option<String>,
    pub question_ids: Vec<String>,
    /// None means background activity is unknown, not empty.
    pub background: Option<Vec<String>>,
    pub capabilities: BTreeMap<String, bool>,
    pub controls: Option<SessionControls>,
    pub authentication: Option<Authentication>,
    pub usage: Option<Usage>,
    /// The name the agent gave the session, if any (sill's own title is `title`).
    #[serde(default)]
    pub agent_title: Option<String>,
    /// Slash commands the agent offers now; empty when it offers none or never said.
    #[serde(default)]
    pub commands: Vec<AgentCommand>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCommand {
    pub name: String,
    pub description: String,
    /// What to type after the command, when it takes free text.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ControlChoice { pub value: String, pub title: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlValue { Id { value: String }, Boolean { value: bool } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ControlOption {
    pub option_id: String, pub title: String, pub category: String,
    pub value: ControlValue, pub choices: Vec<ControlChoice>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionControls {
    pub current_mode: Option<String>, pub modes: Vec<ControlChoice>, pub options: Vec<ControlOption>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Authentication { pub message: String, pub methods: Vec<AuthMethod> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuthMethod {
    pub method_id: String, pub title: String, pub description: Option<String>, pub terminal: Option<AuthTerminal>,
    /// The method takes an API key the person types: the provider the agent names (`google`,
    /// `openai`), empty when it names none. Send it with `authenticate`'s `api_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuthTerminal { pub args: Vec<String>, pub env: BTreeMap<String, String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage { pub used: String, pub size: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Content {
    pub block_id: String,
    /// text, thought, plan_entry, image (`mime` + `upload_id` or `omitted`), message_metadata,
    /// extension, or a future display type. Show unknown types by `text` or skip them; never fail.
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self { block_id: "text".into(), kind: "text".into(), text: Some(text.into()), extra: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tool {
    pub title: String,
    pub kind: String,
    pub status: String,
    pub detail: String,
    pub diff: String,
    pub parent_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    pub item_id: String,
    pub created_seq: String,
    pub turn_id: Option<String>,
    pub submission_id: Option<String>,
    pub agent_generation_id: String,
    /// user, agent, thought, tool, note, notice, plan. A plan's content is one `plan_entry`
    /// block per step (`status`, `priority`); a notice's `severity` is on its text block.
    pub kind: String,
    pub content: Vec<Content>,
    pub tool: Option<Tool>,
}

/// Mutable fields replace the whole named field; omitted fields remain unchanged.
/// Null is invalid here; a tool cannot be cleared or turned into another item kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ItemPatch {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    #[schemars(with = "Vec<Content>")]
    pub content: Option<Vec<Content>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    #[schemars(with = "Tool")]
    pub tool: Option<Tool>,
}
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error> where D: serde::Deserializer<'de>, T: Deserialize<'de> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Turn {
    pub turn_id: String,
    pub agent_generation_id: String,
    pub activity: String,
    pub outcome: String,
    pub reason: Option<Reason>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Reason { pub code: String, pub message: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Attachment {
    /// Immutable inline data; the daemon validates decoded length before acceptance.
    Inline { name: String, mime: String, data_base64: String },
    /// Immutable uploaded object scoped to this session.
    Uploaded { upload_id: String, name: String, mime: String },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub session_id: String,
    pub submission_id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Submission {
    pub submission_id: String,
    pub session_id: String,
    pub accepted_seq: String,
    pub agent_generation_id: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub acceptance: String,
    pub delivery: String,
    pub held: bool,
    pub hold_reason: Option<Reason>,
    pub insertion: String,
    pub outcome: String,
    pub reason: Option<Reason>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubmissionKey { pub session_id: String, pub submission_id: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionOption { pub option_id: String, pub title: String, pub kind: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Choice { pub value: String, pub title: String, pub description: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FormField {
    pub key: String,
    pub title: String,
    pub description: String,
    /// choice, choices, text, boolean, number. Unknown types cannot be answered.
    pub kind: String,
    pub choices: Vec<Choice>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub question_id: String,
    pub agent_generation_id: String,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    /// Display-only permission subject. Unknown fields never imply an authorization decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Content>,
    pub tool_item_id: Option<String>,
    pub options: Vec<PermissionOption>,
    pub fields: Vec<FormField>,
    pub state: String,
    pub answer_id: Option<String>,
    pub answer: Option<Answer>,
    pub delivery: Option<String>,
    pub reason: Option<Reason>,
}
/// Open in the schema because questions carry it in results and events; the daemon still refuses
/// unknown fields in `questions.answer` (requests are strict).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Permission { option_id: String },
    Form { values: BTreeMap<String, serde_json::Value> },
    Cancel,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerQuestion {
    pub session_id: String,
    pub question_id: String,
    pub answer_id: String,
    pub answer: Answer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "item.created")] ItemCreated { item: Item },
    #[serde(rename = "item.patched")] ItemPatched { item_id: String, set: ItemPatch },
    #[serde(rename = "item.text_appended")] ItemTextAppended { item_id: String, block_id: String, text: String },
    #[serde(rename = "item.removed")] ItemRemoved { item_id: String, reason: Reason },
    #[serde(rename = "session.updated")] SessionUpdated { session: SessionState },
    #[serde(rename = "turn.updated")] TurnUpdated { turn: Turn },
    #[serde(rename = "submission.updated")] SubmissionUpdated { submission: Submission },
    #[serde(rename = "question.updated")] QuestionUpdated { question: Question },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Session { session: SessionState },
    Item { item: Item },
    Turn { turn: Turn },
    Submission { submission: Submission },
    Question { question: Question },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StoredEvent { pub seq: String, pub event: Event }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionEvent {
    pub session_id: String,
    pub subscription_id: String,
    pub seq: String,
    pub event: Event,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Attach { pub session_id: String, #[serde(default)] pub since: Option<Cursor> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Attached {
    Replay { subscription_id: String, after: Cursor, through: Cursor },
    Snapshot { subscription_id: String, snapshot: SnapshotLease },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotLease { pub token: String, pub at: Cursor, pub expires_at: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotRequest { pub token: String, #[serde(default)] pub page: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotPage {
    pub at: Cursor, pub records: Vec<Record>, pub next_page: Option<String>,
    /// A large record's JSON is delivered in consecutive UTF-8 fragments. No ordinary
    /// records are interleaved. Concatenate through `last` before decoding the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment: Option<RecordFragment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordFragment { pub record: String, pub offset: String, pub json: String, pub last: bool }

#[derive(Default)]
pub struct SnapshotDecoder { pending: Option<(String, String)> }
impl SnapshotDecoder {
    pub fn records(&mut self, page: &SnapshotPage) -> Result<Vec<Record>, String> {
        if let Some(f) = &page.fragment {
            if !page.records.is_empty() { return Err("fragment interleaved with records".into()); }
            let (id, data) = self.pending.get_or_insert_with(|| (f.record.clone(), String::new()));
            if id != &f.record || f.offset != data.len().to_string() { return Err("noncontiguous record fragment".into()); }
            if data.len().saturating_add(f.json.len()) > 64 * 1024 * 1024 { return Err("record exceeds 64 MiB".into()); }
            data.push_str(&f.json);
            if f.last {
                let record = serde_json::from_str(data).map_err(|e| format!("invalid fragmented record: {e}"))?;
                self.pending = None;
                return Ok(vec![record]);
            }
            if page.next_page.is_none() { return Err("incomplete fragmented record".into()); }
            Ok(vec![])
        } else {
            if self.pending.is_some() { return Err("incomplete fragmented record".into()); }
            Ok(page.records.clone())
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Detach { pub session_id: String, pub subscription_id: String }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag="type", rename_all="snake_case")]
pub enum ConfigValue { Id { value:String }, Bool { value:bool } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag="type", rename_all="snake_case")]
pub enum Control {
    /// `api_key`: for a method marked `api_key`. The daemon keeps it where the agent's jail cannot
    /// read it and answers the agent's next login request with it; it is never journaled.
    Authenticate { method_id:String, #[serde(default, skip_serializing_if = "Option::is_none")] api_key:Option<crate::wire::Secret> }, RetrySession,
    SetMode { mode_id:String }, SetConfig { option_id:String, value:ConfigValue },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionControl { pub session_id:String, pub control:Control }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CancelTurn { pub session_id:String, pub turn_id:String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadCreate {
    pub session_id: String,
    pub upload_id: String,
    pub name: String,
    pub mime: String,
    pub data_base64: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UploadCreated { pub upload_id: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadGet { pub session_id: String, pub upload_id: String }
/// An upload's immutable bytes. Submissions reference attachments by upload ID, so snapshots
/// never carry attachment data; a frontend fetches what it displays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Upload { pub upload_id: String, pub name: String, pub mime: String, pub data_base64: String }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_schema_and_decoder_agree_on_omission_and_null() {
        let schema=serde_json::to_value(schemars::schema_for!(ItemPatch)).unwrap();
        assert_eq!(schema["properties"]["content"]["type"],"array");
        assert!(schema.get("required").is_none());
        assert_eq!(serde_json::from_str::<ItemPatch>("{}").unwrap(),ItemPatch::default());
        assert!(serde_json::from_str::<ItemPatch>(r#"{"content":null}"#).is_err());
        assert!(serde_json::from_str::<ItemPatch>(r#"{"tool":null}"#).is_err());
    }
}

#[cfg(test)]
mod snapshot_decoder_tests {
    use super::*;
    #[test]
    fn fragments_reject_gaps_interleaving_and_incomplete_final_pages() {
        let mut page=SnapshotPage{expires_at:None,at:Cursor{session_id:"s".into(),seq:"0".into()},records:vec![],next_page:Some("1".into()),fragment:Some(RecordFragment{record:"0".into(),offset:"0".into(),json:"🦀".into(),last:false})};
        let mut decoder=SnapshotDecoder::default();assert!(decoder.records(&page).unwrap().is_empty());
        page.fragment.as_mut().unwrap().offset="1".into();assert!(decoder.records(&page).is_err());
        page.fragment.as_mut().unwrap().offset="4".into();page.next_page=None;assert!(decoder.records(&page).is_err());
        page.fragment=None;assert!(decoder.records(&page).is_err());
    }
}

/// Cancellation acknowledges a request, never proof that the agent stopped.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CancellationRequested { pub requested: bool }
