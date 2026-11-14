//! Public configured-agent administration. Commands refer to the selected daemon host.
use std::collections::BTreeMap;
use serde::{Serialize, Deserialize};
use schemars::JsonSchema;
use crate::sessions::HostPath;
/// An agent definition: a result of `agents.get`/`agents.list` and the params of `agents.save`.
/// Open in the schema because it is a result too; the daemon still refuses unknown fields in
/// `agents.save` (requests are strict).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Definition {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub supports_load: bool,
    /// Explicit opt-in to the experimental ACP v2 adapter; ACP v1 remains the default.
    #[serde(default)]
    pub experimental_acp_v2: bool,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub sandbox_off_env: BTreeMap<String, String>,
    #[serde(default)]
    pub home_paths: Option<Vec<HomePath>>,
    #[serde(default)]
    pub login_files: Vec<String>,
    #[serde(default)]
    pub host_login: Vec<String>,
    #[serde(default)]
    pub home_env: Option<String>,
    #[serde(default)]
    pub home_env_parent: bool,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub memory_paths: Vec<String>,
}
/// Exact persistent locations; absence of home_paths means setup is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HomePath {
    pub path: String,
    pub kind: HomePathKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_content: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HomePathKind { Directory, File }
impl HomePath {
    pub fn directory(path: &str) -> Self {
        Self { path: path.into(), kind: HomePathKind::Directory, initial_content: None }
    }
    pub fn file(path: &str, initial: &str) -> Self {
        Self { path: path.into(), kind: HomePathKind::File, initial_content: Some(initial.into()) }
    }
}
fn yes()->bool{true}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentKey { pub agent_id:String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List { pub after:Option<String>, pub limit:Option<u16> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page { pub agents:Vec<Definition>, pub next:Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Saved { pub path:HostPath }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Removed { pub removed:bool }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Available { pub path:Option<HostPath> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Defaults { pub agent_id:String, pub settings:crate::wire::AgentSettings }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Installed { pub version:String, pub kind:String, pub command:Vec<String>, pub env:BTreeMap<String,String>, pub source:String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Installation { pub installed:Option<Installed> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryKey { pub entry_id:String, pub version:String, pub source:String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Icon { pub bytes_base64:Option<String> }
