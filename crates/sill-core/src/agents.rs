//! Agent registry (SPEC §4.1): how to start each ACP agent inside the jail. Built-in entries plus
//! TOML files in `~/.config/sill/agents/` and `/usr/share/sill/agents/` (user files win).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf, Component};
use anyhow::{Context, Result, bail, ensure};
pub use sill_proto::agent_admin::{HomePath, HomePathKind};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Short id used in the store and the UI (`claude`, `codex`, …).
    pub id: String,
    pub name: String,
    /// Offered in the "+" menu. Off hides it without losing its settings.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Command line inside the jail.
    pub command: Vec<String>,
    /// Extra environment for the agent process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Advertises `loadSession`: conversations can be resumed with history replay.
    #[serde(default)]
    pub supports_load: bool,
    /// Explicit opt-in to the experimental ACP v2 adapter; ACP v1 remains the default.
    #[serde(default)]
    pub experimental_acp_v2: bool,
    /// Domains the agent needs beyond the policy's allow-list (merged into the proxy rules).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Environment that switches the agent's own sandbox off inside the jail (SPEC §4.2).
    #[serde(default)]
    pub sandbox_off_env: BTreeMap<String, String>,
    /// Exact persistent roots. None means setup required; Some([]) explicitly declares statelessness.
    #[serde(default, deserialize_with = "declared_paths")]
    pub home_paths: Option<Vec<HomePath>>,
    /// Credential files relative to the private home; no automatic discovery.
    #[serde(default)]
    pub login_files: Vec<String>,
    /// Login methods (ids from `initialize`) that run on the host instead of inside the jail: a
    /// browser flow with a localhost callback cannot be reached from the jail's own network
    /// namespace, so sill runs the agent on the host for the login only and the result lands in the
    /// declared credential files (see [`crate::login`]).
    #[serde(default)]
    pub host_login: Vec<String>,
    /// Environment variable that relocates the agent's home directory (`CODEX_HOME`). A host-side
    /// login points it at `home_paths[0]` inside the fresh scratch tree. Only declared
    /// credentials are imported into the persistent profile after successful login.
    #[serde(default)]
    pub home_env: Option<String>,
    /// `home_env` names the directory *containing* `home_paths[0]` rather than that path itself:
    /// `GEMINI_CLI_HOME` is a `$HOME` replacement (`$GEMINI_CLI_HOME/.gemini`), `CODEX_HOME` is the
    /// `~/.codex` directory.
    #[serde(default)]
    pub home_env_parent: bool,
    /// Symbolic icon name for the "+" menu and tabs (`sill-claude-symbolic`, any theme icon). Unknown
    /// or missing: a neutral placeholder.
    #[serde(default)]
    pub icon: Option<String>,
    /// Where the agent keeps its own memory, as patterns under `~` (`~/.claude/projects/*/memory`).
    /// Unused since the Agents page's Reset replaced "forget memory" (2026-11-03); kept so
    /// definitions that name it still load (unknown fields are refused).
    #[serde(default)]
    pub memory_paths: Vec<String>,
}

fn declared_paths<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vec<HomePath>>, D::Error> {
    use serde::de::Error;
    let value = Option::<Vec<serde_json::Value>>::deserialize(d)?;
    value.map(|entries| entries.into_iter().map(|entry| {
        if let Some(object) = entry.as_object() {
            if let Some(key) = object.keys().find(|key| !["path", "kind", "initial_content"].contains(&key.as_str())) {
                return Err(D::Error::custom(format!("unknown home_paths field {key}")));
            }
        }
        serde_json::from_value(entry).map_err(D::Error::custom)
    }).collect()).transpose()
}

fn yes() -> bool {
    true
}

/// The user's override file for an agent.
pub fn override_path(home: &Path, id: &str) -> std::path::PathBuf {
    home.join(".config/sill/agents").join(format!("{id}.toml"))
}

/// Write an agent as the user's override (the whole entry: overrides replace built-ins).
pub fn save_override(home: &Path, agent: &Agent) -> anyhow::Result<std::path::PathBuf> {
    agent.validate()?;
    let path = override_path(home, &agent.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(agent)?;
    crate::safe_fs::replace_file(path.parent().expect("override parent"), path.file_name().expect("override filename"), text.as_bytes())?;
    Ok(path)
}

/// Drop the user's override; the built-in (or the system file) applies again.
pub fn remove_override(home: &Path, id: &str) -> anyhow::Result<bool> {
    match std::fs::remove_file(override_path(home, id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn builtin() -> Vec<Agent> {
    vec![
        Agent {
            id: "claude".into(),
            name: "Claude Code".into(),
            enabled: true,
            command: vec!["claude-agent-acp".into()],
            env: Default::default(),
            supports_load: true,
            experimental_acp_v2: false,
            // claude.com: its login exchanges the code and renews the token at platform.claude.com
            // (refused there, a login never finished and an expired token was not renewed, 2026-11-05)
            domains: vec!["api.anthropic.com".into(), "*.anthropic.com".into(), "claude.ai".into(), "*.claude.ai".into(), "claude.com".into(), "*.claude.com".into()],
            sandbox_off_env: Default::default(),
            home_paths: Some(vec![HomePath::directory("~/.claude"), HomePath::file("~/.claude.json", "{}")]),
            login_files: vec![".claude/.credentials.json".into()],
            // sill's Claude logs in on its own (terminal login inside the jail, written to the sill-owned
            // home): a separate grant from the host's Claude Code / Zed, so neither can revoke the other
            host_login: vec![],
            home_env: None,
            home_env_parent: false,
            icon: Some("sill-claude-symbolic".into()),
            memory_paths: vec!["~/.claude/projects/*/memory".into()],
        },
        Agent {
            id: "codex".into(),
            name: "Codex".into(),
            enabled: true,
            command: vec!["codex-acp".into()],
            env: Default::default(),
            supports_load: true,
            experimental_acp_v2: false,
            domains: vec!["api.openai.com".into(), "*.openai.com".into(), "chatgpt.com".into(), "*.chatgpt.com".into()],
            // Codex wraps every command in a bubblewrap of its own, which the jail's user
            // namespace rule refuses (bwrap: ENOSPC); codex-acp's "Full access" mode runs them
            // without it, the jail being the boundary (2026-11-12)
            sandbox_off_env: BTreeMap::from([("INITIAL_AGENT_MODE".into(), "agent-full-access".into())]),
            home_paths: Some(vec![HomePath::directory("~/.codex")]),
            login_files: vec![".codex/auth.json".into()],
            // no host login is shared into jails: Codex in sill logs in on its own (the host-side
            // login lands in the sill-owned home through `CODEX_HOME`)
            host_login: vec!["chat-gpt".into(), "chatgpt".into()],
            home_env: Some("CODEX_HOME".into()),
            home_env_parent: false,
            icon: Some("sill-openai-symbolic".into()),
            memory_paths: vec!["~/.codex/memories".into()],
        },
        Agent {
            // what its login wrote on 2026-11-08 (acp_token.json, settings.json under the
            // Gemini-style directory), plus the skills it writes while working; registry entry
            // `antigravity-acp`, a binary install, so the command comes from the install
            id: "antigravity-acp".into(),
            name: "Google Antigravity".into(),
            enabled: true,
            command: vec!["agy_acp_server.par".into(), "--uid=".into()],
            env: Default::default(),
            supports_load: true,
            experimental_acp_v2: false,
            domains: vec!["*.googleapis.com".into(), "accounts.google.com".into()],
            sandbox_off_env: Default::default(),
            home_paths: Some(vec![HomePath::directory("~/.gemini/antigravity-acp"), HomePath::directory("~/skills")]),
            login_files: vec![".gemini/antigravity-acp/acp_token.json".into(), ".gemini/antigravity-acp/settings.json".into()],
            // "Log in with Google" and the Enterprise login open a browser with a localhost callback
            host_login: vec!["oauth-personal".into(), "oauth-business".into()],
            home_env: None,
            home_env_parent: false,
            icon: None, // the registry's icon, by id
            memory_paths: vec![],
        },
        Agent {
            // Kimi Code 2.5.2: directories and login files from the tested registry setup.
            id: "kimi-code".into(),
            name: "Kimi Code".into(),
            enabled: true,
            command: vec!["kimi".into(), "acp".into()],
            env: Default::default(),
            supports_load: true,
            experimental_acp_v2: false,
            domains: vec!["*.kimi.ai".into(), "*.kimi.com".into()],
            sandbox_off_env: Default::default(),
            home_paths: Some(vec![HomePath::directory("~/.kimi-code")]),
            login_files: vec![
                ".kimi-code/config.toml".into(),
                ".kimi-code/credentials/kimi-code.json".into(),
                // Global endpoint slot: SHA-256 of the public OAuth/API endpoint pair,
                // truncated by Kimi to 16 hex digits; independent of account/device.
                ".kimi-code/credentials/kimi-code-env-0e4f99c69cc27850.json".into(),
                ".kimi-code/device_id".into(),
                ".kimi-code/region.json".into(),
            ],
            host_login: vec!["login".into()],
            home_env: Some("KIMI_CODE_HOME".into()),
            home_env_parent: false,
            icon: None,
            memory_paths: vec![],
        },
        Agent {
            // Mistral Vibe 3.1.1 as the person's login left it on 2026-11-13; registry entry
            // `mistral-vibe`, a binary install, so the command comes from the install
            id: "mistral-vibe".into(),
            name: "Mistral Vibe".into(),
            enabled: true,
            command: vec!["vibe-acp".into()],
            env: Default::default(),
            supports_load: true,
            experimental_acp_v2: false,
            // console.mistral.ai for the sign-in, api.mistral.ai for the models
            domains: vec!["*.mistral.ai".into()],
            sandbox_off_env: Default::default(),
            home_paths: Some(vec![HomePath::directory("~/.vibe")]),
            // the API key the sign-in exchanges for; Vibe writes it here where it has no keyring
            login_files: vec![".vibe/.env".into()],
            // "Sign in through Mistral AI Studio" opens a browser itself and waits for it
            host_login: vec!["browser-auth".into()],
            home_env: None,
            home_env_parent: false,
            icon: None, // the registry's icon, by id
            memory_paths: vec![],
        },
        Agent {
            id: "gemini".into(),
            name: "Gemini CLI".into(),
            enabled: true,
            command: vec!["gemini".into(), "--experimental-acp".into()],
            env: Default::default(),
            supports_load: false,
            experimental_acp_v2: false,
            domains: vec!["*.googleapis.com".into(), "accounts.google.com".into()],
            sandbox_off_env: Default::default(),
            home_paths: Some(vec![HomePath::directory("~/.gemini")]),
            login_files: vec![".gemini/oauth_creds.json".into(), ".gemini/google_accounts.json".into()],
            // its login lands in the sill-owned home (the host-side login relocates GEMINI_CLI_HOME)
            // "Log in with Google" opens a browser and waits on a 127.0.0.1 callback port: host only
            host_login: vec!["oauth-personal".into()],
            home_env: Some("GEMINI_CLI_HOME".into()),
            home_env_parent: true,
            icon: Some("sill-gemini-symbolic".into()),
            // the `save_memory` tool appends to this one file
            memory_paths: vec!["~/.gemini/GEMINI.md".into()],
        },
    ]
}

/// All agents, by id; files override built-ins with the same id.
pub fn load(home: &Path) -> Result<BTreeMap<String, Agent>> {
    let mut out: BTreeMap<String, Agent> = builtin().into_iter().map(|a| (a.id.clone(), a)).collect();
    for dir in [PathBuf::from("/usr/share/sill/agents"), home.join(".config/sill/agents")] {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read agent definitions {}", dir.display())),
        };
        for entry in rd {
            let p = entry?.path();
            if p.extension().and_then(|x| x.to_str()) != Some("toml") { continue; }
            let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
            let a: Agent = toml::from_str(&text).with_context(|| format!("invalid agent definition {}; update it to the current format", p.display()))?;
            a.validate().with_context(|| format!("invalid agent definition {}", p.display()))?;
            out.insert(a.id.clone(), a);
        }
    }
    Ok(out)
}

/// Strict relative path validation: never normalize away traversal or a shared parent.
pub fn relative_path(path: &str) -> Result<PathBuf> {
    ensure!(!path.is_empty() && !path.contains(['\0', '*', '?', '[', ']']), "expected an exact relative path");
    ensure!(path.split('/').all(|c| !c.is_empty() && c != "." && c != ".."), "path has empty, dot or parent components");
    let p = PathBuf::from(path);
    ensure!(p.components().all(|c| matches!(c, Component::Normal(_))), "expected a relative path");
    Ok(p)
}

impl Agent {
    pub fn validate(&self) -> Result<()> {
        ensure!(plain_id(&self.id), "invalid agent id");
        ensure!(!self.home_env_parent || self.home_env.is_some(), "home_env_parent requires home_env");
        let Some(paths) = &self.home_paths else {
            ensure!(self.login_files.is_empty() && self.home_env.is_none(), "declare home_paths before credentials or home_env");
            return Ok(());
        };
        let mut roots = Vec::new();
        for entry in paths {
            let rel = relative_path(entry.path.strip_prefix("~/").context("home_paths must start with ~/")?)?;
            ensure!(![".config", ".local", ".local/share", ".local/state", ".local/bin", ".cache"].iter().any(|p| rel == Path::new(p)), "{} is a shared parent", entry.path);
            ensure!(entry.kind == HomePathKind::File || entry.initial_content.is_none(), "directory cannot have initial_content");
            ensure!(!roots.iter().any(|p: &PathBuf| rel.starts_with(p) || p.starts_with(&rel)), "overlapping home_paths: {}", entry.path);
            roots.push(rel);
        }
        for credential in &self.login_files {
            let rel = relative_path(credential)?;
            ensure!(paths.iter().zip(&roots).any(|(entry, root)| entry.kind == HomePathKind::Directory && rel != *root && rel.starts_with(root)), "credential {credential} must be below a declared directory");
        }
        if self.home_env.is_some() {
            ensure!(paths.first().is_some_and(|p| p.kind == HomePathKind::Directory), "home_env requires a directory as the first home_paths entry");
        }
        Ok(())
    }
    pub fn ready(&self, home: &Path) -> Result<&[HomePath]> {
        self.validate()?;
        match &self.home_paths {
            Some(paths) => Ok(paths),
            None => bail!("agent {} needs directory setup: declare home_paths in {} before login or launch", self.id, override_path(home, &self.id).display()),
        }
    }
}

/// Reset an agent to how a fresh install starts: each of `homes` (its sill-owned home, the copies
/// in ghost sessions) removed whole — login, settings, history, its memory. A link inside is
/// removed, never what it points to. Stop the agent's running sessions first: a running agent
/// holds its login and writes to its home. How many files went.
pub fn reset_homes(homes: &[std::path::PathBuf]) -> anyhow::Result<usize> {
    let mut n = 0;
    for home in homes {
        if std::fs::symlink_metadata(home).is_err() {
            continue;
        }
        n += count_files(home);
        std::fs::remove_dir_all(home).map_err(|e| anyhow::anyhow!("removing {}: {e}", home.display()))?;
    }
    Ok(n)
}

/// Where sill keeps the API keys the person typed for an agent's login methods: in sill's state,
/// not in any folder bound into a jail (`~/.local/state/sill` shows only `tools/` there), so the
/// agent's commands cannot read them; the agent gets a key only in its login request.
pub fn api_key_dir(home: &Path, agent_id: &str) -> std::path::PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("keys")
        .join(agent_id)
}

fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id != "." && id != ".." && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// The file of the key for `method_id`; refused for an id that is not a plain file name.
pub fn api_key_file(home: &Path, agent_id: &str, method_id: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(plain_id(agent_id) && plain_id(method_id), "the agent or its login method has an id sill cannot keep a key under");
    Ok(api_key_dir(home, agent_id).join(method_id))
}

/// Keep `key` for the method, readable by the person only; the file it is in.
pub fn save_api_key(home: &Path, agent_id: &str, method_id: &str, key: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let file = api_key_file(home, agent_id, method_id)?;
    let dir = file.parent().expect("a key file has a folder");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    for d in [dir, dir.parent().expect("keys/")] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(".{method_id}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(key.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &file)?;
    Ok(file)
}

/// The agent's saved API keys, if any.
pub fn api_key_files(home: &Path, agent_id: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(api_key_dir(home, agent_id))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect()
}

/// Forget the agent's saved API keys; how many went.
pub fn remove_api_keys(home: &Path, agent_id: &str) -> anyhow::Result<usize> {
    let files = api_key_files(home, agent_id);
    for f in &files {
        std::fs::remove_file(f)?;
    }
    let _ = std::fs::remove_dir(api_key_dir(home, agent_id));
    Ok(files.len())
}

/// Files at a path: the file itself, or those below a directory, recursively.
pub fn count_files(dir: &Path) -> usize {
    if dir.is_file() {
        return 1;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            Ok(_) => 1,
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod login_tests {
    use super::*;

    #[test]
    fn declarations_reject_legacy_keys_unknown_fields_and_ambiguous_paths() {
        let base = "id = 'fixture'\nname = 'Fixture'\ncommand = ['true']\n";
        for extra in ["state_dirs = []", "shared_files = []", "seed_files = []", "home_paths = ['~/.fixture']", "home_paths = [{path = '~/.fixture', kind = 'directory', typo = true}]"] {
            assert!(toml::from_str::<Agent>(&format!("{base}{extra}")).is_err(), "{extra}");
        }
        let mut a = builtin().remove(1);
        a.login_files.clear();
        a.home_env = None;
        for path in ["~", "~/", "~/.config", "~/.local/share", "/tmp/example", "~/.codex/../.ssh", "~/.config/*", "~/.codex//nested", "~/.codex/."] {
            a.home_paths = Some(vec![HomePath::directory(path)]);
            assert!(a.validate().is_err(), "{path}");
        }
        a.home_paths = Some(vec![HomePath::directory("~/.config/example"), HomePath::directory("~/.config/example/child")]);
        assert!(a.validate().is_err());
        a.home_paths = Some(vec![HomePath::file("~/.example.json", "{}")]);
        a.home_env = Some("EXAMPLE_HOME".into());
        assert!(a.validate().is_err());
    }

    #[test]
    fn an_invalid_override_is_an_error_and_never_falls_back_to_builtin() {
        let home = std::env::temp_dir().join(format!("sill-invalid-agent-{:032x}", rand::random::<u128>()));
        let path = override_path(&home, "codex");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "id = 'codex'\nname = 'Codex'\ncommand = ['custom']\nhome_paths = ['~/.codex']").unwrap();
        let error = load(&home).unwrap_err().to_string();
        assert!(error.contains("invalid agent definition") && error.contains("codex.toml"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_curated_agents_share_no_login_with_the_host() {
        for a in builtin() {
            a.validate().unwrap();
            assert!(a.home_paths.is_some());
            assert!(!a.login_files.is_empty());
        }
    }

    #[test]
    fn a_reset_removes_the_whole_home_and_links_never_what_they_point_to() {
        let dir = std::env::temp_dir().join(format!("sill-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (own, ghost, host) = (dir.join("own"), dir.join("ghost"), dir.join("host"));
        for d in [own.join(".claude/projects"), ghost.join(".claude"), host.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(own.join(".claude/.credentials.json"), "sill's login").unwrap();
        std::fs::write(own.join(".claude/projects/history.jsonl"), "history").unwrap();
        // a ghost's copy that is a link to the person's own login on the host
        std::fs::write(host.join(".credentials.json"), "the host's login").unwrap();
        std::os::unix::fs::symlink(host.join(".credentials.json"), ghost.join(".claude/.credentials.json")).unwrap();
        assert!(reset_homes(&[own.clone(), ghost.clone(), dir.join("missing")]).unwrap() >= 2);
        assert!(!own.exists() && !ghost.exists());
        assert_eq!(std::fs::read_to_string(host.join(".credentials.json")).unwrap(), "the host's login");
        assert_eq!(reset_homes(&[own, ghost]).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
