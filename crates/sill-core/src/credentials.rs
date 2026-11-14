//! The sole state shared with ghosts: declared credential files, never whole agent homes.
//! Each jail keeps real private directories so adapters may atomically replace their tokens.
//! The daemon reconciles changed credentials; it never imports settings or executable content.
use crate::{agents::Agent, safe_fs};
use anyhow::{Result, bail};
use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
const LIMIT: u64 = 1024 * 1024;

/// Paths are relative to the sill-owned home, not to the person's real home.
/// Custom agents may declare their own `login_files`; known credential names written by a
/// host login are discovered without assuming that all its output is authentication state.
pub fn files(agent: &Agent, base: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: BTreeSet<PathBuf> = agent.login_files.iter().map(PathBuf::from).collect();
    let known: &[&str] = match agent.id.as_str() {
        "claude" => &[".claude/.credentials.json"],
        "codex" => &[".codex/auth.json"],
        "gemini" => &[".gemini/oauth_creds.json", ".gemini/google_accounts.json"],
        "opencode" | "opencode-v2" => &[".local/share/opencode/auth.json"],
        "vibe" => &[".vibe/.env"],
        _ => &[],
    };
    paths.extend(known.iter().map(PathBuf::from));
    fn discover(
        base: &Path,
        rel: &Path,
        depth: usize,
        budget: &mut usize,
        out: &mut BTreeSet<PathBuf>,
    ) {
        use std::os::fd::AsRawFd;
        if depth == 0 || *budget == 0 {
            return;
        }
        let Ok(dir) = safe_fs::open_dir_beneath(base, rel, false) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())) else {
            return;
        };
        for e in entries.flatten() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let path = rel.join(e.file_name());
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir()
                && !matches!(
                    e.file_name().to_str(),
                    Some(
                        "projects"
                            | "sessions"
                            | "plugins"
                            | "skills"
                            | "logs"
                            | "log"
                            | "memories"
                            | "cache"
                            | ".cache"
                            | "tmp"
                    )
                )
            {
                discover(base, &path, depth - 1, budget, out);
            } else if kind.is_file()
                && matches!(
                    e.file_name().to_str(),
                    Some(
                        "auth.json"
                            | ".credentials.json"
                            | "oauth_creds.json"
                            | "google_accounts.json"
                    )
                )
            {
                out.insert(path);
            }
        }
    }
    discover(base, Path::new(""), 5, &mut 512, &mut paths);
    for p in &paths {
        if p.components().any(|c| !matches!(c, Component::Normal(_))) || p.components().count() < 2
        {
            bail!("login_files must name files below a private directory");
        }
    }
    Ok(paths.into_iter().collect())
}

pub fn seed(agent: &Agent, base: &Path, ghost: &Path) -> Result<()> {
    for rel in files(agent, base)? {
        if let Some(bytes) = safe_fs::read_file_beneath(base, &rel, LIMIT)? {
            safe_fs::replace_private_beneath(ghost, &rel, &bytes)?;
        }
    }
    auth_selection(agent, ghost)?;
    Ok(())
}

// Reconstruct only login-selection metadata; never copy the settings that may name host commands.
fn auth_selection(agent: &Agent, root: &Path) -> Result<()> {
    let (credential, config, pointer, value) = match agent.id.as_str() {
        "gemini" => (
            ".gemini/oauth_creds.json",
            ".gemini/settings.json",
            "/security/auth/selectedType",
            serde_json::json!("oauth-personal"),
        ),
        "claude" => (
            ".claude/.credentials.json",
            ".claude.json",
            "/hasCompletedOnboarding",
            serde_json::json!(true),
        ),
        _ => return Ok(()),
    };
    if safe_fs::read_file_beneath(root, Path::new(credential), LIMIT)?.is_none() {
        return Ok(());
    }
    let current = safe_fs::read_file_beneath(root, Path::new(config), LIMIT)?;
    let mut config_value: serde_json::Value = match current {
        Some(bytes) => serde_json::from_slice(&bytes)?,
        None => serde_json::json!({}),
    };
    if !config_value.is_object() {
        bail!("login settings must be a JSON object");
    }
    if config_value.pointer(pointer).is_some() {
        return Ok(());
    }
    if agent.id == "gemini" {
        for pointer in ["/security", "/security/auth"] {
            if config_value
                .pointer(pointer)
                .is_some_and(|v| !v.is_object() && !v.is_null())
            {
                bail!("login settings contain a non-object at {pointer}");
            }
        }
    }
    if agent.id == "gemini" {
        config_value["security"]["auth"]["selectedType"] = value;
    } else {
        config_value["hasCompletedOnboarding"] = value;
    }
    safe_fs::replace_private_beneath(
        root,
        Path::new(config),
        &serde_json::to_vec_pretty(&config_value)?,
    )
}

fn fingerprint(bytes: &Option<Vec<u8>>) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    bytes.as_ref().map(|b| Sha256::digest(b).into())
}

#[derive(Default)]
pub struct Sync {
    // Fingerprints only: keep no long-lived copies of tokens in daemon memory.
    seen: HashMap<PathBuf, Option<[u8; 32]>>,
}
impl Sync {
    /// A changed existing member wins over an unchanged member. New empty ghosts never erase
    /// the login. Simultaneous independent updates use mtime, then canonical-home order.
    pub fn reconcile(&mut self, agent: &Agent, base: &Path, ghosts: &[PathBuf]) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(base)?;
        let roots: Vec<&Path> = std::iter::once(base)
            .chain(ghosts.iter().map(PathBuf::as_path))
            .filter(|p| p.is_dir())
            .collect();
        let mut paths: BTreeSet<PathBuf> = files(agent, base)?.into_iter().collect();
        for root in &roots {
            paths.extend(files(agent, root)?);
        }
        for rel in paths {
            let mut values = Vec::new();
            for root in &roots {
                let key = root.join(&rel);
                let value = safe_fs::read_file_beneath(root, &rel, LIMIT)?;
                let fingerprint = fingerprint(&value);
                if rel.extension().is_some_and(|e| e == "json")
                    && let Some(bytes) = &value
                {
                    // An in-place writer may still be halfway through its save. Retry next poll
                    // rather than publish an empty/truncated token to every other session.
                    if serde_json::from_slice::<serde_json::Value>(bytes).is_err() {
                        bail!("credential file is not complete JSON: {}", rel.display());
                    }
                }
                let previous = self.seen.get(&key);
                let changed = previous.is_some_and(|old| old != &fingerprint);
                let time = std::fs::symlink_metadata(&key)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                values.push((key, value, changed, time));
            }
            let bootstrapping = !values.iter().any(|v| self.seen.contains_key(&v.0));
            let winner = values
                .iter()
                .enumerate()
                .filter(|(_, v)| v.2)
                .max_by_key(|(i, v)| (v.3, std::cmp::Reverse(*i)))
                .or_else(|| {
                    if bootstrapping {
                        values
                            .iter()
                            .enumerate()
                            .filter(|(_, v)| v.1.is_some())
                            .max_by_key(|(i, v)| (v.3, std::cmp::Reverse(*i)))
                    } else {
                        values.iter().enumerate().find(|(_, v)| v.1.is_some())
                    }
                });
            let Some((_, winner)) = winner else { continue };
            let wanted = winner.1.clone();
            for (root, (key, value, _, _)) in roots.iter().zip(values) {
                if value != wanted {
                    if let Some(bytes) = &wanted {
                        safe_fs::replace_private_beneath(root, &rel, bytes)?;
                    } else if value.is_some() {
                        safe_fs::remove_file_beneath(root, &rel)?;
                    }
                }
                self.seen.insert(key, fingerprint(&wanted));
            }
        }
        for root in &roots {
            auth_selection(agent, root)?;
        }
        // Do not keep tokens belonging to deleted ghost homes in the daemon's memory.
        self.seen.retain(|p, _| p.exists());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        dir: PathBuf,
        agent: Agent,
        base: PathBuf,
        ghosts: Vec<PathBuf>,
    }
    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sill-credentials-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let base = dir.join("base");
            let ghosts = vec![dir.join("ghost-one"), dir.join("ghost-two")];
            for root in std::iter::once(&base).chain(&ghosts) {
                std::fs::create_dir_all(root).unwrap();
            }
            let agent = crate::agents::builtin()
                .into_iter()
                .find(|a| a.id == "codex")
                .unwrap();
            Self {
                dir,
                agent,
                base,
                ghosts,
            }
        }
        fn put(&self, root: &Path, path: &str, bytes: &[u8]) {
            let bytes = if path.ends_with("auth.json") {
                serde_json::to_vec(
                    &serde_json::json!({"credential": String::from_utf8_lossy(bytes)}),
                )
                .unwrap()
            } else {
                bytes.to_vec()
            };
            safe_fs::replace_private_beneath(root, Path::new(path), &bytes).unwrap();
        }
        fn read(&self, root: &Path, path: &str) -> Option<Vec<u8>> {
            safe_fs::read_file_beneath(root, Path::new(path), LIMIT)
                .unwrap()
                .map(|bytes| {
                    if path.ends_with("auth.json") {
                        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["credential"]
                            .as_str()
                            .unwrap()
                            .as_bytes()
                            .to_vec()
                    } else {
                        bytes
                    }
                })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn ghost_seed_contains_credentials_only_and_no_links_or_settings() {
        let f = Fixture::new("seed");
        for path in [
            ".codex/auth.json",
            ".codex/config.toml",
            ".codex/sessions/old.json",
            ".codex/skills/x",
            ".claude.json",
        ] {
            f.put(&f.base, path, b"private");
        }
        seed(&f.agent, &f.base, &f.ghosts[0]).unwrap();
        assert_eq!(
            f.read(&f.ghosts[0], ".codex/auth.json"),
            Some(b"private".to_vec())
        );
        for path in [
            ".codex/config.toml",
            ".codex/sessions/old.json",
            ".codex/skills/x",
            ".claude.json",
        ] {
            assert_eq!(
                f.read(&f.ghosts[0], path),
                None,
                "{path} escaped the boundary"
            );
        }
        std::fs::remove_file(f.base.join(".codex/auth.json")).unwrap();
        std::os::unix::fs::symlink(f.dir.join("outside"), f.base.join(".codex/auth.json")).unwrap();
        assert!(seed(&f.agent, &f.base, &f.ghosts[1]).is_err());
    }

    #[test]
    fn terminal_login_and_atomic_refresh_reach_normal_home_and_other_ghosts() {
        let f = Fixture::new("refresh");
        let mut sync = Sync::default();
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        f.put(&f.ghosts[0], ".codex/auth.json", b"first login");
        f.put(&f.ghosts[0], ".codex/config.toml", b"ghost-only");
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        for root in [&f.base, &f.ghosts[1]] {
            assert_eq!(
                f.read(root, ".codex/auth.json"),
                Some(b"first login".to_vec())
            );
            assert_eq!(f.read(root, ".codex/config.toml"), None);
        }
        // Replacing the inode is how token refresh libraries commonly save their credentials.
        f.put(&f.ghosts[1], ".codex/auth.json", b"refreshed");
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        for root in [&f.base, &f.ghosts[0]] {
            assert_eq!(
                f.read(root, ".codex/auth.json"),
                Some(b"refreshed".to_vec())
            );
        }
        std::fs::remove_file(f.ghosts[0].join(".codex/auth.json")).unwrap();
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        assert_eq!(
            f.read(&f.base, ".codex/auth.json"),
            None,
            "logout must propagate too"
        );
        assert_eq!(f.read(&f.ghosts[1], ".codex/auth.json"), None);
    }

    #[test]
    fn registry_credentials_are_discovered_but_symlink_destinations_are_refused() {
        let mut f = Fixture::new("registry");
        f.agent.id = "custom-agent".into();
        f.agent.home_paths.clear();
        f.put(&f.base, ".local/share/custom/auth.json", b"login");
        f.put(&f.base, ".local/share/custom/settings.json", b"hooks");
        let mut sync = Sync::default();
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        assert_eq!(
            f.read(&f.ghosts[0], ".local/share/custom/auth.json"),
            Some(b"login".to_vec())
        );
        assert_eq!(
            f.read(&f.ghosts[0], ".local/share/custom/settings.json"),
            None
        );
        let outside = f.dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(f.ghosts[1].join(".local")).unwrap();
        std::os::unix::fs::symlink(&outside, f.ghosts[1].join(".local")).unwrap();
        assert!(sync.reconcile(&f.agent, &f.base, &f.ghosts).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn credential_paths_cannot_escape_the_owned_home() {
        let mut f = Fixture::new("paths");
        for path in ["../secret", "/tmp/secret", ".codex/../secret", ".codex"] {
            f.agent.login_files = vec![path.into()];
            assert!(files(&f.agent, &f.base).is_err(), "{path}");
        }
    }
    #[test]
    fn malformed_in_place_save_never_overwrites_a_good_token() {
        let f = Fixture::new("partial");
        let mut sync = Sync::default();
        f.put(&f.base, ".codex/auth.json", b"good");
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        safe_fs::write_file_beneath(&f.ghosts[0], Path::new(".codex/auth.json"), b"{", 0o600)
            .unwrap();
        assert!(sync.reconcile(&f.agent, &f.base, &f.ghosts).is_err());
        assert_eq!(f.read(&f.base, ".codex/auth.json"), Some(b"good".to_vec()));
        f.put(&f.ghosts[0], ".codex/auth.json", b"completed");
        sync.reconcile(&f.agent, &f.base, &f.ghosts).unwrap();
        assert_eq!(
            f.read(&f.base, ".codex/auth.json"),
            Some(b"completed".to_vec())
        );
    }

    #[test]
    fn daemon_restart_recovers_a_refresh_from_the_newest_private_file() {
        let f = Fixture::new("restart");
        f.put(&f.base, ".codex/auth.json", b"old");
        Sync::default()
            .reconcile(&f.agent, &f.base, &f.ghosts)
            .unwrap();
        f.put(&f.ghosts[0], ".codex/auth.json", b"refreshed");
        let file = std::fs::File::open(f.ghosts[0].join(".codex/auth.json")).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)),
        )
        .unwrap();
        Sync::default()
            .reconcile(&f.agent, &f.base, &f.ghosts)
            .unwrap();
        assert_eq!(
            f.read(&f.base, ".codex/auth.json"),
            Some(b"refreshed".to_vec())
        );
    }

    #[test]
    fn gemini_auth_selection_does_not_import_mcp_settings() {
        let mut f = Fixture::new("selection");
        f.agent = crate::agents::builtin()
            .into_iter()
            .find(|a| a.id == "gemini")
            .unwrap();
        f.put(
            &f.base,
            ".gemini/oauth_creds.json",
            br#"{"refresh_token":"test"}"#,
        );
        f.put(
            &f.base,
            ".gemini/settings.json",
            br#"{"mcpServers":{"host-only":{"command":"bad"}}}"#,
        );
        seed(&f.agent, &f.base, &f.ghosts[0]).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&f.read(&f.ghosts[0], ".gemini/settings.json").unwrap())
                .unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"security":{"auth":{"selectedType":"oauth-personal"}}})
        );
        Sync::default()
            .reconcile(&f.agent, &f.base, &f.ghosts)
            .unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&f.read(&f.base, ".gemini/settings.json").unwrap()).unwrap();
        assert_eq!(settings["mcpServers"]["host-only"]["command"], "bad");
    }
}
