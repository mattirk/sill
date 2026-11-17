//! Public memory access. All pages read live state; refresh traversal after concurrent edits.
use crate::method;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub session_id: String,
    pub agent: String,
    pub title: String,
    pub ended_at: String,
    pub summary: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    pub sessions: Vec<Session>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextRead {
    pub snapshot_id: Option<String>,
    pub session_id: String,
    pub format: Format,
    #[serde(default)]
    pub offset: Option<String>,
    pub length: Option<u32>,
    pub grep: Option<String>,
    pub fingerprint: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Show,
    Transcript,
    Export,
    Recall,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TextPage {
    pub snapshot_id: String,
    pub text: String,
    pub next: Option<String>,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub query: String,
    pub after: Option<u16>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    pub session_id: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hits {
    pub hits: Vec<Hit>,
    pub next: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotesList {
    pub session_id: String,
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    pub cursor: String,
    pub timestamp: String,
    pub kind: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Notes {
    pub notes: Vec<Note>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddNote {
    pub session_id: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveNote {
    pub session_id: String,
    pub timestamp: String,
    pub text: String,
}
impl crate::Client {
    pub fn memory_text(&self, session_id: String, format: Format, grep: Option<String>) -> anyhow::Result<String> {
        let mut text = String::new();
        let mut offset = None;
        let mut fingerprint = None;
        let mut snapshot_id = None;
        loop {
            let page: TextPage = self.call(
                method::MEMORY_TEXT,
                TextRead {
                    snapshot_id: snapshot_id.clone(),
                    session_id: session_id.clone(),
                    format: format.clone(),
                    offset,
                    length: None,
                    grep: grep.clone(),
                    fingerprint,
                },
            )?;
            snapshot_id = Some(page.snapshot_id.clone());
            text.push_str(&page.text);
            offset = page.next;
            fingerprint = Some(page.fingerprint);
            if offset.is_none() {
                let _: anyhow::Result<crate::Empty> = self.call(method::MEMORY_TEXT_CLOSE, TextClose { snapshot_id: page.snapshot_id });
                return Ok(text);
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Compacted {
    pub compacted: String,
    pub trimmed: String,
    pub deleted: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Forgotten {
    pub deleted: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextClose {
    pub snapshot_id: String,
}
