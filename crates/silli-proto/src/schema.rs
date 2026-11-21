//! `silli daemon schema` (docs/daemon.md decision 7, docs/protocol.md): the whole protocol as one
//! JSON document — every method with the type of its params and its result, every notification,
//! and the JSON Schema (draft 2020-12) of every type under `$defs` — for a frontend in another
//! language to generate from. [`METHODS`] is also what keeps the method table in docs/protocol.md
//! honest: a method the daemon answers must be listed here.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Value, json};

use crate::method;

/// Request (answered) or notification (no id, no answer; `from` says who sends it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    /// Daemon → every attached frontend.
    Notification,
}

pub struct Method {
    pub name: &'static str,
    pub kind: Kind,
    pub doc: &'static str,
    /// The params type's schema, or None when the method takes none.
    params: Option<fn(&mut SchemaGenerator) -> Schema>,
    /// The result type's schema; None for a notification or an empty `{}` answer.
    result: Option<fn(&mut SchemaGenerator) -> Schema>,
    /// The capability (and its version) a client must have been answered to call it: every
    /// method added after the freeze has one.
    capability: Option<(&'static str, u32)>,
}

const fn req<P: JsonSchema, R: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: Some(|g| g.subschema_for::<R>()), capability: None }
}

/// A request a client may call only when the daemon answered `capability` at `version` or later.
const fn req_needing<P: JsonSchema, R: JsonSchema>(name: &'static str, capability: &'static str, version: u32, doc: &'static str) -> Method {
    Method { capability: Some((capability, version)), ..req::<P, R>(name, doc) }
}

/// A request answered with `{}`.
const fn req_empty<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: None, capability: None }
}

/// A request answered with `{}`, callable only when the daemon answered `capability` at `version` or later.
const fn req_empty_needing<P: JsonSchema>(name: &'static str, capability: &'static str, version: u32, doc: &'static str) -> Method {
    Method { capability: Some((capability, version)), ..req_empty::<P>(name, doc) }
}

/// A notification sent only to connections the daemon answered `capability` at `version` or later.
const fn notification_needing<P: JsonSchema>(name: &'static str, capability: &'static str, version: u32, doc: &'static str) -> Method {
    Method { capability: Some((capability, version)), ..notification::<P>(name, doc) }
}

const fn notification<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Notification, doc, params: Some(|g| g.subschema_for::<P>()), result: None, capability: None }
}

/// Every method the daemon answers and every notification it sends, in the order of docs/protocol.md.
pub const METHODS: &[Method] = &[
    req_needing::<crate::Empty, Vec<crate::storage::SessionUsage>>(
        method::STORAGE_SESSIONS,
        "silli.storage",
        1,
        "Measure session-owned directories and report reclaim eligibility.",
    ),
    req_empty_needing::<crate::storage::Reclaim>(
        method::STORAGE_RECLAIM,
        "silli.storage",
        1,
        "Reclaim an ended session's jail after its work has been settled.",
    ),
    req_needing::<crate::Empty, crate::storage::Space>(
        method::STORAGE_SPACE,
        "silli.storage",
        1,
        "Read used, total and available bytes on the daemon host home and state filesystems.",
    ),
    req_needing::<crate::Empty, crate::storage::Caches>(
        method::STORAGE_CACHES,
        "silli.storage",
        1,
        "Scan known session build caches; active sessions cannot be cleared.",
    ),
    req_empty_needing::<crate::storage::ClearBuildCache>(
        method::STORAGE_CLEAR_BUILD_CACHE,
        "silli.storage",
        1,
        "Detach and remove only the known build cache of an inactive session; read caches again after uncertainty before repeating.",
    ),
    req_needing::<crate::crosscheck::Empty, crate::crosscheck::Reviewers>(
        method::CROSSCHECK_REVIEWERS,
        "silli.crosscheck",
        1,
        "Read the saved Reviewers and, for every agent that can review, the options its latest session reported.",
    ),
    req_needing::<crate::crosscheck::SetReviewers, crate::crosscheck::Reviewers>(
        method::CROSSCHECK_SET_REVIEWERS,
        "silli.crosscheck",
        1,
        "Save the Reviewers. Read them after uncertainty before repeating.",
    ),
    req_needing::<crate::crosscheck::WorkspaceKey, crate::crosscheck::Current>(
        method::CROSSCHECK_GET,
        "silli.crosscheck",
        1,
        "Read a workspace's review, when it has one, and whether its files are still the ones reviewed.",
    ),
    req_needing::<crate::crosscheck::Start, crate::crosscheck::Run>(
        method::CROSSCHECK_START,
        "silli.crosscheck",
        1,
        "Capture the workspace's work and start a Reviewer on it, replacing the workspace's earlier review. The same operation_id answers with the review it started; never replay uncertain work under a new one.",
    ),
    req_empty_needing::<crate::crosscheck::RunKey>(
        method::CROSSCHECK_REMOVE,
        "silli.crosscheck",
        1,
        "Remove a review: one under way is cancelled and its Reviewer stopped. A review already gone is not an error.",
    ),
    notification_needing::<crate::crosscheck::Notice>(
        method::CROSSCHECK_CHANGED,
        "silli.crosscheck",
        1,
        "A workspace's review changed or was removed; read it again. Sent only to connections answered silli.crosscheck.",
    ),
    req_empty::<crate::memory::TextClose>(method::MEMORY_TEXT_CLOSE, "Release this connection's immutable memory text snapshot early."),
    req_empty::<crate::sessions::SessionKey>(
        method::SESSIONS_DELETE,
        "Delete one exact session. Inspect existence after uncertainty; never replay against a replacement identity.",
    ),
    req_empty::<crate::sessions::SessionKey>(method::SESSIONS_RESET, "Explicitly replace a conversation generation. Never blindly retry after uncertainty."),
    req::<crate::sessions::Log, crate::Text>(method::SESSIONS_LOG, "Read a bounded daemon-host session log tail."),
    req::<crate::discovery::Find, crate::discovery::Found>(
        method::FILES_FIND,
        "Read daemon-host file/repository information with exact path bytes and bounded results.",
    ),
    req::<crate::discovery::PathKey, crate::discovery::Root>(
        method::REPOS_ROOT,
        "The repository containing the path: the nearest directory with `.git`, the path itself when it is a directory; null outside any repository.",
    ),
    req::<crate::discovery::PathKey, crate::discovery::HeadResult>(
        method::REPOS_HEAD,
        "Read daemon-host file/repository information with exact path bytes and bounded results.",
    ),
    req::<crate::discovery::SummaryKey, crate::discovery::SummaryResult>(
        method::REPOS_SUMMARY,
        "Read daemon-host file/repository information with exact path bytes and bounded results.",
    ),
    req::<crate::discovery::ChangedLines, crate::discovery::Lines>(
        method::REPOS_CHANGED_LINES,
        "Read daemon-host file/repository information with exact path bytes and bounded results.",
    ),
    req::<crate::discovery::Status, crate::discovery::StatusPage>(
        method::REPOS_STATUS,
        "Read daemon-host file/repository information with exact path bytes and bounded results.",
    ),
    req::<crate::Empty, crate::memory::Compacted>(
        method::MEMORY_COMPACT,
        "Apply configured compaction and retention; inspect memory after uncertainty before retrying.",
    ),
    req::<crate::Empty, crate::memory::Forgotten>(
        method::MEMORY_FORGET,
        "Delete ended sessions; do not blindly retry because newly ended sessions may appear.",
    ),
    req::<crate::admin::SessionImage, crate::admin::FileBytes>(
        method::FILES_IMAGE,
        "Read an image visible through the session filesystem view in bounded chunks; revision detects concurrent edits.",
    ),
    req::<crate::admin::DeniedList, crate::admin::NetworkDenied>(
        method::POLICY_DENIED,
        "Read network denials and their acknowledgement state (4 MiB response bound).",
    ),
    req_empty_needing::<crate::admin::NetworkHost>(
        method::POLICY_QUIET_HOST,
        "silli.network_quiet",
        1,
        "Hide refusal banners for an exact hostname across sessions; network access is unchanged.",
    ),
    req_empty::<crate::admin::NetworkHost>(method::POLICY_REMEMBER_DENIED, "Acknowledge a denial; inspect denied state after uncertainty before retrying."),
    req::<crate::admin::NetworkHost, crate::admin::AllowedHost>(
        method::POLICY_ALLOW_HOST,
        "Approve a concrete host for this session only; policy limits apply. Inspect policy after uncertainty.",
    ),
    req_needing::<crate::admin::DesktopCapture, crate::admin::DesktopFrame>(
        method::DESKTOP_CAPTURE,
        "silli.desktop_view",
        1,
        "Take a picture of the session's test desktop (Desktop development), on the host, outside the jail. A new one is kept for `_silli/desktop.read`; when it is still the `since` revision nothing is encoded and `changed` is false. `not_found` when the session has no test desktop running.",
    ),
    req_needing::<crate::admin::DesktopRead, crate::admin::FileBytes>(
        method::DESKTOP_READ,
        "silli.desktop_view",
        1,
        "Read the latest test desktop picture (PNG) in chunks of at most 1 MiB; `conflict` when a newer capture replaced that revision.",
    ),
    req_needing::<crate::admin::NetworkHost, crate::admin::SavedHost>(
        method::POLICY_SAVE_HOST,
        "silli.permission_modes",
        1,
        "Save for future sessions: add a host pattern to the session agent's network defaults. Existing sessions and their grants are unchanged.",
    ),
    req_needing::<crate::admin::PermissionChoice, crate::admin::PermissionsChanged>(
        method::PERMISSIONS_SET,
        "silli.permission_modes",
        1,
        "Remember a contained capability choice for this session within policy limits; blocked widening is refused. Inspect the preview after uncertainty.",
    ),
    req_needing::<crate::sessions::SessionKey, crate::admin::PermissionsChanged>(
        method::PERMISSIONS_APPLY_DEVELOPMENT,
        "silli.permission_modes",
        1,
        "Apply development permissions: remember the test desktop and the private bus as session choices together; refused as a whole, recording nothing, when a limit or a missing backend blocks either.",
    ),
    req_needing::<crate::host_control::Activate, crate::host_control::State>(
        method::HOST_CONTROL_ACTIVATE,
        "silli.host_control",
        1,
        "Activate Host control for a running session with exactly these operations, or change an active one's scope: a new generation each time. Refused for ghosts, a policy that forbids it and a stopped session; never a saved choice.",
    ),
    req_needing::<crate::sessions::SessionKey, crate::host_control::State>(
        method::HOST_CONTROL_REVOKE,
        "silli.host_control",
        1,
        "End the session's Host control activation; a no-op without one. Host processes it launched are stopped, except those marked to keep.",
    ),
    req_needing::<crate::host_control::Keep, crate::Empty>(
        method::HOST_CONTROL_KEEP,
        "silli.host_control",
        1,
        "Mark a bridge-launched host process (by its process-list name, host:<id>) to keep running after Host control ends, or not.",
    ),
    req_needing::<crate::sessions::SessionKey, crate::host_control::State>(
        method::HOST_CONTROL_STATE,
        "silli.permission_modes",
        1,
        "A session's Host control state: active, generation, operations, displaying frontends, the last end and the policy's ban. Session records carry the active brief for every client.",
    ),
    req_needing::<crate::sessions::SessionKey, crate::admin::PermissionsChanged>(
        method::PERMISSIONS_RESTORE_DEFAULTS,
        "silli.permission_modes",
        1,
        "Restore defaults: remove this session's added capability and host grants, keeping its policy and pins. A running jail keeps its launch state until restarted.",
    ),
    notification::<crate::Empty>(method::CHANGES_CHANGED, "Filesystem changes changed; refresh bounded public pages."),
    req_needing::<crate::workspaces::List, crate::workspaces::Listed>(
        method::WORKSPACES_LIST,
        "silli.workspaces",
        1,
        "Workspaces, newest first, in live keyset pages (1–128), optionally one session's; the first page also lists discarded work still undoable. Reading settles: a workspace whose session ended is a result, or is freed when nothing is left to apply.",
    ),
    req_needing::<crate::workspaces::SessionKey, crate::workspaces::SessionView>(
        method::WORKSPACES_SESSION,
        "silli.workspaces",
        1,
        "A session's pinned repositories as its jail has them: whether it works in workspaces, each workspace (its clone stands at the repository's path in the jail), and repositories kept live with why.",
    ),
    req_needing::<crate::workspaces::Key, crate::workspaces::Detail>(
        method::WORKSPACES_GET,
        "silli.workspaces",
        1,
        "A workspace with its current revision and what an Apply would bring (since the last Apply), through the hardened Git view.",
    ),
    req_needing::<crate::workspaces::DiffRequest, crate::workspaces::DiffText>(
        method::WORKSPACES_DIFF,
        "silli.workspaces",
        1,
        "The workspace's working tree against what the person already has, or against where it started; at most 4 MiB.",
    ),
    req_needing::<crate::workspaces::Operate, crate::workspaces::Prepared>(
        method::WORKSPACES_PREPARE,
        "silli.workspaces",
        1,
        "When the person's checkout moved, rebase the workspace onto it in a helper jail with the session's restrictions — its uncommitted work committed first, as for an Apply (refused as uncommitted while the session's agent can commit it) — and return the revision an Apply would bring. A rebase that stops leaves conflicts (resolve in the workspace, or abort).",
    ),
    req_needing::<crate::workspaces::Apply, crate::workspaces::Changed>(
        method::WORKSPACES_APPLY,
        "silli.workspaces",
        1,
        "Bring the workspace's commits into the person's checkout by fast-forward, exactly this revision; repeatable, the workspace goes on. Commits only: uncommitted work is refused as uncommitted while the session's agent can commit it, and committed by silli as the person otherwise. Refusals: invalid_state with reason stale_revision, source_moved, source_overlap, untracked_collision, uncommitted, no_identity, workspace_detached, writer_active, rebase_in_progress.",
    ),
    req_needing::<crate::workspaces::Apply, crate::workspaces::Changed>(
        method::WORKSPACES_COPY,
        "silli.workspaces",
        1,
        "Apply without commit: write the workspace's whole state, committed or not, into the person's checkout as uncommitted changes for them to try, exactly this revision. No commit is made on either side and the workspace keeps its work for a later apply; repeatable. Refusals: invalid_state with reason stale_revision, source_moved, source_overlap, untracked_collision, workspace_detached, writer_active, rebase_in_progress.",
    ),
    req_needing::<crate::workspaces::Apply, crate::workspaces::Branched>(
        method::WORKSPACES_BRANCH,
        "silli.workspaces",
        1,
        "Apply as branch: make a new branch in the person's checkout at the workspace's commits, exactly this revision's work, without switching to it: the checkout's branch, index and files are untouched. The name is the branch the agent works on, or the session's title in snake_case when the workspace is on silli's own branch. Commits only, as for apply. Repeating it with the branch already there at those commits does nothing. Refusals: invalid_state with reason branch_exists (the name holds other commits; never moved), stale_revision, uncommitted, no_identity, workspace_detached, writer_active, rebase_in_progress.",
    ),
    req_needing::<crate::workspaces::Apply, crate::workspaces::Changed>(
        method::WORKSPACES_DISCARD,
        "silli.workspaces",
        1,
        "Throw away what the workspace has beyond the last Apply, exactly this revision; the work is kept 7 days for Undo.",
    ),
    req_needing::<crate::workspaces::Undo, crate::workspaces::Changed>(
        method::WORKSPACES_UNDO,
        "silli.workspaces",
        1,
        "Put discarded work back: into its workspace when nothing happened there since (changed_since otherwise), or into a workspace of its own when that is gone.",
    ),
    req_needing::<crate::workspaces::Operate, crate::workspaces::Changed>(
        method::WORKSPACES_REMOVE,
        "silli.workspaces",
        1,
        "Remove a workspace that cannot be read (its operation is of kind read) or whose folder or repository is gone: its folder and record go with whatever work the folder holds, and there is no Undo. Refusals: invalid_state with reason session_active (its session has not ended), readable (apply or discard it instead).",
    ),
    req_needing::<crate::workspaces::Operate, crate::workspaces::Changed>(
        method::WORKSPACES_ABORT,
        "silli.workspaces",
        1,
        "Abort a rebase that prepare left stopped, in the same helper jail.",
    ),
    notification_needing::<crate::workspaces::Notice>(
        method::WORKSPACES_CHANGED,
        "silli.workspaces",
        1,
        "A workspace changed; refresh it. Sent only to connections answered silli.workspaces.",
    ),
    req::<crate::changes::List, crate::changes::Page>(
        method::CHANGES_LIST,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::Diff, crate::changes::DiffPage>(
        method::CHANGES_DIFF,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::Keep, crate::changes::Done>(
        method::CHANGES_KEEP,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::Revert, crate::changes::Done>(
        method::CHANGES_REVERT,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::Undo, crate::changes::Done>(
        method::CHANGES_UNDO,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::GitRevert, crate::Text>(
        method::CHANGES_GIT_REVERT,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::changes::History, crate::changes::HistoryPage>(
        method::CHANGES_HISTORY,
        "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry.",
    ),
    req::<crate::memory::List, crate::memory::Page>(
        method::MEMORY_LIST,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::memory::TextRead, crate::memory::TextPage>(
        method::MEMORY_TEXT,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::memory::Search, crate::memory::Hits>(
        method::MEMORY_SEARCH,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::memory::NotesList, crate::memory::Notes>(
        method::MEMORY_NOTES_LIST,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::memory::AddNote, crate::Empty>(
        method::MEMORY_NOTES_ADD,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::memory::RemoveNote, crate::Empty>(
        method::MEMORY_NOTES_REMOVE,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::Empty, crate::Empty>(
        method::MEMORY_REFRESH,
        "Read live bounded memory pages; after uncertain mutations inspect current state before retrying.",
    ),
    req::<crate::agent_admin::List, crate::agent_admin::Page>(
        method::AGENTS_LIST,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Definition>(
        method::AGENTS_GET,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::Definition, crate::agent_admin::Saved>(
        method::AGENTS_SAVE,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Removed>(
        method::AGENTS_REMOVE,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Available>(
        method::AGENTS_AVAILABLE,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::AgentKey, crate::wire::AgentSettings>(
        method::AGENTS_DEFAULTS_GET,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::Defaults, crate::Empty>(
        method::AGENTS_DEFAULTS_SET,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req_needing::<crate::agent_admin::AgentKey, crate::agent_admin::Discovered>(
        method::AGENTS_DISCOVERED,
        "silli.agent_setup",
        1,
        "What the agent's last first-start probe created under an empty home: proposals for its home_paths, or none when no probe has run.",
    ),
    req::<crate::Empty, crate::Empty>(
        method::REGISTRY_REFRESH,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req_needing::<crate::Empty, crate::providers::Listed>(
        method::PROVIDERS_LIST,
        "silli.providers",
        1,
        "Model providers on the daemon host, each with whether a key is saved (never the key) and the host:port sessions may reach, and the agents the daemon hands providers to.",
    ),
    req_needing::<crate::providers::Save, crate::providers::Saved>(
        method::PROVIDERS_SAVE,
        "silli.providers",
        1,
        "Write a provider record; a key, when given, replaces the saved one. Running sessions of agents that use providers become outdated. Inspect the list after uncertainty before retrying.",
    ),
    req_needing::<crate::providers::Key, crate::providers::Removed>(
        method::PROVIDERS_REMOVE,
        "silli.providers",
        1,
        "Remove a provider record and its key. A provider already gone is not an error.",
    ),
    req_needing::<crate::providers::Test, crate::providers::Tested>(
        method::PROVIDERS_TEST,
        "silli.providers",
        1,
        "Ask an endpoint for its models from the daemon host, with the given key or the one saved for provider_id. Changes nothing; fails with the endpoint's own error.",
    ),
    req_needing::<crate::providers::DetectLimits, crate::providers::DetectedLimits>(
        method::PROVIDERS_DETECT_LIMITS,
        "silli.provider_limits",
        1,
        "Read Ollama's active context allocation for one model, from the same host and path prefix as the provider; does not load or modify models.",
    ),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Installation>(
        method::REGISTRY_INSTALLED,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::EntryKey, crate::Empty>(
        method::REGISTRY_UNINSTALL,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::agent_admin::EntryKey, crate::agent_admin::Icon>(
        method::REGISTRY_ICON,
        "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation.",
    ),
    req::<crate::pins::List, crate::pins::Page>(method::PINS_LIST, "List global or session pins in bounded live byte-ordered pages."),
    req::<crate::pins::GlobalPin, crate::pins::Pinned>(method::PINS_ADD, "Ensure global pin membership; returns the canonical daemon-host path."),
    req_empty::<crate::pins::GlobalPin>(method::PINS_REMOVE, "Ensure a path is not globally pinned; inspect membership after uncertainty."),
    req_empty::<crate::pins::SessionPin>(method::SESSIONS_PIN, "Ensure session pin membership; inspect membership after uncertainty."),
    req_empty::<crate::pins::SessionPin>(method::SESSIONS_UNPIN, "Ensure a path is not pinned in a session; inspect membership after uncertainty."),
    req::<crate::host::OperationKey, crate::host::Operation>(
        method::OPERATIONS_GET,
        "Recover durable login/install status after disconnect or restart. Unknown outcomes must not be replayed.",
    ),
    req::<crate::Empty, crate::sessions::HostPath>(method::FILES_HOME, "Read the daemon host home directory."),
    req::<crate::admin::FileKey, crate::admin::FileMetadata>(method::FILES_METADATA, "Read metadata of an exact daemon-host path."),
    req::<crate::admin::FilesList, crate::admin::FilesPage>(method::FILES_LIST, "List directory children in bounded live byte-ordered pages."),
    req::<crate::admin::FileRead, crate::admin::FileBytes>(
        method::FILES_READ,
        "Read at most 1 MiB from a regular daemon-host file; chunks are not an immutable snapshot.",
    ),
    req::<crate::admin::FileWrite, crate::Empty>(
        method::FILES_WRITE,
        "Atomically replace a daemon-host file with at most 1 MiB. Do not blindly retry; read the file after uncertainty.",
    ),
    req::<crate::Empty, crate::admin::Settings>(method::SETTINGS_GET, "Read daemon settings."),
    req_needing::<crate::admin::ActiveTimeQuery, crate::admin::ActiveTime>(
        method::USAGE_ACTIVE_TIME,
        "silli.usage",
        1,
        "Read the time agents worked per UTC hour over the last days, summed over all sessions including deleted ones.",
    ),
    req::<crate::admin::SettingsPatch, crate::Empty>(
        method::SETTINGS_PATCH,
        "Apply named settings changes. Read settings after an uncertain outcome before retrying.",
    ),
    req::<crate::Empty, crate::Text>(method::SETTINGS_INSTRUCTIONS_GET, "Read global instructions."),
    req::<crate::admin::Text, crate::Empty>(
        method::SETTINGS_INSTRUCTIONS_SET,
        "Replace global instructions (at most 1 MiB). Read back after uncertainty before retrying.",
    ),
    req::<crate::admin::NamesList, crate::admin::Names>(method::POLICY_LIST, "List available daemon policy names (4 MiB result bound)."),
    req::<crate::admin::PolicyKey, crate::admin::PolicyEntry>(method::POLICY_GET, "Read a named daemon policy."),
    req::<crate::sessions::SessionKey, crate::admin::PolicyView>(
        method::POLICY_PREVIEW,
        "Preview next-build session rules and diagnostics; this view is not enforced by a running jail.",
    ),
    req::<crate::sessions::SessionKey, crate::admin::PolicySnapshot>(
        method::POLICY_SNAPSHOT,
        "Inspect retained launch rules; null means no snapshot was recorded.",
    ),
    req::<crate::admin::PolicyCheck, crate::admin::PolicyChecked>(
        method::POLICY_CHECK,
        "Validate at most 1 MiB of transferred complete-policy text without installation or grants.",
    ),
    req::<crate::admin::PolicyImport, crate::admin::PolicyEntry>(
        method::POLICY_IMPORT,
        "Install at most 1 MiB of complete policy text in the daemon host user policy directory. Explicit replace permits overwrite; inspect named policy after uncertain replies.",
    ),
    req::<crate::admin::ProcessesList, crate::admin::ProcessesPage>(method::PROCS_LIST, "List managed session processes in bounded live name-ordered pages."),
    req::<crate::admin::ProcessTail, crate::Text>(method::PROCS_TAIL, "Read at most 10000 lines and 4 MiB of managed-process output."),
    req::<crate::admin::ProcessKey, crate::Empty>(
        method::PROCS_STOP,
        "Request stop of one named process. List its state after an uncertain outcome; do not blindly retry.",
    ),
    req_needing::<crate::sessions::SessionKey, crate::admin::JailProcesses>(
        method::PROCS_RUNNING,
        "silli.jail_processes",
        1,
        "What runs in the session's jail now: the commands the agent started, each tree once at its root, and services (managed processes and programs listening on a port). A reading of this moment; empty when the jail is not running.",
    ),
    req_needing::<crate::admin::CommandKey, crate::Empty>(
        method::PROCS_STOP_COMMAND,
        "silli.jail_processes",
        1,
        "Stop a command running in the jail, with everything it started (asked, then made to after 3 s). The pid is the jail's, as _silli/procs.running names it.",
    ),
    req::<crate::admin::ProcessKey, crate::Empty>(method::PROCS_DISMISS, "Dismiss one named process. List after an uncertain outcome; do not blindly retry."),
    req::<crate::sessions::List, crate::sessions::Page>(
        method::SESSIONS_LIST,
        "List sessions in bounded live pages ordered by immutable creation time and identity.",
    ),
    req::<crate::sessions::SessionKey, crate::sessions::Session>(method::SESSIONS_GET, "Read one exact session identity."),
    req::<crate::sessions::Create, crate::sessions::SessionKey>(
        method::SESSIONS_CREATE,
        "Atomically create a session; retry the same create_id and input to recover its identity.",
    ),
    req_empty::<crate::sessions::Update>(method::SESSIONS_UPDATE, "Update one session field. Do not blindly retry after connection loss."),
    req::<crate::sessions::SessionKey, crate::sessions::Started>(method::SESSIONS_START, "Start a jail. Do not blindly retry after connection loss."),
    req_empty::<crate::sessions::SessionKey>(method::SESSIONS_STOP, "Stop the jail, preserving the session. Do not blindly retry after connection loss."),
    req::<crate::sessions::End, crate::sessions::Ending>(
        method::SESSIONS_END,
        "End a session, possibly waiting for its summary. Do not blindly retry after connection loss.",
    ),
    req_empty::<crate::sessions::SessionKey>(method::SESSIONS_CHECKPOINT, "Request a summary. Do not blindly retry after connection loss."),
    req::<crate::host::PortOpen, crate::host::HostStream>(
        method::STREAMS_PORT,
        "Connect to a TCP port in a running session jail; close the byte stream to close the connection. Do not blindly retry.",
    ),
    req_needing::<crate::host::OperationKey, crate::host::HostStream>(
        method::STREAMS_LOGIN_CALLBACK,
        "silli.login_browser",
        1,
        "Connect to the loopback callback port a running login reported in `open_url`, on the daemon host; raw TCP bytes. `not_found` when no such login runs, `invalid_state` when it reported no port or nothing listens. Do not blindly retry.",
    ),
    req_needing::<crate::host::OperationKey, crate::Empty>(
        method::OPERATIONS_CANCEL,
        "silli.login_browser",
        1,
        "Stop a running login (also one run by directory setup): its agent and helpers end and the operation fails with code `cancelled`. `not_found` when no such login runs.",
    ),
    req::<crate::host::AgentLogin, crate::host::JobStream>(
        method::AGENTS_LOGIN,
        "Start host login with tagged progress. Disconnect does not cancel; do not blindly retry.",
    ),
    req_needing::<crate::host::AgentDiscover, crate::host::JobStream>(
        method::AGENTS_DISCOVER,
        "silli.agent_setup",
        1,
        "Start the agent once in a throwaway jail with an empty home and record what it creates as home_paths proposals, with tagged progress. Declares nothing. Disconnect does not cancel; do not blindly retry.",
    ),
    req::<crate::host::AgentReset, crate::host::AgentResetDone>(
        method::AGENTS_RESET,
        "Reset an agent as if newly installed: stop its running sessions, then remove its silli-owned home (login, settings, history, memory), ghost copies and saved API keys. The host's own files are untouched.",
    ),
    req::<crate::host::RegistryInstall, crate::host::JobStream>(
        method::REGISTRY_INSTALL,
        "Install a daemon registry entry with tagged progress. Disconnect does not cancel; do not blindly retry.",
    ),
    req::<crate::host::RegistryList, crate::host::RegistryPage>(
        method::REGISTRY_LIST,
        "List the daemon's registry in bounded live pages, including install selectors and trust source.",
    ),
    req::<crate::terminals::Open, crate::terminals::Terminal>(
        method::TERMINALS_OPEN,
        "Open a daemon-owned terminal in a session's jail or on the daemon host. Do not blindly retry after connection loss: list first.",
    ),
    req::<crate::terminals::List, crate::terminals::Terminals>(method::TERMINALS_LIST, "Running and recently exited terminals, at most 64."),
    req::<crate::terminals::TerminalKey, crate::terminals::Terminal>(method::TERMINALS_GET, "Read one terminal, including its exit."),
    req::<crate::terminals::TerminalKey, crate::terminals::Stream>(
        method::TERMINALS_ATTACH,
        "A byte-stream token: recent output first, then live; closing the stream detaches without ending the terminal.",
    ),
    req_empty::<crate::terminals::Resize>(method::TERMINALS_RESIZE, "Set the terminal's window size."),
    req::<crate::terminals::TerminalKey, crate::terminals::Cwd>(method::TERMINALS_CWD, "A host terminal's current working directory."),
    req_empty::<crate::terminals::TerminalKey>(method::TERMINALS_CLOSE, "Hang up the terminal and forget it; attached streams end."),
    req::<crate::sfp::UploadCreate, crate::sfp::UploadCreated>(method::UPLOADS_CREATE, "Create an immutable session upload with an idempotency key."),
    req::<crate::sfp::UploadGet, crate::sfp::Upload>(method::UPLOADS_GET, "Read an upload's immutable bytes (attachments are referenced, never embedded)."),
    req_empty::<crate::sfp::SessionControl>(
        method::SESSIONS_CONTROL,
        "Change adapter controls; do not blindly retry an uncertain login or retry-session action.",
    ),
    req::<crate::sfp::CancelTurn, crate::sfp::CancellationRequested>(method::SESSIONS_CANCEL, "Request cancellation of one identified active turn."),
    req::<crate::initialize::Initialize, crate::initialize::Initialized>(crate::initialize::METHOD, "Initialize an SFP control connection."),
    req::<crate::sfp::Attach, crate::sfp::Attached>(method::SESSIONS_ATTACH, "Subscribe with a durable cursor or immutable paginated snapshot."),
    req::<crate::sfp::SnapshotRequest, crate::sfp::SnapshotPage>(method::SESSIONS_SNAPSHOT, "Read a connection-bound immutable snapshot page."),
    req_empty::<crate::sfp::Detach>(method::SESSIONS_DETACH, "Detach this subscription without cancelling work."),
    req::<crate::sfp::Submit, crate::sfp::Submission>(method::SESSIONS_SUBMIT, "Durably accept an idempotent submission; queue during work."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>(method::SUBMISSIONS_GET, "Recover the outcome of a submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>(method::SUBMISSIONS_CANCEL, "Cancel a knowingly unsent queued submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>(method::SUBMISSIONS_RESUME, "Resume a held queued submission, retaining acceptance order."),
    req_needing::<crate::sfp::SubmissionKey, crate::sfp::Submission>(
        method::SUBMISSIONS_SEND_NOW,
        "silli.send_now",
        1,
        "Send a queued submission next, stopping the active turn.",
    ),
    req::<crate::sfp::AnswerQuestion, crate::sfp::Question>(method::QUESTIONS_ANSWER, "Record the first valid answer with a durable idempotency key."),
    notification::<crate::sfp::SessionEvent>(method::SESSION_EVENT, "A committed ordered session event."),
    req::<crate::Empty, Value>(method::DAEMON_SCHEMA, "The Silli Frontend Protocol schema and method capabilities, for remote negotiation."),
    req::<crate::Empty, crate::DaemonStatus>(method::DAEMON_STATUS, "Version, protocol, pid, mode and session counts of the daemon."),
    req_empty::<crate::Empty>(method::DAEMON_SHUTDOWN, "Ask the daemon to leave; sessions keep running and the next daemon adopts them."),
];

/// The protocol as one document: `methods`, each with `params` and `result` as `$ref`s into
/// `$defs`, where every wire type's schema is.
pub fn document() -> Value {
    let mut g = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let methods: Vec<Value> = METHODS
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "kind": match m.kind { Kind::Request => "request", Kind::Notification => "notification" },
                "doc": m.doc,
                "capabilities": m.capability.map_or(json!({}), |(name, version)| json!({name: version})),
                "params": m.params.map(|f| f(&mut g).to_value()),
                // an empty answer is `{}` today; like every result it may gain fields (clients ignore
                // unknown fields in what the daemon sends)
                "result": m.result.map(|f| f(&mut g).to_value()).unwrap_or(json!({"type": "object"})),
            })
        })
        .collect();
    let progress = g.subschema_for::<crate::host::Progress>().to_value();
    let error = g.subschema_for::<crate::rpc::Error>().to_value();
    let domain_data = g.subschema_for::<crate::rpc::DomainData>().to_value();
    let details: serde_json::Map<String, Value> = crate::rpc::detail_codes()
        .into_iter()
        .map(|(code, always)| {
            let shape = match code {
                "incompatible" => g.subschema_for::<crate::rpc::IncompatibleDetails>(),
                "stale_answer" => g.subschema_for::<crate::rpc::StaleAnswerDetails>(),
                "limit_exceeded" => g.subschema_for::<crate::rpc::LimitDetails>(),
                "unsupported" => g.subschema_for::<crate::rpc::UnsupportedDetails>(),
                "invalid_state" => g.subschema_for::<crate::rpc::InvalidStateDetails>(),
                other => unreachable!("detail schema for {other}"),
            }
            .to_value();
            (code.to_string(), json!({"required": always, "shape": shape}))
        })
        .collect();
    let capabilities: Vec<Value> = crate::initialize::CAPABILITIES.iter().map(|c| json!({"name": c.name, "version": c.version, "doc": c.doc})).collect();
    let defs = g.take_definitions(true);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Silli Frontend Protocol (SFP)",
        "protocol": crate::initialize::PROTOCOL_ID,
        "limits": crate::initialize::Limits::default(),
        "transport": "JSON-RPC 2.0, one message per line, over the unix socket at $XDG_RUNTIME_DIR/silli/daemon.sock (docs/protocol.md)",
        "capabilities": {"offered": capabilities,
            "rules": "Names are namespace.feature (lowercase ASCII, digits, '_'); versions are cumulative from 1; the daemon answers min(offered, supported) and omits unknown ones. A missing capability disables that feature only; it is never a protocol mismatch."},
        "methods": methods,
        "$defs": defs,
        "errors": {"shape": error, "domain_data": domain_data, "domain_numeric_code": -32000,
            "codes": ["incompatible","not_initialized","not_found","gone","invalid_state","unsupported","busy","id_conflict","stale_answer","invalid_cursor","snapshot_expired","limit_exceeded","permission_denied","operation_failed"],
            "details": details,
            "details_rule": "Codes not listed carry no details in sfp/1. Listed shapes may only gain optional fields; clients ignore unknown fields and unknown codes.",
            "retry": "Unknown codes, timeouts and disconnections never authorize automatic mutation retry. Recover durable outcomes or read current state as specified by the method."},
        "streams": {"host_progress": {"framing": "ndjson", "event": progress, "max_text_bytes": crate::host::MAX_PROGRESS_TEXT, "max_jobs": crate::host::MAX_JOBS}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The freeze guard (docs/protocol.md "Freeze guard"). Breaking the recorded document fails
    /// and cannot be accepted; an additive change fails until recorded with
    /// `SILLI_SFP_SCHEMA_ACCEPT=1`, so every extension shows up in review as a diff of the file.
    #[test]
    fn the_protocol_only_grows_from_the_recorded_sfp_1_document() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/sfp-1.schema.json");
        let current = document();
        let pretty = serde_json::to_string_pretty(&current).unwrap() + "\n";
        let Ok(text) = std::fs::read_to_string(&path) else {
            std::fs::write(&path, pretty).unwrap();
            panic!("recorded a new baseline at {}; review and commit it", path.display());
        };
        let recorded: Value = serde_json::from_str(&text).unwrap();
        let breaks = crate::schema_compat::breaking_changes(&recorded, &current);
        assert!(breaks.is_empty(), "incompatible with the recorded sfp/1 protocol (that is sfp/2):\n  {}", breaks.join("\n  "));
        if recorded != current {
            if std::env::var_os("SILLI_SFP_SCHEMA_ACCEPT").is_some() {
                std::fs::write(&path, pretty).unwrap();
            } else {
                panic!("the protocol changed compatibly; record it with SILLI_SFP_SCHEMA_ACCEPT=1 cargo test -p silli-proto and commit {}", path.display());
            }
        }
    }

    /// docs/protocol.md "Unknown fields": clients ignore fields they don't know in what the daemon
    /// sends, so nothing reachable from a result, a notification, an error or a stream may forbid
    /// them in the schema (a strict client validating against it would refuse a later addition).
    #[test]
    fn what_the_daemon_sends_is_open_to_new_fields() {
        let doc = document();
        let defs = doc["$defs"].as_object().unwrap();
        fn walk<'a>(node: &'a Value, defs: &'a serde_json::Map<String, Value>, seen: &mut Vec<String>, closed: &mut Vec<String>, at: &str) {
            match node {
                Value::Object(o) => {
                    if o.get("additionalProperties") == Some(&Value::Bool(false)) {
                        closed.push(at.to_string());
                    }
                    if let Some(name) = o.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/"))
                        && !seen.iter().any(|s| s == name)
                    {
                        seen.push(name.to_string());
                        walk(&defs[name], defs, seen, closed, name);
                    }
                    for v in o.values() {
                        walk(v, defs, seen, closed, at);
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| walk(v, defs, seen, closed, at)),
                _ => {}
            }
        }
        let (mut seen, mut closed) = (Vec::new(), Vec::new());
        for m in doc["methods"].as_array().unwrap() {
            let name = m["name"].as_str().unwrap();
            if m["kind"] == "notification" {
                walk(&m["params"], defs, &mut seen, &mut closed, name);
            }
            walk(&m["result"], defs, &mut seen, &mut closed, name);
        }
        walk(&doc["errors"], defs, &mut seen, &mut closed, "errors");
        walk(&doc["streams"], defs, &mut seen, &mut closed, "streams");
        closed.sort();
        closed.dedup();
        assert!(closed.is_empty(), "closed to new fields but sent by the daemon: {closed:?}");
    }

    /// Requests stay strict where the type is open because it is a result too.
    #[test]
    fn strict_params_refuse_unknown_fields() {
        let ok: crate::sfp::Answer = crate::rpc::strict(json!({"type": "permission", "option_id": "yes"})).unwrap();
        assert_eq!(ok, crate::sfp::Answer::Permission { option_id: "yes".into() });
        let e = crate::rpc::strict::<crate::sfp::Answer>(json!({"type": "permission", "option_id": "yes", "extra": 1})).unwrap_err();
        assert!(e.message.contains("extra"), "{}", e.message);
        assert!(crate::rpc::strict::<crate::sfp::Answer>(json!({"type": "cancel"})).is_ok());
        let def = json!({"id": "a", "name": "A", "command": ["a"]});
        assert!(crate::rpc::strict::<crate::agent_admin::Definition>(def.clone()).is_ok());
        let mut extra = def;
        extra["typo"] = json!(true);
        assert!(crate::rpc::strict::<crate::agent_admin::Definition>(extra).is_err());
    }

    /// A name in `method` without an entry here could be called and never answered; an entry
    /// here is named through its constant, so the two lists are one.
    #[test]
    fn the_names_and_the_table_are_the_same_list() {
        let mut names: Vec<&str> = crate::method::ALL.to_vec();
        let mut table: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        table.sort_unstable();
        assert_eq!(names, table);
    }

    #[test]
    fn every_method_is_listed_once_and_the_document_builds() {
        let mut names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "a method twice");
        let doc = document();
        assert_eq!(doc["protocol"], json!(crate::initialize::PROTOCOL_ID));
        assert_eq!(doc["methods"].as_array().unwrap().len(), METHODS.len());
        let defs = doc["$defs"].as_object().unwrap();
        for t in ["Event", "Answer", "Session", "DaemonStatus", "Submission", "Question", "HostPath"] {
            assert!(defs.contains_key(t), "{t} missing from $defs");
        }
        // a frontend reads the method table: the attach params say what to send
        let attach = doc["methods"].as_array().unwrap().iter().find(|m| m["name"] == method::SESSIONS_ATTACH).unwrap();
        assert!(attach["params"]["$ref"].as_str().unwrap().ends_with("/Attach"), "{attach}");
    }

    #[test]
    fn error_details_and_capabilities_are_exported() {
        let doc = document();
        let details = doc["errors"]["details"].as_object().unwrap();
        for (code, required) in crate::rpc::detail_codes() {
            assert!(doc["errors"]["codes"].as_array().unwrap().contains(&json!(code)), "{code} not a listed code");
            assert_eq!(details[code]["required"], json!(required), "{code}");
        }
        // a method's capability is one the daemon offers, at a version it has
        let offered = doc["capabilities"]["offered"].as_array().unwrap();
        for m in METHODS.iter().filter_map(|m| m.capability) {
            assert!(offered.iter().any(|c| c["name"] == m.0 && c["version"].as_u64() >= Some(m.1 as u64)), "{m:?}");
        }
        assert!(METHODS.iter().all(|m| !m.name.starts_with("_silli/internal/")));
        // a limit error's data reads back as typed details
        let e = crate::rpc::Error::limit(crate::rpc::LimitScope::Result, Some(4), "too big");
        let data: crate::rpc::DomainData = serde_json::from_value(e.data.unwrap()).unwrap();
        assert!(matches!(
            data.details,
            Some(crate::rpc::DomainDetails::LimitExceeded(crate::rpc::LimitDetails { scope: crate::rpc::LimitScope::Result, maximum: Some(4) }))
        ));
    }

    /// docs/protocol.md is the human reading of this table: a method the daemon answers is in it.
    #[test]
    fn the_protocol_doc_names_every_method() {
        let doc = include_str!("../../../docs/protocol.md");
        let missing: Vec<&str> = METHODS.iter().map(|m| m.name).filter(|n| !doc.contains(&format!("`{n}`"))).collect();
        assert!(missing.is_empty(), "not in docs/protocol.md: {missing:?}");
        assert!(doc.contains(crate::initialize::PROTOCOL_ID));
    }
}
