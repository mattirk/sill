//! File search and repository inspection on the daemon host; paths are exact absolute bytes.
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::sessions::HostPath;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathKey { pub path: HostPath }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Root { pub path: Option<HostPath> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Head { pub label: String, pub detached: bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeadResult { pub head: Option<Head> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Find { pub bases: Vec<HostPath>, pub query: String, pub limit: Option<u16>, pub visits: Option<u32> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Found { pub paths: Vec<HostPath>, pub truncated: bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Status { pub repo: HostPath, pub after: Option<HostPath>, pub limit: Option<u16> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusEntry { pub path: HostPath, pub status: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusPage { pub entries: Vec<StatusEntry>, pub next: Option<HostPath> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangedLines { pub repo: HostPath, pub path: HostPath }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LineRange { pub start: String, pub end: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Lines { pub ranges: Option<Vec<LineRange>> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Summary { pub insertions: String, pub deletions: String, pub untracked: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SummaryResult { pub summary: Option<Summary> }
