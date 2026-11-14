//! The daemon protocol (docs/daemon.md §3.3): what a frontend and `sill daemon` say to each other.
//! JSON-RPC 2.0, one message per line, over the unix socket at [`socket_path`]; sill's methods are
//! ACP extension methods — `_sill/<group>.<name>` — so the same connection can carry ACP's own
//! `session/*` traffic once the conversation moves into the daemon (M1). This crate knows nothing
//! of jails or GTK: a frontend links it and nothing else of sill.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub mod client;
pub mod rpc;

pub use client::Client;

/// Bumped when a method or a type changes shape; the daemon reports its own in [`DaemonStatus`].
pub const PROTOCOL: u32 = 3;

/// Method names. Groups follow the table in docs/daemon.md §3.3.
pub mod method {
    pub const DAEMON_STATUS: &str = "_sill/daemon.status";
    pub const DAEMON_SHUTDOWN: &str = "_sill/daemon.shutdown";
    pub const SESSIONS_LIST: &str = "_sill/sessions.list";
    pub const SESSIONS_GET: &str = "_sill/sessions.get";
    /// Start a stored session's jail (or find it running): `Key` → `Started`.
    pub const SESSIONS_START: &str = "_sill/sessions.start";
    /// Stop a session's jail: `Key` → `{}`.
    pub const SESSIONS_STOP: &str = "_sill/sessions.stop";
    /// The tail of a session's log: `LogRequest` → `Text`.
    pub const SESSIONS_LOG: &str = "_sill/sessions.log";
    /// Start the session's jail and agent if needed and subscribe this connection to its
    /// conversation: `Attach` → `{}`; `SESSION_EVENT` notifications follow, the session's current
    /// state first (docs/daemon.md §3.3).
    pub const SESSIONS_ATTACH: &str = "_sill/sessions.attach";
    /// Unsubscribe: `Key` → `{}`. The agent stays the daemon's.
    pub const SESSIONS_DETACH: &str = "_sill/sessions.detach";
    /// A command to a running session (prompt, cancel, mode, option, login): `SessionCommand` → `{}`.
    pub const SESSIONS_COMMAND: &str = "_sill/sessions.command";
    /// The answer to an agent's question (permission, form): `SessionAnswer` → `{}`.
    pub const SESSIONS_ANSWER: &str = "_sill/sessions.answer";
    /// Notification, daemon → attached frontends: `SessionEvent`.
    pub const SESSION_EVENT: &str = "_sill/session.event";
    /// End a session as closing its tab would (docs/memory.md §2.2): the summary turn when the
    /// agent can be asked, the row written or deleted, the jail stopped. Params [`End`].
    pub const SESSIONS_END: &str = "_sill/sessions.end";
    /// How many unkept writes a session has outside its pins (not counting build output); answers
    /// [`Count`]. The shelf asks before a ghost's tab closes (docs/memory.md §3.3).
    pub const SESSIONS_CHANGES: &str = "_sill/sessions.changes";
    /// *Summarize now*: the summary turn without the end; the answer comes as a `Summarized` event.
    pub const SESSIONS_CHECKPOINT: &str = "_sill/sessions.checkpoint";
    /// The writes that were the shelf's (M1c): `sill.db` has one writer, the daemon.
    pub const SESSIONS_NEW: &str = "_sill/sessions.new";
    /// One field of a session's row; params [`SessionSet`] with a `sill_core::wire::SessionSet`.
    pub const SESSIONS_SET: &str = "_sill/sessions.set";
    /// Stop the jail, remove the session's state and its row.
    pub const SESSIONS_DELETE: &str = "_sill/sessions.delete";
    pub const SESSIONS_PIN: &str = "_sill/sessions.pin";
    pub const SESSIONS_UNPIN: &str = "_sill/sessions.unpin";
    pub const MEMORY_NOTE_ADD: &str = "_sill/memory.note_add";
    pub const MEMORY_NOTE_REMOVE: &str = "_sill/memory.note_remove";
    /// Compaction and retention now (docs/memory.md §2.4); answers [`Compacted`].
    pub const MEMORY_COMPACT: &str = "_sill/memory.compact";
    /// Delete every ended session; answers [`Count`].
    pub const MEMORY_FORGET: &str = "_sill/memory.forget";
    /// An agent type's default settings (`sill_core::store::AgentSettings` as JSON).
    pub const AGENTS_SET_SETTINGS: &str = "_sill/agents.set_settings";
    /// The Changes page (docs/daemon.md §7 M2): answers a `sill_core::wire::ChangesList`.
    pub const CHANGES_LIST: &str = "_sill/changes.list";
    /// Params a `sill_core::wire::DiffOf`; answers [`Text`].
    pub const CHANGES_DIFF: &str = "_sill/changes.diff";
    /// Params [`ChangesKeep`]; the next four answer a `sill_core::wire::Done`.
    pub const CHANGES_KEEP: &str = "_sill/changes.keep";
    /// Params [`ChangesPaths`].
    pub const CHANGES_REVERT: &str = "_sill/changes.revert";
    /// Params [`ChangesUndo`].
    pub const CHANGES_UNDO: &str = "_sill/changes.undo";
    /// A pinned repository's path back to HEAD; params [`GitRevert`], answers [`Text`].
    pub const CHANGES_GIT_REVERT: &str = "_sill/changes.git_revert";
    /// The ledger across time (SPEC §3.6): params a `sill_core::history::Filter`, answers its events.
    pub const CHANGES_HISTORY: &str = "_sill/changes.history";
    pub const MEMORY_LIST: &str = "_sill/memory.list";
    pub const MEMORY_SHOW: &str = "_sill/memory.show";
    pub const MEMORY_SEARCH: &str = "_sill/memory.search";
    pub const MEMORY_TRANSCRIPT: &str = "_sill/memory.transcript";
    pub const SETTINGS_GET: &str = "_sill/settings.get";
}

/// `$XDG_RUNTIME_DIR/sill/daemon.sock` — the directory is the one the shim sockets live in (0700,
/// no path inside any jail).
pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

pub fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).context("XDG_RUNTIME_DIR not set: no place for the daemon socket")?;
    Ok(base.join("sill"))
}

/// `_sill/daemon.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub protocol: u32,
    pub pid: u32,
    /// Unix seconds.
    pub started: i64,
    /// `unit` (socket-activated systemd user service) or `direct` (spawned by its first client).
    pub mode: String,
    /// The session launcher the daemon found (`launcher::describe`).
    pub launcher: String,
    /// Sessions get resource limits (a cgroup root was found).
    pub limits: bool,
    /// Session logs are journald streams (`journalctl --user -t sill-<id>`), else files.
    pub journal: bool,
    pub socket: String,
    /// Sessions in the store, and how many of them have a live jail.
    pub sessions: usize,
    pub running: usize,
}

/// A session as the store holds it (`store::Session`), plus what the daemon knows about its jail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub title_kind: String,
    pub created_at: i64,
    pub last_used: i64,
    pub policy: String,
    pub auto_allow: bool,
    pub desktop: String,
    pub ended_at: Option<i64>,
    pub summary: String,
    pub summary_kind: String,
    pub memory_scope: String,
    pub pins: Vec<PathBuf>,
    /// Its jail is up.
    pub running: bool,
    /// A ghost session (docs/memory.md §3).
    #[serde(default)]
    pub ghost: bool,
}

/// `_sill/sessions.get`, `_sill/memory.show`: an id, its first characters, or the exact title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Key {
    pub key: String,
}

/// `_sill/memory.list`: an ended session, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySession {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub ended_at: i64,
    /// The summary's first line.
    pub summary: String,
}

/// `_sill/memory.search`: the words, plus the filters the CLI takes as flags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemorySearch {
    pub words: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

/// `_sill/memory.transcript`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
}

/// `_sill/sessions.start`: the shim socket of the running jail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Started {
    pub socket: PathBuf,
    /// This call started the jail (false: it was already running).
    pub fresh: bool,
}

/// `_sill/sessions.log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRequest {
    pub key: String,
    pub lines: usize,
}

/// `_sill/sessions.attach`. `fresh`: an agent still running in the jail is started again instead
/// of being reattached (adapters that refuse a second `initialize`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attach {
    pub key: String,
    #[serde(default)]
    pub fresh: bool,
}

/// `_sill/sessions.command`: `command` is a `sill_core::wire::Command` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCommand {
    pub key: String,
    pub command: serde_json::Value,
}

/// `_sill/sessions.answer`: `answer` is a `sill_core::wire::Answer` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAnswer {
    pub key: String,
    pub request: u64,
    pub answer: serde_json::Value,
}

/// `_sill/sessions.end`. `reason`: `close` (the tab), `idle` (the policy's idle timeout), `quit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct End {
    pub key: String,
    #[serde(default)]
    pub reason: String,
}

/// `_sill/sessions.end`'s answer. `pending`: the agent writes its summary first; the session's
/// `SessionEnded` event says when it is done. Otherwise the end is complete when this arrives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ending {
    pub pending: bool,
    /// The session was deleted (nothing was asked in it) instead of remembered.
    pub deleted: bool,
}

/// `_sill/sessions.new`: a session row. `pins` are copied as given; with `remember_pins` and none
/// given, the pins of the latest session that had any (Settings → Remember pins). Answers [`Key`]
/// with the new id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewSession {
    pub agent: String,
    pub policy: String,
    #[serde(default)]
    pub pins: Vec<PathBuf>,
    #[serde(default)]
    pub desktop: String,
    #[serde(default)]
    pub memory_scope: String,
    /// The ended session this one follows (*New here*).
    #[serde(default)]
    pub follows: String,
    #[serde(default)]
    pub remember_pins: bool,
    /// A ghost session (docs/memory.md §3): no pins (refused when given), no memory written, the
    /// row and every trace deleted at its end.
    #[serde(default)]
    pub ghost: bool,
}

/// `_sill/sessions.set`: `set` is a `sill_core::wire::SessionSet` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSet {
    pub key: String,
    pub set: serde_json::Value,
}

/// `_sill/sessions.pin|unpin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub key: String,
    pub path: PathBuf,
}

/// `_sill/memory.note_add`; `_sill/memory.note_remove` with `ts` (a note is its time and text).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub key: String,
    pub text: String,
    #[serde(default)]
    pub ts: i64,
}

/// `_sill/agents.set_settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettingsSet {
    pub agent: String,
    pub settings: serde_json::Value,
}

/// `_sill/changes.keep`: each host path with the hash it was listed with (None: no check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesKeep {
    pub session: String,
    pub items: Vec<(PathBuf, Option<String>)>,
}

/// `_sill/changes.revert`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesPaths {
    pub session: String,
    pub paths: Vec<PathBuf>,
}

/// `_sill/changes.undo`: keep ids; none = every undoable keep of the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesUndo {
    pub session: String,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub force: bool,
}

/// `_sill/changes.git_revert`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRevert {
    pub repo: PathBuf,
    pub rel: PathBuf,
}

/// `_sill/memory.compact`'s answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Compacted {
    pub compacted: usize,
    pub trimmed: usize,
    pub deleted: usize,
}

/// A count (`_sill/memory.forget`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Count {
    pub n: usize,
}

/// `_sill/session.event`: `event` is a `sill_core::wire::Event` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub key: String,
    pub event: serde_json::Value,
}

/// A result that is text the way the CLI prints it (`memory.show`, `memory.search`, `memory.transcript`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

/// No parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}
