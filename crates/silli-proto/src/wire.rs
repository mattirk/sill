//! A session's conversation and the store's rows as they cross the daemon socket (docs/daemon.md
//! §3.3, docs/protocol.md): what the daemon, the ACP client of every running session, tells the
//! frontends attached to it, what they ask it to do, and the data the other methods carry. Plain
//! data, every type with a JSON Schema (`silli daemon schema`): a frontend in another language
//! generates from it, and no type here knows a jail, a store or the ACP crate.
//!
//! The conversions to and from `silli_core::acp` (an agent's question carries a reply channel there,
//! a request number here) live in `silli_core::wire`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// --- the agent's side, as the frontends see it (ACP, reduced to what silli shows) -----------------

/// What a permission option means (ACP `PermissionOptionKind`, the same on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind this protocol does not know (a newer ACP, or an agent's own `_…`): its string as is.
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// The agent's adjustable session settings (SPEC: modes + config options), as the UI shows them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Controls {
    /// (current mode id, [(id, name)])
    pub modes: Option<(String, Vec<(String, String)>)>,
    pub options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// `mode` | `model` | `model_config` | `thought_level` | other
    pub category: String,
    pub kind: ConfigKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum ConfigKind {
    /// (current value id, [(value id, name)])
    Select(String, Vec<(String, String)>),
    Boolean(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum ConfigValue {
    Id(String),
    Bool(bool),
}

/// One field of a form the agent asks the person to fill (ACP `elicitation/create`, form mode —
/// Claude's AskUserQuestion arrives this way: one choice field per question plus an "Other" text).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormField {
    pub key: String,
    pub title: String,
    pub description: String,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum FieldKind {
    /// One of these: (value, title, description).
    Choice(Vec<(String, String, String)>),
    /// Any number of these.
    Choices(Vec<(String, String, String)>),
    Text,
    Bool,
    Number,
}

/// One way to log in, as the agent advertises it in `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Terminal method: run the agent's own command with these extra args and env in a terminal the
    /// person can see; the agent never gets `authenticate` for it. None: the `authenticate` RPC.
    pub terminal: Option<(Vec<String>, BTreeMap<String, String>)>,
    /// The method takes an API key the person types (ACP `_meta["api-key"]`): the provider it
    /// names (`google`, `openai`), empty when it names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// A secret the person typed (an API key): serialized as its string, never shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Something sent along with a prompt (SPEC §3.2 attachments): a pasted screenshot, a chosen file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub kind: AttachmentKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum AttachmentKind {
    /// Sent inline as an ACP image block (base64).
    Image { data_base64: String },
    /// Copied where the jail can read it; sent as a resource link plus a line of text naming the path.
    File { jail_path: PathBuf },
}

/// Why a turn ended (ACP `StopReason`, the same on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// A reason this protocol does not know (a newer ACP): its string as is.
    #[serde(untagged)]
    Other(String),
}

// --- the store's rows that cross the wire ---------------------------------------------------------

/// An agent's adjustable settings as silli stores them (SPEC §3.2): per session (what that session
/// runs with) and per agent type (the default a new session starts from).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentSettings {
    pub mode: Option<String>,
    /// option id → value id (selects) or "true"/"false" (booleans)
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// One line of a session's `kept/log.jsonl`: a keep and what it replaced.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Kept {
    /// Name of the payload directory below `kept/`; `prior` inside it is the host's previous version.
    pub id: String,
    pub ts: i64,
    /// Host path.
    #[serde(with = "stored_path")]
    #[schemars(with = "stored_path::Encoding")]
    pub path: PathBuf,
    /// What keep did: `added` `updated` `removed` `symlink` `directory`.
    pub what: String,
    /// What the host had before: `absent` `file` `symlink` `dir` `other` (no payload for `absent` and
    /// `other`; for `dir` the payload is the whole tree when keep removed it, else just its mode).
    pub before: String,
    /// SHA-256 of the file keep wrote (files only): a later edit on the host is told apart on undo.
    pub hash: Option<String>,
    #[serde(default)]
    pub undone_at: Option<i64>,
}

// Stored paths: UTF-8 ones as strings, only non-UTF-8 ones as explicit bytes. Public SFP
// methods use HostPath instead of this storage shape.
mod stored_path {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(untagged)]
    pub enum Encoding {
        Text(String),
        Bytes { bytes_base64: String },
    }
    pub fn serialize<S: serde::Serializer>(path: &std::path::Path, serializer: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => Encoding::Text(text.into()),
            #[cfg(unix)]
            None => Encoding::Bytes { bytes_base64: STANDARD.encode(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())) },
            // only Unix has paths that are arbitrary bytes, and only the daemon stores any
            #[cfg(not(unix))]
            None => return Err(serde::ser::Error::custom("a stored path that is not Unicode")),
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let bytes = match Encoding::deserialize(deserializer)? {
            Encoding::Text(text) => text.into_bytes(),
            Encoding::Bytes { bytes_base64 } => STANDARD.decode(bytes_base64).map_err(serde::de::Error::custom)?,
        };
        if bytes.contains(&0) {
            return Err(serde::de::Error::custom("stored path contains NUL"));
        }
        #[cfg(unix)]
        return Ok(std::os::unix::ffi::OsStringExt::from_vec(bytes).into());
        #[cfg(not(unix))]
        String::from_utf8(bytes).map(PathBuf::from).map_err(|_| serde::de::Error::custom("a stored path that is not Unicode"))
    }
}

/// What an overlay change did to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Kind {
    Added,
    Modified,
    Deleted,
    /// File type differs between host and upper (file ↔ symlink ↔ dir).
    TypeChanged,
    /// Directory replaced wholesale (opaque); its new contents are listed as Added.
    Replaced,
}

impl Kind {
    pub fn letter(self) -> char {
        match self {
            Kind::Added => 'A',
            Kind::Modified => 'M',
            Kind::Deleted => 'D',
            Kind::TypeChanged => 'T',
            Kind::Replaced => 'R',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Other,
}

/// One overlay change (the ledger's row).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    /// Host path.
    pub path: PathBuf,
    /// Path inside the upper tree (absent for deletions it is the whiteout node).
    pub upper: PathBuf,
    pub kind: Kind,
    pub file_type: FileType,
    pub size: u64,
    /// The host changed this path after the session started (SPEC §4.4 conflicts).
    pub conflict: bool,
    /// Matches a `ledger_ignore` pattern: recorded, not hashed per turn, never auto-kept.
    pub ignored: bool,
}

// --- the conversation -------------------------------------------------------------------------------

/// What the transcript gets when a prompt is accepted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Entry {
    /// The person's prompt as they typed it (without silli's prefix) and the attachments' names.
    User { text: String, names: Vec<String> },
    /// A turn silli asks for itself (the summary): a note says what it was.
    Note(String),
}

/// What a frontend asks of a running session (`_silli/sessions.command`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Command {
    /// `text` is what the agent gets (standing block and pin prefix included).
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
        entry: Option<Entry>,
    },
    Authenticate(String),
    /// A method that takes an API key (`AuthMethodInfo::api_key`), with the key the person typed.
    AuthenticateWithKey {
        method_id: String,
        key: Secret,
    },
    RetrySession,
    SetMode(String),
    SetConfig(String, ConfigValue),
    Cancel,
    /// The frontend is done with the session (its tab closed): it detaches; the agent stays the
    /// daemon's until the session is stopped.
    Quit,
}

/// One thing that happened in a session (`_silli/session.event`). `request` numbers a question the
/// agent asked; it is answered with [`Answer`] through `_silli/sessions.answer` and, once any
/// frontend answered, every attached one gets [`Event::Resolved`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Event {
    Ready {
        session_id: String,
        resumed: bool,
        controls: Controls,
    },
    Capabilities {
        images: bool,
    },
    Controls(Controls),
    TurnStarted,
    UserText(String),
    Text(String),
    Thought(String),
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        detail: Option<String>,
        diff: Option<String>,
        parent: Option<String>,
    },
    ToolCallUpdate {
        id: String,
        status: Option<String>,
        title: Option<String>,
        detail: Option<String>,
        diff: Option<String>,
    },
    SubagentText {
        parent: String,
        text: String,
    },
    PermissionRequest {
        request: u64,
        tool_call_id: String,
        title: String,
        options: Vec<PermissionOption>,
    },
    AutoAnswered {
        tool_call_id: String,
        title: String,
        allowed: bool,
    },
    Elicitation {
        request: u64,
        message: String,
        fields: Vec<FormField>,
    },
    /// A question was answered (by this frontend or another): drop its dialog.
    Resolved {
        request: u64,
    },
    Other(String),
    Usage {
        used: u64,
        size: u64,
    },
    AuthRequired {
        message: String,
        methods: Vec<AuthMethodInfo>,
    },
    Stopped(StopReason),
    Ended(Option<String>),
    /// The daemon ended the session (`sessions.end`): its row is written — or deleted, when
    /// nothing was asked in it — and its jail is stopping. Frontends drop the session.
    SessionEnded {
        deleted: bool,
        reason: String,
    },
    /// A line for the transcript's reader that silli wrote itself (the summary turn's mark).
    Note(String),
    /// A checkpoint summary (`sessions.checkpoint`) is in: the text as stored (None when the agent
    /// wrote nothing), and the name it gave the session if the first line was one.
    Summarized {
        text: Option<String>,
        title: Option<String>,
    },
    /// A prompt with what went with it (`UserText` stays for prompts without attachments).
    UserMessage {
        text: String,
        attachments: Vec<UserAttachment>,
    },
    /// Images in the agent's message, by session upload (SFP `image` blocks that were kept).
    AgentImages(Vec<UserAttachment>),
    /// The agent's whole current plan; each replaces the turn's last.
    Plan(Vec<PlanEntry>),
    /// The slash commands the agent offers now.
    Commands(Vec<AgentCommand>),
}

/// An attachment as a frontend shows it: inline data, or an upload it fetches when displayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserAttachment {
    pub name: String,
    pub mime: String,
    pub upload_id: Option<String>,
    pub data_base64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCommand {
    pub name: String,
    pub description: String,
    pub hint: Option<String>,
}

/// The answer to a question: the chosen permission option (None = cancel), or the form's values
/// (None = cancel).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Answer {
    Permission(Option<String>),
    Form(Option<BTreeMap<String, serde_json::Value>>),
}

/// One change to a session's row (`_silli/sessions.set`). `kind` as in `Session::title_kind` /
/// `summary_kind`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum SessionSet {
    Name(String),
    Title {
        title: String,
        kind: String,
    },
    AutoAllow(bool),
    MemoryScope(String),
    Policy(String),
    Summary {
        text: String,
        kind: String,
    },
    /// The session's own agent settings (mode, model, options — SPEC §3.2).
    Settings(AgentSettings),
    /// An ended session comes back: `ended_at` cleared.
    Reopen,
}

// --- changes ------------------------------------------------------------------------------------------

/// The Changes page over the wire (docs/daemon.md §7 M2): what `_silli/changes.list` answers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ChangesList {
    /// Pinned folders that are git repositories, with git's view of their working tree (live:
    /// host = jail); only those with something to show.
    pub live: Vec<LiveRepo>,
    /// Every session with overlay changes or undoable keeps, newest first.
    pub sessions: Vec<SessionChanges>,
    /// Sessions whose writes could not be listed (`changes::Page::unreadable`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<crate::changes::Unreadable>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveRepo {
    pub repo: PathBuf,
    /// `git status` rows: (path relative to the repository, status letter).
    pub status: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionChanges {
    pub id: String,
    pub title: String,
    pub ghost: bool,
    /// Its overlay changes outside the agent's own state (build output left out).
    pub changes: Vec<ChangeRow>,
    /// The agent's own files that landed in the overlay (shown folded).
    pub agent_state: Vec<ChangeRow>,
    /// Keeps that can still be undone, newest first.
    pub kept: Vec<Kept>,
}

/// One overlay change as listed, with what a frontend needs without reading the session's files.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeRow {
    pub change: Change,
    /// The upper entry's last write (unix seconds): the auto-revert countdown counts from it.
    pub written: Option<i64>,
    /// The enclosing git repository on the host; empty outside any.
    pub repo: PathBuf,
    /// SHA-256 of a regular file's content as listed: keep copies only that (a swap is refused).
    pub hash: Option<String>,
    /// Why silli never keeps this path itself (its own files, a repository's executable config).
    pub protected: Option<String>,
}

/// What `_silli/changes.diff` shows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum DiffOf {
    /// A session's overlay change: the upper content against the host's.
    Change { session: String, path: PathBuf },
    /// A pinned repository's own change: git's diff.
    Live { repo: PathBuf, rel: PathBuf },
    /// A keep: the host as it is against the version it replaced.
    Kept { session: String, id: String },
}

/// What a keep, a revert or an undo did, path by path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Done {
    pub done: Vec<(PathBuf, String)>,
    pub failed: Vec<(PathBuf, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_enums_keep_their_wire_form() {
        assert_eq!(serde_json::to_string(&StopReason::EndTurn).unwrap(), "\"end_turn\"");
        assert_eq!(serde_json::to_string(&PermissionOptionKind::AllowAlways).unwrap(), "\"allow_always\"");
        // an unknown string survives the round trip as itself
        let r: StopReason = serde_json::from_str("\"something_new\"").unwrap();
        assert!(matches!(&r, StopReason::Other(s) if s == "something_new"));
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"something_new\"");
        let k: PermissionOptionKind = serde_json::from_str("\"_custom\"").unwrap();
        assert!(matches!(&k, PermissionOptionKind::Other(s) if s == "_custom"));
        assert_eq!(serde_json::to_string(&k).unwrap(), "\"_custom\"");
    }
}
