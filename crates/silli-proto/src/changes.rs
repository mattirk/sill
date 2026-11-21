//! Public Changes records. Operational paths are exact daemon-host paths, never overlay internals.
use crate::sessions::HostPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<String>,
    /// Sessions whose writes could not be listed, with why (a launch recorded by an older Silli,
    /// say); on the first page only. The rest of the listing is whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<Unreadable>,
}
/// A session left out of a Changes listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Unreadable {
    pub session_id: String,
    pub title: String,
    pub why: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Live {
        repo: HostPath,
        path: HostPath,
        status: String,
    },
    Overlay {
        session_id: String,
        title: String,
        ghost: bool,
        agent_state: bool,
        path: HostPath,
        kind: String,
        file_type: String,
        size: String,
        conflict: bool,
        written: Option<String>,
        repo: Option<HostPath>,
        hash: Option<String>,
        protected: Option<String>,
    },
    Kept {
        session_id: String,
        title: String,
        ghost: bool,
        keep_id: String,
        timestamp: String,
        path: HostPath,
        what: String,
        before: String,
        hash: Option<String>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Change { session_id: String, path: HostPath },
    Live { repo: HostPath, path: HostPath },
    Kept { session_id: String, keep_id: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    pub target: Target,
    pub offset: Option<String>,
    pub fingerprint: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiffPage {
    pub text: String,
    pub next_offset: Option<String>,
    pub fingerprint: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeepItem {
    pub path: HostPath,
    pub expected_hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    pub session_id: String,
    pub items: Vec<KeepItem>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Revert {
    pub session_id: String,
    pub paths: Vec<HostPath>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Undo {
    pub session_id: String,
    pub keep_ids: Vec<String>,
    pub force: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRevert {
    pub repo: HostPath,
    pub path: HostPath,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Outcome {
    pub path: HostPath,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Done {
    pub done: Vec<Outcome>,
    pub failed: Vec<Outcome>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub before: Option<String>,
    pub since: Option<String>,
    pub path: Option<HostPath>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub session_id: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryPage {
    pub events: Vec<HistoryEvent>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEvent {
    pub event_id: String,
    pub timestamp: String,
    pub kind: String,
    pub session_id: Option<String>,
    pub session_title: String,
    pub agent: String,
    pub ghost: bool,
    pub path: Option<HostPath>,
    pub what: String,
    pub hash_new: Option<String>,
    pub hash_old: Option<String>,
    pub count: String,
    pub paths: Vec<HostPath>,
    pub keep_id: Option<String>,
    pub head_start: Option<String>,
    pub head_end: Option<String>,
    pub commits: Option<String>,
    pub files: Option<String>,
}
// Between the daemon's own paths and the wire's: the daemon's side only.
#[cfg(unix)]
impl From<crate::wire::Done> for Done {
    fn from(d: crate::wire::Done) -> Self {
        let convert = |v: Vec<(std::path::PathBuf, String)>| v.into_iter().map(|(p, message)| Outcome { path: HostPath::from_path(&p), message }).collect();
        Self { done: convert(d.done), failed: convert(d.failed) }
    }
}
#[cfg(unix)]
impl TryFrom<Done> for crate::wire::Done {
    type Error = anyhow::Error;
    fn try_from(d: Done) -> anyhow::Result<Self> {
        let convert = |v: Vec<Outcome>| v.into_iter().map(|o| Ok((o.path.to_path()?, o.message))).collect::<anyhow::Result<_>>();
        Ok(Self { done: convert(d.done)?, failed: convert(d.failed)? })
    }
}
