//! Session policies (SPEC §4.2, §4.8, §4.9). A policy is a TOML file; `default` is compiled in and
//! shipped as `/etc/sill/policies/default.toml`. Lookup order for a name: `~/.config/sill/policies/`,
//! `/etc/sill/policies/`, built-in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const BUILTIN_DEFAULT: &str = include_str!("../../../policies/default.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub jail: JailOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// `none` | `all` | `allow` (only the domains in `allow`)
    #[serde(default)]
    pub mode: NetMode,
    /// Domain patterns (`api.anthropic.com`, `*.githubusercontent.com`). Host-local addresses
    /// (loopback, link-local, RFC 1918) are refused in every mode unless listed in `allow_local`.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub allow_local: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    #[default]
    Allow,
    None,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    /// Written straight to the host, unrecorded. Each entry needs a reason (SPEC §4.2).
    #[serde(default)]
    pub direct: Vec<String>,
    /// Hidden behind an empty tmpfs (dirs) or /dev/null (files), in the jail and in the host view.
    #[serde(default)]
    pub mask: Vec<String>,
    /// Visible, never writable.
    #[serde(default)]
    pub ro: Vec<String>,
    /// Session-private scratch (caches): a directory on the state filesystem bound at this path,
    /// persistent across turns, deleted with the session, never written to the host.
    #[serde(default)]
    pub private: Vec<String>,
    /// Host mount points left out of the jail entirely (they appear as empty directories).
    #[serde(default)]
    pub hidden_mounts: Vec<String>,
    /// Overlaid roots whose changes are kept automatically at quiescent moments (SPEC §4.4). Pinned
    /// folders don't need this: they are bound live (host = jail), see `session::binds_for`.
    #[serde(default)]
    pub auto_keep: Vec<String>,
    /// Relative patterns the ledger records but neither hashes per turn nor auto-keeps.
    #[serde(default)]
    pub ledger_ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpu_weight: u32,
    pub io_weight: u32,
    pub memory_high: String,
    pub memory_max: String,
    pub tasks_max: u32,
    pub idle_timeout_minutes: u32,
    /// Stop the agent process (not the jail) after this long without traffic and without a connected
    /// shelf; 0 = never.
    #[serde(default = "default_agent_idle")]
    pub agent_idle_minutes: u32,
}

fn default_agent_idle() -> u32 {
    10
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_weight: 20,
            io_weight: 20,
            memory_high: "8G".into(),
            memory_max: "12G".into(),
            tasks_max: 512,
            idle_timeout_minutes: 120,
            agent_idle_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct JailOptions {
    /// Let processes in the jail create user namespaces (an agent's own sandbox). Off by default:
    /// nested namespaces + overlayfs is the largest kernel attack surface here (SPEC §4.10 #10).
    #[serde(default)]
    pub allow_userns: bool,
    /// Give the jail the X11 display (socket + Xauthority). Off by default: X11 lets any client read
    /// every keystroke and drive every window (SPEC §4.10 #3). On = the agent can run and see GUI apps.
    #[serde(default)]
    pub x11: bool,
    /// Give the jail the D-Bus session bus. Off by default: the session bus is a full escape (systemd
    /// user units, keyring secrets). Prefer an `xdg-dbus-proxy` allow-list when that exists.
    #[serde(default)]
    pub dbus: bool,
    /// Extra environment for every process in the jail.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self> {
        let p: Policy = toml::from_str(text).context("parse policy")?;
        if p.name.is_empty() {
            bail!("policy has no name");
        }
        Ok(p)
    }

    pub fn builtin_default() -> Self {
        Self::parse(BUILTIN_DEFAULT).expect("built-in default policy must parse")
    }

    /// Resolve a policy by name (search paths) or by path (contains `/` or ends in `.toml`).
    pub fn load(name_or_path: &str, home: &Path) -> Result<Self> {
        if name_or_path.contains('/') || name_or_path.ends_with(".toml") {
            let text = std::fs::read_to_string(name_or_path)
                .with_context(|| format!("read policy {name_or_path}"))?;
            return Self::parse(&text);
        }
        for dir in search_dirs(home) {
            let path = dir.join(format!("{name_or_path}.toml"));
            if path.is_file() {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read policy {}", path.display()))?;
                return Self::parse(&text);
            }
        }
        if name_or_path == "default" {
            return Ok(Self::builtin_default());
        }
        bail!("no policy named {name_or_path} (looked in ~/.config/sill/policies, /etc/sill/policies)")
    }
}

pub fn search_dirs(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".config/sill/policies"), PathBuf::from("/etc/sill/policies")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_default_parses() {
        let p = Policy::builtin_default();
        assert_eq!(p.name, "default");
        assert_eq!(p.network.mode, NetMode::Allow);
        assert!(p.paths.mask.iter().any(|m| m.contains(".ssh")));
        assert!(!p.jail.allow_userns);
    }
}
