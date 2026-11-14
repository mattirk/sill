//! Host control (docs/permission-modes.md, Host control): an explicit, per-session activation
//! of named host operations outside the jail. Never a saved choice: the daemon holds an
//! activation in memory only, for as long as a displaying frontend is connected.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The frontend-declared capability of a program that shows Host control: the permanent
/// session caption and the window-level line naming every session with active control. Only
/// such a connection may activate or revoke, and while it is attached to a session it counts as
/// displaying that session's control.
pub const CAPABILITY: &str = "sill.host_control";

/// The separately grantable operations, in the specification's order.
pub const OPERATIONS: [&str; 7] = ["view", "input", "inspect", "launch", "stop", "stop_existing", "commands"];

/// What an operation name means, for frontends and the CLI.
pub fn describe(operation: &str) -> &'static str {
    match operation {
        "view" => "Display viewing: pictures of the real desktop or one of its windows",
        "input" => "Display input: mouse and keyboard on the real desktop",
        "inspect" => "Process inspection: list host processes and their status",
        "launch" => "Process launch: start a program on the host with explicit arguments",
        "stop" => "Process stop: stop processes the bridge launched",
        "stop_existing" => "Stop existing processes: stop host processes the bridge did not launch",
        "commands" => "General host commands: unrestricted execution as the host user",
        _ => "unknown operation",
    }
}

/// Activate Host control for a session with exactly these operations, or change an active
/// one's scope to them. Each call that takes effect is a new generation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Activate {
    pub session_id: String,
    pub operations: Vec<String>,
}

/// A session's Host control state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    pub session_id: String,
    pub active: bool,
    /// Counts every activation and scope change of this session since the daemon started; 0
    /// before the first. Actions carry the generation they were made under; a stale one is refused.
    pub generation: u64,
    pub operations: Vec<String>,
    /// Connections displaying this session's control; the activation ends when the last goes.
    pub displaying: u32,
    /// Unix seconds, decimal, of the current activation.
    pub since: Option<String>,
    /// Why and when the last activation ended, until the next one.
    pub ended: Option<Ended>,
    /// The base policy's ban, when it forbids Host control: no exception can lift it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden: Option<String>,
    /// Repository restrictions host operations can bypass, from the fragments of the session's
    /// pinned repositories as they are now. An activation whose operations meet one needs a
    /// recorded exception first ([`RecordException`]); the jail keeps enforcing them.
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    /// Why the restrictions could not be read (a fragment that does not parse): activation is
    /// refused until they can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicts_error: Option<String>,
    /// The repository exception: recorded for the next activation (no generation yet), or the one
    /// the active generation runs under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<Exception>,
}

/// One repository restriction host operations can bypass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Conflict {
    /// The fragment file.
    pub source: crate::sessions::HostPath,
    /// Its content's SHA-256: an exception names the content the person read.
    pub sha256: String,
    /// host_control_forbidden, offline, public_web_forbidden, read_only_paths, masked_paths,
    /// resource_limits or port_exposure.
    pub restriction: String,
    /// The rule as the fragment states it.
    pub detail: String,
    /// What host code can do despite it.
    pub consequence: String,
    /// The operations that bypass it.
    pub operations: Vec<String>,
}

impl Conflict {
    /// Whether an activation with `operations` meets this restriction.
    pub fn applies(&self, operations: &[String]) -> bool {
        self.operations.iter().any(|op| operations.contains(op))
    }
    pub fn key(&self) -> Acknowledged {
        Acknowledged { source: self.source.clone(), sha256: self.sha256.clone(), restriction: self.restriction.clone() }
    }
}

/// A repository exception: the person's acknowledgement that host operations may bypass these
/// restrictions, for one activation only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Exception {
    /// The operations it was recorded for; the activation's are a subset.
    pub operations: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// Unix seconds, decimal.
    pub at: String,
    /// The activation generation it was consumed by; none while it waits for the activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// A restriction as the person acknowledges it: the fragment, its content, the rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Acknowledged {
    pub source: crate::sessions::HostPath,
    pub sha256: String,
    pub restriction: String,
}

/// Record a repository exception for the session's next activation with (a subset of) these
/// operations. `acknowledged` must name exactly the restrictions they meet now, as `state` lists
/// them: a changed fragment needs a fresh look. It activates nothing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordException {
    pub session_id: String,
    pub operations: Vec<String>,
    pub acknowledged: Vec<Acknowledged>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ended {
    pub reason: String,
    pub at: String,
}

/// Active Host control on a session record, for a window-level indication of every such session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Brief {
    pub generation: u64,
    pub operations: Vec<String>,
    /// The activation runs under a repository exception.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exception: bool,
}

/// Keep (or stop keeping) a bridge-launched host process running after Host control ends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    pub session_id: String,
    /// The process's name in the process list (`host:<id>`).
    pub name: String,
    pub keep: bool,
}
