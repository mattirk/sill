//! Workspaces from a frontend (docs/parallel-coding.md §12): a session's repositories as its jail
//! has them, what each workspace would bring, and the person's Apply, Discard, Undo and Abort.
//! Blocking calls for a frontend's worker thread; every mutation carries a fresh `operation_id`,
//! so a retry of the same request never repeats the work.
use sill_proto::workspaces as api;

use crate::actions::Actions;

/// A session's workspaces and, for each, its current state.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub view: api::SessionView,
    pub details: Vec<api::Detail>,
}

impl Loaded {
    /// The (repository path, workspace folder) pairs the file worker reads through.
    pub fn aliases(&self) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
        self.view
            .workspaces
            .iter()
            .filter_map(|w| Some((w.source.to_path().ok()?, w.path.to_path().ok()?)))
            .collect()
    }

    pub fn detail(&self, workspace_id: &str) -> Option<&api::Detail> {
        self.details.iter().find(|d| d.workspace.workspace_id == workspace_id)
    }
}

/// Whether a workspace has anything to apply, or a stopped operation to finish. A checkout that
/// moved on (`needs_rebase`) is not by itself something to apply: after a Discard, or before the
/// agent's first change, there is nothing to bring however far the checkout has gone.
pub fn pending(d: &api::Detail) -> bool {
    d.summary.commits > 0 || d.summary.files > 0 || d.workspace.operation.is_some()
}

/// The branch the workspace started from and the one the person's checkout is on now, when they
/// differ: Apply would land on the other branch, so the frontend asks first (the person,
/// 2026-11-10: confirm rather than refuse).
pub fn other_branch(d: &api::Detail) -> Option<(&str, &str)> {
    let (from, now) = (d.workspace.source_branch.as_str(), d.revision.destination_branch.as_str());
    (!from.is_empty() && !now.is_empty() && from != now).then_some((from, now))
}

/// What the frontend says before the second click of an Apply onto another branch.
pub fn other_branch_question(from: &str, now: &str) -> String {
    format!("This workspace started from {from}, but your checkout is on {now} now. Apply again to apply onto {now}, or switch your checkout back to {from}.")
}

/// A short line for what an Apply would bring: `3 files +40 −12 · 2 commits`.
pub fn describe(s: &api::Summary) -> String {
    let mut parts = Vec::new();
    if s.files > 0 {
        parts.push(format!("{} file{} +{} −{}", s.files, if s.files == 1 { "" } else { "s" }, s.added, s.removed));
    }
    if s.commits > 0 {
        parts.push(format!("{} commit{}", s.commits, if s.commits == 1 { "" } else { "s" }));
    }
    parts.join(" · ")
}

/// A refusal's text without the method's name the client puts in front of it.
pub fn message(e: &anyhow::Error) -> String {
    e.root_cause().to_string()
}

/// The details the daemon sent with the list, and one `get` for each workspace it left out (a
/// daemon from before the lists carried details).
fn details(
    mut call: impl FnMut(&str) -> anyhow::Result<api::Detail>,
    workspaces: &[api::Workspace],
    sent: Vec<api::Detail>,
    skip_failures: bool,
) -> anyhow::Result<Vec<api::Detail>> {
    let mut details = sent;
    for w in workspaces {
        if details.iter().any(|d| d.workspace.workspace_id == w.workspace_id) {
            continue;
        }
        match call(&w.workspace_id) {
            Ok(d) => details.push(d),
            Err(_) if skip_failures => {}
            Err(e) => return Err(e),
        }
    }
    Ok(details)
}

pub fn load(actions: &Actions, session: &str) -> anyhow::Result<Loaded> {
    actions.reading(|c| {
        let mut view: api::SessionView = c.call("_sill/workspaces.session", api::SessionKey { session_id: session.into() })?;
        let sent = std::mem::take(&mut view.details);
        let details = details(|id| c.call("_sill/workspaces.get", api::Key { workspace_id: id.into() }), &view.workspaces, sent, false)?;
        Ok(Loaded { view, details })
    })
}

/// Every workspace and the discarded work still undoable, with each workspace's state.
pub fn all(actions: &Actions) -> anyhow::Result<(api::Listed, Vec<api::Detail>)> {
    actions.reading(|c| {
        let mut listed: api::Listed = c.call("_sill/workspaces.list", api::List { session_id: None, after: None, limit: Some(128) })?;
        let sent = std::mem::take(&mut listed.details);
        let details = details(|id| c.call("_sill/workspaces.get", api::Key { workspace_id: id.into() }), &listed.workspaces, sent, true)?;
        Ok((listed, details))
    })
}

/// What an Apply would bring, as a unified diff: all of it, or `path`'s alone.
pub fn diff(actions: &Actions, workspace_id: &str, path: Option<&sill_proto::sessions::HostPath>) -> anyhow::Result<String> {
    let d: api::DiffText =
        actions.read("_sill/workspaces.diff", serde_json::json!(api::DiffRequest { workspace_id: workspace_id.into(), against: api::Against::Applied, path: path.cloned() }))?;
    Ok(if d.truncated { format!("{}\n… (cut at 4 MiB)", d.text) } else { d.text })
}

/// How the workspace's work reaches the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Its commits, by fast-forward (`workspaces.apply`).
    Commits,
    /// Its whole state as uncommitted changes, to try (`workspaces.copy`): Apply without commit.
    Uncommitted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// The checkout had moved and the rebase onto it stopped on these files: nothing applied.
    Conflicts(Vec<String>),
    /// The workspace holds uncommitted changes its session's agent can still commit: nothing
    /// applied. Apply brings commits only, so the agent is asked to commit first.
    Uncommitted,
}

/// Whether the daemon refused for `reason` (a refusal's `details.reason`).
fn refused_for(e: &anyhow::Error, reason: &str) -> bool {
    e.downcast_ref::<sill_proto::rpc::Error>()
        .and_then(|e| e.data.as_ref())
        .and_then(|d| d.get("details"))
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str())
        == Some(reason)
}

/// Apply what the person saw (`seen`). When the checkout moved, the workspace is rebased onto it
/// first and, when that goes cleanly, applied in the same click at the rebased revision: the
/// rebase only puts the person's own commits under the agent's work (the person, 2026-11-09:
/// Apply that only rebased looked as if it did nothing). A stopped rebase applies nothing, and
/// neither does uncommitted work the agent is still there to commit ([`ApplyOutcome::Uncommitted`]).
pub fn apply(actions: &Actions, workspace_id: &str, seen: &api::Revision, how: How) -> anyhow::Result<ApplyOutcome> {
    actions.once(|c| match apply_steps(|method, params| c.call(method, params), workspace_id, seen, how) {
        Err(e) if refused_for(&e, "uncommitted") => Ok(ApplyOutcome::Uncommitted),
        other => other,
    })
}

/// [`apply`]'s calls, through `call` (the daemon's methods by name).
fn apply_steps(
    mut call: impl FnMut(&str, serde_json::Value) -> anyhow::Result<serde_json::Value>,
    workspace_id: &str,
    seen: &api::Revision,
    how: How,
) -> anyhow::Result<ApplyOutcome> {
    let d: api::Detail = serde_json::from_value(call("_sill/workspaces.get", serde_json::json!(api::Key { workspace_id: workspace_id.into() }))?)?;
    // the person agreed to land on the branch they saw: a checkout switched since is looked at again
    let switched = || anyhow::anyhow!("Your checkout switched branch since you looked: look again, then apply.");
    if d.revision.destination_branch != seen.destination_branch {
        return Err(switched());
    }
    let mut revision = seen.clone();
    if d.needs_rebase {
        let p: api::Prepared = serde_json::from_value(call(
            "_sill/workspaces.prepare",
            serde_json::json!(api::Operate { operation_id: crate::request_id("prepare"), workspace_id: workspace_id.into() }),
        )?)?;
        if p.needs_rebase || !p.conflicts.is_empty() {
            return Ok(ApplyOutcome::Conflicts(p.conflicts));
        }
        if p.revision.destination_branch != seen.destination_branch {
            return Err(switched());
        }
        revision = p.revision;
    }
    let (method, kind) = match how {
        How::Commits => ("_sill/workspaces.apply", "apply"),
        How::Uncommitted => ("_sill/workspaces.copy", "copy"),
    };
    call(method, serde_json::json!(api::Apply { operation_id: crate::request_id(kind), workspace_id: workspace_id.into(), revision }))?;
    Ok(ApplyOutcome::Applied)
}

/// What Apply as branch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchOutcome {
    /// The branch, made in the checkout (or already there at the same commits).
    Made(String),
    /// Uncommitted work the session's agent can still commit, as for [`ApplyOutcome::Uncommitted`].
    Uncommitted,
}

/// Apply as branch (`workspaces.branch`): the work the person saw as a new branch in their
/// checkout, which stays where it is. No rebase first: the branch need not sit on the checkout's
/// tip, and the checkout's branch is not compared.
pub fn branch(actions: &Actions, workspace_id: &str, seen: &api::Revision) -> anyhow::Result<BranchOutcome> {
    actions.once(|c| {
        match c.call::<api::Branched>("_sill/workspaces.branch", serde_json::json!(api::Apply { operation_id: crate::request_id("branch"), workspace_id: workspace_id.into(), revision: seen.clone() })) {
            Ok(b) => Ok(BranchOutcome::Made(b.branch)),
            Err(e) if refused_for(&e, "uncommitted") => Ok(BranchOutcome::Uncommitted),
            Err(e) => Err(e),
        }
    })
}

pub fn discard(actions: &Actions, workspace_id: &str, seen: &api::Revision) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.discard", api::Apply { operation_id: crate::request_id("discard"), workspace_id: workspace_id.into(), revision: seen.clone() }))
}

pub fn undo(actions: &Actions, discard_id: &str) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.undo", api::Undo { operation_id: crate::request_id("undo"), discard_id: discard_id.into() }))
}

/// Remove a workspace that cannot be read: folder and record, no Undo.
pub fn remove(actions: &Actions, workspace_id: &str) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.remove", api::Operate { operation_id: crate::request_id("remove"), workspace_id: workspace_id.into() }))
}

pub fn abort(actions: &Actions, workspace_id: &str) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.abort", api::Operate { operation_id: crate::request_id("abort"), workspace_id: workspace_id.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(files: u32, commits: u32, uncommitted: &[&str]) -> api::Summary {
        api::Summary { commits, files, added: "40".into(), removed: "12".into(), uncommitted: uncommitted.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn a_summary_reads_as_a_short_line() {
        assert_eq!(describe(&summary(3, 0, &["a", "b", "c"])), "3 files +40 −12");
        assert_eq!(describe(&summary(1, 2, &[])), "1 file +40 −12 · 2 commits");
        assert_eq!(describe(&summary(0, 0, &["already applied"])), "");
        assert_eq!(describe(&summary(0, 0, &[])), "");
    }

    #[test]
    fn a_moved_checkout_alone_is_nothing_to_apply() {
        let path = |p: &str| serde_json::json!(sill_proto::sessions::HostPath::from_path(std::path::Path::new(p)));
        let detail = |files: u32, operation: serde_json::Value| -> api::Detail {
            serde_json::from_value(serde_json::json!({
                "workspace": {"workspace_id": "ws-1", "name": "repo", "session_id": "s", "source": path("/home/me/repo"),
                    "source_branch": "main", "base": "b0", "branch": "sill/repo-ws-1", "path": path("/ws/repo-ws-1"),
                    "state": "open", "operation": operation, "limitations": [], "created_at": "t", "updated_at": "t"},
                "revision": {"tip": "t1", "worktree": "", "destination_branch": "main", "destination_tip": "b1"},
                "summary": {"commits": 0, "files": files, "added": "0", "removed": "0", "uncommitted": []},
                "needs_rebase": true}))
            .unwrap()
        };
        // discarded, and the person's checkout has a commit of its own since
        assert!(!pending(&detail(0, serde_json::Value::Null)));
        assert!(pending(&detail(1, serde_json::Value::Null)));
    }

    fn revision(tip: &str, destination_tip: &str) -> serde_json::Value {
        serde_json::json!({"tip": tip, "worktree": "", "destination_branch": "main", "destination_tip": destination_tip})
    }

    /// The daemon's answers to `apply_steps`, and the calls it got.
    fn run_apply(needs_rebase: bool, conflicts: &[&str]) -> (anyhow::Result<ApplyOutcome>, Vec<(String, serde_json::Value)>) {
        run_apply_as(needs_rebase, conflicts, How::Commits)
    }

    fn run_apply_as(needs_rebase: bool, conflicts: &[&str], how: How) -> (anyhow::Result<ApplyOutcome>, Vec<(String, serde_json::Value)>) {
        let path = |p: &str| serde_json::json!(sill_proto::sessions::HostPath::from_path(std::path::Path::new(p)));
        let workspace = serde_json::json!({"workspace_id": "ws-1", "name": "repo", "session_id": "s", "source": path("/home/me/repo"),
            "source_branch": "main", "base": "b0", "branch": "sill/repo-ws-1", "path": path("/ws/repo-ws-1"),
            "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"});
        let mut calls = Vec::new();
        let seen: api::Revision = serde_json::from_value(revision("t1", "b0")).unwrap();
        let out = apply_steps(
            |method, params| {
                calls.push((method.to_string(), params));
                Ok(match method {
                    "_sill/workspaces.get" => serde_json::json!({"workspace": workspace, "revision": revision("t1", "b1"),
                        "summary": {"commits": 0, "files": 1, "added": "1", "removed": "1", "uncommitted": ["f"]}, "needs_rebase": needs_rebase}),
                    "_sill/workspaces.prepare" => serde_json::json!({"workspace": workspace, "revision": revision("t2", "b1"),
                        "needs_rebase": !conflicts.is_empty(), "conflicts": conflicts}),
                    _ => serde_json::json!({"workspace": workspace, "operation": null}),
                })
            },
            "ws-1",
            &seen,
            how,
        );
        (out, calls)
    }

    #[test]
    fn apply_without_commit_asks_for_a_copy_after_the_same_rebase() {
        let (out, calls) = run_apply_as(false, &[], How::Uncommitted);
        assert_eq!(out.unwrap(), ApplyOutcome::Applied);
        assert_eq!(calls.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["_sill/workspaces.get", "_sill/workspaces.copy"]);
        let (_, calls) = run_apply_as(true, &[], How::Uncommitted);
        assert_eq!(calls.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["_sill/workspaces.get", "_sill/workspaces.prepare", "_sill/workspaces.copy"]);
        assert_eq!(calls[2].1["revision"]["tip"], "t2");
    }

    #[test]
    fn only_the_uncommitted_refusal_asks_for_the_agents_commit() {
        let refusal = |reason: &str| -> anyhow::Error {
            sill_proto::rpc::Error { code: -32000, message: "refused".into(), data: Some(serde_json::json!({"code": "invalid_state", "details": {"reason": reason, "paths": []}})) }.into()
        };
        assert!(refused_for(&refusal("uncommitted"), "uncommitted"));
        assert!(refused_for(&refusal("uncommitted").context("_sill/workspaces.apply"), "uncommitted"), "under the method's context too");
        assert!(!refused_for(&refusal("source_overlap"), "uncommitted"));
        assert!(!refused_for(&anyhow::anyhow!("connection lost"), "uncommitted"));
    }

    #[test]
    fn apply_after_a_clean_rebase_applies_in_the_same_click() {
        let (out, calls) = run_apply(true, &[]);
        assert_eq!(out.unwrap(), ApplyOutcome::Applied);
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["_sill/workspaces.get", "_sill/workspaces.prepare", "_sill/workspaces.apply"]);
        assert_eq!(calls[2].1["revision"]["tip"], "t2", "applied at the rebased revision, not the one seen before it");
    }

    #[test]
    fn a_stopped_rebase_applies_nothing_and_an_unmoved_checkout_applies_what_was_seen() {
        let (out, calls) = run_apply(true, &["f"]);
        assert_eq!(out.unwrap(), ApplyOutcome::Conflicts(vec!["f".into()]));
        assert!(calls.iter().all(|(m, _)| m != "_sill/workspaces.apply"));
        let (out, calls) = run_apply(false, &[]);
        assert_eq!(out.unwrap(), ApplyOutcome::Applied);
        assert_eq!(calls.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(), ["_sill/workspaces.get", "_sill/workspaces.apply"]);
        assert_eq!(calls[1].1["revision"]["tip"], "t1");
    }

    #[test]
    fn a_checkout_on_another_branch_is_named_and_a_switch_since_is_refused() {
        let (out, calls) = run_apply_switched();
        let said = out.unwrap_err().to_string();
        assert!(said.starts_with("Your checkout switched branch"), "{said}");
        assert!(calls.iter().all(|(m, _)| m != "_sill/workspaces.apply" && m != "_sill/workspaces.prepare"));
        let path = |p: &str| serde_json::json!(sill_proto::sessions::HostPath::from_path(std::path::Path::new(p)));
        let detail = |now: &str| -> api::Detail {
            serde_json::from_value(serde_json::json!({
                "workspace": {"workspace_id": "ws-1", "name": "repo", "session_id": "s", "source": path("/home/me/repo"),
                    "source_branch": "main", "base": "b0", "branch": "sill/repo-ws-1", "path": path("/ws/repo-ws-1"),
                    "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"},
                "revision": {"tip": "t1", "worktree": "", "destination_branch": now, "destination_tip": "b1"},
                "summary": {"commits": 1, "files": 1, "added": "0", "removed": "0", "uncommitted": []},
                "needs_rebase": false}))
            .unwrap()
        };
        assert_eq!(other_branch(&detail("main")), None);
        assert_eq!(other_branch(&detail("")), None, "no branch to compare: the daemon refuses on its own");
        assert_eq!(other_branch(&detail("feature")), Some(("main", "feature")));
    }

    /// The person saw `main`; the checkout is on `feature` by the time Apply reads it.
    fn run_apply_switched() -> (anyhow::Result<ApplyOutcome>, Vec<(String, serde_json::Value)>) {
        let mut calls = Vec::new();
        let seen: api::Revision = serde_json::from_value(revision("t1", "b0")).unwrap();
        let path = |p: &str| serde_json::json!(sill_proto::sessions::HostPath::from_path(std::path::Path::new(p)));
        let workspace = serde_json::json!({"workspace_id": "ws-1", "name": "repo", "session_id": "s", "source": path("/home/me/repo"),
            "source_branch": "main", "base": "b0", "branch": "sill/repo-ws-1", "path": path("/ws/repo-ws-1"),
            "state": "open", "operation": null, "limitations": [], "created_at": "t", "updated_at": "t"});
        let out = apply_steps(
            |method, params| {
                calls.push((method.to_string(), params));
                Ok(serde_json::json!({"workspace": workspace,
                    "revision": {"tip": "t1", "worktree": "", "destination_branch": "feature", "destination_tip": "b9"},
                    "summary": {"commits": 1, "files": 1, "added": "1", "removed": "1", "uncommitted": []}, "needs_rebase": true}))
            },
            "ws-1",
            &seen,
            How::Commits,
        );
        (out, calls)
    }
}
