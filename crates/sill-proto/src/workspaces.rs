//! Workspaces (`sill.workspaces: 1`, docs/parallel-coding.md §12): a session's own clone of each
//! repository it pins, shown in its jail at the repository's path. The person applies the agent's
//! work into their checkout as often as they like, or discards it (Undo for 7 days).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host::Operation;
use crate::sessions::HostPath;

pub const CAPABILITY: &str = "sill.workspaces";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Its session has not ended: it may be working in it.
    Open,
    /// Its session ended with work the person has not applied or discarded.
    Result,
}

/// What the person saw: Apply and Discard name it, and a different current one is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Revision {
    /// The workspace branch's tip.
    pub tip: String,
    /// A fingerprint of the uncommitted state; empty when clean.
    pub worktree: String,
    /// The checkout's branch an Apply moves, and its tip when this revision was taken.
    pub destination_branch: String,
    pub destination_tip: String,
}

/// An operation on the workspace that stopped part way or that a restart interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceOperation {
    /// `apply`, `rebase`.
    pub kind: String,
    /// `interrupted` (its effect could not be established; shown, never redone) or `conflict`.
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub session_id: String,
    /// The repository as the person and the jail know it.
    pub source: HostPath,
    pub source_branch: String,
    pub base: String,
    pub branch: String,
    /// Where the clone is on the host.
    pub path: HostPath,
    pub state: WorkspaceState,
    pub operation: Option<WorkspaceOperation>,
    /// e.g. `shallow_source`.
    pub limitations: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Work the person discarded, kept for Undo until `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Discarded {
    pub discard_id: String,
    pub workspace_id: String,
    pub name: String,
    pub source: HostPath,
    pub until: String,
    pub created_at: String,
}

/// What is not applied yet: the workspace against what the person already has. Line counts are
/// decimal strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub commits: u32,
    pub files: u32,
    pub added: String,
    pub removed: String,
    /// Uncommitted paths, untracked included (applied too, as uncommitted changes).
    pub uncommitted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    /// Only this session's workspaces.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Listed {
    pub workspaces: Vec<Workspace>,
    /// Each workspace's current state, one look at the daemon: absent for a workspace whose folder
    /// or repository is gone.
    #[serde(default)]
    pub details: Vec<Detail>,
    /// Discarded work still undoable (first page only).
    pub discarded: Vec<Discarded>,
    pub next: Option<String>,
}

/// A session's pinned repositories as its jail has them: each a workspace, or live with why.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    /// True when the session works in workspaces (Automatic workspaces, or chosen at its start).
    pub workspaces_mode: bool,
    pub workspaces: Vec<Workspace>,
    /// Each workspace's current state, one look at the daemon: absent for a workspace whose folder
    /// or repository is gone.
    #[serde(default)]
    pub details: Vec<Detail>,
    /// Repositories the session pins live although it works in workspaces: the reason.
    pub live: Vec<LiveRepository>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveRepository {
    pub source: HostPath,
    /// `submodules`, `lfs`… (docs/parallel-coding.md §10 item 9).
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionKey {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Detail {
    pub workspace: Workspace,
    pub revision: Revision,
    /// Not applied yet.
    pub summary: Summary,
    /// The checkout moved past what the workspace started from: Apply rebases first (prepare).
    pub needs_rebase: bool,
    /// The person's own uncommitted files in the checkout that the workspace does not have: a
    /// workspace starts from HEAD (docs/parallel-coding.md §5). The agent's applied work is not
    /// counted.
    #[serde(default)]
    pub checkout_only: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Against {
    /// What the person already has: what an Apply would bring.
    Applied,
    /// Where the workspace started.
    Base,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffRequest {
    pub workspace_id: String,
    pub against: Against,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiffText {
    pub text: String,
    /// The diff exceeded 4 MiB and was cut there.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Operate {
    pub operation_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Apply {
    pub operation_id: String,
    pub workspace_id: String,
    /// The revision the person saw.
    pub revision: Revision,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Undo {
    pub operation_id: String,
    pub discard_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Prepared {
    pub workspace: Workspace,
    pub revision: Revision,
    /// The result still does not descend from the checkout (the rebase stopped or could not run).
    pub needs_rebase: bool,
    /// The rebase stopped on these files: resolve them in the workspace or abort.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Changed {
    /// Absent when the workspace's folder was freed (its session ended with nothing left).
    pub workspace: Option<Workspace>,
    pub operation: Option<Operation>,
    /// A Discard's record, for its Undo.
    pub discarded: Option<Discarded>,
}

/// `_sill/workspaces.changed`: refresh the workspace (invalidation only).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Notice {
    pub workspace_id: String,
}
