//! The ACP agent registry (SPEC §4.1): a **snapshot** of the protocol project's registry compiled
//! into sill (`registry/snapshot.json`, built from the registry's git repository at one commit by
//! `sill registry snapshot`, with a full npm lockfile per package — security.md S13), the **live**
//! index the project publishes for what is newer (cached under `~/.local/state/sill/registry/`,
//! masked in every jail), and installs into a sill-owned tools directory
//! (`~/.local/state/sill/tools/<id>/`) that never touches the person's own npm/uv/bin setup. A
//! snapshot entry installs exactly what the snapshot pinned (`npm ci` against its lockfile, `uv`
//! with `--no-build` and an `==` pin, a binary with its sha256); a live-only entry is installable
//! after the person confirms, with the index's exact version and no lockfile. An
//! installed entry becomes a registry override (`~/.config/sill/agents/<id>.toml`), so it shows up
//! in the "+" menu like the built-ins; entries that are the built-ins' own packages (Claude Code,
//! Codex, Gemini) keep the built-in's curated jail settings and only get their command replaced.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::agents::{self, Agent};

pub const URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
/// The registry's source: one `<id>/agent.json` per agent, what the published index is built from.
pub const REPO: &str = "agentclientprotocol/registry";

/// Where an entry's description of itself comes from, which decides how it is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// The live index: nothing but TLS to the CDN vouches for it.
    #[default]
    Live,
    /// The snapshot compiled into this build, reviewed with the commit that updated it.
    Snapshot,
}

/// The compiled-in snapshot (`registry/snapshot.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub source: String,
    pub commit: String,
    pub date: String,
    pub agents: Vec<Entry>,
}

const SNAPSHOT_JSON: &str = include_str!("../registry/snapshot.json");

pub fn snapshot() -> &'static Snapshot {
    static S: std::sync::OnceLock<Snapshot> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let mut s: Snapshot = serde_json::from_str(SNAPSHOT_JSON).expect("registry/snapshot.json");
        for a in &mut s.agents {
            a.trust = Trust::Snapshot;
        }
        s
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agents: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub distribution: Distribution,
    /// Snapshot only: the `package-lock.json` and `package.json` an `npx` entry is installed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_lock: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_manifest: Option<serde_json::Value>,
    #[serde(skip)]
    pub trust: Trust,
    /// A snapshot entry's newer version in the live index, when there is one (unverified).
    #[serde(skip)]
    pub newer: Option<Box<Entry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<PackageDist>,
    #[serde(default)]
    pub uvx: Option<PackageDist>,
    #[serde(default)]
    pub binary: Option<BTreeMap<String, BinaryDist>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDist {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryDist {
    pub archive: String,
    #[serde(default)]
    pub sha256: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// What sill installed for an entry (`tools/<id>/installed.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
    /// `npx` | `uvx` | `binary`
    pub kind: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Where the install came from: the snapshot, or the live index after a confirmation.
    #[serde(default)]
    pub trust: Trust,
}

impl Entry {
    /// How this entry would be installed here, in order of preference: a package through npm or uv
    /// (reproducible, no scripts), else a binary for this platform.
    pub fn how(&self) -> Option<&'static str> {
        if self.distribution.npx.is_some() {
            Some("npx")
        } else if self.distribution.uvx.is_some() {
            Some("uvx")
        } else if self.distribution.binary.as_ref().is_some_and(|b| b.contains_key(platform_key())) {
            Some("binary")
        } else {
            None
        }
    }

    /// The package name without its version pin (`@scope/name@1.2.3` → `@scope/name`).
    pub fn package_name(&self) -> Option<String> {
        let p = self.distribution.npx.as_ref().or(self.distribution.uvx.as_ref())?;
        Some(strip_version(&p.package))
    }

    /// The package spec as it will be installed (`@scope/name@1.2.3`, `name==1.2.3`, or the
    /// archive URL), for showing before a confirmation.
    pub fn install_spec(&self) -> String {
        match self.how() {
            Some("npx") => self.distribution.npx.as_ref().map(|d| d.package.clone()).unwrap_or_default(),
            Some("uvx") => self.distribution.uvx.as_ref().map(|d| d.package.clone()).unwrap_or_default(),
            Some(_) => self.distribution.binary.as_ref().and_then(|b| b.get(platform_key())).map(|b| b.archive.clone()).unwrap_or_default(),
            None => String::new(),
        }
    }
}

fn strip_version(spec: &str) -> String {
    match spec.rfind('@') {
        Some(i) if i > 0 => spec[..i].to_string(),
        _ => spec.to_string(),
    }
}

/// The exact version an npm spec pins (`@scope/name@1.2.3` → `1.2.3`); none for a bare name, a
/// range or a tag — what the registry's own schema requires, and what an install is held to.
fn npm_exact_version(spec: &str) -> Option<&str> {
    let i = spec.rfind('@').filter(|&i| i > 0)?;
    let v = &spec[i + 1..];
    let mut parts = v.split('.');
    let numeric = |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
    // `1.2.3` or `1.2.3-pre.1`: three numeric parts, the last may carry a prerelease
    let third = parts.nth(2).map(|t| t.split('-').next().unwrap_or(""));
    (numeric(Some(v.split('.').next().unwrap_or(""))) && numeric(v.split('.').nth(1)) && numeric(third) && !v.contains(['^', '~', '>', '<', '*', ' ', 'x', 'X'])).then_some(v)
}

/// The exact version a Python spec pins (`name==1.2.3` → `1.2.3`).
fn uv_exact_version(spec: &str) -> Option<&str> {
    let (_, v) = spec.split_once("==")?;
    let v = v.trim();
    (!v.is_empty() && !v.contains(['*', ',', ' ', '<', '>', '!'])).then_some(v)
}

fn platform_key() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux-aarch64",
        _ => "linux-x86_64",
    }
}

/// Registry ids known to work inside sill's jail end to end (install, login, a prompt). Everything
/// else is still installable but hidden by default on the Agents page until someone tests it.
pub const TESTED: &[&str] = &["claude-acp", "codex-acp", "antigravity-acp"];

pub fn is_tested(id: &str) -> bool {
    TESTED.contains(&id)
}

/// The built-in sill agent an entry is the package of, if any: it keeps the built-in's jail
/// settings (home paths, login, domains) and only takes the command.
pub fn builtin_for(entry: &Entry) -> Option<&'static str> {
    let pkg = entry.package_name().unwrap_or_default();
    match pkg.as_str() {
        "@agentclientprotocol/claude-agent-acp" | "@zed-industries/claude-code-acp" => Some("claude"),
        "@agentclientprotocol/codex-acp" | "@zed-industries/codex-acp" => Some("codex"),
        "@google/gemini-cli" => Some("gemini"),
        _ => None,
    }
}

// --- locations ------------------------------------------------------------------------------------

/// The live index and icons: under the state directory, which every jail masks (security.md S13 —
/// a cache the agent could write would be the index the next Install reads).
fn cache_dir(home: &Path) -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("registry")
}

pub fn cache_path(home: &Path) -> PathBuf {
    cache_dir(home).join("registry.json")
}

/// Where registry icons are cached (`<id>.svg`).
pub fn icon_cache_dir(home: &Path) -> PathBuf {
    cache_dir(home).join("icons")
}

pub fn tools_dir(home: &Path) -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("tools")
}

// --- the index --------------------------------------------------------------------------------------

/// The cached index and when it was fetched.
pub fn load_cached(home: &Path) -> Option<(Registry, SystemTime)> {
    let p = cache_path(home);
    let text = std::fs::read_to_string(&p).ok()?;
    let reg = serde_json::from_str::<Registry>(&text).ok()?;
    let when = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    Some((reg, when))
}

/// What the Agents page lists: the snapshot's entries, each carrying the live index's newer
/// version as `newer` when it has one, then the live entries the snapshot does not know, as
/// [`Trust::Live`]. The time the live index was fetched, if it is cached.
pub fn merged(home: &Path) -> (Vec<Entry>, Option<SystemTime>) {
    let mut out: Vec<Entry> = snapshot().agents.clone();
    let Some((live, when)) = load_cached(home) else { return (out, None) };
    for mut l in live.agents {
        l.trust = Trust::Live;
        match out.iter_mut().find(|s| s.id == l.id) {
            Some(s) => {
                // most agent.json files name no icon; the published index adds one
                if s.icon.is_none() {
                    s.icon = l.icon.clone();
                }
                if l.version != s.version && !l.version.is_empty() {
                    s.newer = Some(Box::new(l));
                }
            }
            None => out.push(l),
        }
    }
    (out, Some(when))
}

/// Download the index into the cache (curl: it honours the proxy variables and system CAs).
pub fn fetch(home: &Path) -> Result<Registry> {
    let p = cache_path(home);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.part");
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "30", "-o"])
        .arg(&tmp)
        .arg(URL)
        .output()
        .context("run curl")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("registry download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = std::fs::read_to_string(&tmp)?;
    let reg = serde_json::from_str::<Registry>(&text).context("registry json")?;
    std::fs::rename(&tmp, &p)?;
    Ok(reg)
}

/// Fetch an entry's icon into the icon cache; the path if it is there afterwards.
pub fn fetch_icon(home: &Path, entry: &Entry) -> Option<PathBuf> {
    let url = entry.icon.as_deref()?;
    let dir = icon_cache_dir(home);
    std::fs::create_dir_all(&dir).ok()?;
    let p = dir.join(format!("{}.svg", entry.id));
    if p.is_file() {
        return Some(p);
    }
    let tmp = p.with_extension("svg.part");
    let ok = Command::new("curl")
        .args(["-fsSL", "--max-time", "20", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && std::fs::rename(&tmp, &p).is_ok() { Some(p) } else { let _ = std::fs::remove_file(&tmp); None }
}

// --- installed state ------------------------------------------------------------------------------

pub fn installed(home: &Path, id: &str) -> Option<Installed> {
    let text = std::fs::read_to_string(tools_dir(home).join(id).join("installed.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether an agent's command can run here: an absolute path that exists, or a name found on PATH
/// (plus `~/.local/bin`, where npm's global installs land).
pub fn available(agent: &Agent, home: &Path) -> Option<PathBuf> {
    resolve_program(agent.command.first()?, home)
}

// --- install ---------------------------------------------------------------------------------------

/// Install an entry into the tools dir and register it; `log` gets the installer's output lines.
pub fn install(home: &Path, entry: &Entry, log: &mut dyn FnMut(String)) -> Result<Agent> {
    let dir = tools_dir(home).join(&entry.id);
    std::fs::create_dir_all(&dir)?;
    let how = entry.how().ok_or_else(|| anyhow!("no way to install {} on this platform", entry.name))?;
    log(match entry.trust {
        Trust::Snapshot => format!("from sill's registry snapshot ({} @ {})", snapshot().source, &snapshot().commit[..snapshot().commit.len().min(12)]),
        Trust::Live => "from the live index: version pinned as the index names it, nothing else verified".to_string(),
    });
    let (command, env) = match how {
        "npx" => match entry.trust {
            Trust::Snapshot => install_npm_locked(&dir, entry, log)?,
            Trust::Live => install_npm(&dir, entry.distribution.npx.as_ref().unwrap(), log)?,
        },
        "uvx" => install_uv(&dir, entry.distribution.uvx.as_ref().unwrap(), log)?,
        _ => install_binary(&dir, &entry.distribution.binary.as_ref().unwrap()[platform_key()], log)?,
    };
    let icon = fetch_icon(home, entry);
    let supports_load = match probe_load_session(&command, &env, home) {
        Ok(b) => b,
        Err(e) => {
            log(format!("probe (in a throwaway jail) failed: {e:#}"));
            false
        }
    };
    log(format!("probe: {}", if supports_load { "resumes sessions" } else { "no session resume" }));

    let installed = Installed { version: entry.version.clone(), kind: how.to_string(), command: command.clone(), env: env.clone(), trust: entry.trust };
    std::fs::write(dir.join("installed.json"), serde_json::to_string_pretty(&installed)?)?;

    let agent = match builtin_for(entry) {
        Some(id) => {
            let mut a = agents::builtin().into_iter().find(|a| a.id == id).expect("builtin");
            a.command = command;
            a.env.extend(env);
            a
        }
        None => Agent {
            id: entry.id.clone(),
            name: entry.name.clone(),
            enabled: true,
            command,
            env,
            supports_load,
            domains: vec![],
            sandbox_off_env: Default::default(),
            state_dirs: vec![],
            home_paths: vec![],
            seed_files: vec![],
            shared_files: vec![],
            host_login: vec![],
            home_env: None,
            home_env_parent: false,
            icon: icon.map(|p| p.display().to_string()),
            memory_paths: vec![],
            // where a registry agent keeps its login is not known: no log out for it
            login_files: vec![],
        },
    };
    agents::save_override(home, &agent)?;
    Ok(agent)
}

/// Remove an install and its registry override (a built-in goes back to its own command).
pub fn uninstall(home: &Path, entry: &Entry) -> Result<()> {
    let dir = tools_dir(home).join(&entry.id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)?;
    }
    let id = builtin_for(entry).map(str::to_string).unwrap_or_else(|| entry.id.clone());
    agents::remove_override(home, &id)?;
    Ok(())
}

fn run_logged(cmd: &mut Command, log: &mut dyn FnMut(String)) -> Result<()> {
    let line = format!("$ {:?}", cmd).replace('"', "");
    log(line);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for reader in [BufReader::new(Box::new(out) as Box<dyn std::io::Read + Send>), BufReader::new(Box::new(err) as Box<dyn std::io::Read + Send>)] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for l in reader.lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    for l in rx {
        if !l.trim().is_empty() {
            log(l);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

/// A live-index `npx` entry: the index's exact version, dependencies resolved by npm now.
fn install_npm(dir: &Path, d: &PackageDist, log: &mut dyn FnMut(String)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let version = npm_exact_version(&d.package).ok_or_else(|| anyhow!("{}: the index pins no exact version; refusing to install whatever npm resolves", d.package))?.to_string();
    // no lifecycle scripts: nothing from the package runs on the host at install time
    let mut cmd = Command::new("npm");
    cmd.args(["install", "--prefix"])
        .arg(dir)
        .args(["--no-audit", "--no-fund", "--ignore-scripts", "--loglevel=error", "--no-package-lock"])
        .arg(&d.package)
        .current_dir(dir);
    run_logged(&mut cmd, log).context("npm install")?;
    npm_retry_optionals(dir, &mut cmd, log)?;
    npm_command(dir, d, &version)
}

/// A snapshot `npx` entry: `npm ci` against the lockfile the snapshot carries, so every package in
/// the tree is the one the snapshot's commit reviewed, by integrity hash.
fn install_npm_locked(dir: &Path, entry: &Entry, log: &mut dyn FnMut(String)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let d = entry.distribution.npx.as_ref().unwrap();
    let (Some(lock), Some(manifest)) = (&entry.npm_lock, &entry.npm_manifest) else {
        bail!("the snapshot carries no lockfile for {}: rebuild it with `sill registry snapshot`", entry.id);
    };
    let version = npm_exact_version(&d.package).ok_or_else(|| anyhow!("{}: no exact version in the snapshot", d.package))?.to_string();
    std::fs::write(dir.join("package.json"), serde_json::to_string_pretty(manifest)?)?;
    std::fs::write(dir.join("package-lock.json"), serde_json::to_string(lock)?)?;
    let mut cmd = Command::new("npm");
    cmd.args(["ci", "--no-audit", "--no-fund", "--ignore-scripts", "--loglevel=error"]).current_dir(dir);
    run_logged(&mut cmd, log).context("npm ci")?;
    npm_retry_optionals(dir, &mut cmd, log)?;
    npm_command(dir, d, &version)
}

/// npm treats a failed fetch of an optional dependency as no error and says nothing at
/// --loglevel=error; the platform packages that carry an agent's native binary are optional
/// (`@anthropic-ai/claude-agent-sdk-linux-x64`, ~50 MB), so one dropped download leaves an
/// install that starts fine and fails at the first session. Verify, retry once, then refuse.
fn npm_retry_optionals(dir: &Path, cmd: &mut Command, log: &mut dyn FnMut(String)) -> Result<()> {
    let mut missing = missing_platform_optionals(dir)?;
    if !missing.is_empty() {
        log(format!("optional platform package(s) not installed: {}; retrying", missing.join(", ")));
        run_logged(cmd, log).context("npm install (retry)")?;
        missing = missing_platform_optionals(dir)?;
    }
    if !missing.is_empty() {
        bail!("platform package(s) missing after npm install: {}", missing.join(", "));
    }
    Ok(())
}

/// The installed package's executable (checked to be the pinned version) plus the entry's args.
fn npm_command(dir: &Path, d: &PackageDist, version: &str) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let name = strip_version(&d.package);
    let pkg_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("node_modules").join(&name).join("package.json")).context("package.json")?)?;
    let got = pkg_json.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if got != version {
        bail!("npm installed {name} {got}, not the pinned {version}");
    }
    let bin_name = match pkg_json.get("bin") {
        Some(serde_json::Value::String(_)) => name.rsplit('/').next().unwrap_or(&name).to_string(),
        Some(serde_json::Value::Object(m)) => {
            let keys: Vec<&String> = m.keys().collect();
            keys.iter()
                .find(|k| k.contains("acp"))
                .or_else(|| keys.first())
                .map(|k| k.to_string())
                .ok_or_else(|| anyhow!("package has no executable"))?
        }
        _ => bail!("package {name} has no executable"),
    };
    let bin = dir.join("node_modules/.bin").join(&bin_name);
    if !bin.exists() {
        bail!("expected executable {} is missing", bin.display());
    }
    let mut command = vec![bin.display().to_string()];
    command.extend(d.args.iter().cloned());
    Ok((command, d.env.clone()))
}

/// Optional dependencies for this platform, declared by any installed package but absent from the
/// tree. Platform packages are recognised by name (`…-linux-x64`, `…-linux-x64-gnu`; the musl
/// variants are left alone), which is how the SDKs that ship native binaries name them.
fn missing_platform_optionals(dir: &Path) -> Result<Vec<String>> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let tag = format!("{}-{arch}", std::env::consts::OS);
    let root = dir.join("node_modules");
    let mut declared = Vec::new();
    let mut pending = vec![(root.clone(), 0usize)];
    while let Some((nm, depth)) = pending.pop() {
        let Ok(rd) = std::fs::read_dir(&nm) else { continue };
        let mut pkgs = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let fname = e.file_name();
            let fname = fname.to_string_lossy();
            if fname.starts_with('@') {
                if let Ok(scoped) = std::fs::read_dir(&p) {
                    pkgs.extend(scoped.flatten().map(|e| e.path()));
                }
            } else if !fname.starts_with('.') {
                pkgs.push(p);
            }
        }
        for pkg in pkgs {
            let Ok(text) = std::fs::read_to_string(pkg.join("package.json")) else { continue };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if let Some(opt) = json.get("optionalDependencies").and_then(|v| v.as_object()) {
                for name in opt.keys() {
                    if (name.ends_with(&tag) || name.ends_with(&format!("{tag}-gnu"))) && !name.contains("musl") {
                        declared.push((name.clone(), pkg.clone()));
                    }
                }
            }
            if depth < 4 {
                pending.push((pkg.join("node_modules"), depth + 1));
            }
        }
    }
    let mut missing: Vec<String> = declared
        .into_iter()
        .filter(|(name, pkg)| {
            // resolved the way node does: every node_modules directory from the dependant upwards
            !pkg.ancestors()
                .take_while(|a| a.starts_with(&root))
                .filter(|a| a.file_name().is_some_and(|f| f == "node_modules"))
                .any(|nm| nm.join(name).join("package.json").is_file())
        })
        .map(|(name, _)| name)
        .collect();
    missing.sort();
    missing.dedup();
    Ok(missing)
}

fn install_uv(dir: &Path, d: &PackageDist, log: &mut dyn FnMut(String)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let bin_dir = dir.join("bin");
    uv_exact_version(&d.package).ok_or_else(|| anyhow!("{}: no `==` version pin; refusing to install whatever uv resolves", d.package))?;
    // wheels only: an sdist's build backend is arbitrary code, and this runs on the host
    let mut cmd = Command::new("uv");
    cmd.args(["tool", "install", "--force", "--no-build"])
        .arg(&d.package)
        .env("UV_TOOL_DIR", dir.join("uv"))
        .env("UV_TOOL_BIN_DIR", &bin_dir)
        .current_dir(dir);
    run_logged(&mut cmd, log).context("uv tool install")?;
    let mut exes: Vec<PathBuf> = std::fs::read_dir(&bin_dir)?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_file()).collect();
    exes.sort();
    let name = strip_version(&d.package);
    let pick = exes
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains("acp")))
        .or_else(|| exes.iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == name)))
        .or_else(|| exes.first())
        .ok_or_else(|| anyhow!("uv installed no executable"))?;
    let mut command = vec![pick.display().to_string()];
    command.extend(d.args.iter().cloned());
    Ok((command, d.env.clone()))
}

fn install_binary(dir: &Path, d: &BinaryDist, log: &mut dyn FnMut(String)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let url = &d.archive;
    let file = url.rsplit('/').next().unwrap_or("archive").split('?').next().unwrap_or("archive").to_string();
    let archive = dir.join(&file);
    let mut cmd = Command::new("curl");
    cmd.args(["-fSL", "--max-time", "600", "-o"]).arg(&archive).arg(url);
    run_logged(&mut cmd, log).context("download")?;
    // a download without a checksum is whatever the CDN served: not installed (security.md S3)
    let Some(want) = &d.sha256 else {
        let _ = std::fs::remove_file(&archive);
        bail!("the registry gives no sha256 for {url}: refusing to install an unverifiable binary");
    };
    {
        use sha2::{Digest, Sha256};
        let got: String = Sha256::digest(std::fs::read(&archive)?).iter().map(|b| format!("{b:02x}")).collect();
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&archive);
            bail!("checksum mismatch: expected {want}, got {got}");
        }
        log("checksum ok".into());
    }
    let lower = file.to_lowercase();
    let cmd_rel = d.cmd.trim_start_matches("./");
    if lower.ends_with(".zip") {
        let mut c = Command::new("unzip");
        c.args(["-oq"]).arg(&archive).arg("-d").arg(dir);
        run_logged(&mut c, log).context("unzip")?;
    } else if [".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".tar"].iter().any(|e| lower.ends_with(e)) {
        let mut c = Command::new("tar");
        c.arg("-xf").arg(&archive).arg("-C").arg(dir);
        run_logged(&mut c, log).context("tar")?;
    } else {
        // a raw executable
        let target = dir.join(cmd_rel);
        if target != archive {
            std::fs::rename(&archive, &target)?;
        }
    }
    let exe = dir.join(cmd_rel);
    if !exe.is_file() {
        bail!("executable {} not found after extraction", exe.display());
    }
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&exe)?.permissions();
    perm.set_mode(perm.mode() | 0o755);
    std::fs::set_permissions(&exe, perm)?;
    let mut command = vec![exe.display().to_string()];
    command.extend(d.args.iter().cloned());
    Ok((command, d.env.clone()))
}

/// A program by name on the shelf's `PATH` (and `~/.local/bin`), or as given when absolute.
fn resolve_program(name: &str, home: &Path) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default();
    dirs.push(home.join(".local/bin"));
    dirs.into_iter().map(|d| d.join(name)).find(|c| c.is_file())
}

/// Ask a freshly installed agent what it can do: `initialize` over its stdio with a short deadline.
/// Returns whether it advertises `loadSession`. The package was just downloaded, so it runs in a
/// throwaway jail, not on the host: no network, no capabilities, the system read-only, the home
/// hidden but for the tools dir and the runtime the command needs (security.md S3).
fn probe_load_session(command: &[String], env: &BTreeMap<String, String>, home: &Path) -> Result<bool> {
    let Some(first) = command.first() else { bail!("no command") };
    let prog = resolve_program(first, home).with_context(|| format!("{first}: not found"))?;
    let tools = tools_dir(home);
    let mut b = Command::new("bwrap");
    b.args(["--unshare-all", "--disable-userns", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"]);
    b.args(["--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run"]);
    b.arg("--tmpfs").arg(crate::jail::STATE_FS); // other sessions' upper trees
    let seccomp = crate::seccomp::memfd(false)?; // alive until the spawn
    if let Some(f) = &seccomp {
        b.arg("--seccomp").arg(std::os::fd::AsRawFd::as_raw_fd(f).to_string());
    }
    b.arg("--tmpfs").arg(home);
    b.arg("--ro-bind").arg(&tools).arg(&tools);
    // the runtime the command needs may live under the home (nvm's node, a venv): just those dirs
    let mut path_dirs: Vec<PathBuf> = ["/usr/local/bin", "/usr/bin", "/bin"].iter().map(PathBuf::from).collect();
    let mut needed: Vec<PathBuf> = prog.parent().map(Path::to_path_buf).into_iter().collect();
    for runtime in ["node", "python3", "uv"] {
        if let Some(p) = resolve_program(runtime, home).and_then(|p| p.parent().map(Path::to_path_buf)) {
            needed.push(p);
        }
    }
    for d in needed {
        if d.starts_with(home) && !d.starts_with(&tools) {
            b.arg("--ro-bind").arg(&d).arg(&d);
        }
        if !path_dirs.contains(&d) {
            path_dirs.push(d);
        }
    }
    let path = std::env::join_paths(path_dirs).context("PATH")?;
    b.arg("--setenv").arg("HOME").arg(home).arg("--setenv").arg("PATH").arg(path).arg("--chdir").arg(home);
    for (k, v) in env {
        b.arg("--setenv").arg(k).arg(v);
    }
    b.arg("--").arg(&prog).args(&command[1..]);
    let mut child = b.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("bwrap")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let init = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": 1,
        "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
        "clientInfo": { "name": "sill", "version": env!("CARGO_PKG_VERSION") },
    }});
    let _ = writeln!(stdin, "{init}");
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let result = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break Err(anyhow!("no answer to initialize"));
        }
        match rx.recv_timeout(deadline - now) {
            Ok(v) if v.get("id").and_then(|i| i.as_u64()) == Some(1) => {
                break Ok(v.pointer("/result/agentCapabilities/loadSession").and_then(|b| b.as_bool()).unwrap_or(false));
            }
            Ok(_) => {}
            Err(_) => break Err(anyhow!("agent exited during initialize")),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod platform_optional_tests {
    use super::missing_platform_optionals;
    use std::fs;

    #[test]
    fn missing_platform_optional_is_reported_and_resolved_up_the_tree() {
        let t = tempdir();
        let sdk = t.join("node_modules/@acme/sdk");
        fs::create_dir_all(&sdk).unwrap();
        let tag = format!("{}-{}", std::env::consts::OS, match std::env::consts::ARCH { "x86_64" => "x64", "aarch64" => "arm64", o => o });
        fs::write(
            sdk.join("package.json"),
            format!(r#"{{"name":"@acme/sdk","optionalDependencies":{{"@acme/sdk-{tag}":"1","@acme/sdk-{tag}-musl":"1","@acme/sdk-darwin-arm64":"1"}}}}"#),
        )
        .unwrap();
        assert_eq!(missing_platform_optionals(&t).unwrap(), vec![format!("@acme/sdk-{tag}")]);
        // hoisted next to the dependant's parent node_modules: found
        let native = t.join(format!("node_modules/@acme/sdk-{tag}"));
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("package.json"), "{}").unwrap();
        assert!(missing_platform_optionals(&t).unwrap().is_empty());
        fs::remove_dir_all(&t).unwrap();
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sill-opt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
}

// --- the snapshot builder (`sill registry snapshot`) ---------------------------------------------

/// Build `registry/snapshot.json` from the registry repository's `main` at its current commit:
/// every `<id>/agent.json` not in `quarantine.json`, the `preview` channel dropped, icons as URLs
/// at that commit, and for every `npx` entry a lockfile resolved now with `npm install
/// --package-lock-only` (no package code runs). Entries whose spec pins no exact version are kept
/// for the list but get no lockfile, so they cannot be installed from the snapshot. The result is
/// for review and commit: the commit that updates the snapshot is what vouches for it.
pub fn build_snapshot(out: &Path, log: &mut dyn FnMut(String)) -> Result<()> {
    let api = format!("https://api.github.com/repos/{REPO}");
    let head: serde_json::Value = serde_json::from_slice(&curl(&format!("{api}/commits/main"))?).context("commit json")?;
    let commit = head["sha"].as_str().context("commit sha")?.to_string();
    let date = head["commit"]["committer"]["date"].as_str().unwrap_or("").to_string();
    log(format!("{REPO} @ {commit} ({date})"));
    let raw = |path: &str| format!("https://raw.githubusercontent.com/{REPO}/{commit}/{path}");
    let quarantine: BTreeMap<String, String> = serde_json::from_slice(&curl(&raw("quarantine.json"))?).unwrap_or_default();
    let tree: Vec<serde_json::Value> = serde_json::from_slice(&curl(&format!("{api}/contents?ref={commit}"))?).context("repo contents")?;
    let mut agents: Vec<Entry> = Vec::new();
    for item in tree {
        let (Some(kind), Some(name)) = (item["type"].as_str(), item["name"].as_str()) else { continue };
        if kind != "dir" || name.starts_with('.') {
            continue;
        }
        if let Some(why) = quarantine.get(name) {
            log(format!("{name}: quarantined upstream ({why}), skipped"));
            continue;
        }
        let bytes = match curl(&raw(&format!("{name}/agent.json"))) {
            Ok(b) => b,
            Err(e) => {
                log(format!("{name}: no agent.json ({e:#}), skipped"));
                continue;
            }
        };
        let mut e: Entry = serde_json::from_slice(&bytes).with_context(|| format!("{name}/agent.json"))?;
        if e.id != name {
            log(format!("{name}: agent.json says id {}, skipped", e.id));
            continue;
        }
        if let Some(icon) = &e.icon
            && let Some(rel) = icon.strip_prefix("./")
        {
            e.icon = Some(raw(&format!("{name}/{rel}")));
        }
        if let Some(d) = &e.distribution.npx {
            match npm_exact_version(&d.package) {
                Some(_) => match npm_lockfile(&d.package) {
                    Ok((manifest, lock)) => {
                        log(format!("{name}: {} locked ({} packages)", d.package, lock["packages"].as_object().map(|m| m.len()).unwrap_or(0)));
                        e.npm_manifest = Some(manifest);
                        e.npm_lock = Some(lock);
                    }
                    Err(err) => log(format!("{name}: {} — no lockfile ({err:#}); listed, not installable from the snapshot", d.package)),
                },
                None => log(format!("{name}: {} pins no exact version; listed, not installable from the snapshot", d.package)),
            }
        }
        if let Some(d) = &e.distribution.uvx
            && uv_exact_version(&d.package).is_none()
        {
            log(format!("{name}: {} pins no exact version; listed, not installable", d.package));
        }
        agents.push(e);
    }
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    #[derive(Serialize)]
    struct Out<'a> {
        source: &'a str,
        commit: &'a str,
        date: &'a str,
        agents: &'a [Entry],
    }
    let text = serde_json::to_string_pretty(&Out { source: REPO, commit: &commit, date: &date, agents: &agents })?;
    std::fs::write(out, text.as_bytes())?;
    log(format!("{}: {} agents, {} KiB", out.display(), agents.len(), text.len() / 1024));
    Ok(())
}

fn curl(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "60"]).arg(url).output().context("run curl")?;
    if !out.status.success() {
        bail!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Resolve a package's dependency tree without installing it: the `package.json` and
/// `package-lock.json` npm writes in an empty directory.
fn npm_lockfile(spec: &str) -> Result<(serde_json::Value, serde_json::Value)> {
    let dir = std::env::temp_dir().join(format!("sill-lock-{}-{}", std::process::id(), spec.replace(['/', '@'], "_")));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let out = Command::new("npm")
        .args(["install", "--package-lock-only", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"])
        .arg(spec)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .context("run npm")?;
    let r = (|| -> Result<_> {
        if !out.status.success() {
            bail!("npm: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json"))?)?;
        let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package-lock.json"))?)?;
        Ok((manifest, lock))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    r
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn exact_versions() {
        assert_eq!(npm_exact_version("@agentclientprotocol/claude-agent-acp@0.99.1"), Some("0.99.1"));
        assert_eq!(npm_exact_version("cline@3.0.74"), Some("3.0.74"));
        assert_eq!(npm_exact_version("@agentclientprotocol/codex-acp@2.5.1-preview.13"), Some("2.5.1-preview.13"));
        assert_eq!(npm_exact_version("@scope/name"), None);
        assert_eq!(npm_exact_version("name@latest"), None);
        assert_eq!(npm_exact_version("name@^1.2.3"), None);
        assert_eq!(npm_exact_version("name@1.2"), None);
        assert_eq!(uv_exact_version("fast-agent-acp==0.10.1"), Some("0.10.1"));
        assert_eq!(uv_exact_version("minion-code"), None);
        assert_eq!(uv_exact_version("minion-code>=0.1"), None);
    }

    #[test]
    fn snapshot_parses_and_pins() {
        let s = snapshot();
        assert!(!s.commit.is_empty());
        for a in &s.agents {
            assert_eq!(a.trust, Trust::Snapshot);
            if let Some(d) = &a.distribution.npx
                && a.npm_lock.is_some()
            {
                let v = npm_exact_version(&d.package).expect("pinned");
                assert_eq!(a.version, v, "{}", a.id);
                let name = strip_version(&d.package);
                let locked = a.npm_lock.as_ref().unwrap()["packages"][format!("node_modules/{name}")]["version"].as_str();
                assert_eq!(locked, Some(v), "{}: lockfile pins the entry's version", a.id);
                assert!(a.npm_manifest.as_ref().unwrap()["dependencies"][&name].is_string(), "{}: manifest names the package", a.id);
            }
        }
        assert!(s.agents.iter().any(|a| a.id == "claude-acp" && a.npm_lock.is_some()));
    }

    #[test]
    fn merged_marks_live_entries() {
        let dir = std::env::temp_dir().join(format!("sill-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let cache = cache_path(&home);
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        let snap = snapshot();
        let known = &snap.agents[0];
        let live = serde_json::json!({ "version": "1", "agents": [
            { "id": known.id, "name": known.name, "version": "999.0.0", "icon": "https://example/x.svg", "distribution": { "npx": { "package": "x@999.0.0" } } },
            { "id": "brand-new", "name": "New", "version": "1.0.0", "distribution": { "npx": { "package": "new@1.0.0" } } }
        ]});
        std::fs::write(&cache, live.to_string()).unwrap();
        let (entries, when) = merged(&home);
        assert!(when.is_some());
        assert_eq!(entries.len(), snap.agents.len() + 1);
        let k = entries.iter().find(|e| e.id == known.id).unwrap();
        assert_eq!(k.trust, Trust::Snapshot);
        assert_eq!(k.version, known.version, "the snapshot's version stays");
        assert_eq!(k.newer.as_ref().map(|n| n.version.as_str()), Some("999.0.0"));
        assert_eq!(k.newer.as_ref().unwrap().trust, Trust::Live);
        assert_eq!(k.icon.as_deref(), known.icon.as_deref().or(Some("https://example/x.svg")), "the live icon fills a missing one");
        let n = entries.iter().find(|e| e.id == "brand-new").unwrap();
        assert_eq!(n.trust, Trust::Live);
        assert_eq!(n.install_spec(), "new@1.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
