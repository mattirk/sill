//! Sill Frontend Protocol control messages, schemas and blocking client.
//! Public conversation records live in `sfp`; `initialize` selects the development identity.
//! Host administration and conversation methods share the public protocol.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub mod client;
pub mod initialize;
pub mod rpc;
pub mod schema;
pub mod schema_compat;
pub mod wire;
pub mod sfp;
pub mod sessions;
pub mod terminals;
pub mod host;
pub mod host_control;
pub mod admin;
pub mod memory;
pub mod transport;

pub use client::Client;

/// Public daemon method names.
pub mod method {
    pub const DAEMON_SCHEMA: &str = "_sill/daemon.schema";
    pub const DAEMON_STATUS: &str = "_sill/daemon.status";
    pub const DAEMON_SHUTDOWN: &str = "_sill/daemon.shutdown";
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DaemonStatus {
    pub version: String,
    pub protocol: String,
    pub pid: u32,
    /// UTC RFC 3339.
    pub started: String,
    /// `unit` (socket-activated systemd user service) or `direct` (spawned by its first client).
    pub mode: String,
    /// The session launcher the daemon found (`launcher::describe`).
    pub launcher: String,
    /// Sessions get resource limits (a cgroup root was found).
    pub limits: bool,
    /// Session logs are journald streams (`journalctl --user -t sill-<id>`), else files.
    pub journal: bool,
    pub socket: sessions::HostPath,
    /// Sessions in the store, and how many of them have a live jail.
    pub sessions: usize,
    pub running: usize,
}

/// `_sill/sessions.start`: the shim socket of the running jail.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Started {
    pub socket: PathBuf,
    /// This call started the jail (false: it was already running).
    pub fresh: bool,
}

/// `_sill/sessions.end`'s answer. `pending`: the agent writes its summary first; the session's
/// `SessionEnded` event says when it is done. Otherwise the end is complete when this arrives.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Ending {
    pub pending: bool,
    /// The session was deleted (nothing was asked in it) instead of remembered.
    pub deleted: bool,
}

/// `_sill/sessions.new`: a session row. `pins` are copied as given; with `remember_pins` and none
/// given, the pins of the latest session that had any (Settings → Remember pins). Answers [`Key`]
/// with the new id.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewSession {
    pub agent: String,
    pub policy: String,
    #[serde(default)]
    pub pins: Vec<PathBuf>,
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

/// `_sill/sessions.jail_view`: each overlay layer (host path, its upper tree) and the masked paths.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JailView {
    pub layers: Vec<(PathBuf, PathBuf)>,
    pub masked: Vec<PathBuf>,
}

/// `_sill/changes.keep`: each host path with the hash it was listed with (None: no check).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangesKeep {
    pub session: String,
    pub items: Vec<(PathBuf, Option<String>)>,
}

/// `_sill/changes.revert`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangesPaths {
    pub session: String,
    pub paths: Vec<PathBuf>,
}

/// `_sill/changes.undo`: keep ids; none = every undoable keep of the session.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangesUndo {
    pub session: String,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub force: bool,
}

/// `_sill/changes.git_revert`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitRevert {
    pub repo: PathBuf,
    pub rel: PathBuf,
}

/// `_sill/memory.compact`'s answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Compacted {
    pub compacted: usize,
    pub trimmed: usize,
    pub deleted: usize,
}

/// A result that is text the way the CLI prints it (`memory.show`, `memory.search`, `memory.transcript`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Text {
    pub text: String,
}

/// No parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Empty {}

pub mod pins;

pub mod agent_admin;

pub mod changes;

pub mod discovery;
