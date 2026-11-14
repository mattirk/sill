//! `_sill/workspaces.*` (`sill.workspaces: 1`, docs/parallel-coding.md §12): the daemon's side of
//! workspaces. Everything but sessions and the store lives in `sill_core::workspace::records`;
//! this module answers the methods, makes a workspace session's workspaces when its jail starts,
//! gives records the store, the sessions' state and the rebase helper jail, and tells the
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

    fn life(&self, session: &str) -> Life {
        match self.0.with_store(|s| s.session_exact(session)) {
            Ok(Some(row)) if row.ended_at.is_some() => Life::Ended,
            Ok(Some(_)) => Life::Alive,
            Ok(None) => Life::Missing,
            // unknown: treat it as alive, which only ever keeps work
            Err(_) => Life::Alive,
        }
    }

    fn title(&self, session: &str) -> Option<String> {
        self.0.with_store(|s| s.session_exact(session)).ok().flatten().map(|row| row.title)
    }

    fn busy(&self, session: &str) -> bool {
        self.0.runner(session).is_ok_and(|r| r.busy())
    }

    fn session_pins(&self, session: &str) -> Result<Vec<PathBuf>> {
        self.0.with_store(|s| s.pins(session))
    }

    fn root(&self) -> PathBuf {
        root()
    }

    /// The layers of the session's last launch (its recorded plan); none before it first ran.
    fn overlay(&self, session: &str) -> Result<Option<records::Overlay>> {
        let Some(row) = self.0.with_store(|s| s.session_exact(session))? else { return Ok(None) };
        if sill_core::session::policy_snapshot(session)?.is_none() {
            return Ok(None);
        }
        let view = self.0.jail_view(session)?;
        Ok(Some(records::Overlay { layers: view.layers, started: row.created_at }))
    }

    /// The rebase helper: `sill jail run` with the session's next build — its masks, pins, agent
    /// home and its workspaces at their repositories' paths — no network, a throwaway state
    /// directory on the state filesystem, the resolution in a private file. The person only acts
    /// between the agent's turns, so the helper is the only one writing the workspace then.
    fn run_jailed(&self, session: &str, cwd: &Path, argv: &[&str]) -> Result<std::process::Output> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let hub = self.0;
        let (row, pins, binds) = hub.with_store(|s| {
            let row = s.session_exact(session)?.ok_or_else(|| anyhow::anyhow!("no session {session}"))?;
            Ok((row, s.jail_pins(session)?, s.workspace_mask_all(session)?))
        })?;
        let resolved = sill_core::session::next_build(&row, &hub.home, &pins, &binds)?;
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

/// `$XDG_DATA_HOME/sill/workspaces`, usually `~/.local/share/sill/workspaces`.
fn root() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| sill_core::changes::home().ok().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join("sill/workspaces")
}

impl Hub {
    pub(super) fn workspace_detail_for_crosscheck(&self, id: &str) -> Result<api::Detail> {
        records::get(&Daemon(self), id)
    }
    /// At start, before workspace requests are worth anything: settle interrupted operations.
    pub(super) fn workspaces_recover(&self) {
        if let Err(e) = records::reconcile(&Daemon(self)) {
            eprintln!("sill daemon: workspaces: {e:#}");
        }
    }

    /// Before a session's jail starts (§12): its workspaces, made or kept. A failure keeps the
    /// session from starting rather than letting it write into the person's checkout.
    pub(super) fn workspaces_for_start(&self, session: &str) -> Result<()> {
        let h = Daemon(self);
        if !records::ensure_for_session(&h, session)?.is_empty() {
            for w in records::session_view(&h, session)?.workspaces {
                self.workspaces_changed(&w.workspace_id);
            }
        }
        Ok(())
    }

    /// After a session's workspaces were matched to its pins: every one of them, those left as
    /// results too, is news to the shelf and Changes.
    pub(super) fn workspaces_settled(&self, session: &str) {
        match records::session_view(&Daemon(self), session) {
            Ok(view) => {
                for w in view.workspaces {
                    self.workspaces_changed(&w.workspace_id);
                }
            }
            Err(e) => eprintln!("sill daemon: session {session}: workspaces: {e:#}"),
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

    pub(super) fn workspaces(self: &Arc<Self>, conn: &Conn, method: &str, params: Value) -> Option<Result<Value>> {
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
        // Whether this request is the one that applies or discards, not a repeat of one already
        // recorded: a repeat answers with the earlier receipt and changes no files.
        let first = |operation: &str| h.db(|c| Ok(sill_core::operations::Journal::new(c)?.get(operation).is_err()));
        let mut work_gone = false;
        let result = (|| -> Result<(Value, Option<String>)> {
            Ok(match method {
                "_sill/workspaces.list" => (json!(records::list(&h, &args(params)?)?), None),
                "_sill/workspaces.session" => (json!(records::session_view(&h, &args::<api::SessionKey>(params)?.session_id)?), None),
                "_sill/workspaces.get" => (json!(records::get(&h, &args::<api::Key>(params)?.workspace_id)?), None),
                "_sill/workspaces.diff" => (json!(records::diff(&h, &args(params)?)?), None),
                "_sill/workspaces.prepare" => {
                    let a: api::Operate = args(params)?;
                    (json!(records::prepare(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.apply" => {
                    let a: api::Apply = args(params)?;
                    let first = first(&a.operation_id)?;
                    let done = json!(records::apply(&h, &a)?);
                    work_gone = first;
                    (done, Some(a.workspace_id))
                }
                "_sill/workspaces.copy" => {
                    let a: api::Apply = args(params)?;
                    (json!(records::copy(&h, &a)?), Some(a.workspace_id))
                }
                "_sill/workspaces.discard" => {
                    let a: api::Apply = args(params)?;
                    let first = first(&a.operation_id)?;
                    let done = json!(records::discard(&h, &a)?);
                    work_gone = first;
                    (done, Some(a.workspace_id))
                }
                "_sill/workspaces.undo" => {
                    let a: api::Undo = args(params)?;
                    let done = records::undo(&h, &a)?;
                    let id = done.workspace.as_ref().map(|w| w.workspace_id.clone());
                    (json!(done), id)
                }
                "_sill/workspaces.abort" => {
                    let a: api::Operate = args(params)?;
                    (json!(records::abort(&h, &a)?), Some(a.workspace_id))
                }
                _ => return Err(Error::method_not_found(method).into()),
            })
        })();
        Some(result.map(|(value, changed)| {
            if let Some(id) = changed {
                // A review is of work waiting to be applied: applied or discarded, the work is
                // not waiting, and its review goes with it, a Reviewer still at work stopped
                // (docs/crosscheck.md). Only the request that did it removes the review: a repeat
                // of an earlier Apply or Discard may arrive when newer work has a review of its
                // own. A copy into the checkout leaves the work waiting.
                if work_gone && value["operation"]["state"] == "completed" {
                    self.crosscheck_work_gone(&id);
                }
                self.workspaces_changed(&id);
                // an Apply changes a checkout the Changes page shows
                self.changes_changed();
            }
            value
        }))
    }
}
