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
    /// Where the agent keeps its own memory, as patterns under `~` (`~/.claude/projects/*/memory`).
    /// Unused since the Agents page's Reset replaced "forget memory" (2026-11-03); kept so
    /// definitions that name it still load (unknown fields are refused).
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
            // no host login is shared into jails: Codex in sill logs in on its own (the host-side
            // login lands in the sill-owned home through `CODEX_HOME`)
            shared_files: vec![],
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
            // its login lands in the sill-owned home (the host-side login relocates GEMINI_CLI_HOME)
            shared_files: vec![],
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
    fn the_curated_agents_share_no_login_with_the_host() {
        for a in builtin() {
            assert!(a.shared_files.is_empty(), "{} shares a host file", a.id);
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
