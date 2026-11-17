//! Model providers (`_sill/providers.*`, docs/model-providers.md): records on this daemon host.
use anyhow::Result;
use serde_json::{Value, json};
use sill_core::providers as core;
use sill_proto::method;
use sill_proto::{providers as api, rpc::Error};

fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::invalid_params(e).into())
}

fn invalid(e: anyhow::Error) -> anyhow::Error {
    Error::invalid_params(format!("{e:#}")).into()
}

/// A save as the daemon reads it: the record strict, as its file is.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    provider: core::Provider,
    #[serde(default)]
    key: Option<sill_proto::wire::Secret>,
    #[serde(default)]
    clear_key: bool,
}

fn public(p: core::Provider) -> Result<api::Provider> {
    Ok(serde_json::from_value(json!(p))?)
}

// A saved credential belongs to its saved API root, not an arbitrary edited address.
fn request_key(home: &std::path::Path, base_url: &str, key: Option<sill_proto::wire::Secret>, id: Option<&str>) -> Result<Option<String>> {
    if let Some(key) = key {
        return Ok(Some(key.0));
    }
    let Some(id) = id else {
        return Ok(None);
    };
    let key = core::key(home, id).map_err(invalid)?;
    if key.is_none() {
        return Ok(None);
    }
    let providers = core::load(home)?;
    let provider = providers.get(id).ok_or_else(|| Error::invalid_params("unknown provider"))?;
    if provider.base_url.trim_end_matches('/') != base_url.trim_end_matches('/') {
        return Err(Error::invalid_params("The address changed; enter a key explicitly or save the new address before testing or detecting context").into());
    }
    Ok(key)
}

/// The requests answered here, each named once (`HANDLERS` in hub.rs holds every such list).
pub(super) const METHODS: &[&str] =
    &[method::PROVIDERS_LIST, method::PROVIDERS_SAVE, method::PROVIDERS_REMOVE, method::PROVIDERS_TEST, method::PROVIDERS_DETECT_LIMITS];
pub fn handle(conn: &super::Conn, m: &str, p: Value) -> Option<Result<Value>> {
    if !METHODS.contains(&m) {
        return None;
    }
    Some((|| {
        let home = sill_core::changes::home()?;
        Ok(match m {
            method::PROVIDERS_LIST => {
                let _: sill_proto::Empty = args(p)?;
                let mut providers = vec![];
                for p in core::load(&home)?.into_values() {
                    let (has_key, endpoint) = (core::key(&home, &p.id)?.is_some(), p.endpoint().pattern());
                    providers.push(api::Entry { provider: public(p)?, has_key, endpoint });
                }
                let agents = core::TRANSLATED.iter().map(|id| id.to_string()).collect();
                json!(api::Listed { providers, agents })
            }
            method::PROVIDERS_SAVE => {
                let mut a: Save = args(p)?;
                a.provider.validate().map_err(invalid)?;
                if !a.provider.model_limits.is_empty() && !conn.has_capability(api::LIMITS_CAPABILITY) {
                    return Err(Error::needs_capability(api::LIMITS_CAPABILITY, "Model token limits need sill.provider_limits").into());
                }
                let saved = core::load(&home)?;
                // An old frontend cannot edit limits, so leaving them out must not erase them:
                // they stay with the models it kept, at the address they were measured for.
                if !conn.has_capability(api::LIMITS_CAPABILITY)
                    && let Some(saved) = saved.get(&a.provider.id).filter(|saved| saved.base_url == a.provider.base_url)
                {
                    a.provider.model_limits = saved
                        .model_limits
                        .iter()
                        .filter(|(model, _)| a.provider.models.contains(model))
                        .map(|(model, limits)| (model.clone(), limits.clone()))
                        .collect();
                }
                if saved.len() >= 64 && !saved.contains_key(&a.provider.id) {
                    return Err(Error::domain("limit_exceeded", "at most 64 model providers").into());
                }
                // the key first: a record never names a key that failed to save
                match (&a.key, a.clear_key) {
                    (Some(key), _) => core::save_key(&home, &a.provider.id, &key.0).map_err(invalid)?,
                    (None, true) => {
                        core::remove_key(&home, &a.provider.id)?;
                    }
                    (None, false) => {}
                }
                core::save(&home, &a.provider)?;
                json!(api::Saved { endpoint: a.provider.endpoint().pattern() })
            }
            method::PROVIDERS_REMOVE => {
                let a: api::Key = args(p)?;
                json!(api::Removed { removed: core::remove(&home, &a.provider_id).map_err(invalid)? })
            }
            method::PROVIDERS_DETECT_LIMITS => {
                if !conn.has_capability(api::LIMITS_CAPABILITY) {
                    return Err(Error::needs_capability(api::LIMITS_CAPABILITY, "Update the frontend for model token limits").into());
                }
                let a: api::DetectLimits = args(p)?;
                let key = request_key(&home, &a.base_url, a.key, a.provider_id.as_deref())?;
                let context = core::detect_context(&a.base_url, key.as_deref(), &a.model).map_err(|e| Error::domain("operation_failed", format!("{e:#}")))?;
                json!(api::DetectedLimits { context })
            }
            method::PROVIDERS_TEST => {
                let a: api::Test = args(p)?;
                let endpoint = core::endpoint(&a.base_url).map_err(invalid)?.pattern();
                let key = request_key(&home, &a.base_url, a.key, a.provider_id.as_deref())?;
                let models =
                    core::list_models(&a.base_url, key.as_deref()).map_err(|e| anyhow::Error::from(Error::domain("operation_failed", format!("{e:#}"))))?;
                json!(api::Tested { models, endpoint })
            }
            _ => unreachable!(),
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_record_and_the_stored_one_are_the_same_shape() {
        let stored = core::Provider {
            id: "lan".into(),
            name: "LAN".into(),
            api: core::Api::Openai,
            base_url: "http://192.168.1.20:11434/v1".into(),
            models: vec!["a".into()],
            default_model: Some("a".into()),
            model_limits: [("a".into(), core::ModelLimits { context: Some(32768), output: Some(4096) })].into(),
        };
        let back: core::Provider = serde_json::from_value(json!(public(stored.clone()).unwrap())).unwrap();
        assert_eq!(back, stored);
    }

    #[test]
    fn a_save_refuses_fields_it_does_not_know() {
        let save = json!({ "provider": { "id": "x", "name": "X", "api": "openai", "base_url": "https://x.example/v1" }, "key": "sk-1" });
        assert!(args::<Save>(save.clone()).is_ok());
        let mut extra = save.clone();
        extra["provider"]["secret"] = json!(1);
        assert!(args::<Save>(extra).is_err());
        let mut extra = save;
        extra["other"] = json!(1);
        assert!(args::<Save>(extra).is_err());
    }
}
