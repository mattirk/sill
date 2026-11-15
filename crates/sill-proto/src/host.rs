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
    /// Durable retry identity, scoped to this daemon store.
    pub operation_id: Option<String>,
    pub agent_id: String,
    pub method_id: String,
}
/// Find an installed agent's directories (`_sill/agents.discovered`), as proposals for its
/// `home_paths`. Without `method_id`: one first start in a throwaway jail with an empty home.
/// With it: that login method run on the daemon host with an empty home, as host logins are;
/// what it wrote is proposed and copied into the agent's sill-owned home. Declares nothing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDiscover {
    /// Durable retry identity, scoped to this daemon store.
    pub operation_id: Option<String>,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentReset {
    pub agent_id: String,
}
/// A reset: the agent's running sessions stopped first (a running agent holds its login and writes
/// to its home), then its sill-owned home, its ghost sessions' copies and its saved API keys
/// removed. The install and its definition stay: it starts as a fresh install does.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentResetDone {
    /// Files removed.
    pub removed: u32,
    /// Sessions stopped; their conversations stay and start again when used.
    pub stopped: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryInstall {
    /// Durable retry identity, scoped to this daemon store.
    pub operation_id: Option<String>,
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
    pub description: String,
    pub repository: Option<String>,
    pub website: Option<String>,
    /// Installation facts from the daemon's platform; frontends never derive these locally.
    pub install_kind: Option<String>,
    pub install_spec: String,
    pub package_name: Option<String>,
    pub has_icon: bool,
    pub builtin_agent_id: Option<String>,
    /// Known to work inside sill's jail end to end (install, login, a prompt). The daemon's list
    /// is the only one; a frontend shows the other entries on request.
    #[serde(default)]
    pub tested: bool,
    pub newer: Option<Box<RegistryEntry>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RegistryPage {
    pub entries: Vec<RegistryEntry>,
    pub next: Option<String>,
    /// Decimal Unix seconds when the live index was fetched, or null if never fetched.
    pub fetched_at: Option<String>,
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
    /// A login page for the frontend to open in the person's browser (capability
    /// `sill.login_browser`); the daemon opens none. `callback_port`: the daemon host's loopback
    /// port the page redirects to, which a frontend on another machine relays with
    /// `_sill/streams.login_callback`.
    OpenUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_port: Option<u16>,
    },
}

/// The capability of browser-login handling: `open_url` progress, the callback relay and cancel.
pub const LOGIN_BROWSER: &str = "sill.login_browser";
/// Failure code of a cancelled login.
pub const CANCELLED: &str = "cancelled";

/// A durable receipt survives stream loss and daemon restart. Unknown never authorizes replay.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Operation {
    pub operation_id: String,
    pub method: String,
    pub state: OperationState,
    pub agent_id: Option<String>,
    pub code: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationState { Running, Completed, Failed, Unknown }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationKey { pub operation_id: String }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStream {
    /// Present for a newly started job only. Repeated requests recover the receipt, not output.
    pub token: Option<String>,
    pub operation: Operation,
}
