//! Public session administration. These records are independent of store and ACP types.
use base64::{Engine, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Exact bytes name a path on the daemon host. Display text is never operational.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct HostPath {
    pub bytes_base64: String,
    pub display: String,
}
impl HostPath {
    pub fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        Self {
            bytes_base64: STANDARD.encode(path.as_os_str().as_bytes()),
            display: path.to_string_lossy().into_owned(),
        }
    }
    pub fn to_path(&self) -> anyhow::Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        anyhow::ensure!(self.bytes_base64.len() <= 5464, "path exceeds 4096 bytes");
        let bytes = STANDARD.decode(&self.bytes_base64)?;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 4096 && !bytes.contains(&0) && bytes[0] == b'/',
            "expected an absolute host path without NUL"
        );
        Ok(std::ffi::OsString::from_vec(bytes).into())
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Settings {
    pub mode: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    pub session_id: String,
    pub agent: String,
    /// Human-editable session name, initially generated; independent of the summary title.
    #[serde(default)]
    pub name: Option<String>,
    pub title: String,
    pub title_kind: String,
    /// Unix seconds, encoded as decimal strings.
    pub created_at: String,
    pub last_used: String,
    pub ended_at: Option<String>,
    pub policy: String,
    pub auto_allow: bool,
    pub memory_scope: String,
    pub summary: String,
    pub summary_kind: String,
    pub ghost: bool,
    pub running: bool,
    pub pins: Vec<HostPath>,
    pub settings: Settings,
    pub follows: Option<String>,
    pub compacted: String,
    /// The session's policy file changed after its running jail was built: stop and start the
    /// session to apply it. False when the session is not running.
    #[serde(default)]
    pub policy_changed: bool,
    /// Active Host control (docs/permission-modes.md): its generation and operations; absent
    /// otherwise. For the window-level indication naming every such session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_control: Option<crate::host_control::Brief>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionKey {
    pub session_id: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub after: Option<String>,
    pub limit: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    pub sessions: Vec<Session>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub create_id: String,
    pub agent: String,
    pub policy: String,
    #[serde(default)]
    pub pins: Vec<HostPath>,
    #[serde(default)]
    pub memory_scope: String,
    #[serde(default)]
    pub follows: String,
    #[serde(default)]
    pub remember_pins: bool,
    #[serde(default)]
    pub ghost: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub session_id: String,
    pub change: Change,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Name { text: String },
    Title { text: String, kind: String },
    AutoAllow { enabled: bool },
    MemoryScope { value: String },
    Policy { value: String },
    Summary { text: String, kind: String },
    Settings { settings: Settings },
    Reopen,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct End {
    pub session_id: String,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Started {
    pub session_id: String,
    pub fresh: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Ending {
    pub pending: bool,
    pub deleted: bool,
}

// Conversion helpers are local conveniences, never additional wire shapes.
impl From<crate::wire::AgentSettings> for Settings {
    fn from(s: crate::wire::AgentSettings) -> Self {
        Self {
            mode: s.mode,
            options: s.config,
        }
    }
}
impl From<Settings> for crate::wire::AgentSettings {
    fn from(s: Settings) -> Self {
        Self {
            mode: s.mode,
            config: s.options,
        }
    }
}
impl From<crate::wire::SessionSet> for Change {
    fn from(s: crate::wire::SessionSet) -> Self {
        use crate::wire::SessionSet as S;
        match s {
            S::Name(text) => Self::Name { text },
            S::Title { title, kind } => Self::Title { text: title, kind },
            S::AutoAllow(enabled) => Self::AutoAllow { enabled },
            S::MemoryScope(value) => Self::MemoryScope { value },
            S::Policy(value) => Self::Policy { value },
            S::Summary { text, kind } => Self::Summary { text, kind },
            S::Settings(s) => Self::Settings { settings: s.into() },
            S::Reopen => Self::Reopen,
        }
    }
}
impl From<Change> for crate::wire::SessionSet {
    fn from(s: Change) -> Self {
        match s {
            Change::Name { text } => Self::Name(text),
            Change::Title { text, kind } => Self::Title { title: text, kind },
            Change::AutoAllow { enabled } => Self::AutoAllow(enabled),
            Change::MemoryScope { value } => Self::MemoryScope(value),
            Change::Policy { value } => Self::Policy(value),
            Change::Summary { text, kind } => Self::Summary { text, kind },
            Change::Settings { settings } => Self::Settings(settings.into()),
            Change::Reopen => Self::Reopen,
        }
    }
}
impl crate::Client {
    pub fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let mut after = None;
        let mut sessions = vec![];
        loop {
            let page: Page = self.call("_sill/sessions.list", List { after, limit: Some(128) })?;
            sessions.extend(page.sessions);
            after = page.next;
            if after.is_none() {
                return Ok(sessions);
            }
        }
    }
    /// Prefix/title resolution belongs to the UI; public mutations use exact identities.
    pub fn resolve_session(&self, key: &str) -> anyhow::Result<Session> {
        let all = self.list_sessions()?;
        if let Some(s) = all.iter().find(|s| s.session_id == key) {
            return Ok(s.clone());
        }
        let mut found = all.into_iter().filter(|s| s.session_id.starts_with(key) || s.title == key);
        let first = found.next().ok_or_else(|| anyhow::anyhow!("no session {key}"))?;
        anyhow::ensure!(found.next().is_none(), "ambiguous session {key}");
        Ok(first)
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    #[test]
    fn exact_path_bytes_survive_wire_roundtrip_and_ignore_display() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/pin/\xff".to_vec()));
        let mut wire = HostPath::from_path(&path);
        wire.display = "/different".into();
        let decoded: HostPath = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(decoded.to_path().unwrap(), path);
        for bytes in [b"relative".as_slice(), b"/nul\0path", b""] {
            assert!(HostPath { bytes_base64: STANDARD.encode(bytes), display: "/valid".into() }.to_path().is_err());
        }
    }
}

#[derive(Debug,Clone,serde::Serialize,serde::Deserialize,schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Log { pub session_id:String, pub lines:Option<u16> }
