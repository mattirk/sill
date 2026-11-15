//! Data operations performed on the daemon's host for a rendering frontend.
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    PolicyText { name: String },
    Config,
    PatchConfig { fields: std::collections::BTreeMap<String, serde_json::Value> },
    Instructions,
    SetInstructions { text: String },
    Denied { session: String },
    RememberDenied { session: String, host: String },
    AllowHost { session: String, host: String },
    ProcTail { session: String, name: String, lines: usize },
    ProcStop { session: String, name: String },
    ProcDismiss { session: String, name: String },
    Policies,
    RefreshSnapshot,
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

/// Registry presentation from the daemon. Package manifests and installation commands stay
/// on the daemon; trust is accepted only from its explicit source field.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub repository: Option<String>,
    pub website: Option<String>,
    pub trust: crate::registry::Trust,
    pub newer: Option<Box<RegistryEntry>>,
    pub install_kind: Option<String>,
    pub install_spec: String,
    pub package_name: Option<String>,
    pub has_icon: bool,
    pub builtin_agent_id: Option<String>,
}
impl RegistryEntry {
    pub fn how(&self)->Option<&str> { self.install_kind.as_deref() }
    pub fn install_spec(&self)->&str { &self.install_spec }
    pub fn package_name(&self)->Option<&str> {self.package_name.as_deref()}
}
pub fn registry_trust(source:&str)->anyhow::Result<crate::registry::Trust> {
    match source {
        "snapshot"=>Ok(crate::registry::Trust::Snapshot),
        "live"=>Ok(crate::registry::Trust::Live),
        _=>anyhow::bail!("unknown registry source: {source}"),
    }
}
impl TryFrom<sill_proto::host::RegistryEntry> for RegistryEntry {
    type Error=anyhow::Error;
    fn try_from(value:sill_proto::host::RegistryEntry)->anyhow::Result<Self> {
        Ok(Self {id:value.entry_id,name:value.name,version:value.version,description:value.description,
            repository:value.repository,website:value.website,trust:registry_trust(&value.source)?,
            newer:value.newer.map(|n|Self::try_from(*n).map(Box::new)).transpose()?,
            install_kind:if value.installable {value.install_kind}else{None},install_spec:value.install_spec,package_name:value.package_name,has_icon:value.has_icon,builtin_agent_id:value.builtin_agent_id})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_presentation_retains_daemon_platform_and_trust() {
        let wire=sill_proto::host::RegistryEntry {entry_id:"test".into(),name:"Test".into(),version:"1".into(),source:"snapshot".into(),installable:true,
            description:"Details".into(),repository:Some("https://example.com/source".into()),website:None,install_kind:Some("binary".into()),install_spec:"daemon-platform.tar.gz".into(),package_name:None,has_icon:false,builtin_agent_id:Some("codex".into()),tested:false,newer:None};
        let entry=RegistryEntry::try_from(wire.clone()).unwrap();
        assert_eq!(entry.trust,crate::registry::Trust::Snapshot);
        assert_eq!(entry.how(),Some("binary"));
        assert_eq!(entry.install_spec(),"daemon-platform.tar.gz");
        assert_eq!(entry.builtin_agent_id.as_deref(),Some("codex"));
        let mut unknown=wire;unknown.source="future".into();
        assert!(RegistryEntry::try_from(unknown).is_err());
    }
}
