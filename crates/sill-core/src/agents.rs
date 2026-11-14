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
}

pub fn builtin() -> Vec<Agent> {
    vec![
        Agent {
            id: "claude".into(),
            name: "Claude Code".into(),
            command: vec!["claude-agent-acp".into()],
            env: Default::default(),
            supports_load: true,
            domains: vec!["api.anthropic.com".into(), "*.anthropic.com".into(), "claude.ai".into(), "*.claude.ai".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.claude".into(), "~/.claude.json".into()],
            home_paths: vec!["~/.claude".into(), "~/.claude.json".into()],
            seed_files: vec!["~/.claude.json".into()],
            shared_files: vec!["~/.claude/.credentials.json".into()],
        },
        Agent {
            id: "codex".into(),
            name: "Codex".into(),
            command: vec!["codex-acp".into()],
            env: Default::default(),
            supports_load: false,
            domains: vec!["api.openai.com".into(), "*.openai.com".into(), "chatgpt.com".into(), "*.chatgpt.com".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.codex".into()],
            home_paths: vec!["~/.codex".into()],
            seed_files: vec![],
            shared_files: vec!["~/.codex/auth.json".into()],
        },
        Agent {
            id: "gemini".into(),
            name: "Gemini CLI".into(),
            command: vec!["gemini".into(), "--experimental-acp".into()],
            env: Default::default(),
            supports_load: false,
            domains: vec!["*.googleapis.com".into(), "*.google.com".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.gemini".into()],
            home_paths: vec!["~/.gemini".into()],
            seed_files: vec![],
            shared_files: vec!["~/.gemini/oauth_creds.json".into()],
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
