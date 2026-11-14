//! Model providers (`_sill/providers.*`, docs/model-providers.md): records on this daemon host.
use anyhow::Result;
use serde_json::{Value, json};
use sill_core::providers as core;
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

pub fn handle(m: &str, p: Value) -> Option<Result<Value>> {
    if !matches!(m, "_sill/providers.list" | "_sill/providers.save" | "_sill/providers.remove" | "_sill/providers.test") { return None; }
    Some((|| {
        let home = sill_core::changes::home()?;
        Ok(match m {
            "_sill/providers.list" => {
                let _: sill_proto::Empty = args(p)?;
                let mut providers = vec![];
                for p in core::load(&home)?.into_values() {
                    let (has_key, endpoint) = (core::key(&home, &p.id)?.is_some(), p.endpoint().pattern());
                    providers.push(api::Entry { provider: public(p)?, has_key, endpoint });
                }
                let agents = core::TRANSLATED.iter().map(|id| id.to_string()).collect();
                json!(api::Listed { providers, agents })
            }
            "_sill/providers.save" => {
                let a: Save = args(p)?;
                a.provider.validate().map_err(invalid)?;
                if core::load(&home)?.len() >= 64 && !core::load(&home)?.contains_key(&a.provider.id) {
                    return Err(Error::domain("limit_exceeded", "at most 64 model providers").into());
                }
                // the key first: a record never names a key that failed to save
                match (&a.key, a.clear_key) {
                    (Some(key), _) => core::save_key(&home, &a.provider.id, &key.0).map_err(invalid)?,
                    (None, true) => { core::remove_key(&home, &a.provider.id)?; }
                    (None, false) => {}
                }
                core::save(&home, &a.provider)?;
                json!(api::Saved { endpoint: a.provider.endpoint().pattern() })
            }
            "_sill/providers.remove" => {
                let a: api::Key = args(p)?;
                json!(api::Removed { removed: core::remove(&home, &a.provider_id).map_err(invalid)? })
            }
            "_sill/providers.test" => {
                let a: api::Test = args(p)?;
                let endpoint = core::endpoint(&a.base_url).map_err(invalid)?.pattern();
                let key = match (a.key, &a.provider_id) {
                    (Some(key), _) => Some(key.0),
                    (None, Some(id)) => core::key(&home, id).map_err(invalid)?,
                    (None, None) => None,
                };
                let models = core::list_models(&a.base_url, key.as_deref()).map_err(|e| anyhow::Error::from(Error::domain("operation_failed", format!("{e:#}"))))?;
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
        let stored = core::Provider { id: "lan".into(), name: "LAN".into(), api: core::Api::Openai, base_url: "http://192.168.1.20:11434/v1".into(), models: vec!["a".into()], default_model: Some("a".into()) };
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
