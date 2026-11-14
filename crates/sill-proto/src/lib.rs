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
pub const PROTOCOL: u32 = 1;

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

/// A result that is text the way the CLI prints it (`memory.show`, `memory.search`, `memory.transcript`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

/// No parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}
