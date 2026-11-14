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
    /// `home_env` names the directory *containing* `home_paths[0]` rather than that path itself:
    /// `GEMINI_CLI_HOME` is a `$HOME` replacement (`$GEMINI_CLI_HOME/.gemini`), `CODEX_HOME` is the
    /// `~/.codex` directory.
    #[serde(default)]
    pub home_env_parent: bool,
    /// Symbolic icon name for the "+" menu and tabs (`sill-claude-symbolic`, any theme icon). Unknown
    /// or missing: a neutral placeholder.
    #[serde(default)]
    pub icon: Option<String>,
    /// Where the agent keeps its own memory (what it learned about the person and their projects),
    /// as patterns under `~` that may glob (`~/.claude/projects/*/memory`), directories or files;
    /// resolved inside the sill-owned home. The agents page offers to forget them in one click (sill's own memory,
    /// docs/memory.md, remembers the sessions).
    #[serde(default)]
    pub memory_paths: Vec<String>,
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
            // sill's Claude logs in on its own (terminal login inside the jail, written to the sill-owned
            // home): a separate grant from the host's Claude Code / Zed, so neither can revoke the other
            shared_files: vec![],
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
            domains: vec!["api.openai.com".into(), "*.openai.com".into(), "chatgpt.com".into(), "*.chatgpt.com".into()],
            sandbox_off_env: Default::default(),
            state_dirs: vec!["~/.codex".into()],
            home_paths: vec!["~/.codex".into()],
            seed_files: vec![],
            shared_files: vec!["~/.codex/auth.json".into()],
            host_login: vec!["chat-gpt".into(), "chatgpt".into()],
            home_env: Some("CODEX_HOME".into()),
            home_env_parent: false,
            icon: Some("sill-openai-symbolic".into()),
            memory_paths: vec!["~/.codex/memories".into()],
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
                && let Ok(mut a) = toml::from_str::<Agent>(&text)
            {
                // an override written before a field existed keeps the built-in's value for it
                if let Some(b) = out.get(&a.id) {
                    if a.icon.is_none() {
                        a.icon = b.icon.clone();
                    }
                    if a.host_login.is_empty() {
                        a.host_login = b.host_login.clone();
                    }
                    if a.home_env.is_none() {
                        a.home_env = b.home_env.clone();
                        a.home_env_parent = b.home_env_parent;
                    }
                    if a.memory_paths.is_empty() {
                        a.memory_paths = b.memory_paths.clone();
                    }
                }
                out.insert(a.id.clone(), a);
            }
        }
    }
    out
}

/// The agent's memory paths (directories or files) that exist in its sill-owned home (`memory_paths`,
/// each `~/<dir>/…` mapped to `<agents_base>/<dir>/…` the way [`crate::session::binds_for`] lays the
/// home out).
pub fn memory_dirs_in(agent: &Agent, agents_base: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for p in &agent.memory_paths {
        let Some(rel) = p.strip_prefix("~/") else { continue };
        let pattern = agents_base.join(rel);
        let s = pattern.to_string_lossy();
        if s.contains(['*', '?', '[']) {
            if let Ok(paths) = glob::glob(&s) {
                out.extend(paths.filter_map(Result::ok));
            }
        } else if pattern.exists() {
            out.push(pattern);
        }
    }
    out.sort();
    out
}

/// Same, for the agent's home under `~`.
pub fn memory_dirs(agent: &Agent, home: &Path) -> Vec<std::path::PathBuf> {
    memory_dirs_in(agent, &crate::session::agent_home(home, &agent.id))
}

/// Delete the agent's memory paths; how many files went.
pub fn forget_memory(agent: &Agent, home: &Path) -> anyhow::Result<usize> {
    let mut n = 0;
    for p in memory_dirs(agent, home) {
        n += count_files(&p);
        if p.is_dir() { std::fs::remove_dir_all(&p)? } else { std::fs::remove_file(&p)? }
    }
    Ok(n)
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
mod tests {
    use super::*;

    /// Claude's memory lives per project; the glob finds each, a plain path itself, nothing absent.
    #[test]
    fn memory_dirs_follow_the_patterns() {
        let base = std::env::temp_dir().join(format!("sill-agents-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let claude = builtin().into_iter().find(|a| a.id == "claude").unwrap();
        assert!(memory_dirs_in(&claude, &base).is_empty());
        for proj in ["-home-x", "-home-y"] {
            let m = base.join(".claude/projects").join(proj).join("memory");
            std::fs::create_dir_all(&m).unwrap();
            std::fs::write(m.join("MEMORY.md"), "").unwrap();
        }
        std::fs::create_dir_all(base.join(".claude/projects/-home-z")).unwrap(); // no memory yet
        let dirs = memory_dirs_in(&claude, &base);
        assert_eq!(dirs, vec![base.join(".claude/projects/-home-x/memory"), base.join(".claude/projects/-home-y/memory")]);
        assert_eq!(dirs.iter().map(|d| count_files(d)).sum::<usize>(), 2);
        let codex = builtin().into_iter().find(|a| a.id == "codex").unwrap();
        std::fs::create_dir_all(base.join(".codex/memories/a")).unwrap();
        assert_eq!(memory_dirs_in(&codex, &base), vec![base.join(".codex/memories")]);
        let gemini = builtin().into_iter().find(|a| a.id == "gemini").unwrap();
        assert!(memory_dirs_in(&gemini, &base).is_empty());
        std::fs::create_dir_all(base.join(".gemini")).unwrap();
        std::fs::write(base.join(".gemini/GEMINI.md"), "- likes tea\n").unwrap();
        let g = memory_dirs_in(&gemini, &base);
        assert_eq!(g, vec![base.join(".gemini/GEMINI.md")]);
        assert_eq!(count_files(&g[0]), 1);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
