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
/// `_sill/usage.active_time`: how far back to look.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActiveTimeQuery {
    /// Whole days before now, 1 to 366.
    pub days: u16,
}
/// The time agents worked, by UTC hour, so a frontend can group it into its own local days.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ActiveTime {
    /// Ascending; hours without work are left out.
    pub hours: Vec<ActiveHour>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActiveHour {
    /// Unix time in whole hours.
    pub hour: u64,
    /// Milliseconds worked in that hour, summed over sessions (more than an hour when several
    /// agents worked at once).
    pub ms: u64,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopCapture {
    pub session_id: String,
    /// The revision the frontend shows: when the desktop still looks so, nothing is encoded.
    #[serde(default)]
    pub since: Option<String>,
}
/// A frame of a session's test desktop, taken by the daemon (`_sill/desktop.capture`). The PNG
/// itself is read with `_sill/desktop.read` while it is the latest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DesktopFrame {
    /// The SHA-256 of the picture's pixels: an unchanged desktop has the same revision.
    pub revision: String,
    /// False when `since` named this picture: there is nothing new to read.
    pub changed: bool,
    /// The picture's size in pixels; the desktop's own size is this divided by `scale`.
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    /// The PNG's length in bytes (decimal); 0 when not `changed`.
    pub size: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopRead {
    pub session_id: String,
    pub revision: String,
    pub offset: String,
    pub length: Option<u32>,
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
    /// Exact filesystem rules; None means no launch evidence was recorded.
    pub filesystem: Option<FilesystemView>,
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
    pub fingerprint: Option<String>,
    pub diagnostics: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemView {
    pub rules: Vec<PathRule>,
    pub unused: Vec<HostPath>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PathRule {
    pub target: HostPath,
    pub backing: Option<HostPath>,
    pub behavior: String,
    pub source: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicySnapshot {
    /// Older or never-started sessions may have no retained launch snapshot.
    pub snapshot: Option<PolicyView>,
    /// What the session's last launch warned about, in the launcher's words: a host socket in a
    /// pinned folder, missing resource limits, a state filesystem that cannot be hidden. For
    /// the person to read; empty when the launch had nothing to say.
    #[serde(default)]
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyCheck {
    /// A complete policy's text, transferred from the client.
    pub text: String,
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
/// What runs in a session's jail now (docs/jail-processes.md), as sill sees it from outside the
/// agent. Empty when the session's jail is not running.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct JailProcesses {
    /// Programs the agent started that are running now, each the root of its own tree.
    #[serde(default)]
    pub commands: Vec<JailProcess>,
    /// Long-running programs: managed processes (`sill-proc`) and anything listening on a port.
    #[serde(default)]
    pub services: Vec<JailProcess>,
}
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct JailProcess {
    /// The pid in the jail's own namespace.
    pub pid: u32,
    pub argv: Vec<String>,
    /// Unix milliseconds.
    pub started_ms: u64,
    /// Processes in its tree, itself included.
    pub count: u32,
    /// The managed process's name, for a service started with `sill-proc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A port it listens on, the lowest when several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}
/// A command running in the jail, by its root's pid there (`JailProcesses::commands`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandKey {
    pub session_id: String,
    pub pid: u32,
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
    /// `jail` (the shim's managed processes) or `host` (launched on this machine through Host
    /// control; such a name begins with `host:`).
    #[serde(default = "jail_location")]
    pub location: String,
    /// A host process marked to keep running after Host control ends.
    #[serde(default)]
    pub kept: bool,
}
fn jail_location() -> String {
    "jail".into()
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
    pub network_quiet_hosts: Vec<String>,
    pub theme: String,
    pub drawer_follow_changes: Option<bool>,
    pub default_page: Option<String>,
    pub timer_auto: Option<bool>,
    pub auto_revert_minutes: Option<u32>,
    pub tray: Option<bool>,
    pub remember_pins: Option<bool>,
    pub automatic_workspaces: Option<bool>,
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
    NetworkQuietHosts { value: Vec<String> },
    Theme { value: String },
    DrawerFollowChanges { value: Option<bool> },
    DefaultPage { value: Option<String> },
    TimerAuto { value: Option<bool> },
    AutoRevertMinutes { value: Option<u32> },
    Tray { value: Option<bool> },
    RememberPins { value: Option<bool> },
    AutomaticWorkspaces { value: Option<bool> },
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
