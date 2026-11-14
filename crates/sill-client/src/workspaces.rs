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

pub fn load(actions: &Actions, session: &str) -> anyhow::Result<Loaded> {
    actions.reading(|c| {
        let view: api::SessionView = c.call("_sill/workspaces.session", api::SessionKey { session_id: session.into() })?;
        let mut details = Vec::new();
        for w in &view.workspaces {
            details.push(c.call("_sill/workspaces.get", api::Key { workspace_id: w.workspace_id.clone() })?);
        }
        Ok(Loaded { view, details })
    })
}

/// Every workspace and the discarded work still undoable, with each workspace's state.
pub fn all(actions: &Actions) -> anyhow::Result<(api::Listed, Vec<api::Detail>)> {
    actions.reading(|c| {
        let listed: api::Listed = c.call("_sill/workspaces.list", api::List { session_id: None, after: None, limit: Some(128) })?;
        let mut details = Vec::new();
        for w in &listed.workspaces {
            if let Ok(d) = c.call::<api::Detail>("_sill/workspaces.get", api::Key { workspace_id: w.workspace_id.clone() }) {
                details.push(d);
            }
        }
        Ok((listed, details))
    })
}

/// What an Apply would bring, as a unified diff.
pub fn diff(actions: &Actions, workspace_id: &str) -> anyhow::Result<String> {
    let d: api::DiffText = actions.read("_sill/workspaces.diff", serde_json::json!(api::DiffRequest { workspace_id: workspace_id.into(), against: api::Against::Applied }))?;
    Ok(if d.truncated { format!("{}\n… (cut at 4 MiB)", d.text) } else { d.text })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// The checkout had moved and the rebase onto it stopped on these files: nothing applied.
    Conflicts(Vec<String>),
}

/// Apply what the person saw (`seen`). When the checkout moved, the workspace is rebased onto it
/// first and, when that goes cleanly, applied in the same click at the rebased revision: the
/// rebase only puts the person's own commits under the agent's work (the person, 2026-11-09:
/// Apply that only rebased looked as if it did nothing). A stopped rebase applies nothing.
pub fn apply(actions: &Actions, workspace_id: &str, seen: &api::Revision) -> anyhow::Result<ApplyOutcome> {
    actions.once(|c| apply_steps(|method, params| c.call(method, params), workspace_id, seen))
}

/// [`apply`]'s calls, through `call` (the daemon's methods by name).
fn apply_steps(
    mut call: impl FnMut(&str, serde_json::Value) -> anyhow::Result<serde_json::Value>,
    workspace_id: &str,
    seen: &api::Revision,
) -> anyhow::Result<ApplyOutcome> {
    let d: api::Detail = serde_json::from_value(call("_sill/workspaces.get", serde_json::json!(api::Key { workspace_id: workspace_id.into() }))?)?;
    let mut revision = seen.clone();
    if d.needs_rebase {
        let p: api::Prepared = serde_json::from_value(call(
            "_sill/workspaces.prepare",
            serde_json::json!(api::Operate { operation_id: crate::request_id("prepare"), workspace_id: workspace_id.into() }),
        )?)?;
        if p.needs_rebase || !p.conflicts.is_empty() {
            return Ok(ApplyOutcome::Conflicts(p.conflicts));
        }
        revision = p.revision;
    }
    call(
        "_sill/workspaces.apply",
        serde_json::json!(api::Apply { operation_id: crate::request_id("apply"), workspace_id: workspace_id.into(), revision }),
    )?;
    Ok(ApplyOutcome::Applied)
}

pub fn discard(actions: &Actions, workspace_id: &str, seen: &api::Revision) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.discard", api::Apply { operation_id: crate::request_id("discard"), workspace_id: workspace_id.into(), revision: seen.clone() }))
}

pub fn undo(actions: &Actions, discard_id: &str) -> anyhow::Result<api::Changed> {
    actions.once(|c| c.call("_sill/workspaces.undo", api::Undo { operation_id: crate::request_id("undo"), discard_id: discard_id.into() }))
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
        );
        (out, calls)
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
}
