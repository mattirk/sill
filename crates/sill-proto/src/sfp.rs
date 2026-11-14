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
    pub turn_id: Option<String>,
    pub question_ids: Vec<String>,
    /// None means background activity is unknown, not empty.
    pub background: Option<Vec<String>>,
    pub capabilities: BTreeMap<String, bool>,
    pub controls: Option<SessionControls>,
    pub authentication: Option<Authentication>,
    pub usage: Option<Usage>,
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
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuthTerminal { pub args: Vec<String>, pub env: BTreeMap<String, String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Usage { pub used: String, pub size: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Content {
    pub block_id: String,
    /// text, thought, or a future display type. Preserve unknown display data.
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
    /// user, agent, thought, tool, note.
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
    pub tool_item_id: Option<String>,
    pub options: Vec<PermissionOption>,
    pub fields: Vec<FormField>,
    pub state: String,
    pub answer_id: Option<String>,
    pub answer: Option<Answer>,
    pub delivery: Option<String>,
    pub reason: Option<Reason>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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
pub struct SnapshotPage { pub at: Cursor, pub records: Vec<Record>, pub next_page: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Detach { pub session_id: String, pub subscription_id: String }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag="type", rename_all="snake_case")]
pub enum ConfigValue { Id { value:String }, Bool { value:bool } }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag="type", rename_all="snake_case")]
pub enum Control {
    Authenticate { method_id:String }, RetrySession,
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
