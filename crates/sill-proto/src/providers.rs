//! Model providers (`sill.providers: 1`, docs/model-providers.md): records on the daemon host for
//! OpenAI-compatible endpoints. The daemon derives an agent's configuration, the key in its
//! environment and the network grant from them, for agents it has a translation for.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "sill.providers";

/// The API an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderApi")]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// OpenAI-compatible: `/chat/completions` and `/models` under the base URL.
    Openai,
}

/// A provider record: a result of `providers.list` and part of the params of `providers.save`.
/// Open in the schema because it is a result too; the daemon refuses unknown fields in a save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Provider {
    /// 1–64 lowercase letters, digits and `-`.
    pub id: String,
    pub name: String,
    pub api: Api,
    /// `http(s)://host[:port][/path]`. Loopback is refused: in a jail it is the jail's own.
    pub base_url: String,
    /// Model ids offered to agents, as the endpoint names them.
    #[serde(default)]
    pub models: Vec<String>,
    /// One of `models`: what a session starts with unless the person picked another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderEntry")]
pub struct Entry {
    pub provider: Provider,
    /// A key is saved for it. The key itself is never returned.
    pub has_key: bool,
    /// The one `host:port` sessions are allowed to reach for it.
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProvidersListed")]
pub struct Listed {
    pub providers: Vec<Entry>,
    /// Registry ids of the agents the daemon can hand providers to (it has a translation for
    /// them), installed or not.
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderSave")]
#[serde(deny_unknown_fields)]
pub struct Save {
    pub provider: Provider,
    /// Replaces the saved key. Absent: the saved key stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<crate::wire::Secret>,
    /// Forget the saved key (ignored when `key` is given).
    #[serde(default)]
    pub clear_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderSaved")]
pub struct Saved {
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderKey")]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub provider_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderRemoved")]
pub struct Removed {
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderTest")]
#[serde(deny_unknown_fields)]
pub struct Test {
    pub base_url: String,
    /// The key to try. Absent: the key saved for `provider_id`, when that is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<crate::wire::Secret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "ProviderTested")]
pub struct Tested {
    /// The endpoint's model ids, sorted; at most 2000.
    pub models: Vec<String>,
    pub endpoint: String,
}
