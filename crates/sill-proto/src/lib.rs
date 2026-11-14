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
pub mod workspaces;
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
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = match base { Some(base) => Some(base), None => Some(macos_runtime_base()?) };
    let base = base.context("XDG_RUNTIME_DIR not set: no place for the daemon socket")?;
    Ok(base.join("sill"))
}

/// macOS has no XDG_RUNTIME_DIR, and its TMPDIR is too long for SSH's control sockets (ssh adds a
/// random suffix while making one; a socket path has 104 bytes): a private `/tmp/sill-UID`,
/// refused when someone else owns it or can enter it.
#[cfg(target_os = "macos")]
fn macos_runtime_base() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = rustix::process::getuid().as_raw();
    let dir = PathBuf::from(format!("/tmp/sill-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(&dir).with_context(|| format!("checking {}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        anyhow::bail!("{} is not a private directory of this user", dir.display());
    }
    Ok(dir)
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

/// Per-account startup preference, independent of the daemon's settings/store.
pub fn autostart_disabled_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".config"),
    };
    Ok(config.join("sill/daemon-autostart-disabled"))
}

pub fn autostart_enabled() -> Result<bool> {
    match std::fs::metadata(autostart_disabled_path()?) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
