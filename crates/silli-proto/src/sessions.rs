//! Public session administration. These records are independent of store and ACP types.
use crate::method;
use base64::{Engine, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::{Path, PathBuf};

/// Exact bytes name a path on the daemon host. Display text is never operational.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct HostPath {
    pub bytes_base64: String,
    pub display: String,
}
impl HostPath {
    /// The path with these exact bytes, shown as text where they are not UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self { bytes_base64: STANDARD.encode(bytes), display: String::from_utf8_lossy(bytes).into_owned() }
    }

    /// The path's exact bytes: absolute, at most 4096 of them, without NUL.
    pub fn bytes(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(self.bytes_base64.len() <= 5464, "path exceeds 4096 bytes");
        let bytes = STANDARD.decode(&self.bytes_base64)?;
        anyhow::ensure!(!bytes.is_empty() && bytes.len() <= 4096 && !bytes.contains(&0) && bytes[0] == b'/', "expected an absolute host path without NUL");
        Ok(bytes)
    }

    // A box's path is `/`-separated bytes whatever system the frontend runs on: on Windows the
    // system's own path type joins with `` and does not call `/home/x` absolute. So a frontend
    // takes paths apart and puts them together with these, never with `std::path`.

    /// The path without its last component; `None` for `/` and for a path that is not valid.
    pub fn parent(&self) -> Option<HostPath> {
        let bytes = self.bytes().ok()?;
        let trimmed = trim_slashes(&bytes);
        let cut = trimmed.iter().rposition(|b| *b == b'/')?;
        (trimmed.len() > 1).then(|| HostPath::from_bytes(if cut == 0 { b"/" } else { &trimmed[..cut] }))
    }

    /// The last component's bytes; empty for `/`.
    pub fn file_name(&self) -> Vec<u8> {
        let bytes = self.bytes().unwrap_or_default();
        let trimmed = trim_slashes(&bytes);
        trimmed.rsplit(|b| *b == b'/').next().unwrap_or_default().to_vec()
    }

    /// This path with `relative` below it (`relative` may itself hold `/`).
    pub fn join(&self, relative: &[u8]) -> anyhow::Result<HostPath> {
        let mut bytes = trim_slashes(&self.bytes()?).to_vec();
        let relative = relative.strip_prefix(b"/").unwrap_or(relative);
        if !relative.is_empty() {
            if bytes != b"/" {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(relative);
        }
        Ok(HostPath::from_bytes(&bytes))
    }

    /// What is left of this path below `base`: empty when they are the same path, `None` when
    /// this path is not `base` or below it.
    pub fn strip_prefix(&self, base: &HostPath) -> Option<Vec<u8>> {
        let (path, base) = (self.bytes().ok()?, base.bytes().ok()?);
        let (path, base) = (trim_slashes(&path), trim_slashes(&base));
        if base == b"/" {
            return Some(path[1..].to_vec());
        }
        let rest = path.strip_prefix(base)?;
        match rest.first() {
            None => Some(Vec::new()),
            Some(b'/') => Some(rest[1..].to_vec()),
            Some(_) => None,
        }
    }

    /// This path is `base` or below it.
    pub fn starts_with(&self, base: &HostPath) -> bool {
        self.strip_prefix(base).is_some()
    }

    /// How many components the path has; `/` has none.
    pub fn depth(&self) -> usize {
        self.bytes().map_or(0, |bytes| bytes.split(|b| *b == b'/').filter(|part| !part.is_empty()).count())
    }

    /// On the daemon's own system a host path is a path of that system.
    #[cfg(unix)]
    pub fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        Self { bytes_base64: STANDARD.encode(path.as_os_str().as_bytes()), display: path.to_string_lossy().into_owned() }
    }

    #[cfg(unix)]
    pub fn to_path(&self) -> anyhow::Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(self.bytes()?).into())
    }
}

/// Without slashes at the end, `/` itself kept.
fn trim_slashes(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().rposition(|b| *b != b'/').map_or(1, |last| last + 1);
    &bytes[..end.min(bytes.len())]
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
    #[serde(default)]
    pub always_apply: bool,
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
    /// The session state's `active_ms` and `active_since_ms` (the time the agent worked), so a
    /// frontend that has not attached shows the same time as one that has.
    #[serde(default)]
    pub active_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since_ms: Option<u64>,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always_apply: bool,
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
    /// Work in workspaces for the session's pinned repositories (docs/parallel-coding.md §12);
    /// absent follows Settings → Automatic workspaces. Needs `silli.workspaces`.
    #[serde(default)]
    pub workspaces: Option<bool>,
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
        Self { mode: s.mode, options: s.config }
    }
}
impl From<Settings> for crate::wire::AgentSettings {
    fn from(s: Settings) -> Self {
        Self { mode: s.mode, config: s.options }
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
            let page: Page = self.call(method::SESSIONS_LIST, List { after, limit: Some(128) })?;
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
mod byte_path_tests {
    use super::*;
    #[test]
    fn a_box_path_is_taken_apart_and_put_together_as_bytes() {
        let p = |text: &str| HostPath::from_bytes(text.as_bytes());
        assert_eq!(p("/home/x/a.rs").parent(), Some(p("/home/x")));
        assert_eq!(p("/home/").parent(), Some(p("/")));
        assert_eq!(p("/").parent(), None);
        assert_eq!(p("/home/x/a.rs").file_name(), b"a.rs");
        assert_eq!(p("/home/x/").file_name(), b"x");
        assert_eq!(p("/").file_name(), b"");
        assert_eq!(p("/home/x").join(b"src/main.rs").unwrap(), p("/home/x/src/main.rs"));
        assert_eq!(p("/").join(b"etc").unwrap(), p("/etc"));
        assert_eq!(p("/home/x").join(b"").unwrap(), p("/home/x"));
        assert_eq!(p("/home/x/src/a.rs").strip_prefix(&p("/home/x")), Some(b"src/a.rs".to_vec()));
        assert_eq!(p("/home/x").strip_prefix(&p("/home/x/")), Some(Vec::new()));
        assert_eq!(p("/home/xy").strip_prefix(&p("/home/x")), None, "a name that only begins the same");
        assert_eq!(p("/etc").strip_prefix(&p("/")), Some(b"etc".to_vec()));
        assert!(p("/mnt/c/Users").starts_with(&p("/mnt/c")) && !p("/mnt/d").starts_with(&p("/mnt/c")));
        assert_eq!((p("/").depth(), p("/a/b/").depth()), (0, 2));
        // a backslash is a character of a name, never a separator
        assert_eq!(p(r"/a/b\c").file_name(), br"b\c");
        assert!(HostPath { bytes_base64: STANDARD.encode(b"relative"), display: String::new() }.parent().is_none());
    }
}

#[cfg(all(test, unix))]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub session_id: String,
    pub lines: Option<u16>,
}
