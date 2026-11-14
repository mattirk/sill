//! Pins identify exact absolute paths on the daemon host. Lists are live keyset pages.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::sessions::HostPath;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    /// Omit to list global pins; a session ID lists only that session's own pins.
    pub session_id: Option<String>,
    pub after: Option<HostPath>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page { pub pins: Vec<HostPath>, pub next: Option<HostPath> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobalPin { pub path: HostPath }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionPin { pub session_id: String, pub path: HostPath }
