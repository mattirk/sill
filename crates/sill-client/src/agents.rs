//! Agent catalog and installation through the selected daemon, independent of a toolkit.
use anyhow::{Context, Result, bail};
use sill_proto::{Client, Empty, agent_admin as api, host};
use std::collections::BTreeMap;
use std::io::BufRead;

#[derive(Debug, Clone)]
pub struct Entry {
    pub registry: host::RegistryEntry,
    pub installed: Option<api::Installed>,
    pub definition: Option<api::Definition>,
    pub available: bool,
}
impl Entry {
    pub fn present(&self) -> bool {
        self.installed.is_some() || self.available
    }
    pub fn tested(&self) -> bool {
        ["claude-acp", "codex-acp", "antigravity-acp"].contains(&self.registry.entry_id.as_str())
    }
    pub fn selector(&self) -> api::EntryKey {
        api::EntryKey {
            entry_id: self.registry.entry_id.clone(),
            version: self.registry.version.clone(),
            source: self.registry.source.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub entries: Vec<Entry>,
    pub refresh_due: bool,
}

pub fn load(client: &Client) -> Result<Catalog> {
    let mut definitions = BTreeMap::new();
    let mut after = None;
    loop {
        let page: api::Page = client.call(
            "_sill/agents.list",
            api::List {
                after,
                limit: Some(128),
            },
        )?;
        for d in page.agents {
            definitions.insert(d.id.clone(), d);
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let mut entries = Vec::new();
    let mut refresh_due = false;
    let mut after = None;
    loop {
        let page: host::RegistryPage = client.call(
            "_sill/registry.list",
            host::RegistryList {
                after: after.clone(),
                limit: Some(128),
            },
        )?;
        if after.is_none() {
            let fetched = page
                .fetched_at
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            refresh_due = fetched.is_none_or(|f| now.saturating_sub(f) > 6 * 3600);
        }
        for registry in page.entries {
            let id = registry
                .builtin_agent_id
                .as_deref()
                .unwrap_or(&registry.entry_id);
            let definition = definitions.get(id).cloned();
            let installed: api::Installation = client.call(
                "_sill/registry.installed",
                api::AgentKey {
                    agent_id: registry.entry_id.clone(),
                },
            )?;
            let available = if let Some(d) = &definition {
                client
                    .call::<api::Available>(
                        "_sill/agents.available",
                        api::AgentKey {
                            agent_id: d.id.clone(),
                        },
                    )?
                    .path
                    .is_some()
            } else {
                false
            };
            entries.push(Entry {
                registry,
                installed: installed.installed,
                definition,
                available,
            });
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    entries.sort_by_key(|e| e.registry.name.to_lowercase());
    Ok(Catalog {
        entries,
        refresh_due,
    })
}

pub fn refresh(client: &Client) -> Result<()> {
    let _: serde_json::Value = client.call("_sill/registry.refresh", Empty::default())?;
    Ok(())
}

/// Never replay an install after stream loss: report its durable receipt instead.
pub fn install(
    client: &Client,
    entry: &host::RegistryEntry,
    mut progress: impl FnMut(String),
) -> Result<()> {
    let operation_id = crate::request_id("install");
    let job: host::JobStream = client
        .call(
            "_sill/registry.install",
            host::RegistryInstall {
                operation_id: Some(operation_id.clone()),
                entry_id: entry.entry_id.clone(),
                version: entry.version.clone(),
                source: entry.source.clone(),
                allow_unreviewed: entry.source == "live",
            },
        )
        .with_context(|| format!("Install operation {operation_id}; refresh before retrying"))?;
    if let Some(token) = job.token {
        let stream = sill_proto::transport::open_stream(&token).with_context(|| {
            format!(
                "Install operation {operation_id}; progress unavailable, refresh before retrying"
            )
        })?;
        struct Stop(std::sync::Arc<dyn Fn() + Send + Sync>);
        impl Drop for Stop {
            fn drop(&mut self) {
                (self.0)();
            }
        }
        let _stop = Stop(stream.interrupt);
        for line in std::io::BufReader::new(stream.reader).lines() {
            let Ok(line) = line else { break };
            match serde_json::from_str::<host::Progress>(&line)? {
                host::Progress::Status { text }
                | host::Progress::Output { text }
                | host::Progress::Stderr { text } => progress(text),
                host::Progress::Completed { .. } => return Ok(()),
                host::Progress::Failed { message, .. } => bail!("{message}"),
            }
        }
    }
    let receipt: host::Operation = client.call(
        "_sill/operations.get",
        host::OperationKey {
            operation_id: operation_id.clone(),
        },
    )?;
    if receipt.state == host::OperationState::Completed {
        return Ok(());
    }
    bail!(
        "Installation {:?}; operation {operation_id}. Refresh before retrying.",
        receipt.state
    )
}

/// A domain pattern for the compact Settings editor. Keep URLs and paths out of proxy rules.
pub fn domain_pattern(input: &str) -> Option<String> {
    let pattern = input.trim().to_ascii_lowercase();
    let (host, port) = if let Some(rest) = pattern.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        host.parse::<std::net::Ipv6Addr>().ok()?;
        (
            host,
            if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix(':')?)
            },
        )
    } else if pattern.parse::<std::net::IpAddr>().is_ok() {
        (pattern.as_str(), None)
    } else {
        match pattern.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (pattern.as_str(), None),
        }
    };
    if let Some(port) = port {
        if port != "*" && !port.parse::<u16>().is_ok_and(|p| p > 0) {
            return None;
        }
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    let valid = host.parse::<std::net::IpAddr>().is_ok()
        || (!host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }));
    valid.then_some(pattern)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn domain_patterns_reject_urls_and_preserve_explicit_scope() {
        for input in [
            "example.com",
            "*.example.com",
            "example.com:8443",
            "example.com:*",
            "[::1]:8080",
            "::1",
        ] {
            assert_eq!(domain_pattern(input).as_deref(), Some(input));
        }
        assert_eq!(
            domain_pattern("  GOOGLE.com  ").as_deref(),
            Some("google.com")
        );
        for input in [
            "",
            "https://google.com",
            "google.com/path",
            "a,b",
            "*.com/",
            "a:0",
            "a:65536",
            "a:",
            "a:abc",
            "bad\nname",
            "-bad.com",
            "a..com",
            "*",
        ] {
            assert_eq!(domain_pattern(input), None, "{input}");
        }
    }

    #[test]
    fn an_install_receipt_is_read_without_replaying_the_mutation() {
        let (client, mut peer) = crate::fake::pair();
        let worker = std::thread::spawn(move || {
            let req = peer.expect("_sill/registry.install");
            let id = req["params"]["operation_id"].as_str().unwrap().to_owned();
            assert_eq!(req["params"]["allow_unreviewed"], true);
            peer.reply(&req, json!({"token":null,"operation":{
                "operation_id":id,"method":"_sill/registry.install","state":"running","agent_id":null,"code":null
            }}));
            let req = peer.expect("_sill/operations.get");
            assert_eq!(req["params"]["operation_id"], id);
            peer.reply(&req, json!({"operation_id":id,"method":"_sill/registry.install","state":"completed","agent_id":"example","code":null}));
        });
        let entry = serde_json::from_value(json!({
            "entry_id":"example","name":"Example","version":"1","source":"live",
            "installable":true,"description":"","repository":null,"website":null,
            "install_kind":"npx","install_spec":"example@1","package_name":"example",
            "has_icon":false,"builtin_agent_id":null,"newer":null
        }))
        .unwrap();
        install(&client, &entry, |_| {}).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn catalog_paginates_and_uses_host_installation_facts() {
        let (client, mut peer) = crate::fake::pair();
        let worker = std::thread::spawn(move || {
            let req = peer.expect("_sill/agents.list");
            peer.reply(&req, json!({"agents":[],"next":null}));
            let req = peer.expect("_sill/registry.list");
            peer.reply(
                &req,
                json!({"entries":[{
                "entry_id":"remote-agent", "name":"Remote", "version":"2", "source":"snapshot",
                "installable":true, "description":"Agent", "repository":null, "website":null,
                "install_kind":"binary", "install_spec":"binary v2", "package_name":null,
                "has_icon":false,"builtin_agent_id":null,"newer":null
            }],"next":"remote-agent","fetched_at":null}),
            );
            let req = peer.expect("_sill/registry.installed");
            assert_eq!(req["params"]["agent_id"], "remote-agent");
            peer.reply(&req, json!({"installed":{"version":"1","kind":"binary","command":["/remote/agent"],"env":{},"source":"snapshot"}}));
            let req = peer.expect("_sill/registry.list");
            assert_eq!(req["params"]["after"], "remote-agent");
            peer.reply(&req, json!({"entries":[],"next":null,"fetched_at":null}));
        });
        let catalog = load(&client).unwrap();
        assert!(catalog.refresh_due);
        let entries = catalog.entries;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].present());
        assert_eq!(entries[0].installed.as_ref().unwrap().version, "1");
        worker.join().unwrap();
    }
}

/// Icons are fetched and cached by the daemon; paths never refer to the frontend host.
pub fn icon(client: &Client, selector: api::EntryKey) -> Result<Option<Vec<u8>>> {
    let icon: api::Icon = client.call("_sill/registry.icon", selector)?;
    icon.bytes_base64
        .map(|b| sill_proto::admin::decode_bytes(&b))
        .transpose()
}
