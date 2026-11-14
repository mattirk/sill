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
    pub x11: bool,
    pub dbus: bool,
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
    pub desktop_x11: Option<bool>,
    pub desktop_dbus: Option<bool>,
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
    DesktopX11 { value: Option<bool> },
    DesktopDbus { value: Option<bool> },
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AllowedHost {
    pub agent_name: String,
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
