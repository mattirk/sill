//! Data operations performed on the daemon's host for a rendering frontend.
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

pub const METHOD: &str = sill_proto::method::FRONTEND_REQUEST;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Home,
    Sessions,
    Pins { session: String, global: bool },
    GlobalPins,
    Entries { session: String, before: Option<i64>, limit: usize },
    EntryCount { session: String },
    Memories { session: String },
    AgentSettings { agent: String },
    Search { text: String, limit: usize },
    Export,
    Recall { key: String },
    PolicyText { name: String },
    Config,
    SetConfig(crate::config::Config),
    PatchConfig { fields: std::collections::BTreeMap<String, serde_json::Value> },
    Instructions,
    SetInstructions { text: String },
    Agents,
    SaveAgent(crate::agents::Agent),
    RemoveAgent { id: String },
    AgentMemory { id: String },
    ForgetAgentMemory { id: String },
    Registry,
    FetchRegistry,
    Installed { id: String },
    Available(crate::agents::Agent),
    Install(crate::registry::Entry),
    Uninstall(crate::registry::Entry),
    Icon(crate::registry::Entry),
    Metadata { path: PathBuf },
    List { path: PathBuf },
    Find { bases: Vec<PathBuf>, query: String, cap: usize, visits: usize },
    Read { path: PathBuf },
    Write { path: PathBuf, bytes: Vec<u8> },
    Head { path: PathBuf },
    RepoRoot { path: PathBuf },
    ChangedLines { repo: PathBuf, rel: PathBuf },
    JailOutdated { session: String, policy: String },
    GitStatus { repo: PathBuf },
    Upload { session: String, name: String, bytes: Vec<u8> },
    ResolveImage { session: String, path: String },
    Denied { session: String },
    RememberDenied { session: String, host: String },
    AllowHost { session: String, host: String },
    ProcList { session: String },
    ProcTail { session: String, name: String, lines: usize },
    ProcStop { session: String, name: String },
    ProcDismiss { session: String, name: String },
    ReadLog { source: crate::session::LogSource, lines: usize },
    Policies,
    PolicyDesktop { policy: String },
    Kept { session: String },
    RefreshSnapshot,
    GitSummary { path: PathBuf },
    OpenPort { session: String, port: u16 },
    HostLogin { agent: String, method: String },
    ProcessPolicy { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metadata {
    pub file: bool,
    pub directory: bool,
    pub symlink: bool,
    pub target: Option<PathBuf>,
    pub length: u64,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub path: PathBuf,
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMemory {
    pub dirs: Vec<PathBuf>,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportFile {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Denied {
    pub hosts: Vec<String>,
    pub answered: Vec<String>,
    pub quiet: Vec<String>,
    pub agent_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Opened {
    pub token: String,
    pub pid: u32,
}

/// The index's Entry deliberately skips trust when deserializing untrusted registry JSON. Carry
/// the daemon's presentation metadata separately, without letting an index claim snapshot trust.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub entry: crate::registry::Entry,
    pub trust: crate::registry::Trust,
    pub newer: Option<Box<RegistryEntry>>,
}
impl From<crate::registry::Entry> for RegistryEntry {
    fn from(mut entry: crate::registry::Entry) -> Self {
        let trust = entry.trust;
        let newer = entry.newer.take().map(|entry| Box::new(Self::from(*entry)));
        Self { entry, trust, newer }
    }
}
impl From<RegistryEntry> for crate::registry::Entry {
    fn from(value: RegistryEntry) -> Self {
        let mut entry = value.entry;
        entry.trust = value.trust;
        entry.newer = value.newer.map(|entry| Box::new(Self::from(*entry)));
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_presentation_keeps_trust_and_updates_without_changing_index_parsing() {
        let mut entry = crate::registry::snapshot().agents[0].clone();
        entry.trust = crate::registry::Trust::Snapshot;
        let mut newer = entry.clone();
        newer.version = "next".into();
        newer.trust = crate::registry::Trust::Live;
        entry.newer = Some(Box::new(newer));
        let bytes = serde_json::to_vec(&RegistryEntry::from(entry.clone())).unwrap();
        let decoded: crate::registry::Entry = serde_json::from_slice::<RegistryEntry>(&bytes).unwrap().into();
        assert_eq!(decoded.trust, crate::registry::Trust::Snapshot);
        assert_eq!(decoded.newer.unwrap().version, "next");
        let index = serde_json::to_value(entry).unwrap();
        assert_eq!(serde_json::from_value::<crate::registry::Entry>(index).unwrap().trust, crate::registry::Trust::Live);
    }
}
