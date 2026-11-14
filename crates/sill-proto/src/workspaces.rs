//! Workspaces (`sill.workspaces: 1`, docs/parallel-coding.md §11): a session's own clone and
//! branch of a repository, merged into the source or discarded only by the person.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host::Operation;
use crate::sessions::HostPath;

pub const CAPABILITY: &str = "sill.workspaces";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// A session may be working in it.
    Open,
    /// No writer: the result waits for the person.
    Result,
    /// Merged into the source; the clone stays until the person discards it.
    Merged,
    /// Moved aside; Undo restores it until `retained_until`.
    Discarded,
}

/// What the person saw: Merge and Discard name it, and a different current one is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Revision {
    /// The workspace branch's tip.
    pub tip: String,
    /// A fingerprint of the uncommitted state; empty when clean.
    pub worktree: String,
    /// The source branch a Merge would move, and its tip when this revision was taken.
    pub destination_branch: String,
    pub destination_tip: String,
}

/// An operation on the workspace that a restart interrupted or that stopped part way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceOperation {
    /// `create`, `merge`, `discard`, `undo`.
    pub kind: String,
    /// `interrupted`: its effect could not be established; shown, never redone.
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub source: HostPath,
    pub source_branch: String,
    pub base: String,
    pub branch: String,
    pub path: HostPath,
    pub state: WorkspaceState,
    /// The session working in it now; null when nothing writes.
    pub owner_session: Option<String>,
    /// The latest session pinned to it, owner or not.
    pub session_id: Option<String>,
    /// The revision last prepared for Merge.
    pub reviewed: Option<Revision>,
    pub operation: Option<WorkspaceOperation>,
    pub retained_until: Option<String>,
    /// e.g. `shallow_source`.
    pub limitations: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The result against `base` (§6). Counts of lines are decimal strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Summary {
    pub commits: u32,
    pub files: u32,
    pub added: String,
    pub removed: String,
    /// Uncommitted paths, untracked included: not part of a Merge.
    pub uncommitted: Vec<String>,
    /// Ignored paths: outside the result, deleted with the clone.
    pub ignored: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Listed {
    pub workspaces: Vec<Workspace>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Detail {
    pub workspace: Workspace,
    /// Absent once the clone is gone (discarded).
    pub revision: Option<Revision>,
    pub summary: Option<Summary>,
    /// The source moved past the base: a Merge needs the result rebased first.
    pub needs_rebase: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Against {
    Base,
    Destination,
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
pub struct Create {
    pub operation_id: String,
    /// The source repository's top.
    pub source: HostPath,
    pub name: String,
    pub agent: String,
    pub policy: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Created {
    pub workspace: Workspace,
    pub session_id: String,
    /// The source had uncommitted work the workspace does not include (from HEAD only).
    pub source_dirty: bool,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reopen {
    pub operation_id: String,
    pub workspace_id: String,
    /// For a new session when the last one ended; unused while it is open.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
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
pub struct Prepared {
    pub workspace: Workspace,
    pub revision: Revision,
    /// Different from the revision prepared before: the person must look again.
    pub changed: bool,
    pub needs_rebase: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Changed {
    pub workspace: Workspace,
    /// Present for methods that take an `operation_id`.
    pub operation: Option<Operation>,
    /// The session pinned to it (reopen).
    pub session_id: Option<String>,
}

/// `_sill/workspaces.changed`: refresh the workspace (invalidation only).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Notice {
    pub workspace_id: String,
}
