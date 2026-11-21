//! Host control (docs/permission-modes.md, Host control): an explicit, per-session activation
//! of named host operations outside the jail. Never a saved choice: the daemon holds an
//! activation in memory only, for as long as a displaying frontend is connected.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The frontend-declared capability of a program that shows Host control: the permanent
/// session caption and the window-level line naming every session with active control. Only
/// such a connection may activate or revoke, and while it is attached to a session it counts as
/// displaying that session's control.
pub const CAPABILITY: &str = "silli.host_control";

/// The separately grantable operations, in the specification's order.
pub const OPERATIONS: [&str; 10] = ["view", "input", "mac_view", "mac_input", "mac_commands", "inspect", "launch", "stop", "stop_existing", "commands"];

/// Host control operations available in an Always Apply session.
pub fn available_for_review(operation: &str) -> bool {
    matches!(operation, "inspect" | "view" | "mac_view")
}

/// The operations on the Mac the daemon's VM runs on (docs/silli-mac.md), served by Silli.app;
/// the others act on the daemon's own host.
pub const MAC_OPERATIONS: [&str; 3] = ["mac_view", "mac_input", "mac_commands"];

/// What an operation name means, for frontends and the CLI.
pub fn describe(operation: &str) -> &'static str {
    match operation {
        "view" => "Display viewing: pictures of the real desktop or one of its windows",
        "input" => "Display input: mouse and keyboard on the real desktop",
        "mac_view" => "Mac screen viewing: the Mac's displays and windows, and pictures of them",
        "mac_input" => "Mac control: mouse and keyboard on the whole Mac desktop, aimed in a recent picture",
        "mac_commands" => "Mac commands: unrestricted programs run as the person on the Mac, from a shared Mac folder",
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
    /// The session policy's ban, when it forbids Host control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden: Option<String>,
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
