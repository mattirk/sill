//! Agent registry (SPEC §4.1): how to start each ACP agent inside the jail. Built-in entries plus
//! TOML files in `~/.config/sill/agents/` and `/usr/share/sill/agents/` (user files win).

use std::collections::BTreeMap;
use std::path::Path;

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
    /// Domains the agent needs beyond the policy's allow-list (merged into the proxy rules).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Environment that switches the agent's own sandbox off inside the jail (SPEC §4.2).
    #[serde(default)]
    pub sandbox_off_env: BTreeMap<String, String>,
    /// Where the agent keeps its own files (`~/.claude`); changes there are "agent state" in the UI.
    #[serde(default)]
    pub state_dirs: Vec<String>,
    /// Paths replaced inside the jail by a sill-owned copy (`~/.local/state/sill/agents/<id>/…`), so
    /// the agent runs completely inside: its credentials, transcripts, settings and caches never touch
    /// the host's own installation of the agent (SPEC §4.10 #4).
    #[serde(default)]
    pub home_paths: Vec<String>,
    /// Files copied from the host into the sill-owned home when it is first created (login only).
    #[serde(default)]
    pub seed_files: Vec<String>,
    /// Files bound from the host *over* the sill-owned home: the login itself. OAuth refresh tokens
    /// rotate, so two copies of the credentials revoke each other; one shared file is the only
    /// workable option (and the only thing the agent writes to the host).
    #[serde(default)]
    pub shared_files: Vec<String>,
    /// Login methods (ids from `initialize`) that run on the host instead of inside the jail: a
    /// browser flow with a localhost callback cannot be reached from the jail's own network
    /// namespace, so sill runs the agent on the host for the login only and the result lands in the
    /// `shared_files` login file (see [`crate::login`]).
    #[serde(default)]
    pub host_login: Vec<String>,
    /// Environment variable that relocates the agent's home directory (`CODEX_HOME`). A host-side
    /// login is run with it pointing at the sill-owned copy of `home_paths[0]`, so the login file
    /// lands there directly and the host never gets a `~/.codex` of its own.
    #[serde(default)]
    pub home_env: Option<String>,
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
    let path = override_path(home, &agent.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(agent)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
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
            domains: vec!["api.anthropic.com".into(), "*.anthropic.com".into(), "claude.ai".into(), "*.claude.ai".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.claude".into(), "~/.claude.json".into()],
            home_paths: vec!["~/.claude".into(), "~/.claude.json".into()],
            seed_files: vec!["~/.claude.json".into()],
            shared_files: vec!["~/.claude/.credentials.json".into()],
            host_login: vec![],
            home_env: None,
        },
        Agent {
            id: "codex".into(),
            name: "Codex".into(),
            enabled: true,
            command: vec!["codex-acp".into()],
            env: Default::default(),
            supports_load: true,
            domains: vec!["api.openai.com".into(), "*.openai.com".into(), "chatgpt.com".into(), "*.chatgpt.com".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.codex".into()],
            home_paths: vec!["~/.codex".into()],
            seed_files: vec![],
            shared_files: vec!["~/.codex/auth.json".into()],
            host_login: vec!["chat-gpt".into(), "chatgpt".into()],
            home_env: Some("CODEX_HOME".into()),
        },
        Agent {
            id: "gemini".into(),
            name: "Gemini CLI".into(),
            enabled: true,
            command: vec!["gemini".into(), "--experimental-acp".into()],
            env: Default::default(),
            supports_load: false,
            domains: vec!["*.googleapis.com".into(), "*.google.com".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.gemini".into()],
            home_paths: vec!["~/.gemini".into()],
            seed_files: vec![],
            shared_files: vec!["~/.gemini/oauth_creds.json".into()],
            host_login: vec![],
            home_env: None,
        },
    ]
}

/// All agents, by id; files override built-ins with the same id.
pub fn load(home: &Path) -> BTreeMap<String, Agent> {
    let mut out: BTreeMap<String, Agent> = builtin().into_iter().map(|a| (a.id.clone(), a)).collect();
    for dir in [std::path::PathBuf::from("/usr/share/sill/agents"), home.join(".config/sill/agents")] {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(a) = toml::from_str::<Agent>(&text)
            {
                out.insert(a.id.clone(), a);
            }
        }
    }
    out
}
