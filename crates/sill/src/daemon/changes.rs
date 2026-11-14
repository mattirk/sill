//! The Changes page's work in the daemon (docs/daemon.md §7 M2): every session's overlay scanned
//! against its policy, the pinned repositories' git status, keep with its hash check, revert (in
//! the jail through the shim while it runs, the upper entry dropped when it does not), undo, the
//! diffs — and the auto-revert sweep, which the frontend ran before the split. Frontends name a change by
//! session and host path; the daemon finds it again in a fresh scan, so nothing a frontend sends
//! points at a file of its choosing.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sill_core::history::Kind as H;
use sill_core::keep::{self, KeepOptions};
use sill_core::ledger::{Change, FileType, Kind};
use sill_core::store::Session;
use sill_core::wire::{ChangeRow, ChangesList, DiffOf, Done, LiveRepo, SessionChanges};

use super::Hub;

impl Hub {
    pub(super) fn changes_list(&self) -> Result<ChangesList> {
        let sessions = self.with_store(|s| s.sessions())?;
        let (uid, user) = who();
        let registry = sill_core::agents::load(&self.home);
        let mut pinned: Vec<PathBuf> = self.pinned_repos()?;
        pinned.sort();
        pinned.dedup();
        let live = pinned
            .into_iter()
            .map(|repo| LiveRepo { status: sill_core::changes::git_status(&repo), repo })
            .filter(|l| !l.status.is_empty())
            .collect();
        let mut out = Vec::new();
        for s in sessions {
            let list: Vec<Change> = self.session_changes(&s, uid, &user).unwrap_or_default().into_iter().filter(|c| !c.ignored).collect();
            let kept = kept_dir(uid, &s.id).map(|d| keep::undoable(&d)).unwrap_or_default();
            if list.is_empty() && kept.is_empty() {
                continue;
            }
            let state_dirs = registry.get(&s.agent).map(|a| sill_core::session::state_paths(a, &self.home)).unwrap_or_default();
            let (agent_state, changes): (Vec<Change>, Vec<Change>) =
                list.into_iter().partition(|c| sill_core::changes::is_agent_state(&c.path, &state_dirs, &self.home));
            out.push(SessionChanges {
                id: s.id.clone(),
                title: s.title.clone(),
                ghost: s.ghost,
                changes: changes.into_iter().map(row).collect(),
                agent_state: agent_state.into_iter().map(row).collect(),
                kept,
            });
        }
        Ok(ChangesList { live, sessions: out })
    }

    pub(super) fn changes_diff(&self, of: DiffOf) -> Result<String> {
        match of {
            DiffOf::Change { session, path } => {
                let (_, c) = self.find_change(&session, &path)?;
                sill_core::changes::unified_diff(&c)
            }
            DiffOf::Live { repo, rel } => {
                self.check_live(&repo, &rel)?;
                Ok(sill_core::changes::git_diff(&repo, &rel))
            }
            DiffOf::Kept { session, id } => {
                let dir = kept_dir(who().0, &session)?;
                let Some(k) = keep::list(&dir).into_iter().find(|k| k.id == id) else { bail!("no keep {id} in session {session}") };
                Ok(sill_core::changes::undo_diff(&k.path, &keep::prior_path(&dir, &k)))
            }
        }
    }

    /// Keep each (path, hash as listed): copied to the host with the previous version saved for undo.
    pub(super) fn changes_keep(&self, session: &str, items: Vec<(PathBuf, Option<String>)>) -> Result<Done> {
        let changes = self.scan_one(session)?;
        let dir = kept_dir(who().0, session)?;
        let mut d = Done::default();
        for (path, hash) in with_new_parents(&changes, items) {
            let Some(c) = changes.iter().find(|c| c.path == path) else {
                d.failed.push((path, "no longer a change of this session".into()));
                continue;
            };
            let opts = KeepOptions { undo: Some(dir.clone()), expected_hash: hash, ..Default::default() };
            match keep::keep(c, &opts) {
                Ok(what) => {
                    // the history (SPEC §3.6): what was written, what was there, the journal's id
                    let rec = keep::list(&dir).into_iter().rev().find(|k| k.path == path);
                    self.history(session, H::Kept, &path, |e| {
                        e.what = what.to_string();
                        if let Some(r) = &rec {
                            e.kept_id = Some(r.id.clone());
                            e.hash_new = r.hash.clone();
                            let prior = keep::prior_path(&dir, r);
                            e.hash_old = prior.is_file().then(|| keep::sha256_file(&prior).ok()).flatten();
                        }
                    });
                    d.done.push((path, what.to_string()))
                }
                Err(e) => d.failed.push((path, format!("{e:#}"))),
            }
        }
        Ok(d)
    }

    /// Revert paths of a session, deepest first: in the jail while it runs, else by dropping the
    /// upper entry (the overlay is not mounted then, and that is the revert).
    pub(super) fn changes_revert(&self, session: &str, mut paths: Vec<PathBuf>) -> Result<Done> {
        let changes = self.scan_one(session)?;
        paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        let found: Vec<Change> = paths.iter().filter_map(|p| changes.iter().find(|c| &c.path == p).cloned()).collect();
        let mut d = revert(session, &found);
        for (p, _) in &d.done {
            self.history(session, H::Reverted, p, |_| {});
        }
        for p in paths.into_iter().filter(|p| !found.iter().any(|c| &c.path == p)) {
            d.failed.push((p, "no longer a change of this session".into()));
        }
        Ok(d)
    }

    /// Undo keeps by id (all of the session's undoable ones when `ids` is empty), newest first;
    /// `force` puts the previous version back even when the host was edited since.
    pub(super) fn changes_undo(&self, session: &str, ids: Vec<String>, force: bool) -> Result<Done> {
        let dir = kept_dir(who().0, session)?;
        let all = keep::undoable(&dir);
        let pick: Vec<_> = if ids.is_empty() { all } else { all.into_iter().filter(|k| ids.contains(&k.id)).collect() };
        let mut d = Done::default();
        for k in pick {
            match keep::undo(&dir, &k, force) {
                Ok(what) => {
                    self.history(session, H::Undone, &k.path, |e| {
                        e.what = what.to_string();
                        e.kept_id = Some(k.id.clone());
                    });
                    d.done.push((k.path.clone(), what.to_string()))
                }
                Err(e) => d.failed.push((k.path.clone(), format!("{e:#}"))),
            }
        }
        Ok(d)
    }

    /// `git checkout HEAD` of one path of a pinned repository (an untracked file is removed).
    pub(super) fn changes_git_revert(&self, repo: &Path, rel: &Path) -> Result<String> {
        self.check_live(repo, rel)?;
        Ok(sill_core::changes::git_revert(repo, rel)?.to_string())
    }

    /// Auto-revert (SPEC §4.4), from the minute sweep: in every session but a ghost (its hour is
    /// `revert_ghosts`) that is not mid-turn, each overlay change outside any git repository whose
    /// last write is older than Settings' delay is reverted; a conflict is left for the person, the
    /// agent's own state alone.
    pub(super) fn auto_revert(&self) {
        let minutes = sill_core::config::load().auto_revert_minutes();
        if minutes == 0 {
            return;
        }
        let after = std::time::Duration::from_secs(minutes as u64 * 60);
        let (uid, user) = who();
        let registry = sill_core::agents::load(&self.home);
        let sessions: Vec<Session> = self.with_store(|s| s.sessions()).unwrap_or_default().into_iter().filter(|s| !s.ghost).collect();
        for s in sessions {
            if self.children.lock().unwrap().contains_key(&s.id) && self.runners.lock().unwrap().get(&s.id).is_none_or(|r| !r.alive() || r.busy()) {
                continue;
            }
            let state_dirs = registry.get(&s.agent).map(|a| sill_core::session::state_paths(a, &self.home)).unwrap_or_default();
            let now = std::time::SystemTime::now();
            let mut due: Vec<Change> = self
                .session_changes(&s, uid, &user)
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.conflict && !c.ignored && sill_core::changes::repo_root(&c.path).is_none())
                .filter(|c| !sill_core::changes::is_agent_state(&c.path, &state_dirs, &self.home))
                .filter(|c| written(c).is_some_and(|t| now.duration_since(t).unwrap_or_default() >= after))
                .collect();
            if due.is_empty() {
                continue;
            }
            due.sort_by_key(|c| std::cmp::Reverse(c.path.components().count()));
            let d = revert(&s.id, &due);
            // one row per sweep and session (SPEC §3.6 decision 3), the paths behind it
            if !d.done.is_empty() {
                let paths: Vec<PathBuf> = d.done.iter().map(|(p, _)| p.clone()).collect();
                let mut e = sill_core::history::event(&s, H::AutoReverted, &sill_memory::history::common_prefix(&paths));
                e.count = paths.len() as i64;
                e.paths = paths;
                if let Err(err) = self.with_store(|st| sill_core::history::record(st, &e)) {
                    eprintln!("sill daemon: history: {err:#}");
                }
            }
            self.changes_changed();
            eprintln!("sill daemon: session {}: auto-reverted {} path(s) outside git{}", s.id, d.done.len(), if d.failed.is_empty() { String::new() } else { format!(", {} failed", d.failed.len()) });
        }
    }

    /// After each turn, on a thread (it scans): the agent's own new dot-paths move into its
    /// sill-owned home (SPEC §4.2, what the frontend's `after_turn` did before the split) — never for a ghost, whose
    /// state must not reach the home every later session shares — and the attached frontends get
    /// a note: how many changes wait in Changes, what was adopted.
    pub fn turn_ended(self: &std::sync::Arc<Self>, id: &str) {
        let hub = self.clone();
        let id = id.to_string();
        let _ = std::thread::Builder::new().name(format!("sill-turn-{id}")).spawn(move || {
            let Some(s) = hub.with_store(|st| st.sessions()).ok().and_then(|v| v.into_iter().find(|s| s.id == id)) else { return };
            let (uid, user) = who();
            let changes = match hub.session_changes(&s, uid, &user) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("sill daemon: session {id}: scanning after the turn: {e:#}");
                    return;
                }
            };
            let registry = sill_core::agents::load(&hub.home);
            let agent = registry.get(&s.agent);
            let mut adopted_note = String::new();
            if let Some(agent) = agent.filter(|_| !s.ghost) {
                let pins = hub.with_store(|st| st.jail_pins(&s.id)).unwrap_or_default();
                match sill_core::session::adopt_agent_state(agent, &hub.home, &changes, &pins) {
                    Ok(adopted) if !adopted.is_empty() => {
                        let names: Vec<String> = adopted.iter().map(|p| format!("~/{}", p.strip_prefix(&hub.home).unwrap_or(p).display())).collect();
                        adopted_note = format!("{} kept as {}'s own state (in its jail home from the next start)", names.join(", "), agent.name);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("sill daemon: session {id}: adopting the agent's state: {e:#}"),
                }
            }
            let state = agent.map(|a| sill_core::session::state_paths(a, &hub.home)).unwrap_or_default();
            let pending = changes.iter().filter(|c| !c.ignored && !sill_core::changes::is_agent_state(&c.path, &state, &hub.home)).count();
            let mut msg = match pending {
                0 => String::new(),
                n if s.ghost => format!("{n} change(s) in Changes — they revert an hour after their last write unless kept"),
                n => format!("{n} change(s) pending in Changes"),
            };
            if !adopted_note.is_empty() {
                if !msg.is_empty() {
                    msg.push_str(" · ");
                }
                msg.push_str(&adopted_note);
            }
            // the turn may have written anything: whatever shows the overlays rescans
            hub.changes_changed();
            if !msg.is_empty() {
                if let Err(error) = hub.sfp_lifecycle(&id, None, Some(msg)) {
                    eprintln!("sill daemon: publishing Changes notice: {error:#}");
                }
            }
        });
    }

    /// One history row by session `id`. A ghost leaves a row only for what changed the host — its
    /// keeps and their undos, anonymous — never for a revert, which touched only its own overlay
    /// (SPEC §3.6 decision 1).
    fn history(&self, id: &str, kind: H, path: &Path, fill: impl FnOnce(&mut sill_core::history::Event)) {
        let Some(s) = self.with_store(|st| st.sessions()).ok().and_then(|v| v.into_iter().find(|s| s.id == id)) else { return };
        if s.ghost && !matches!(kind, H::Kept | H::Undone) {
            return;
        }
        let mut e = sill_core::history::event(&s, kind, path);
        fill(&mut e);
        if let Err(err) = self.with_store(|st| sill_core::history::record(st, &e)) {
            eprintln!("sill daemon: history: {err:#}");
        }
    }

    pub(super) fn changes_history(&self, f: sill_core::history::Filter) -> Result<Vec<sill_core::history::Event>> {
        self.with_store(|st| sill_memory::history::query(st.conn(), &f))
    }

    /// How the session's jail lays the host out (its overlay layers and the masked paths): what a
    /// frontend needs to show a file as the agent sees it — an image the agent names, say.
    pub(in crate::daemon) fn jail_view(&self, id: &str) -> Result<sill_proto::JailView> {
        let Some(s) = self.with_store(|st| st.sessions())?.into_iter().find(|s| s.id == id) else { bail!("no session {id}") };
        let (uid, user) = who();
        let policy = sill_core::session::policy_of(&s, &self.home)?;
        let registry = sill_core::agents::load(&self.home);
        let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
        let sdir = sill_core::session::state_dir(uid, &s.id)?;
        let pins = sill_core::session::launch_pins(&s.id)?;
        let binds = sill_core::session::binds_for(agent, &policy, &sdir, &self.home, &pins, s.ghost)?;
        let plan = sill_core::changes::session_plan(policy, sdir, &self.home, uid, &user, binds)?;
        Ok(sill_proto::JailView { layers: plan.layers.into_iter().map(|l| (l.host, l.upper)).collect(), masked: plan.masked })
    }

    fn scan_one(&self, session: &str) -> Result<Vec<Change>> {
        let Some(s) = self.with_store(|st| st.sessions())?.into_iter().find(|s| s.id == session) else { bail!("no session {session}") };
        let (uid, user) = who();
        self.session_changes(&s, uid, &user)
    }

    fn find_change(&self, session: &str, path: &Path) -> Result<(String, Change)> {
        match self.scan_one(session)?.into_iter().find(|c| c.path == path) {
            Some(c) => Ok((session.to_string(), c)),
            None => bail!("{} is no longer a change of session {session}", path.display()),
        }
    }

    /// Every session's pinned folders, and the global ones, that are git repositories.
    fn pinned_repos(&self) -> Result<Vec<PathBuf>> {
        self.with_store(|st| {
            let mut out = Vec::new();
            for s in st.sessions()? {
                out.extend(st.pins(&s.id)?.into_iter().filter(|p| p.join(".git").exists()));
            }
            out.extend(st.global_pins()?.into_iter().filter(|p| p.join(".git").exists()));
            Ok(out)
        })
    }

    /// A live diff or revert is only for a pinned repository, and only below it.
    fn check_live(&self, repo: &Path, rel: &Path) -> Result<()> {
        if !self.pinned_repos()?.iter().any(|p| p == repo) {
            bail!("{} is not a pinned repository", repo.display());
        }
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            bail!("{}: not a path inside the repository", rel.display());
        }
        Ok(())
    }
}

fn who() -> (u32, String) {
    let uid = rustix::process::getuid().as_raw();
    (uid, std::env::var("USER").unwrap_or_else(|_| uid.to_string()))
}

fn kept_dir(uid: u32, session: &str) -> Result<PathBuf> {
    Ok(sill_core::session::state_dir(uid, session)?.join("kept"))
}

fn written(c: &Change) -> Option<std::time::SystemTime> {
    std::fs::symlink_metadata(&c.upper).ok()?.modified().ok()
}

fn row(c: Change) -> ChangeRow {
    let written = written(&c).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
    let hash = (c.file_type == FileType::File && matches!(c.kind, Kind::Added | Kind::Modified)).then(|| keep::sha256_file(&c.upper).ok()).flatten();
    ChangeRow {
        repo: sill_core::changes::repo_root(&c.path).unwrap_or_default(),
        protected: keep::protected(&c.path).map(str::to_string),
        written,
        hash,
        change: c,
    }
}

/// Revert `changes` of a session in the given order.
fn revert(session: &str, changes: &[Change]) -> Done {
    let mut d = Done::default();
    let mut control = if sill_core::session::is_running(session) {
        sill_core::session::socket_path(session).ok().and_then(|s| sill_core::shim::Control::connect(&s).ok())
    } else {
        None
    };
    for c in changes {
        let r = match control.as_mut() {
            Some(ctl) => ctl.revert(&c.path),
            None => remove_upper_entry(&c.upper).map(|_| "dropped from the session's record".to_string()),
        };
        match r {
            Ok(what) => d.done.push((c.path.clone(), what)),
            Err(e) => d.failed.push((c.path.clone(), format!("{e:#}"))),
        }
    }
    d
}

/// Remove an entry from an unmounted upper tree (whiteout, file, symlink or directory).
fn remove_upper_entry(upper: &Path) -> Result<()> {
    match upper.symlink_metadata() {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(upper)?,
        Ok(_) => std::fs::remove_file(upper)?,
        Err(_) => {}
    }
    Ok(())
}

/// A file kept inside a folder the session created needs that folder on the host first (keep
/// refuses a missing parent): the session's own new ancestor folders join the keep, and everything
/// goes shallowest first. Keeping one file in a new folder failed with "No such file or directory".
fn with_new_parents(changes: &[Change], items: Vec<(PathBuf, Option<String>)>) -> Vec<(PathBuf, Option<String>)> {
    let mut all = items;
    let asked: Vec<PathBuf> = all.iter().map(|(p, _)| p.clone()).collect();
    for path in &asked {
        for parent in path.ancestors().skip(1) {
            if parent == std::path::Path::new("/") || std::fs::symlink_metadata(parent).is_ok() {
                break;
            }
            if changes.iter().any(|c| c.path == parent) && !all.iter().any(|(p, _)| p == parent) {
                all.push((parent.to_path_buf(), None));
            }
        }
    }
    all.sort_by_key(|(p, _)| p.components().count());
    all
}

#[cfg(test)]
mod keep_parent_tests {
    use super::*;

    #[test]
    fn a_file_in_a_new_folder_brings_the_folder_shallowest_first() {
        let dir = std::env::temp_dir().join(format!("sill-keep-parents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let change = |p: PathBuf, ft| Change { path: p, upper: PathBuf::from("/upper"), kind: Kind::Added, file_type: ft, size: 0, conflict: false, ignored: false };
        let (new, deeper) = (dir.join("new"), dir.join("new/deeper"));
        let file = deeper.join("a.txt");
        let changes = vec![change(new.clone(), FileType::Dir), change(deeper.clone(), FileType::Dir), change(file.clone(), FileType::File)];
        let order: Vec<PathBuf> = with_new_parents(&changes, vec![(file.clone(), Some("h".into()))]).into_iter().map(|(p, _)| p).collect();
        assert_eq!(order, [new.clone(), deeper.clone(), file.clone()]);
        // a folder already on the host is not touched, nor anything above it
        std::fs::create_dir_all(&new).unwrap();
        let order: Vec<PathBuf> = with_new_parents(&changes, vec![(file.clone(), None)]).into_iter().map(|(p, _)| p).collect();
        assert_eq!(order, [deeper, file]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
