//! The ACP agent registry (SPEC §4.1): the index published by the protocol project, cached under
//! `~/.cache/sill/registry.json`, and installs from it into a sill-owned tools directory
//! (`~/.local/state/sill/tools/<id>/`) that never touches the person's own npm/uv/bin setup. An
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

#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agents: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<PackageDist>,
    #[serde(default)]
    pub uvx: Option<PackageDist>,
    #[serde(default)]
    pub binary: Option<BTreeMap<String, BinaryDist>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageDist {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
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
}

fn strip_version(spec: &str) -> String {
    match spec.rfind('@') {
        Some(i) if i > 0 => spec[..i].to_string(),
        _ => spec.to_string(),
    }
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

fn cache_dir(home: &Path) -> PathBuf {
    std::env::var("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".cache")).join("sill")
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
    let first = agent.command.first()?;
    let p = Path::new(first);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    dirs.push(home.join(".local/bin"));
    dirs.into_iter().map(|d| d.join(first)).find(|c| c.is_file())
}

// --- install ---------------------------------------------------------------------------------------

/// Install an entry into the tools dir and register it; `log` gets the installer's output lines.
pub fn install(home: &Path, entry: &Entry, log: &mut dyn FnMut(String)) -> Result<Agent> {
    let dir = tools_dir(home).join(&entry.id);
    std::fs::create_dir_all(&dir)?;
    let how = entry.how().ok_or_else(|| anyhow!("no way to install {} on this platform", entry.name))?;
    let (command, env) = match how {
        "npx" => install_npm(&dir, entry.distribution.npx.as_ref().unwrap(), log)?,
        "uvx" => install_uv(&dir, entry.distribution.uvx.as_ref().unwrap(), log)?,
        _ => install_binary(&dir, &entry.distribution.binary.as_ref().unwrap()[platform_key()], log)?,
    };
    let icon = fetch_icon(home, entry);
    let supports_load = probe_load_session(&command, &env, home).unwrap_or(false);
    log(format!("probe: {}", if supports_load { "resumes sessions" } else { "no session resume"}));

    let installed = Installed { version: entry.version.clone(), kind: how.to_string(), command: command.clone(), env: env.clone() };
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

fn install_npm(dir: &Path, d: &PackageDist, log: &mut dyn FnMut(String)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    // no lifecycle scripts: nothing from the package runs on the host at install time
    let mut cmd = Command::new("npm");
    cmd.args(["install", "--prefix"])
        .arg(dir)
        .args(["--no-audit", "--no-fund", "--ignore-scripts", "--loglevel=error", "--no-package-lock"])
        .arg(&d.package)
        .current_dir(dir);
    run_logged(&mut cmd, log).context("npm install")?;
    // npm treats a failed fetch of an optional dependency as no error and says nothing at
    // --loglevel=error; the platform packages that carry an agent's native binary are optional
    // (`@anthropic-ai/claude-agent-sdk-linux-x64`, ~50 MB), so one dropped download leaves an
    // install that starts fine and fails at the first session. Verify, retry once, then refuse.
    let mut missing = missing_platform_optionals(dir)?;
    if !missing.is_empty() {
        log(format!("optional platform package(s) not installed: {}; retrying", missing.join(", ")));
        run_logged(&mut cmd, log).context("npm install (retry)")?;
        missing = missing_platform_optionals(dir)?;
    }
    if !missing.is_empty() {
        bail!("platform package(s) missing after npm install: {}", missing.join(", "));
    }
    let name = strip_version(&d.package);
    let pkg_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("node_modules").join(&name).join("package.json")).context("package.json")?)?;
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
    let mut cmd = Command::new("uv");
    cmd.args(["tool", "install", "--force"])
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
    if let Some(want) = &d.sha256 {
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

/// Ask a freshly installed agent what it can do: `initialize` over its stdio with a short deadline.
/// Returns whether it advertises `loadSession`.
fn probe_load_session(command: &[String], env: &BTreeMap<String, String>, home: &Path) -> Result<bool> {
    let Some(prog) = command.first() else { bail!("no command") };
    let mut child = Command::new(prog)
        .args(&command[1..])
        .envs(env)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
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
