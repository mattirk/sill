//! Public daemon-host files, settings, policy and managed-process contracts.
//! All paths here identify the daemon host; they never identify the frontend host.
use crate::sessions::HostPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub const FILE_CHUNK_BYTES: usize = 1024 * 1024;
pub const TEXT_BYTES: usize = 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileKey {
    pub path: HostPath,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileMetadata {
    pub file: bool,
    pub directory: bool,
    pub symlink: bool,
    pub target: Option<HostPath>,
    pub length: String,
    pub readonly: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    pub path: HostPath,
    pub metadata: FileMetadata,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesList {
    pub path: HostPath,
    pub after: Option<HostPath>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesPage {
    pub entries: Vec<FileEntry>,
    pub next: Option<HostPath>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRead {
    pub path: HostPath,
    pub offset: String,
    pub length: Option<u32>,
    pub revision: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileBytes {
    pub bytes_base64: String,
    pub next_offset: String,
    pub eof: bool,
    pub revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileWrite {
    pub path: HostPath,
    pub bytes_base64: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Text {
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyKey {
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEntry {
    pub name: String,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyStart {
    pub session_id: String,
    pub ignore_roots: Vec<HostPath>,
}
/// Capability defaults and ceilings, evaluated against this view's resolved policy.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionState {
    pub capability: String,
    pub allowed: bool,
    pub default_enabled: bool,
    /// The session's remembered choice; null follows the policy default.
    pub requested: Option<bool>,
    /// The remembered choice could not take effect within this view's limits (see `reason`).
    #[serde(default)]
    pub blocked: bool,
    /// `session` when an applied session choice decides the state, otherwise `policy`.
    #[serde(default)]
    pub source: String,
    pub effective: bool,
    pub available: bool,
    pub reason: String,
}
/// Remember a capability choice for one session; `enabled: null` returns to the policy default.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionChoice {
    pub session_id: String,
    pub capability: String,
    pub enabled: Option<bool>,
}
/// The next build after a permission change. Preview is prospective, never enforcement evidence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionsChanged {
    pub preview: PolicyView,
    /// The running jail does not have the new state; a restart builds it.
    pub restart_required: bool,
}
/// A host approved for this session alone (Allow), remembered across builds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionHostGrant {
    pub host: String,
    /// In this view's resolved allow-list. False when the view's limits leave it out (offline,
    /// or no additional hosts allowed): it stays remembered and applies again when they go.
    pub applied: bool,
    /// Why it is not applied; empty when it is.
    #[serde(default)]
    pub reason: String,
}
/// Policy inspection always identifies its view; previews are never evidence of enforcement.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyView {
    #[serde(default)]
    pub permissions: Vec<PermissionState>,
    /// The session's remembered host approvals as this view resolves them. A launch snapshot
    /// lists those its build consulted; hosts approved later reach the running proxy live.
    #[serde(default)]
    pub session_hosts: Vec<SessionHostGrant>,
    pub view: String,
    pub session_id: String,
    pub name: String,
    pub text: Option<String>,
    pub fragment_ro: Vec<HostPath>,
    pub fragment_masks: Vec<HostPath>,
    pub sources: Vec<PolicySource>,
    pub suggestions: Vec<PolicySuggestion>,
    pub fingerprint: Option<String>,
    pub diagnostics: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicySource {
    pub root: HostPath,
    pub path: HostPath,
    pub text: String,
    pub sha256: String,
    pub ignored: bool,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicySuggestion {
    pub host: String,
    pub source: HostPath,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicySnapshot {
    /// Older or never-started sessions may have no retained launch snapshot.
    pub snapshot: Option<PolicyView>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyCheck {
    /// Text is transferred from the client; paths in a fragment identify the daemon host.
    pub text: String,
    #[serde(default)]
    pub fragment: bool,
    pub root: Option<HostPath>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyChecked {
    pub valid: bool,
    pub diagnostics: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyImport {
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub replace: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Names {
    pub names: Vec<String>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessKey {
    pub session_id: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessTail {
    pub session_id: String,
    pub name: String,
    pub lines: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessesList {
    pub session_id: String,
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Process {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: HostPath,
    pub port: Option<u16>,
    pub pid: u32,
    pub started: String,
    pub state: String,
    pub exit_code: Option<i64>,
    pub last_line: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessesPage {
    pub processes: Vec<Process>,
    pub next: Option<String>,
}

/// A result: open to fields added later (clients ignore unknown fields in what the daemon sends).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub width: Option<f64>,
    pub drawer_width: Option<i32>,
    pub drawer_follow_changes: Option<bool>,
    pub hotkey: Option<String>,
    pub default_page: Option<String>,
    pub timer_auto: Option<bool>,
    pub text_scale: Option<f64>,
    pub auto_revert_minutes: Option<u32>,
    pub host_terminal: Option<String>,
    pub tray: Option<bool>,
    pub remember_pins: Option<bool>,
    pub memory: Option<bool>,
    pub memory_recall: Option<u32>,
    pub ghost_memory: Option<bool>,
    pub compact_after: Option<String>,
    pub keep_full_transcripts: Option<String>,
    pub delete_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingChange {
    Theme { value: String },
    Width { value: Option<f64> },
    DrawerWidth { value: Option<i32> },
    DrawerFollowChanges { value: Option<bool> },
    Hotkey { value: Option<String> },
    DefaultPage { value: Option<String> },
    TimerAuto { value: Option<bool> },
    TextScale { value: Option<f64> },
    AutoRevertMinutes { value: Option<u32> },
    HostTerminal { value: Option<String> },
    Tray { value: Option<bool> },
    RememberPins { value: Option<bool> },
    Memory { value: Option<bool> },
    MemoryRecall { value: Option<u32> },
    GhostMemory { value: Option<bool> },
    CompactAfter { value: Option<String> },
    KeepFullTranscripts { value: Option<String> },
    DeleteAfter { value: Option<String> },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub changes: Vec<SettingChange>,
}

pub fn encode_bytes(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
pub fn decode_bytes(text: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkDenied {
    pub next: Option<String>,
    pub hosts: Vec<String>,
    pub answered: Vec<String>,
    pub quiet: Vec<String>,
    pub agent_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkHost {
    pub session_id: String,
    pub host: String,
}
/// An agent-wide network default saved deliberately; it changes no existing session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SavedHost {
    pub agent_id: String,
    pub agent_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AllowedHost {
    pub agent_name: String,
    /// The running session has the host now (a live grant its proxy reads per connection): no
    /// restart is needed. False from older daemons, and for a wildcard, which only a restart
    /// applies.
    #[serde(default)]
    pub live: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionImage {
    pub session_id: String,
    pub path: HostPath,
    pub offset: String,
    pub length: Option<u32>,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NamesList {
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeniedList {
    pub session_id: String,
    pub after: Option<String>,
    pub limit: Option<u16>,
}
