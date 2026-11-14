//! Public host streams. Paths and programs are resolved on the selected daemon box.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_JOBS: usize = 4;
pub const MAX_PROGRESS_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortOpen {
    pub session_id: String,
    pub port: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentLogin {
    pub agent_id: String,
    pub method_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentLogout {
    pub agent_id: String,
}
/// A log out: the agent's running sessions stopped first (a running agent holds its login and may
/// write it back), then its login files removed from its sill home and its ghost sessions' copies.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoggedOut {
    /// Login files removed.
    pub removed: u32,
    /// Sessions stopped; their conversations stay and start again when used.
    pub stopped: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryInstall {
    pub entry_id: String,
    pub version: String,
    /// `snapshot` or `live`. Unknown sources are refused.
    pub source: String,
    /// Required for an entry from the unreviewed live index.
    #[serde(default)]
    pub allow_unreviewed: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryList {
    /// Exclusive entry ID boundary; pages are live, not a frozen snapshot.
    pub after: Option<String>,
    /// 1–128 entries, default 64. Each entry has at most one newer version.
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RegistryEntry {
    pub entry_id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installable: bool,
    pub newer: Option<Box<RegistryEntry>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RegistryPage {
    pub entries: Vec<RegistryEntry>,
    pub next: Option<String>,
}
/// One-use token: claim on a new transport within 60 seconds. No control handshake on that
/// transport. Port bytes are raw; progress streams are newline-delimited Progress objects.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HostStream {
    pub token: String,
}
/// Login/install progress, at most 16 KiB of UTF-8 per text/message field. Exactly one terminal
/// event (`completed` or `failed`) establishes the outcome; EOF alone leaves it unknown.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Progress {
    Status { text: String },
    Stderr { text: String },
    Output { text: String },
    Completed { agent_id: String },
    Failed { code: String, message: String },
}
