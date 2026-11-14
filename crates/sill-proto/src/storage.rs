//! Daemon-host filesystem space and explicitly disposable session build caches.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiskSpace {
    pub total: u64,
    pub used: u64,
    /// Bytes available to this user (excludes reserved filesystem blocks).
    pub available: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Space {
    pub home: DiskSpace,
    pub state: DiskSpace,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildCache {
    pub session_id: String,
    pub title: String,
    pub bytes: u64,
    pub active: bool,
    /// Exact daemon-host path, for review; never accepted as deletion input.
    pub path: crate::sessions::HostPath,
    pub problem: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Caches {
    pub caches: Vec<BuildCache>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClearBuildCache {
    pub session_id: String,
}
