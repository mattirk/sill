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
pub const TESTED: &[&str] = &["claude-acp", "codex-acp", "antigravity-acp", "kimi-code", "mistral-vibe"];

pub fn is_tested(id: &str) -> bool {
    TESTED.contains(&id)
}

/// The built-in sill agent an entry is the package of, if any: it keeps the built-in's jail
/// settings (home paths, login, domains) and only takes the command.
pub fn builtin_for(entry: &Entry) -> Option<&'static str> {
    // binary distributions: no package name to match
    if entry.id == "antigravity-acp" {
        return Some("antigravity-acp");
    }
    if entry.id == "mistral-vibe" {
        return Some("mistral-vibe");
    }
    let pkg = entry.package_name().unwrap_or_default();
    match pkg.as_str() {
        "@agentclientprotocol/claude-agent-acp" | "@zed-industries/claude-code-acp" => Some("claude"),
        "@agentclientprotocol/codex-acp" | "@zed-industries/codex-acp" => Some("codex"),
        "@google/gemini-cli" => Some("gemini"),
        "@moonshot-ai/kimi-code" => Some("kimi-code"),
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
/// [`Trust::Live`]. The duplicate `opencode-v2` listing is omitted in favour of `opencode`;
/// the retired Kimi CLI (`kimi`) is omitted in favour of Kimi Code. Existing installations
/// keep their id and remain usable. The time the live index was fetched,
/// if it is cached.
pub fn merged(home: &Path) -> (Vec<Entry>, Option<SystemTime>) {
    let mut out: Vec<Entry> = snapshot().agents.iter().filter(|e| !matches!(e.id.as_str(), "opencode-v2" | "kimi")).cloned().collect();
    let Some((live, when)) = load_cached(home) else { return (out, None) };
    for mut l in live.agents.into_iter().filter(|e| !matches!(e.id.as_str(), "opencode-v2" | "kimi")) {
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
        .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--max-filesize", "67108864", "--max-time", "30", "-o"])
        .arg(&tmp)
        .arg("--")
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

/// An address the index may send curl to: `https://`, no blanks or control characters, so it is
/// one argument that is never an option, a file or a local protocol.
fn https_url(url: &str) -> bool {
    url.len() <= 2048
        && url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
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
    // the index names this address: only a web address, only https, a small file
    // (audit 2026-11-15 A7)
    if !https_url(url) {
        return None;
    }
    let tmp = p.with_extension("svg.part");
    let ok = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--max-filesize", "1048576", "--max-time", "20", "-o"])
        .arg(&tmp)
        .arg("--")
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && std::fs::rename(&tmp, &p).is_ok() { Some(p) } else { let _ = std::fs::remove_file(&tmp); None }
}

/// An agent's icon by its registry id, whatever version is installed: the cached file when it is
/// there, else fetched from the first listing of that id that names one (the snapshot's, the
/// live index's, or its newer version's). An installed version the index has moved past still
/// has its icon (Kimi Code 2.6.0 against a live 2.6.1 had none, the person, 2026-11-13).
pub fn icon_by_id(home: &Path, id: &str) -> Option<PathBuf> {
    let cached = icon_cache_dir(home).join(format!("{id}.svg"));
    if cached.is_file() {
        return Some(cached);
    }
    merged(home)
        .0
        .into_iter()
        .flat_map(|mut e| {
            let newer = e.newer.take();
            std::iter::once(e).chain(newer.map(|n| *n))
        })
        .filter(|e| e.id == id && e.icon.is_some())
        .find_map(|e| fetch_icon(home, &e))
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
    let supports_load = if builtin_for(entry).is_none() {
        // no definition yet: the same first start also shows which directories it wants
        match discover_into(home, &entry.id, &entry.version, &command, &env, log) {
            Ok((_, supports_load)) => supports_load,
            Err(e) => {
                log(format!("directory discovery (in a throwaway jail) failed: {e:#}; use Find directories on the Agents page"));
                false
            }
        }
    } else { match probe_load_session(&command, &env, home) {
        Ok(b) => b,
        Err(e) => {
            log(format!("probe (in a throwaway jail) failed: {e:#}"));
            false
        }
    }
    };
    log(format!("session resume: {}", if supports_load { "resumes sessions" } else { "no session resume" }));

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
            experimental_acp_v2: false,
            domains: vec![],
            sandbox_off_env: Default::default(),
            home_paths: None,
            login_files: vec![],
            host_login: vec![],
            home_env: None,
            home_env_parent: false,
            icon: icon.map(|p| p.display().to_string()),
            memory_paths: vec![],
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
    if !https_url(url) {
        bail!("the registry names {url:?} as the download: not an https address");
    }
    // The command is a path inside the install directory, nothing above or outside it: a raw
    // download is renamed to it and it is made executable (audit 2026-11-15 A6).
    let cmd_rel = crate::agents::relative_path(d.cmd.trim_start_matches("./"))
        .with_context(|| format!("the registry names {:?} as the command", d.cmd))?;
    let file = url.rsplit('/').next().unwrap_or("archive").split('?').next().unwrap_or("archive").to_string();
    let file = if file.is_empty() || file == "." || file == ".." { "archive".to_string() } else { file };
    let archive = dir.join(&file);
    let mut cmd = Command::new("curl");
    cmd.args(["-fSL", "--proto", "=https", "--proto-redir", "=https", "--max-time", "600", "-o"]).arg(&archive).arg("--").arg(url);
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
        let target = dir.join(&cmd_rel);
        if target != archive {
            std::fs::rename(&archive, &target)?;
        }
    }
    let exe = dir.join(&cmd_rel);
    // a regular file of the package itself, not a link out of it
    if !exe.symlink_metadata().is_ok_and(|m| m.is_file()) {
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

/// A program by name on the daemon's `PATH` (and `~/.local/bin`), or as given when absolute.
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
    let (mut b, _, _seccomp) = throwaway_jail(command, env, home, None)?;
    let mut child = b.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("bwrap")?;
    let result = acp_first_start(&mut child, home, false).map(|(supports_load, _)| supports_load);
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The throwaway jail both probes run in. `scratch` (a fresh host directory) stands in for the
/// home when given, so what the agent writes there can be read afterwards; otherwise the home is
/// an empty tmpfs. The second value: the directories under the home bound into the jail (the
/// tools dir and the command's runtime), relative to the home — bwrap creates them as
/// mountpoints, so they are not the agent's own writes.
fn throwaway_jail(command: &[String], env: &BTreeMap<String, String>, home: &Path, scratch: Option<&Path>) -> Result<(Command, Vec<PathBuf>, Option<std::os::fd::OwnedFd>)> {
    let Some(first) = command.first() else { bail!("no command") };
    let Base { mut b, bound, seccomp, prog, path } = jail_base(first, home, scratch, false)?;
    b.arg("--setenv").arg("HOME").arg(home).arg("--setenv").arg("PATH").arg(path).arg("--chdir").arg(home);
    for (k, v) in env {
        b.arg("--setenv").arg(k).arg(v);
    }
    b.arg("--").arg(&prog).args(&command[1..]);
    Ok((b, bound, seccomp))
}

/// What [`throwaway_jail`] and [`login_jail`] share: the bwrap command up to its mounts.
struct Base {
    b: Command,
    bound: Vec<PathBuf>,
    /// Kept alive by the caller until the spawn.
    seccomp: Option<std::os::fd::OwnedFd>,
    /// The program, resolved on the host.
    prog: PathBuf,
    /// `PATH` inside: the system's directories and the runtime the command needs.
    path: std::ffi::OsString,
}

/// A jail with nothing of the person's in it: no capabilities, the system read-only, `/tmp` and
/// `/run` fresh, the state filesystem hidden, the person's files outside the home hidden
/// ([`crate::mounts::personal_data`]: Mac folders, added disks, network shares), the home an empty
/// tmpfs (or `scratch` in its place) but for the tools dir and the runtime the command needs. Its
/// own network namespace, unless `share_net`.
fn jail_base(program: &str, home: &Path, scratch: Option<&Path>, share_net: bool) -> Result<Base> {
    let prog = resolve_program(program, home).with_context(|| format!("{program}: not found"))?;
    let tools = tools_dir(home);
    let mut b = Command::new("bwrap");
    b.arg("--unshare-all");
    if share_net {
        b.arg("--share-net");
    }
    b.args(["--unshare-user", "--disable-userns", "--die-with-parent", "--new-session", "--cap-drop", "ALL", "--clearenv"]);
    b.args(["--ro-bind", "/", "/", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run"]);
    let mounts = crate::mounts::host_mounts().unwrap_or_default();
    // a link there is not a directory to cover: what it points at is covered where it is
    let hidden: Vec<PathBuf> = crate::mounts::personal_data(&mounts).into_iter().filter(|p| p.symlink_metadata().is_ok_and(|m| m.is_dir())).collect();
    for dir in &hidden {
        b.arg("--tmpfs").arg(dir);
    }
    b.arg("--tmpfs").arg(crate::jail::STATE_FS); // other sessions' upper trees
    // … and the same filesystem wherever else it is mounted (audit 2026-11-15 A2)
    for alias in crate::mounts::state_aliases(&mounts, Path::new(crate::jail::STATE_FS)) {
        if alias != Path::new("/") && !home.starts_with(&alias) {
            b.arg("--tmpfs").arg(alias);
        }
    }
    let seccomp = crate::seccomp::memfd(false)?; // the caller keeps it alive until the spawn
    if let Some(f) = &seccomp {
        b.arg("--seccomp").arg(std::os::fd::AsRawFd::as_raw_fd(f).to_string());
    }
    match scratch {
        Some(dir) => { b.arg("--bind").arg(dir).arg(home); }
        None => { b.arg("--tmpfs").arg(home); }
    }
    let mut bound = Vec::new();
    if tools.starts_with(home) {
        bound.push(tools.strip_prefix(home).unwrap().to_path_buf());
    }
    b.arg("--ro-bind").arg(&tools).arg(&tools);
    // the runtime the command needs may live under the home (nvm's node, a venv): just those dirs
    let mut path_dirs: Vec<PathBuf> = ["/usr/local/bin", "/usr/bin", "/bin"].iter().map(PathBuf::from).collect();
    let programs: Vec<PathBuf> = std::iter::once(prog.clone()).chain(["node", "python3", "uv"].iter().filter_map(|runtime| resolve_program(runtime, home))).collect();
    // directories for PATH, then what their links point at (bound, not on PATH)
    let mut needed: Vec<(PathBuf, bool)> = programs.iter().filter_map(|p| p.parent()).map(|d| (d.to_path_buf(), true)).collect();
    needed.extend(programs.iter().filter_map(|p| link_target_root(p, home)).map(|d| (d, false)));
    let mut binds: Vec<PathBuf> = Vec::new();
    for (d, on_path) in needed {
        // covered above, so bound back; never the home itself or a directory that holds it
        let covered = d.starts_with(home) || hidden.iter().any(|h| d.starts_with(h));
        if covered && !d.starts_with(&tools) && !binds.iter().any(|done| d.starts_with(done)) {
            if home.starts_with(&d) || hidden.iter().any(|h| h.starts_with(&d)) {
                // the program's own directory: without it there is nothing to run
                anyhow::ensure!(prog.parent() != Some(d.as_path()), "{} is in {}, which holds your files: put it in a folder of its own", prog.display(), d.display());
                continue;
            }
            b.arg("--ro-bind").arg(&d).arg(&d);
            if let Ok(rel) = d.strip_prefix(home) {
                bound.push(rel.to_path_buf());
            }
            binds.push(d.clone());
        }
        if on_path && !path_dirs.contains(&d) {
            path_dirs.push(d);
        }
    }
    let path = std::env::join_paths(path_dirs).context("PATH")?;
    Ok(Base { b, bound, seccomp, prog, path })
}

/// The directory to bind for a program that is a link: `~/.nvm/…/bin/agent` points into
/// `lib/node_modules/@scope/agent`, a pipx script into its venv, and binding the link's own
/// directory alone leaves it dangling in the jail. The package's root when one is recognised (a
/// `node_modules` entry, a Python venv, the prefix of a `bin` directory at least two levels
/// below the home), else the target's directory. `None` for a program that is no link.
fn link_target_root(program: &Path, home: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(program).ok()?;
    let plain = std::fs::canonicalize(program.parent()?).ok()?.join(program.file_name()?);
    if real == plain {
        return None;
    }
    let parts: Vec<_> = real.components().collect();
    if let Some(i) = parts.iter().position(|c| c.as_os_str() == "node_modules") {
        let scoped = parts.get(i + 1).is_some_and(|c| c.as_os_str().to_string_lossy().starts_with('@'));
        let n = i + if scoped { 3 } else { 2 };
        if parts.len() > n {
            return Some(parts[..n].iter().collect());
        }
    }
    if let Some(venv) = real.ancestors().skip(1).find(|a| a.join("pyvenv.cfg").is_file()) {
        return Some(venv.to_path_buf());
    }
    let dir = real.parent()?;
    if dir.file_name().is_some_and(|n| n == "bin")
        && let Some(prefix) = dir.parent()
        && prefix.strip_prefix(home).map_or(true, |rel| rel.components().count() >= 2)
    {
        return Some(prefix.to_path_buf());
    }
    Some(dir.to_path_buf())
}

/// What a jailed login keeps of the daemon's environment: the language, and how to reach the
/// network. Everything else of it stays out.
const LOGIN_ENV: &[&str] = &[
    "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TERM", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy",
    "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
];

/// The jail used for every login, regardless of agent or installation source. The person's
/// home is hidden; `scratch`
/// (the login's private home, where its credential lands) and the file `urls` (where the browser
/// helper hands over the page to open) are the only writable places, each at its own path, so
/// `env` reads as it does on the host. The network is the host's: the login reaches its provider
/// and the browser reaches the callback port the agent listens on. `env` is the login's own
/// (`HOME`, the XDG directories, the helper's variables); its `PATH` is replaced by `front`
/// (the browser helper's directory) before the jail's own.
pub fn login_jail(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    home: &Path,
    scratch: &Path,
    urls: &Path,
    front: &Path,
) -> Result<(Command, Option<std::os::fd::OwnedFd>)> {
    let Base { mut b, seccomp, prog, path, .. } = jail_base(program, home, None, true)?;
    // the helper appends to this file; a bind needs it to exist
    std::fs::OpenOptions::new().create(true).append(true).open(urls).with_context(|| format!("create {}", urls.display()))?;
    b.arg("--bind").arg(scratch).arg(scratch).arg("--bind").arg(urls).arg(urls);
    // name resolution: the system's resolv.conf is often a link into /run, which is fresh here
    if let Ok(resolv) = std::fs::canonicalize("/etc/resolv.conf")
        && resolv.starts_with("/run")
    {
        b.arg("--ro-bind").arg(&resolv).arg(&resolv);
    }
    for name in LOGIN_ENV {
        if let Some(value) = std::env::var_os(name) {
            b.arg("--setenv").arg(name).arg(value);
        }
    }
    for (k, v) in env.iter().filter(|(k, _)| k.as_str() != "PATH") {
        b.arg("--setenv").arg(k).arg(v);
    }
    let mut dirs = vec![front.to_path_buf()];
    dirs.extend(std::env::split_paths(&path));
    b.arg("--setenv").arg("PATH").arg(std::env::join_paths(dirs).context("PATH")?);
    b.arg("--chdir").arg(scratch).arg("--").arg(&prog).args(args);
    Ok((b, seccomp))
}

/// `initialize` the agent over its stdio (20 s); with `open_session`, also `session/new` in the
/// home (10 s, its answer not required) and a moment for it to finish writing. Returns whether
/// it advertises `loadSession`, and the login methods it offers.
fn acp_first_start(child: &mut std::process::Child, home: &Path, open_session: bool) -> Result<(bool, Vec<AuthMethodInfo>)> {
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
    let wait_for = |id: u64, limit: Duration| -> Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                bail!("no answer within {} s", limit.as_secs());
            }
            match rx.recv_timeout(deadline - now) {
                Ok(v) if v.get("id").and_then(|i| i.as_u64()) == Some(id) => break Ok(v),
                Ok(_) => {}
                Err(_) => bail!("agent exited"),
            }
        }
    };
    let answer = wait_for(1, Duration::from_secs(20)).context("initialize")?;
    let supports_load = answer.pointer("/result/agentCapabilities/loadSession").and_then(|b| b.as_bool()).unwrap_or(false);
    let methods = answer.pointer("/result/authMethods").and_then(|m| m.as_array()).into_iter().flatten().filter_map(|m| Some(AuthMethodInfo {
        id: m.get("id").and_then(|v| v.as_str())?.to_string(),
        name: m.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        description: m.get("description").and_then(|v| v.as_str()).map(str::to_string),
    })).collect();
    if open_session {
        let new = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": home, "mcpServers": [] }});
        let _ = writeln!(stdin, "{new}");
        let _ = wait_for(2, Duration::from_secs(10)); // a refusal (not logged in) still made it write its state
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok((supports_load, methods))
}

// --- directory discovery -----------------------------------------------------------------------------

/// A path an agent created under a fresh home on its first start: a proposal for its `home_paths`
/// (docs/agent-directories.md, "Directory setup by probe").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// `~/…`, as `home_paths` names it.
    pub path: String,
    pub kind: agents::HomePathKind,
    /// The same path exists in the person's real home: owning it hides the host's copy from this
    /// agent (and is what the person may not want for a shared cache).
    pub on_host: bool,
    /// A file: what the agent wrote, when it is small text. It becomes the file's initial content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_content: Option<String>,
}

/// A login method the agent offered at `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What the last discovery found, kept beside `installed.json` until the definition is set up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    /// Unix seconds.
    pub at: u64,
    /// The installed version it ran.
    pub version: String,
    pub proposals: Vec<Proposal>,
    /// The login methods the agent offers (from the first start's `initialize`).
    #[serde(default)]
    pub auth_methods: Vec<AuthMethodInfo>,
    /// The proposals come from a login with this method (`login::setup`), not from a bare
    /// first start; its files are already in the agent's sill-owned home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_method: Option<String>,
    /// Files the login wrote under the proposed directories, relative to the home: candidates
    /// for `login_files`.
    #[serde(default)]
    pub login_files: Vec<String>,
    /// What went wrong on the way (the first start did not answer `initialize`, …): shown with
    /// the result, since a probe that failed quietly looks like one that found nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn record_discovery(home: &Path, id: &str, discovery: &Discovery) -> Result<()> {
    let dir = tools_dir(home).join(id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("discovered.json"), serde_json::to_string_pretty(discovery)?)?;
    Ok(())
}

const MAX_INITIAL_CONTENT: u64 = 64 * 1024;

pub fn discovered(home: &Path, id: &str) -> Option<Discovery> {
    let text = std::fs::read_to_string(tools_dir(home).join(id).join("discovered.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Start an installed agent once in the throwaway jail with a fresh, readable home, and record
/// what it created there as proposals. Nothing is declared: the person chooses on the Agents page.
pub fn discover(home: &Path, agent: &Agent, log: &mut dyn FnMut(String)) -> Result<Discovery> {
    let version = installed(home, &agent.id).map(|i| i.version).unwrap_or_default();
    discover_into(home, &agent.id, &version, &agent.command, &agent.env, log).map(|(d, _)| d)
}

fn discover_into(home: &Path, id: &str, version: &str, command: &[String], env: &BTreeMap<String, String>, log: &mut dyn FnMut(String)) -> Result<(Discovery, bool)> {
    let dir = tools_dir(home).join(id);
    std::fs::create_dir_all(&dir)?;
    let scratch = dir.join("probe-home");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir(&scratch)?;
    log("first start in a throwaway jail with an empty home, to see which directories it creates".into());
    let result = (|| {
        let (mut b, bound, _seccomp) = throwaway_jail(command, env, home, Some(&scratch))?;
        let mut child = b.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("bwrap")?;
        let started = acp_first_start(&mut child, home, true);
        let _ = child.kill();
        let _ = child.wait();
        let (supports_load, methods, note) = match started {
            Ok((l, m)) => (l, m, None),
            Err(e) => {
                let note = format!("the agent did not complete its first start in the throwaway jail: {e:#}");
                log(format!("{note}; proposing what it wrote anyway"));
                (false, vec![], Some(note))
            }
        };
        for rel in &bound {
            remove_empty_chain(&scratch, rel);
        }
        Ok::<_, anyhow::Error>((proposals_from(&scratch, home), supports_load, methods, note))
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    let (proposals, supports_load, auth_methods, note) = result?;
    if proposals.is_empty() {
        log("it created nothing under the home before a login".into());
    }
    for p in &proposals {
        log(format!("created {}{}", p.path, if p.on_host { " (exists on this machine)" } else { "" }));
    }
    if !auth_methods.is_empty() {
        log(format!("login methods: {}", auth_methods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let discovery = Discovery { at: now_secs(), version: version.to_string(), proposals, auth_methods, login_method: None, login_files: vec![], note };
    record_discovery(home, id, &discovery)?;
    Ok((discovery, supports_load))
}

/// bwrap made `rel` (and its parents) under `scratch` as a mountpoint; take the empty chain away.
fn remove_empty_chain(scratch: &Path, rel: &Path) {
    let mut p = scratch.join(rel);
    while p != scratch && std::fs::remove_dir(&p).is_ok() {
        match p.parent() { Some(parent) => p = parent.to_path_buf(), None => break }
    }
}

/// Container directories whose children, not themselves, are an agent's own (`~/.config` is a
/// shared parent `home_paths` refuses).
const CONTAINERS: &[&str] = &[".config", ".local", ".local/share", ".local/state"];
/// Never an agent's persistent state: tool caches (session-private already), shell leftovers.
const IGNORED: &[&str] = &[".cache", ".npm", ".bun", ".yarn", ".pnpm-store", ".local/bin", ".local/share/pnpm", ".local/share/Trash",
    ".bash_history", ".node_repl_history", ".python_history", ".lesshst", ".wget-hsts", ".Xauthority", ".sudo_as_admin_successful"];

/// The proposals in a home tree an agent wrote: its top-level entries, and inside the container
/// directories their children. A directory holding nothing but one directory stands for that
/// one (Antigravity writes under `~/.gemini/antigravity-acp/`: that, not Gemini CLI's
/// `~/.gemini`). Sorted by path.
pub fn proposals_from(scratch: &Path, real_home: &Path) -> Vec<Proposal> {
    fn single_child_dir(dir: &Path) -> Option<PathBuf> {
        let mut entries = std::fs::read_dir(dir).ok()?.flatten();
        let only = entries.next()?;
        if entries.next().is_some() || !only.file_type().ok()?.is_dir() {
            return None;
        }
        Some(only.path())
    }
    fn walk(scratch: &Path, real_home: &Path, rel: &Path, out: &mut Vec<Proposal>) {
        let Ok(rd) = std::fs::read_dir(scratch.join(rel)) else { return };
        for entry in rd.flatten() {
            let child = rel.join(entry.file_name());
            let text = child.to_string_lossy();
            if IGNORED.contains(&text.as_ref()) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() && CONTAINERS.contains(&text.as_ref()) {
                walk(scratch, real_home, &child, out);
                continue;
            }
            let kind = if meta.is_dir() { agents::HomePathKind::Directory } else if meta.is_file() { agents::HomePathKind::File } else { continue };
            let initial_content = (kind == agents::HomePathKind::File && meta.len() <= MAX_INITIAL_CONTENT)
                .then(|| std::fs::read_to_string(entry.path()).ok()).flatten();
            let mut child = child;
            if kind == agents::HomePathKind::Directory {
                while let Some(only) = single_child_dir(&scratch.join(&child)) {
                    child = child.join(only.file_name().unwrap());
                }
            }
            let text = child.to_string_lossy();
            out.push(Proposal { path: format!("~/{text}"), kind, on_host: real_home.join(&child).exists(), initial_content });
        }
    }
    let mut out = Vec::new();
    walk(scratch, real_home, Path::new(""), &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Names a login never is: logs, databases and their journals, temporary files.
fn incidental(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".log", ".sqlite", ".sqlite-wal", ".sqlite-shm", ".db", ".db-journal", ".tmp", ".lock", ".pid"].iter().any(|e| lower.ends_with(e))
        || lower.starts_with("tmp") || lower.contains(".log.")
}

/// The regular files under the proposed directories in `scratch`, relative to the home: what a
/// login wrote, as candidates for `login_files`. Sorted.
pub fn credential_candidates(scratch: &Path, proposals: &[Proposal]) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                if !incidental(&name) && !["logs", "log", "tmp", "cache"].contains(&name.as_str()) {
                    walk(root, &entry.path(), out);
                }
            } else if meta.is_file() && !incidental(&name) {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    let mut out = Vec::new();
    for p in proposals.iter().filter(|p| p.kind == agents::HomePathKind::Directory) {
        walk(scratch, &scratch.join(p.path.trim_start_matches("~/")), &mut out);
    }
    out.sort();
    out
}

#[cfg(test)]
mod discovery_tests {
    #[test]
    fn the_index_sends_curl_only_to_https_and_installs_only_inside_the_agents_folder() {
        assert!(super::https_url("https://example.org/a/icon.svg"));
        assert!(super::https_url("HTTPS://example.org/x"));
        for bad in ["http://example.org/x", "file:///etc/passwd", "gopher://127.0.0.1/", "-K/tmp/x", "icon.svg", "https://a/b c", "https://a/\n-o/x", ""] {
            assert!(!super::https_url(bad), "{bad:?}");
        }
        let dir = std::env::temp_dir().join(format!("sill-registry-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dist = |cmd: &str, archive: &str| super::BinaryDist { archive: archive.into(), cmd: cmd.into(), sha256: Some("00".into()), args: vec![], env: Default::default() };
        // refused before anything is downloaded
        for cmd in ["../../../.bashrc", "/etc/cron.d/x", "a/../../b", ""] {
            let err = super::install_binary(&dir, &dist(cmd, "https://example.invalid/agent"), &mut |_| {}).unwrap_err();
            assert!(format!("{err:#}").contains("as the command"), "{cmd:?}: {err:#}");
        }
        let err = super::install_binary(&dir, &dist("./agent", "file:///etc/passwd"), &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("not an https address"), "{err:#}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use std::fs;

    #[test]
    fn proposals_are_top_level_entries_and_container_children_with_host_presence() {
        let t = std::env::temp_dir().join(format!("sill-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let scratch = t.join("scratch");
        let real = t.join("real");
        for d in [".antigravity/logs", ".config/antigravity", ".config/sill", ".local/share/agent", ".local/state/sill/tools/x", ".cache/stuff", ".npm/_cacache", ".local/bin"] {
            fs::create_dir_all(scratch.join(d)).unwrap();
        }
        fs::write(scratch.join(".antigravity/settings.json"), "{}").unwrap();
        fs::write(scratch.join(".agentrc"), "{\"a\":1}").unwrap();
        fs::write(scratch.join(".bash_history"), "ls").unwrap();
        fs::write(scratch.join(".config/antigravity/prefs"), "x").unwrap();
        fs::create_dir_all(real.join(".config/sill")).unwrap();
        // what bwrap made for a mount, emptied again
        remove_empty_chain(&scratch, Path::new(".local/state/sill/tools/x"));
        let found = proposals_from(&scratch, &real);
        let paths: Vec<_> = found.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, ["~/.agentrc", "~/.antigravity", "~/.config/antigravity", "~/.config/sill", "~/.local/share/agent"]);
        assert_eq!(found[0].kind, agents::HomePathKind::File);
        assert_eq!(found[0].initial_content.as_deref(), Some("{\"a\":1}"));
        assert_eq!(found[1].kind, agents::HomePathKind::Directory);
        assert!(found[1].initial_content.is_none());
        assert!(found[3].on_host, "~/.config/sill exists in the real home");
        assert!(!found[1].on_host);
        // a directory holding only one directory stands for that one, and the login's files are found under it
        let login = t.join("login");
        for d in [".gemini/antigravity-acp/logs", ".config", ".local/share", ".cache"] {
            fs::create_dir_all(login.join(d)).unwrap();
        }
        fs::write(login.join(".gemini/antigravity-acp/oauth_creds.json"), "{}").unwrap();
        fs::write(login.join(".gemini/antigravity-acp/settings.json"), "{}").unwrap();
        fs::write(login.join(".gemini/antigravity-acp/logs/agent.log"), "x").unwrap();
        fs::write(login.join(".gemini/antigravity-acp/state.sqlite"), "x").unwrap();
        let found = proposals_from(&login, &real);
        assert_eq!(found.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(), ["~/.gemini/antigravity-acp"]);
        assert_eq!(credential_candidates(&login, &found), [".gemini/antigravity-acp/oauth_creds.json", ".gemini/antigravity-acp/settings.json"]);
        fs::remove_dir_all(&t).unwrap();
    }

    /// The real probe around an agent sill installed on this host, named by `SILL_PROBE_AGENT`
    /// (its id under the tools dir): prints what it found. Needs bubblewrap user namespaces.
    #[test]
    #[ignore = "host only: runs an installed agent in the throwaway jail"]
    fn real_discovery_of_an_installed_agent() {
        let Ok(id) = std::env::var("SILL_PROBE_AGENT") else { return };
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let mut agent = agents::load(&home).unwrap().remove(&id).expect("an installed agent");
        agent.home_paths = None;
        let found = discover(&home, &agent, &mut |line| eprintln!("  {line}")).unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&found).unwrap());
        assert!(found.note.is_none(), "the first start did not complete: {:?}", found.note);
    }

    #[test]
    fn proposals_become_valid_home_paths() {
        let t = std::env::temp_dir().join(format!("sill-discover-valid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        fs::create_dir_all(t.join("s/.agent")).unwrap();
        fs::write(t.join("s/.agent.json"), "{}").unwrap();
        let mut a = agents::builtin().remove(0);
        a.login_files.clear();
        a.home_env = None;
        a.home_paths = Some(proposals_from(&t.join("s"), &t.join("none")).iter().map(|p| agents::HomePath { path: p.path.clone(), kind: p.kind, initial_content: p.initial_content.clone() }).collect());
        a.validate().unwrap();
        fs::remove_dir_all(&t).unwrap();
    }
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
    fn mistral_vibe_registry_uses_the_tested_builtin_defaults() {
        let entry: Entry = serde_json::from_value(serde_json::json!({"id": "mistral-vibe", "name": "Mistral Vibe", "version": "3.1.1"})).unwrap();
        assert!(is_tested(&entry.id));
        assert_eq!(builtin_for(&entry), Some("mistral-vibe"));
        let agent = agents::builtin().into_iter().find(|a| a.id == "mistral-vibe").unwrap();
        agent.validate().unwrap();
        assert_eq!(agent.home_paths.unwrap()[0].path, "~/.vibe");
        assert_eq!(agent.login_files, [".vibe/.env"]);
        assert_eq!(agent.host_login, ["browser-auth"]);
    }

    #[test]
    fn kimi_code_registry_uses_the_tested_builtin_defaults() {
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "id": "kimi-code", "name": "Kimi Code", "version": "2.5.2",
            "distribution": {"npx": {"package": "@moonshot-ai/kimi-code@2.5.2"}}
        })).unwrap();
        assert!(is_tested(&entry.id));
        assert_eq!(builtin_for(&entry), Some("kimi-code"));
        let agent = agents::builtin().into_iter().find(|a| a.id == "kimi-code").unwrap();
        agent.validate().unwrap();
        assert_eq!(agent.home_paths.unwrap()[0].path, "~/.kimi-code");
        assert_eq!(agent.host_login, ["login"]);
        assert_eq!(agent.home_env.as_deref(), Some("KIMI_CODE_HOME"));
    }

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
    fn an_icon_is_found_by_id_whatever_version_is_listed() {
        let dir = std::env::temp_dir().join(format!("sill-reg-icon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        // listed nowhere and nothing cached: none, and nothing fetched
        assert_eq!(icon_by_id(&home, "no-such-agent"), None);
        // cached: there, with no listing of that version or of the agent at all
        let icons = icon_cache_dir(&home);
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::write(icons.join("no-such-agent.svg"), "<svg/>").unwrap();
        assert_eq!(icon_by_id(&home, "no-such-agent"), Some(icons.join("no-such-agent.svg")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn merged_marks_live_entries() {
        let dir = std::env::temp_dir().join(format!("sill-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let cache = cache_path(&home);
        let (entries, when) = merged(&home);
        assert!(when.is_none());
        assert!(entries.iter().any(|e| e.id == "opencode"));
        assert!(!entries.iter().any(|e| e.id == "opencode-v2"));
        assert!(!entries.iter().any(|e| e.id == "kimi"));
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        let snap = snapshot();
        let known = &snap.agents[0];
        let live = serde_json::json!({ "version": "1", "agents": [
            { "id": known.id, "name": known.name, "version": "999.0.0", "icon": "https://example/x.svg", "distribution": { "npx": { "package": "x@999.0.0" } } },
            { "id": "brand-new", "name": "New", "version": "1.0.0", "distribution": { "npx": { "package": "new@1.0.0" } } },
            { "id": "opencode-v2", "name": "OpenCode V2", "version": "999.0.0" },
            { "id": "kimi", "name": "Kimi CLI", "version": "999.0.0" },
            { "id": "kimi-code", "name": "Kimi Code", "version": "1.0.0" }
        ]});
        std::fs::write(&cache, live.to_string()).unwrap();
        let (entries, when) = merged(&home);
        assert!(when.is_some());
        assert!(entries.iter().any(|e| e.id == "kimi-code"));
        assert_eq!(entries.len(), snap.agents.iter().filter(|e| !matches!(e.id.as_str(), "opencode-v2" | "kimi")).count() + 2);
        assert!(entries.iter().any(|e| e.id == "opencode"));
        assert!(!entries.iter().any(|e| e.id == "opencode-v2"));
        assert!(!entries.iter().any(|e| e.id == "kimi"));
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

#[cfg(test)]
mod jail_base_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sill-jail-base-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    fn argv(b: &Command) -> String {
        b.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ") + " "
    }

    #[test]
    fn a_linked_program_brings_its_package_and_not_the_home() {
        let home = home("linked");
        let package = home.join(".npm-global/lib/node_modules/@someone/agent");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        std::fs::write(package.join("bin/agent.js"), "#!/usr/bin/env node\n").unwrap();
        let bin = home.join(".npm-global/bin");
        std::fs::create_dir_all(&bin).unwrap();
        symlink("../lib/node_modules/@someone/agent/bin/agent.js", bin.join("agent")).unwrap();
        let base = jail_base(bin.join("agent").to_str().unwrap(), &home, None, true).unwrap();
        let r = argv(&base.b);
        assert!(r.contains(&format!("--ro-bind {d} {d} ", d = bin.display())), "{r}");
        assert!(r.contains(&format!("--ro-bind {d} {d} ", d = package.display())), "the link's target is in the jail: {r}");
        assert!(!r.contains(&format!("--ro-bind {h} {h} ", h = home.display())), "{r}");
        assert!(base.bound.contains(&PathBuf::from(".npm-global/lib/node_modules/@someone/agent")), "{:?}", base.bound);
        assert!(std::env::split_paths(&base.path).any(|d| d == bin) && !std::env::split_paths(&base.path).any(|d| d == package), "{:?}", base.path);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_program_directly_in_the_home_is_refused_instead_of_showing_the_home() {
        let home = home("in-home");
        std::fs::write(home.join("agent"), "").unwrap();
        let Err(e) = jail_base(home.join("agent").to_str().unwrap(), &home, None, true) else { panic!("the home would be bound") };
        assert!(format!("{e:#}").contains("a folder of its own"), "{e:#}");
        // a link from a folder of its own to a file in the home: the folder, never the home
        std::fs::create_dir_all(home.join("bin")).unwrap();
        symlink("../agent", home.join("bin/agent")).unwrap();
        let base = jail_base(home.join("bin/agent").to_str().unwrap(), &home, None, true).unwrap();
        assert!(!argv(&base.b).contains(&format!("--ro-bind {h} {h} ", h = home.display())));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_persons_files_outside_the_home_are_covered() {
        let home = home("covered");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin/agent"), "").unwrap();
        let r = argv(&jail_base(home.join("bin/agent").to_str().unwrap(), &home, None, true).unwrap().b);
        for root in crate::mounts::DATA_ROOTS {
            let covered = r.contains(&format!("--tmpfs {root} "));
            assert_eq!(covered, Path::new(root).symlink_metadata().is_ok_and(|m| m.is_dir()), "{root}: {r}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_links_target_root_is_its_package() {
        let home = home("roots");
        let file = |rel: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        };
        let link = |target: &Path, rel: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            symlink(target, &p).unwrap();
            p
        };
        assert_eq!(link_target_root(&file("plain/bin/agent"), &home), None, "no link, nothing more to bind");
        file("pipx/venvs/agent/pyvenv.cfg");
        let venv = link(&file("pipx/venvs/agent/bin/agent"), "links/venv");
        assert_eq!(link_target_root(&venv, &home), Some(home.join("pipx/venvs/agent")));
        let python = link(&file("share/uv/python/cpython-3/bin/python3.12"), "links/python3");
        assert_eq!(link_target_root(&python, &home), Some(home.join("share/uv/python/cpython-3")));
        let shallow = link(&file("local/bin/tool"), "links/tool");
        assert_eq!(link_target_root(&shallow, &home), Some(home.join("local/bin")), "a prefix right under the home is too much");
        let plain = link(&file("opt/agent/agent"), "links/agent");
        assert_eq!(link_target_root(&plain, &home), Some(home.join("opt/agent")));
        let unscoped = link(&file("lib/node_modules/agent/dist/cli.js"), "links/cli");
        assert_eq!(link_target_root(&unscoped, &home), Some(home.join("lib/node_modules/agent")));
        let _ = std::fs::remove_dir_all(&home);
    }
}
