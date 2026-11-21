//! Model providers (`silli.providers`, docs/model-providers.md) for a frontend.
use anyhow::{Result, bail};
use silli_proto::client::Client;
use silli_proto::method;
use silli_proto::providers as api;
use silli_proto::wire::Secret;

fn supported(client: &Client) -> Result<()> {
    if !client.has_capability(api::CAPABILITY) {
        bail!("the daemon is older than this window and has no model providers; update and restart it");
    }
    Ok(())
}

/// The providers, or `None` from a daemon without them.
pub fn load(client: &Client) -> Result<Option<api::Listed>> {
    if !client.has_capability(api::CAPABILITY) {
        return Ok(None);
    }
    Ok(Some(client.call(method::PROVIDERS_LIST, silli_proto::Empty::default())?))
}

pub fn save(client: &Client, provider: api::Provider, key: Option<String>, clear_key: bool) -> Result<api::Saved> {
    supported(client)?;
    if !provider.model_limits.is_empty() && !client.has_capability(api::LIMITS_CAPABILITY) {
        bail!("update and restart the daemon to save model token limits");
    }
    client.call(method::PROVIDERS_SAVE, api::Save { provider, key: key.map(Secret), clear_key })
}

/// The context a running Ollama model was loaded with, read on the daemon host.
pub fn detect_limits(client: &Client, request: api::DetectLimits) -> Result<api::DetectedLimits> {
    if !client.has_capability(api::LIMITS_CAPABILITY) {
        bail!("update and restart the daemon for model token-limit detection");
    }
    client.call(method::PROVIDERS_DETECT_LIMITS, request)
}

pub fn remove(client: &Client, provider_id: &str) -> Result<()> {
    supported(client)?;
    let _: api::Removed = client.call(method::PROVIDERS_REMOVE, api::Key { provider_id: provider_id.into() })?;
    Ok(())
}

pub fn test(client: &Client, base_url: &str, key: Option<String>, provider_id: Option<String>) -> Result<api::Tested> {
    supported(client)?;
    client.call(method::PROVIDERS_TEST, api::Test { base_url: base_url.into(), key: key.map(Secret), provider_id })
}

/// A provider id from its name: lowercase letters, digits and `-`, not one of `taken`.
pub fn id_for(name: &str, taken: &[&str]) -> String {
    let mut id = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let mut id: String = id.trim_end_matches('-').chars().take(56).collect::<String>().trim_end_matches('-').to_string();
    if id.is_empty() {
        id = "provider".into();
    }
    if !taken.contains(&id.as_str()) {
        return id;
    }
    (2..).map(|n| format!("{id}-{n}")).find(|c| !taken.contains(&c.as_str())).expect("an unused number")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_from_names_and_stay_unique() {
        assert_eq!(id_for("OpenRouter", &[]), "openrouter");
        assert_eq!(id_for("  Ollama (workstation) ", &[]), "ollama-workstation");
        assert_eq!(id_for("Ollama", &["ollama", "ollama-2"]), "ollama-3");
        assert_eq!(id_for("日本", &[]), "provider");
        let long = id_for(&"a-".repeat(80), &[]);
        assert!(long.len() <= 56 && !long.ends_with('-') && !long.starts_with('-'), "{long}");
    }
}
