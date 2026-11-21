//! Public terminals: PTYs the daemon owns, which outlive the frontend showing them. A frontend
//! that disconnects attaches again to the same terminal and receives its recent output first.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::sessions::HostPath;

/// Recent output a terminal keeps for the next attach, in bytes.
pub const REPLAY_BYTES: usize = 256 * 1024;
/// Terminals one daemon holds at once, running or exited.
pub const MAX_TERMINALS: usize = 64;
/// Attached streams per terminal.
pub const MAX_VIEWERS: usize = 8;
/// An exited terminal stays readable this long, then is forgotten.
pub const KEEP_EXITED_SECONDS: u64 = 600;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Open {
    /// The session whose jail runs the terminal; none runs it on the daemon's host.
    pub session_id: Option<String>,
    /// The frontend's own name for the terminal, so it can find it again (at most 128 bytes).
    #[serde(default)]
    pub label: String,
    /// Empty runs the login shell.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// In the jail's namespace for a session terminal, on the daemon host otherwise.
    pub cwd: Option<HostPath>,
    pub cols: u16,
    pub rows: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Terminal {
    pub terminal_id: String,
    pub session_id: Option<String>,
    pub label: String,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    /// `running` or `exited`.
    pub state: String,
    /// The exit code where the daemon could see it (host terminals); none after a signal or in a jail.
    pub exit_code: Option<i32>,
    /// Unix seconds, decimal string.
    pub created_at: String,
    /// Output bytes no longer kept for replay, decimal string. Nonzero means an attach starts
    /// partway: earlier output is gone, not lost in transit.
    pub discarded_bytes: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalKey {
    pub terminal_id: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    /// Only this session's terminals; none lists every terminal.
    pub session_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Terminals {
    pub terminals: Vec<Terminal>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resize {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}
/// A one-use byte-stream token (docs/protocol.md, byte streams).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Stream {
    pub token: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Cwd {
    /// Host terminals only: a jail's working directory is not visible from the host.
    pub cwd: Option<HostPath>,
}
