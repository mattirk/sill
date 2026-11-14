//! `_sill/workspaces.*` (`sill.workspaces: 1`, docs/parallel-coding.md §11): the daemon's side of
//! workspaces. Everything but the session and the store lives in `sill_core::workspace::records`;
//! this module answers the methods, gives records a session and the store, and tells the
//! connections that asked for `_sill/workspaces.changed`.
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use sill_core::workspace::records::{self, Host, Life};
use sill_proto::workspaces as api;

use super::*;

/// The hub as the workspace records see it.
struct Daemon<'a>(&'a Hub);

impl Host for Daemon<'_> {
    fn db<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
        self.0.with_store(|s| f(s.conn()))
    }

    fn new_session(&self, create_id: &str, agent: &str, policy: &str, pin: &Path) -> Result<String> {
        let a = sill_proto::sessions::Create {
            create_id: create_id.into(),
            agent: agent.into(),
            policy: policy.into(),
            pins: vec![sill_proto::sessions::HostPath::from_path(pin)],
            memory_scope: String::new(),
            follows: String::new(),
            remember_pins: false,
            ghost: false,
        };
        Ok(self.0.with_store(|s| super::sessions::create(s, a))?.session_id)
    }

    fn life(&self, session: &str) -> Life {
        match self.0.with_store(|s| s.session_exact(session)) {
            Ok(Some(row)) if row.ended_at.is_some() => Life::Ended,
            Ok(Some(_)) if sill_core::session::is_running(session) => Life::Running,
            Ok(Some(_)) => Life::Stopped,
            Ok(None) => Life::Missing,
            // unknown: treat it as able to write, which only ever refuses
            Err(_) => Life::Running,
        }
    }

    fn root(&self) -> PathBuf {
        root()
    }

    /// The rebase helper (§7 step 3): `sill jail run` with the session's next build — its masks,
    /// pins, agent home and the workspace's hidden source — no network, a throwaway state
    /// directory on the state filesystem, the resolution in a private file. Never while the
    /// session's own jail runs: the helper must be the only writer.
    fn run_jailed(&self, session: &str, cwd: &Path, argv: &[&str]) -> Result<std::process::Output> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let hub = self.0;
        anyhow::ensure!(!sill_core::session::is_running(session), "the session's jail is running");
        let (row, pins, mask) = hub.with_store(|s| {
            let row = s.session_exact(session)?.ok_or_else(|| anyhow::anyhow!("no session {session}"))?;
            Ok((row, s.jail_pins(session)?, s.workspace_mask(session)?))
        })?;
        let resolved = sill_core::session::next_build(&row, &hub.home, &pins, &[], mask.as_ref())?;
        if let Some(plan) = &resolved.filesystem {
            plan.prepare()?;
        }
        let tag = format!("{session}-{}", sill_core::store::new_id());
        let uid = rustix::process::getuid().as_raw();
        let state = Path::new(sill_core::jail::STATE_FS).join(uid.to_string()).join("helpers").join(&tag);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&state)?;
        let dir = sill_proto::runtime_dir()?.join("helpers");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let file = dir.join(format!("{tag}.json"));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file)?;
            f.write_all(&serde_json::to_vec(&resolved)?)?;
        }
        let out = std::process::Command::new(&hub.exe)
            .args(["jail", "run", "--resolved-policy"])
            .arg(&file)
            .arg("--state-dir")
            .arg(&state)
            .arg("--cwd")
            .arg(cwd)
            .args(["--env", "GIT_EDITOR=true", "--env", "GIT_SEQUENCE_EDITOR=true", "--env", "GIT_TERMINAL_PROMPT=0", "--"])
            .args(argv)
            .stdin(std::process::Stdio::null())
            .output();
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&state);
        Ok(out?)
    }
}

/// `$XDG_DATA_HOME/sill/workspaces`, usually `~/.local/share/sill/workspaces` (§3).
fn root() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| sill_core::changes::home().ok().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join("sill/workspaces")
}

impl Hub {
    /// At start, before workspace requests are worth anything: settle interrupted operations.
    pub(super) fn workspaces_recover(&self) {
        if let Err(e) = records::reconcile(&Daemon(self)) {
            eprintln!("sill daemon: workspaces: {e:#}");
        }
    }

    fn workspaces_changed(&self, workspace_id: &str) {
        let req = sill_proto::rpc::Request::new(None, "_sill/workspaces.changed", json!({"workspace_id": workspace_id}));
        let Ok(line) = serde_json::to_string(&req) else { return };
        let conns: Vec<Arc<Conn>> = self.conns.lock().unwrap().iter().filter_map(|w| w.upgrade()).collect();
        for c in conns.into_iter().filter(|c| c.has_capability(api::CAPABILITY)) {
            c.send(line.clone());
        }
    }

    pub(super) fn workspaces(&self, conn: &Conn, method: &str, params: Value) -> Option<Result<Value>> {
        if !method.starts_with("_sill/workspaces.") {
            return None;
        }
        if !conn.has_capability(api::CAPABILITY) {
            let mut error = Error::domain("unsupported", "workspaces need the sill.workspaces capability");
            error.data.as_mut().unwrap()["details"] = json!({"capability": api::CAPABILITY});
            return Some(Err(error.into()));
        }
        fn args<T: serde::de::DeserializeOwned>(p: Value) -> Result<T> {
            serde_json::from_value(p).map_err(|e| Error::invalid_params(e.to_string()).into())
        }
        let h = Daemon(self);
        let result = (|| -> Result<(Value, Option<String>)> {
            Ok(match method {
                "_sill/workspaces.list" => (json!(records::list(&h, &args(params)?)?), None),
                "_sill/workspaces.get" => (json!(records::get(&h, &args::<api::Key>(params)?.workspace_id)?), None),
                "_sill/workspaces.diff" => (json!(records::diff(&h, &args(params)?)?), None),
                "_sill/workspaces.create" => {
                    let made = records::create(&h, &args(params)?)?;
                    let id = made.workspace.workspace_id.clone();
                    (json!(made), Some(id))
                }
                "_sill/workspaces.finish" => {
                    let a: api::Key = args(params)?;
                    (json!(records::finish(&h, &a.workspace_id)?), Some(a.workspace_id))
                }
                "_sill/workspaces.reopen" => {
                    let a: api::Reopen = args(params)?;
                    (json!(records::reopen(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.prepare" => {
                    let a: api::Operate = args(params)?;
                    (json!(records::prepare(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.merge" => {
                    let a: api::Apply = args(params)?;
                    (json!(records::merge(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.discard" => {
                    let a: api::Apply = args(params)?;
                    (json!(records::discard(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.abort" => {
                    let a: api::Operate = args(params)?;
                    (json!(records::abort(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.undo" => {
                    let a: api::Operate = args(params)?;
                    (json!(records::undo(&h, &a)?), Some(a.workspace_id))
                }
                _ => return Err(Error::method_not_found(method).into()),
            })
        })();
        Some(result.map(|(value, changed)| {
            if let Some(id) = changed {
                self.workspaces_changed(&id);
                // a merge moves a pinned checkout; a new workspace adds a repository
                self.changes_changed();
            }
            value
        }))
    }
}
