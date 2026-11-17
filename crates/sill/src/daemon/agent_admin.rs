//! Explicit public agent definitions and daemon-owned registry selectors.
use super::Hub;
use anyhow::Result;
use serde_json::{Value, json};
use sill_proto::method;
use sill_proto::{agent_admin as api, rpc::Error, sessions::HostPath};
fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::invalid_params(e).into())
}
fn err(code: &str, message: &str) -> anyhow::Error {
    Error::domain(code, message).into()
}
fn valid(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || matches!(id, "." | "..") || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')) {
        return Err(Error::invalid_params("invalid agent id").into());
    }
    Ok(())
}
fn source(t: sill_core::registry::Trust) -> &'static str {
    match t {
        sill_core::registry::Trust::Snapshot => "snapshot",
        sill_core::registry::Trust::Live => "live",
    }
}
fn definition(a: sill_core::agents::Agent) -> Result<api::Definition> {
    Ok(serde_json::from_value(json!(a))?)
}
fn select(home: &std::path::Path, a: &api::EntryKey) -> Result<sill_core::registry::Entry> {
    valid(&a.entry_id)?;
    if !matches!(a.source.as_str(), "snapshot" | "live") {
        return Err(Error::invalid_params("invalid source").into());
    }
    sill_core::registry::merged(home)
        .0
        .into_iter()
        .flat_map(|mut e| {
            let newer = e.newer.take();
            std::iter::once(e).chain(newer.map(|n| *n))
        })
        .find(|e| e.id == a.entry_id && e.version == a.version && source(e.trust) == a.source)
        .ok_or_else(|| err("not_found", "registry selector no longer exists"))
}
pub fn handle(hub: &Hub, m: &str, p: Value) -> Option<Result<Value>> {
    if !matches!(
        m,
        method::AGENTS_LIST
            | method::AGENTS_GET
            | method::AGENTS_SAVE
            | method::AGENTS_REMOVE
            | method::AGENTS_AVAILABLE
            | method::AGENTS_DEFAULTS_GET
            | method::AGENTS_DEFAULTS_SET
            | method::AGENTS_DISCOVERED
            | method::REGISTRY_REFRESH
            | method::REGISTRY_INSTALLED
            | method::REGISTRY_UNINSTALL
            | method::REGISTRY_ICON
    ) {
        return None;
    }
    Some((|| {
        if serde_json::to_vec(&p)?.len() > 1024 * 1024 {
            return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "agent input exceeds 1 MiB")));
        }
        let home = sill_core::changes::home()?;
        let result = match m {
            method::AGENTS_LIST => {
                let a: api::List = args(p)?;
                let n = a.limit.unwrap_or(64) as usize;
                if !(1..=128).contains(&n) {
                    return Err(Error::invalid_params("limit must be 1–128").into());
                }
                let mut agents = sill_core::agents::load(&home)?
                    .into_values()
                    .filter(|v| a.after.as_ref().is_none_or(|id| &v.id > id))
                    .take(n + 1)
                    .map(definition)
                    .collect::<Result<Vec<_>>>()?;
                let more = agents.len() > n;
                agents.truncate(n);
                let next = more.then(|| agents.last().unwrap().id.clone());
                json!(api::Page { agents, next })
            }
            method::AGENTS_SAVE => {
                let a: sill_core::agents::Agent = serde_json::from_value(p).map_err(Error::invalid_params)?;
                valid(&a.id)?;
                if a.command.is_empty() || a.command.iter().any(|v| v.contains('\0')) {
                    return Err(Error::invalid_params("nonempty command without NUL required").into());
                }
                let agent = a;
                json!(api::Saved { path: HostPath::from_path(&sill_core::agents::save_override(&home, &agent)?) })
            }
            method::AGENTS_REMOVE => {
                let a: api::AgentKey = args(p)?;
                valid(&a.agent_id)?;
                json!(api::Removed { removed: sill_core::agents::remove_override(&home, &a.agent_id)? })
            }
            method::AGENTS_GET | method::AGENTS_AVAILABLE | method::AGENTS_DEFAULTS_GET => {
                let a: api::AgentKey = args(p)?;
                valid(&a.agent_id)?;
                let agent = sill_core::agents::load(&home)?.remove(&a.agent_id).ok_or_else(|| err("not_found", "agent not found"))?;
                match m {
                    method::AGENTS_GET => json!(definition(agent)?),
                    method::AGENTS_AVAILABLE => json!(api::Available { path: sill_core::registry::available(&agent, &home).map(|p| HostPath::from_path(&p)) }),
                    _ => json!(hub.with_store(|s| Ok(s.agent_settings(&a.agent_id)))?),
                }
            }
            method::AGENTS_DEFAULTS_SET => {
                let a: api::Defaults = args(p)?;
                valid(&a.agent_id)?;
                if !sill_core::agents::load(&home)?.contains_key(&a.agent_id) {
                    return Err(err("not_found", "agent not found"));
                }
                hub.with_store(|s| s.set_agent_settings(&a.agent_id, &a.settings))?;
                json!({})
            }
            method::AGENTS_DISCOVERED => {
                let a: api::AgentKey = args(p)?;
                valid(&a.agent_id)?;
                json!(api::Discovered {
                    discovery: sill_core::registry::discovered(&home, &a.agent_id).map(|d| api::Discovery {
                        at: d.at.to_string(),
                        version: d.version,
                        proposals: d
                            .proposals
                            .into_iter()
                            .map(|p| api::Proposal { path: p.path, kind: p.kind, on_host: p.on_host, initial_content: p.initial_content })
                            .collect(),
                        auth_methods: d.auth_methods.into_iter().map(|m| api::AuthMethodInfo { id: m.id, name: m.name, description: m.description }).collect(),
                        login_method: d.login_method,
                        login_files: d.login_files,
                        note: d.note
                    })
                })
            }
            method::REGISTRY_REFRESH => {
                let _: sill_proto::Empty = args(p)?;
                sill_core::registry::fetch(&home)?;
                json!({})
            }
            method::REGISTRY_INSTALLED => {
                let a: api::AgentKey = args(p)?;
                valid(&a.agent_id)?;
                json!(api::Installation {
                    installed: sill_core::registry::installed(&home, &a.agent_id).map(|v| api::Installed {
                        version: v.version,
                        kind: v.kind,
                        command: v.command,
                        env: v.env,
                        source: source(v.trust).into()
                    })
                })
            }
            method::REGISTRY_UNINSTALL => {
                let a: api::EntryKey = args(p)?;
                valid(&a.entry_id)?;
                let _job = super::host::Job::reserve(format!("install:{}", a.entry_id))?;
                let entry = select(&home, &a)?;
                if let Some(installed) = sill_core::registry::installed(&home, &a.entry_id) {
                    if installed.version != a.version || source(installed.trust) != a.source {
                        return Err(err("invalid_state", "installed version/source changed; refresh before uninstalling"));
                    }
                } else {
                    return Err(err("not_found", "entry is not installed"));
                }
                sill_core::registry::uninstall(&home, &entry)?;
                json!({})
            }
            method::REGISTRY_ICON => {
                use std::io::Read;
                // by id: the icon is the agent's, not a version's, and the installed version may no
                // longer be listed
                let a: api::EntryKey = args(p)?;
                valid(&a.entry_id)?;
                let bytes = if let Some(path) = sill_core::registry::icon_by_id(&home, &a.entry_id) {
                    let mut bytes = vec![];
                    std::fs::File::open(path)?.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
                    if bytes.len() > 1024 * 1024 {
                        return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(
                            sill_proto::rpc::LimitScope::Record,
                            Some(1048576),
                            "icon exceeds 1 MiB",
                        )));
                    }
                    Some(sill_proto::admin::encode_bytes(&bytes))
                } else {
                    None
                };
                json!(api::Icon { bytes_base64: bytes })
            }
            _ => unreachable!(),
        };
        if serde_json::to_vec(&result)?.len() > 4 * 1024 * 1024 {
            return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Result, Some(4194304), "agent response exceeds 4 MiB")));
        }
        Ok(result)
    })())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtin_definitions_roundtrip_through_public_contract() {
        for agent in sill_core::agents::builtin() {
            let original = json!(agent);
            let public = definition(agent).unwrap();
            let restored: sill_core::agents::Agent = serde_json::from_value(json!(public)).unwrap();
            assert_eq!(json!(restored), original);
        }
    }
    #[test]
    fn ids_cannot_escape_definition_directory() {
        for id in ["", "..", "../x", "x/y", "x\0y"] {
            assert!(valid(id).is_err());
        }
        assert!(valid("custom-agent.1").is_ok());
    }
}
