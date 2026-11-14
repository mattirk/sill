//! Model providers (docs/model-providers.md): one record for an OpenAI-compatible endpoint — where
//! it is, an optional key, the models the person picked. A provider belongs to the daemon host
//! and to no agent. An agent can use providers when there is a [`Translation`] for it: the code
//! that turns providers into that agent's configuration. Everything else here knows providers
//! and translations, not a particular agent.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// The API an endpoint speaks. One value now; an agent that speaks another format would be
/// offered only the providers it can talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// `/chat/completions` and `/models` under the base URL (OpenRouter, Ollama's `/v1`, LM
    /// Studio, llama.cpp, vLLM).
    Openai,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Lowercase letters, digits and `-`: a file name, a part of an environment variable name
    /// and of the id the agent shows.
    pub id: String,
    pub name: String,
    pub api: Api,
    /// `http(s)://host[:port][/path]`, the root the API's paths hang under (`…/v1`).
    pub base_url: String,
    /// The model ids offered to agents, as the endpoint names them.
    #[serde(default)]
    pub models: Vec<String>,
    /// One of `models`: the model a session starts with unless the person picked another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// Where a base URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// The proxy pattern that grants exactly this host and port (SPEC §4.8).
    pub fn pattern(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The address is written as one of this machine's neighbours (a private, link-local or
    /// other host-local literal). A name is not resolved here: it may still point at one.
    pub fn local_literal(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok_and(crate::proxy::is_local)
    }
}

/// Parse a base URL. Loopback is refused: inside a jail it is the jail's own (the session
/// environment sets `NO_PROXY` for it), so a server on the daemon host is reached by the
/// host's address on its network, never by `localhost`.
pub fn endpoint(base_url: &str) -> Result<Endpoint> {
    ensure!(base_url.len() <= 2048 && !base_url.chars().any(|c| c.is_control() || c.is_whitespace()), "the address has spaces or control characters, or is too long");
    let (scheme, rest) = base_url.split_once("://").context("the address must start with http:// or https://")?;
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => bail!("the address must start with http:// or https://"),
    };
    ensure!(!rest.contains(['?', '#']), "the address cannot have a query or a fragment");
    let authority = rest.split('/').next().unwrap_or_default();
    ensure!(!authority.contains('@'), "the address cannot carry a user name or a password; the key has its own field");
    ensure!(!authority.starts_with('['), "IPv6 literal addresses are not supported; use a name or an IPv4 address");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok().filter(|p| *p != 0).context("the address has an invalid port")?),
        None => (authority, default_port),
    };
    let host = host.to_ascii_lowercase();
    ensure!(crate::proxy::valid_hostname(&host), "the address has an invalid host name");
    let loopback = host == "localhost" || host.ends_with(".localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    ensure!(!loopback, "inside a session {host} is the session's own jail, not this machine; use the machine's address on its network (for example 192.168.1.20)");
    Ok(Endpoint { host, port })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.starts_with('-') && !id.ends_with('-') && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

impl Provider {
    pub fn validate(&self) -> Result<()> {
        ensure!(valid_id(&self.id), "a provider id is 1–64 lowercase letters, digits and '-'");
        ensure!(valid_text(&self.name, 128), "a provider needs a name of at most 128 characters");
        endpoint(&self.base_url)?;
        ensure!(self.models.len() <= 512, "at most 512 models per provider");
        let mut seen = std::collections::BTreeSet::new();
        for model in &self.models {
            ensure!(valid_text(model, 256) && model.trim() == model, "invalid model id {model:?}");
            ensure!(seen.insert(model), "model {model} is listed twice");
        }
        if let Some(default) = &self.default_model {
            ensure!(self.models.contains(default), "the default model {default} is not among the provider's models");
        }
        Ok(())
    }

    pub fn endpoint(&self) -> Endpoint {
        endpoint(&self.base_url).expect("a validated provider has a valid address")
    }

    /// The environment variable an agent finds this provider's key in.
    pub fn key_env(&self) -> String {
        format!("SILL_PROVIDER_{}_KEY", self.id.to_ascii_uppercase().replace('-', "_"))
    }
}

pub fn dir(home: &Path) -> PathBuf {
    home.join(".config/sill/providers")
}

/// Every provider, by id. A file that does not read is an error, as an agent definition is.
pub fn load(home: &Path) -> Result<BTreeMap<String, Provider>> {
    let dir = dir(home);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read model providers {}", dir.display())),
    };
    let mut out = BTreeMap::new();
    for entry in rd {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("toml") { continue; }
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let provider: Provider = toml::from_str(&text).with_context(|| format!("invalid model provider {}", path.display()))?;
        provider.validate().with_context(|| format!("invalid model provider {}", path.display()))?;
        ensure!(path.file_stem().and_then(|s| s.to_str()) == Some(provider.id.as_str()), "model provider {} is not named after its id {}", path.display(), provider.id);
        out.insert(provider.id.clone(), provider);
    }
    Ok(out)
}

pub fn save(home: &Path, provider: &Provider) -> Result<PathBuf> {
    provider.validate()?;
    let dir = dir(home);
    std::fs::create_dir_all(&dir)?;
    let name = format!("{}.toml", provider.id);
    crate::safe_fs::replace_file(&dir, std::ffi::OsStr::new(&name), toml::to_string_pretty(provider)?.as_bytes())?;
    Ok(dir.join(name))
}

/// Remove the record and its key; whether there was a record.
pub fn remove(home: &Path, id: &str) -> Result<bool> {
    ensure!(valid_id(id), "invalid provider id");
    remove_key(home, id)?;
    match std::fs::remove_file(dir(home).join(format!("{id}.toml"))) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// --- keys ---------------------------------------------------------------------------------------

/// Where provider keys are kept: in sill's state, which no jail sees, as the login keys are
/// (`agents::api_key_dir`). A key reaches a jail only in the environment of an agent that has a
/// translation.
pub fn key_dir(home: &Path) -> PathBuf {
    std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/state")).join("sill").join("provider-keys")
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 4096 && !key.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub fn save_key(home: &Path, id: &str, key: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    ensure!(valid_id(id), "invalid provider id");
    ensure!(valid_key(key), "a key is at most 4096 characters without spaces or control characters");
    let dir = key_dir(home);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(".{id}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(key.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(id))?;
    Ok(())
}

pub fn key(home: &Path, id: &str) -> Result<Option<String>> {
    ensure!(valid_id(id), "invalid provider id");
    match std::fs::read_to_string(key_dir(home).join(id)) {
        Ok(key) => Ok(Some(key).filter(|k| valid_key(k))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read the provider's key"),
    }
}

pub fn remove_key(home: &Path, id: &str) -> Result<bool> {
    ensure!(valid_id(id), "invalid provider id");
    match std::fs::remove_file(key_dir(home).join(id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// --- translations -------------------------------------------------------------------------------

/// How an agent is told about providers. The only agent-specific code of the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translation {
    /// OpenCode: an inline configuration in `OPENCODE_CONFIG_CONTENT`, merged by OpenCode over
    /// its own files, each provider on its generic OpenAI-compatible path.
    OpenCode,
}

/// The agents there is a translation for, by registry id.
pub const TRANSLATED: &[&str] = &["opencode"];

/// The translation for an agent, by its registry id; none for most agents.
pub fn translation(agent_id: &str) -> Option<Translation> {
    match agent_id {
        "opencode" => Some(Translation::OpenCode),
        _ => None,
    }
}

impl Translation {
    fn speaks(self, api: Api) -> bool {
        match self {
            Translation::OpenCode => api == Api::Openai,
        }
    }

    /// The id the agent knows a provider by: prefixed, so that a provider named like one the
    /// agent ships (`openrouter`) is not merged into the agent's own.
    pub fn provider_id(self, provider: &Provider) -> String {
        format!("sill-{}", provider.id)
    }

    fn env(self, providers: &[&Provider], has_key: &dyn Fn(&Provider) -> bool) -> BTreeMap<String, String> {
        match self {
            Translation::OpenCode => {
                let mut entries = serde_json::Map::new();
                for p in providers {
                    let mut options = serde_json::json!({ "baseURL": p.base_url });
                    if has_key(p) {
                        options["apiKey"] = format!("{{env:{}}}", p.key_env()).into();
                    }
                    let models: serde_json::Map<String, serde_json::Value> = p.models.iter().map(|m| (m.clone(), serde_json::json!({}))).collect();
                    entries.insert(self.provider_id(p), serde_json::json!({ "npm": "@ai-sdk/openai-compatible", "name": p.name, "options": options, "models": models }));
                }
                let mut config = serde_json::json!({ "provider": entries });
                // one `model` for the whole configuration: the first provider that names a default
                if let Some((p, model)) = providers.iter().find_map(|p| p.default_model.as_ref().map(|m| (p, m))) {
                    config["model"] = format!("{}/{model}", self.provider_id(p)).into();
                }
                BTreeMap::from([("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string())])
            }
        }
    }
}

/// The providers an agent is offered: those its translation can talk to that have a model.
fn offered(home: &Path, agent_id: &str) -> Result<Option<(Translation, Vec<Provider>)>> {
    let Some(translation) = translation(agent_id) else { return Ok(None) };
    let providers = load(home)?.into_values().filter(|p| translation.speaks(p.api) && !p.models.is_empty()).collect();
    Ok(Some((translation, providers)))
}

/// What launching `agent_id` adds to its environment: the agent's configuration for the
/// providers, and each saved key. Empty without a translation or without providers.
pub fn agent_env(home: &Path, agent_id: &str) -> Result<BTreeMap<String, String>> {
    let Some((translation, providers)) = offered(home, agent_id)? else { return Ok(BTreeMap::new()) };
    if providers.is_empty() { return Ok(BTreeMap::new()); }
    let mut keys = BTreeMap::new();
    for p in &providers {
        if let Some(key) = key(home, &p.id)? {
            keys.insert(p.key_env(), key);
        }
    }
    let refs: Vec<&Provider> = providers.iter().collect();
    let mut env = translation.env(&refs, &|p| keys.contains_key(&p.key_env()));
    env.extend(keys);
    Ok(env)
}

/// The proxy patterns a session of `agent_id` is granted: each offered provider's exact host
/// and port, as host-local grants, whatever the address is.
pub fn agent_endpoints(home: &Path, agent_id: &str) -> Result<Vec<String>> {
    let Some((_, providers)) = offered(home, agent_id)? else { return Ok(vec![]) };
    let mut patterns: Vec<String> = providers.iter().map(|p| p.endpoint().pattern()).collect();
    patterns.sort();
    patterns.dedup();
    Ok(patterns)
}

// --- test ---------------------------------------------------------------------------------------

/// Ask the endpoint for its models (`GET <base_url>/models`), from this machine: the daemon
/// host, where sessions reach it from. curl, as the registry download uses; the key goes in on
/// stdin, never on the command line.
pub fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<String>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    endpoint(base_url)?;
    if let Some(key) = key {
        ensure!(valid_key(key), "a key is at most 4096 characters without spaces or control characters");
    }
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut config = format!("url = {}\n", quoted(&format!("{}/models", base_url.trim_end_matches('/'))));
    if let Some(key) = key {
        config.push_str(&format!("header = {}\n", quoted(&format!("Authorization: Bearer {key}"))));
    }
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "15", "--max-filesize", "4194304", "--proto", "=http,https", "--write-out", "\n%{http_code}", "--config", "-"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().context("run curl")?;
    child.stdin.take().expect("piped stdin").write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("the endpoint could not be reached: {}", String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: "));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text.rsplit_once('\n').context("no answer from the endpoint")?;
    if !status.starts_with('2') {
        let said: String = body.trim().chars().take(300).collect();
        bail!("the endpoint answered {status}{}", if said.is_empty() { String::new() } else { format!(": {said}") });
    }
    parse_models(body)
}

/// `{"data":[{"id":…}]}`, the OpenAI shape; a bare list of the same objects is read too.
fn parse_models(body: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(body).context("the endpoint's answer is not a model list (is the address the API's root, ending in /v1?)")?;
    let list = value.get("data").unwrap_or(&value).as_array().context("the endpoint's answer is not a model list")?;
    let mut models: Vec<String> = list.iter()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
        .filter(|id| valid_text(id, 256) && id.trim() == *id)
        .map(str::to_string)
        .collect();
    models.sort();
    models.dedup();
    models.truncate(2000);
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Home(PathBuf);
    impl Home {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sill-providers-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Home(dir)
        }
    }
    impl Drop for Home {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn provider(id: &str, url: &str) -> Provider {
        Provider { id: id.into(), name: format!("Provider {id}"), api: Api::Openai, base_url: url.into(), models: vec!["qwen/coder".into(), "small".into()], default_model: None }
    }

    #[test]
    fn an_address_names_one_host_and_port() {
        assert_eq!(endpoint("https://openrouter.ai/api/v1").unwrap().pattern(), "openrouter.ai:443");
        assert_eq!(endpoint("http://192.168.1.20:11434/v1").unwrap().pattern(), "192.168.1.20:11434");
        assert_eq!(endpoint("http://GPU-Box.lan/v1").unwrap().pattern(), "gpu-box.lan:80");
        assert!(endpoint("http://192.168.1.20:11434/v1").unwrap().local_literal());
        assert!(!endpoint("https://openrouter.ai/api/v1").unwrap().local_literal());
        for bad in ["openrouter.ai/v1", "ftp://x/v1", "https://user:pw@x.example/v1", "https://x.example/v1?a=b", "https://x.example:0/v1", "https://x.example:99999/v1", "https://[::1]:8080/v1", "https://x .example/v1", "https:///v1"] {
            assert!(endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loopback_is_refused_with_the_reason() {
        for url in ["http://localhost:11434/v1", "http://127.0.0.1:11434/v1", "http://127.1.2.3/v1", "http://ollama.localhost/v1", "http://0.0.0.0:11434/v1"] {
            let e = endpoint(url).unwrap_err().to_string();
            assert!(e.contains("the session's own jail"), "{url}: {e}");
        }
    }

    #[test]
    fn records_are_validated() {
        assert!(provider("lan-ollama", "http://192.168.1.20:11434/v1").validate().is_ok());
        for id in ["", "Upper", "a_b", "-a", "a-", "a/b", ".."] {
            assert!(provider(id, "https://x.example/v1").validate().is_err(), "{id}");
        }
        let mut p = provider("x", "https://x.example/v1");
        p.default_model = Some("absent".into());
        assert!(p.validate().is_err());
        p.default_model = Some("small".into());
        assert!(p.validate().is_ok());
        p.models.push("small".into());
        assert!(p.validate().is_err(), "a model listed twice");
        assert!(toml::from_str::<Provider>("id='x'\nname='X'\napi='openai'\nbase_url='https://x.example/v1'\nextra=1").is_err(), "unknown fields are refused");
        assert!(toml::from_str::<Provider>("id='x'\nname='X'\napi='anthropic'\nbase_url='https://x.example/v1'").is_err(), "one api format");
    }

    #[test]
    fn saved_records_load_and_removal_takes_the_key() {
        let home = Home::new("store");
        assert!(load(&home.0).unwrap().is_empty());
        let p = provider("lan-ollama", "http://192.168.1.20:11434/v1");
        save(&home.0, &p).unwrap();
        assert_eq!(load(&home.0).unwrap().get("lan-ollama"), Some(&p));
        // a file not named after its id would be a second provider under one id
        std::fs::copy(dir(&home.0).join("lan-ollama.toml"), dir(&home.0).join("other.toml")).unwrap();
        assert!(load(&home.0).is_err());
        std::fs::remove_file(dir(&home.0).join("other.toml")).unwrap();
        assert!(remove(&home.0, "lan-ollama").unwrap());
        assert!(!remove(&home.0, "lan-ollama").unwrap());
        assert!(remove(&home.0, "../x").is_err());
    }

    #[test]
    fn opencode_gets_an_inline_configuration_and_the_keys() {
        let t = Translation::OpenCode;
        let mut router = provider("openrouter", "https://openrouter.ai/api/v1");
        router.default_model = Some("qwen/coder".into());
        let lan = provider("lan-ollama", "http://192.168.1.20:11434/v1");
        let env = t.env(&[&lan, &router], &|p| p.id == "openrouter");
        assert_eq!(env.len(), 1);
        let config: serde_json::Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
        let r = &config["provider"]["sill-openrouter"];
        assert_eq!(r["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(r["name"], "Provider openrouter");
        assert_eq!(r["options"]["baseURL"], "https://openrouter.ai/api/v1");
        assert_eq!(r["options"]["apiKey"], "{env:SILL_PROVIDER_OPENROUTER_KEY}");
        assert!(r["models"]["qwen/coder"].is_object() && r["models"]["small"].is_object());
        let l = &config["provider"]["sill-lan-ollama"];
        assert!(l["options"].get("apiKey").is_none(), "no key, no reference to one");
        assert_eq!(config["model"], "sill-openrouter/qwen/coder");
        assert_eq!(lan.key_env(), "SILL_PROVIDER_LAN_OLLAMA_KEY");
        assert!(!env["OPENCODE_CONFIG_CONTENT"].contains("sk-"), "the key itself is never in the configuration");
    }

    #[test]
    fn only_agents_with_a_translation_are_given_providers() {
        let home = Home::new("agents");
        save(&home.0, &provider("openrouter", "https://openrouter.ai/api/v1")).unwrap();
        save(&home.0, &provider("lan", "http://192.168.1.20:11434/v1")).unwrap();
        let mut empty = provider("empty", "https://empty.example/v1");
        empty.models.clear();
        save(&home.0, &empty).unwrap();
        assert_eq!(agent_endpoints(&home.0, "opencode").unwrap(), ["192.168.1.20:11434", "openrouter.ai:443"]);
        for other in ["claude", "codex", "opencode-v2", ""] {
            assert!(agent_endpoints(&home.0, other).unwrap().is_empty(), "{other}");
            assert!(agent_env(&home.0, other).unwrap().is_empty(), "{other}");
        }
        let config = &agent_env(&home.0, "opencode").unwrap()["OPENCODE_CONFIG_CONTENT"];
        assert!(config.contains("sill-lan") && !config.contains("sill-empty"), "a provider without models is not offered");
    }

    #[test]
    fn a_grant_reaches_the_one_port_and_nothing_else_on_the_host() {
        use crate::proxy::{Decision, Rules};
        let home = Home::new("grant");
        save(&home.0, &provider("openrouter", "https://openrouter.ai/api/v1")).unwrap();
        save(&home.0, &provider("lan", "http://192.168.1.20:11434/v1")).unwrap();
        let rules = Rules { mode: crate::policy::NetMode::Allow, allow: vec![], allow_local: agent_endpoints(&home.0, "opencode").unwrap() };
        let allowed = |host: &str, port: u16, default: u16| matches!(rules.decide_name(host, port, default), Decision::Allow("allow_local"));
        assert!(allowed("192.168.1.20", 11434, 80), "the model server, over plain http");
        assert!(!allowed("192.168.1.20", 22, 443), "another port of that machine");
        assert!(!allowed("192.168.1.20", 80, 80));
        assert!(!allowed("192.168.1.21", 11434, 80), "its neighbour");
        assert!(allowed("openrouter.ai", 443, 443));
        assert!(!allowed("openrouter.ai", 80, 80));
        assert!(!allowed("evil.openrouter.ai", 443, 443));
    }

    #[test]
    fn model_lists_are_read_in_both_shapes() {
        assert_eq!(parse_models(r#"{"object":"list","data":[{"id":"b"},{"id":"a"},{"id":"a"},{"name":"no id"}]}"#).unwrap(), ["a", "b"]);
        assert_eq!(parse_models(r#"[{"id":"only"}]"#).unwrap(), ["only"]);
        assert!(parse_models("<html>not found</html>").is_err());
        assert!(parse_models(r#"{"error":"no"}"#).is_err());
    }
}
