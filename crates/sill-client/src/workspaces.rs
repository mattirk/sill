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

/// Whether a workspace has anything to apply.
pub fn pending(d: &api::Detail) -> bool {
    d.summary.commits > 0 || d.summary.files > 0 || d.needs_rebase || d.workspace.operation.is_some()
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
    /// The checkout had moved: the workspace was rebased onto it. What would be applied changed,
    /// so the person looks again before applying (docs/parallel-coding.md §7).
    Rebased,
    /// The rebase stopped on these files.
    Conflicts(Vec<String>),
}

/// Apply what the person saw (`seen`): rebasing first when the checkout moved.
pub fn apply(actions: &Actions, workspace_id: &str, seen: &api::Revision) -> anyhow::Result<ApplyOutcome> {
    actions.once(|c| {
        let d: api::Detail = c.call("_sill/workspaces.get", api::Key { workspace_id: workspace_id.into() })?;
        if d.needs_rebase {
            let p: api::Prepared = c.call("_sill/workspaces.prepare", api::Operate { operation_id: crate::request_id("prepare"), workspace_id: workspace_id.into() })?;
            return Ok(if p.needs_rebase || !p.conflicts.is_empty() { ApplyOutcome::Conflicts(p.conflicts) } else { ApplyOutcome::Rebased });
        }
        let _: api::Changed = c.call("_sill/workspaces.apply", api::Apply { operation_id: crate::request_id("apply"), workspace_id: workspace_id.into(), revision: seen.clone() })?;
        Ok(ApplyOutcome::Applied)
    })
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
}
