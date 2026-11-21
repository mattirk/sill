//! Session policies (SPEC §4.2, §4.8, §4.9). A policy is a TOML file; `default` is compiled in and
//! shipped as `/etc/silli/policies/default.toml`. Lookup order for a name: `~/.config/silli/policies/`,
//! `/etc/silli/policies/`, built-in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const BUILTIN_DEFAULT: &str = include_str!("../../../policies/default.toml");

/// A ghost session ends itself after this long without a prompt (docs/memory.md §3.2), and its
/// writes revert this long after the last one.
pub const GHOST_IDLE_MINUTES: u32 = 60;

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
    #[serde(default)]
    pub processes: Processes,
    #[serde(default)]
    pub permissions: crate::permissions::Permissions,
}

/// Managed processes (docs/tasks.md §2.4, §2.5).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Processes {
    /// Ports a managed process may be exposed on the host's loopback without asking: `"3000"`,
    /// `"3000-3999"`. Anything else is asked once per session in the conversation.
    #[serde(default)]
    pub expose: Vec<String>,
}

impl Processes {
    pub fn allows(&self, port: u16) -> bool {
        self.expose.iter().any(|spec| match spec.split_once('-') {
            Some((a, b)) => matches!((a.trim().parse::<u16>(), b.trim().parse::<u16>()), (Ok(a), Ok(b)) if a <= port && port <= b),
            None => spec.trim().parse::<u16>().ok() == Some(port),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Hard ceiling: whether human approvals may add hosts beyond the policy defaults.
    #[serde(default = "default_true")]
    pub allow_additional_hosts: bool,
    /// `none` | `all` | `allow` (only the domains in `allow`)
    #[serde(default)]
    pub mode: NetMode,
    /// Domain patterns (`api.anthropic.com`, `*.githubusercontent.com`). A pattern grants one port:
    /// 443 for `CONNECT` and 80 for plain HTTP, unless it says `host:8443` or `host:*`. Host-local
    /// addresses (loopback, link-local, RFC 1918 and the other reserved blocks) are refused in every
    /// mode unless listed in `allow_local`, same pattern form.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub allow_local: Vec<String>,
    /// Refused hosts the frontend does not ask about: telemetry and crash reporting the agent would
    /// send anyway. Still refused, just quietly.
    #[serde(default = "quiet_default")]
    pub quiet: Vec<String>,
}

impl Default for Network {
    fn default() -> Self {
        Self { allow_additional_hosts: true, mode: Default::default(), allow: vec![], allow_local: vec![], quiet: quiet_default() }
    }
}

pub fn quiet_default() -> Vec<String> {
    [
        "*.datadoghq.com",
        "*.datadoghq.eu",
        "*.sentry.io",
        "sentry.io",
        "*.ingest.sentry.io",
        "*.statsig.com",
        "*.segment.io",
        "*.segment.com",
        "*.mixpanel.com",
        "*.posthog.com",
        "*.amplitude.com",
        "*.bugsnag.com",
        "*.launchdarkly.com",
        "*.google-analytics.com",
        "*.googletagmanager.com",
        "*.newrelic.com",
        "*.nr-data.net",
        "*.honeycomb.io",
        "*.rollbar.com",
        "*.intercom.io",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
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
    /// Relative patterns the ledger records but doesn't hash per turn.
    #[serde(default)]
    pub ledger_ignore: Vec<String>,
    /// Accepted and ignored: policies written before pinned folders became live (2026-10-28) have it.
    #[serde(default, skip_serializing)]
    pub auto_keep: Vec<String>,
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
    /// frontend; 0 = never.
    #[serde(default = "default_agent_idle")]
    pub agent_idle_minutes: u32,
    /// Refuse to start a session that would run without resource limits (`direct` launcher mode
    /// with no delegated cgroup subtree, SPEC §4.7). `false` starts it anyway, with a warning.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
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
            required: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JailOptions {
    /// Let processes in the jail create user namespaces (an agent's own sandbox). Off by default:
    /// nested namespaces + overlayfs is the largest kernel attack surface here (SPEC §4.10 #10).
    #[serde(default)]
    pub allow_userns: bool,
    /// Extra environment for every process in the jail.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// The seccomp deny-list (`silli_core::seccomp`) as bwrap's second fence (security.md S18). On by
    /// default; off only to find out whether it is what breaks a tool.
    #[serde(default = "default_true")]
    pub seccomp: bool,
}

impl Default for JailOptions {
    fn default() -> Self {
        Self { allow_userns: false, env: Default::default(), seccomp: true }
    }
}

impl Policy {
    pub fn parse(text: &str) -> Result<Self> {
        let mut p: Policy = toml::from_str(text).context("parse policy")?;
        if p.name.is_empty() {
            bail!("policy has no name");
        }
        p.permissions.validate(&p.network)?;
        p.permissions.apply_defaults(&mut p.network);
        Ok(p)
    }

    pub fn builtin_default() -> Self {
        Self::parse(BUILTIN_DEFAULT).expect("built-in default policy must parse")
    }

    /// What a ghost session makes of this policy (docs/memory.md §3.2): it only tightens. `direct`
    /// paths become `ro` (nothing unrecorded reaches the host), X11 and D-Bus are off, a network of
    /// `all` becomes the allow-list of the `default` policy (a ghost does not get a wider world than
    /// a normal session), and the idle timeout is at most an hour (0 = never becomes an hour).
    pub fn ghostly(&mut self, home: &Path) {
        use crate::permissions::{Permissions, Rule};
        self.permissions = Permissions {
            test_desktop: Rule::Forbidden,
            private_bus: Rule::Forbidden,
            gpu: Rule::Forbidden,
            public_web: Rule::Forbidden,
            host_control: Rule::Forbidden,
        };
        let direct = std::mem::take(&mut self.paths.direct);
        for d in direct {
            if !self.paths.ro.contains(&d) {
                self.paths.ro.push(d);
            }
        }
        if self.network.mode == NetMode::All {
            let default = Self::load("default", home).unwrap_or_else(|_| Self::builtin_default());
            self.network.mode = NetMode::Allow;
            self.network.allow = default.network.allow;
            self.network.allow_local = default.network.allow_local;
        }
        let idle = self.limits.idle_timeout_minutes;
        self.limits.idle_timeout_minutes = if idle == 0 { GHOST_IDLE_MINUTES } else { idle.min(GHOST_IDLE_MINUTES) };
    }

    /// Resolve a policy by name (search paths) or by path (contains `/` or ends in `.toml`).
    /// The file a policy name resolves to (None for the built-in default).
    pub fn resolve_path(name_or_path: &str, home: &Path) -> Option<PathBuf> {
        if name_or_path.contains('/') || name_or_path.ends_with(".toml") {
            return Some(PathBuf::from(name_or_path));
        }
        search_dirs(home).into_iter().map(|d| d.join(format!("{name_or_path}.toml"))).find(|p| p.is_file())
    }

    pub fn load(name_or_path: &str, home: &Path) -> Result<Self> {
        if name_or_path.contains('/') || name_or_path.ends_with(".toml") {
            let text = std::fs::read_to_string(name_or_path).with_context(|| format!("read policy {name_or_path}"))?;
            return Self::parse(&text);
        }
        for dir in search_dirs(home) {
            let path = dir.join(format!("{name_or_path}.toml"));
            if path.is_file() {
                let text = std::fs::read_to_string(&path).with_context(|| format!("read policy {}", path.display()))?;
                return Self::parse(&text);
            }
        }
        if name_or_path == "default" {
            return Ok(Self::builtin_default());
        }
        bail!("no policy named {name_or_path} (looked in ~/.config/silli/policies, /etc/silli/policies)")
    }
}

pub fn search_dirs(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".config/silli/policies"), PathBuf::from("/etc/silli/policies")]
}

/// Policy names there are to choose from: the built-in `default` and every `*.toml` in the search
/// directories (a user file shadows a system one of the same name, as in `load`).
pub fn available(home: &Path) -> Vec<String> {
    let mut names = vec!["default".to_string()];
    for dir in search_dirs(home) {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "toml")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && !names.iter().any(|n| n == stem)
            {
                names.push(stem.to_string());
            }
        }
    }
    names
}

/// A bounded live name page, independent of how many policy files exist on disk.
pub fn available_page(home: &Path, after: Option<&str>, limit: usize) -> (Vec<String>, bool) {
    let mut names = std::collections::BTreeSet::new();
    let mut add = |name: String| {
        if after.is_none_or(|a| name.as_str() > a) {
            names.insert(name);
            if names.len() > limit + 1 {
                names.pop_last();
            }
        }
    };
    add("default".into());
    for dir in search_dirs(home) {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "toml")
                && let Some(name) = p.file_stem().and_then(|s| s.to_str())
            {
                add(name.into());
            }
        }
    }
    let more = names.len() > limit;
    if more {
        names.pop_last();
    }
    (names.into_iter().collect(), more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_host_desktop_grants_are_rejected() {
        for field in ["x11", "dbus"] {
            for value in ["true", "false"] {
                let err = Policy::parse(&format!("name='retired'\n[jail]\n{field}={value}")).unwrap_err();
                assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
            }
        }
    }

    /// A ghost only tightens (docs/memory.md §3.2): direct becomes ro, no desktop, `all` becomes the
    /// default allow-list, the idle timeout at most an hour.
    #[test]
    fn ghostly_only_tightens() {
        let home = Path::new("/nonexistent-home");
        let mut p = Policy::builtin_default();
        p.paths.direct = vec!["~/shared".into()];
        p.network.mode = NetMode::All;
        p.network.allow.clear();
        p.limits.idle_timeout_minutes = 0;
        p.ghostly(home);
        assert!(p.paths.direct.is_empty());
        assert!(p.paths.ro.contains(&"~/shared".to_string()));
        assert_eq!(p.network.mode, NetMode::Allow);
        assert_eq!(p.network.allow, Policy::builtin_default().network.allow);
        assert_eq!(p.limits.idle_timeout_minutes, GHOST_IDLE_MINUTES);
        let mut q = Policy::builtin_default();
        q.limits.idle_timeout_minutes = 30;
        q.network.mode = NetMode::None;
        q.ghostly(home);
        assert_eq!(q.limits.idle_timeout_minutes, 30, "a shorter timeout stays");
        assert_eq!(q.network.mode, NetMode::None, "no network stays none");
        let mut r = Policy::builtin_default();
        r.limits.idle_timeout_minutes = 120;
        r.ghostly(home);
        assert_eq!(r.limits.idle_timeout_minutes, 60);
    }

    #[test]
    fn builtin_default_parses() {
        let p = Policy::builtin_default();
        assert_eq!(p.name, "default");
        assert_eq!(p.network.mode, NetMode::Allow);
        for m in ["~/.ssh", "~/.npmrc", "~/.pypirc", "~/.docker", "~/.kube", "~/.vault-token", "~/.password-store"] {
            assert!(p.paths.mask.iter().any(|x| x == m), "missing default mask {m}");
        }
        assert!(!p.jail.allow_userns);
    }
    #[test]
    fn additional_host_limit_is_distinct_from_initial_allow_list() {
        let policy = Policy::parse("name = 'restricted'\n[network]\nallow = ['example.com']\nallow_additional_hosts = false").unwrap();
        assert!(!policy.network.allow_additional_hosts);
        assert_eq!(policy.network.allow, ["example.com"]);
        assert!(Policy::builtin_default().network.allow_additional_hosts);
    }
}
