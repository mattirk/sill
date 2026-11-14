//! Session lifecycle on the host side (SPEC §4.7): a session is a transient systemd user unit running
//! `sill launch`, reachable through its shim socket. Starting is idempotent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::policy::Policy;
use crate::shim;

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub session: String,
    pub policy: String,
    pub cwd: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub unsafe_state: bool,
    pub share_net: bool,
    pub env: Vec<String>,
    pub shim_bin: Option<PathBuf>,
    /// Registry id of the agent the session runs: its domains join the proxy allow-list.
    pub agent: Option<String>,
    /// Extra read-write binds (host, jail), see [`binds_for`].
    pub binds: Vec<(PathBuf, PathBuf)>,
}

/// The binds a session needs beyond the policy's mounts: the agent's sill-owned home paths and the
/// session-private scratch directories. Creates what is missing (seeding the agent home from the host
/// on first use, login files only).
/// The sill-owned home of an agent: `~/.local/state/sill/agents/<id>/`, holding the jail's copy of
/// each `home_paths` entry under its basename (`.codex`, `.claude`, `.claude.json`).
pub fn agent_home(home: &Path, agent_id: &str) -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("agents")
        .join(agent_id)
}

pub fn binds_for(
    agent: &crate::agents::Agent,
    policy: &Policy,
    state_dir: &Path,
    home: &Path,
    pins: &[PathBuf],
) -> Result<Vec<(PathBuf, PathBuf)>> {
    use crate::paths::{expand_home, expand_pattern, layer_name};
    let mut out = Vec::new();
    // agent home: ~/.local/state/sill/agents/<id>/<basename of the path>
    let agents_base = agent_home(home, &agent.id);
    for p in &agent.home_paths {
        let inside = expand_home(p, home);
        let name = inside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let owned = agents_base.join(&name);
        let host_is_dir = inside.is_dir();
        if !owned.exists() {
            std::fs::create_dir_all(&agents_base)?;
            if host_is_dir || !inside.exists() {
                std::fs::create_dir_all(&owned)?;
            } else {
                // a file path (~/.claude.json): seed from the host or start empty
                let content = std::fs::read(&inside).unwrap_or_else(|_| b"{}".to_vec());
                std::fs::write(&owned, content)?;
            }
            for seed in &agent.seed_files {
                let src = expand_home(seed, home);
                if let Ok(rel) = src.strip_prefix(&inside)
                    && owned.is_dir()
                    && src.is_file()
                {
                    let dst = owned.join(rel);
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
        // bwrap needs the mount point to exist inside: a missing ~/.claude.json would be created as
        // a copy-up in the overlay, which is harmless (it is skipped by the ledger as a bind)
        out.push((owned, inside));
    }
    // the login file shared with the host (bound after the home so it wins)
    for p in &agent.shared_files {
        let inside = expand_home(p, home);
        if inside.is_file() {
            out.push((inside.clone(), inside));
        }
    }
    // pinned folders: live, host = jail (git or not)
    for p in pins {
        if p.is_dir() && !out.iter().any(|(_, i)| i == p) {
            out.push((p.clone(), p.clone()));
        }
    }
    // session-private scratch
    for pat in &policy.paths.private {
        for inside in expand_pattern(pat, home).into_iter().chain(std::iter::once(expand_home(pat, home))) {
            if out.iter().any(|(_, i)| *i == inside) {
                continue;
            }
            let scratch = state_dir.join("private").join(layer_name(&inside));
            std::fs::create_dir_all(&scratch)?;
            out.push((scratch, inside));
        }
    }
    Ok(out)
}

/// Where a session's upper trees live on the state filesystem (SPEC §4.9); created 0700 for the user.
pub fn state_dir(uid: u32, session: &str) -> Result<PathBuf> {
    let state_fs = Path::new(crate::jail::STATE_FS);
    let all = crate::mounts::host_mounts()?;
    if !crate::mounts::is_mount_point(&all, state_fs) {
        bail!("{} is not mounted (install the sill state units, SPEC §4.9)", state_fs.display());
    }
    let mine = state_fs.join(uid.to_string());
    if !mine.is_dir() {
        std::fs::create_dir(&mine).with_context(|| format!("create {}", mine.display()))?;
    }
    std::fs::set_permissions(&mine, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(mine.join("sessions").join(session))
}

/// Remove a session's upper trees (needs a user namespace: overlayfs creates root-owned work dirs).
pub fn remove_state(uid: u32, session: &str) -> Result<()> {
    let dir = state_dir(uid, session)?;
    if dir.is_dir() {
        let st = std::process::Command::new("unshare").args(["-Ur", "rm", "-rf"]).arg(&dir).status()?;
        if !st.success() {
            bail!("rm -rf {} failed", dir.display());
        }
    }
    Ok(())
}

pub fn unit_name(session: &str) -> String {
    format!("sill-{session}")
}

pub fn socket_path(session: &str) -> Result<PathBuf> {
    Ok(shim::runtime_dir(session)?.join("shim.sock"))
}

pub fn is_running(session: &str) -> bool {
    socket_path(session).map(|p| shim::Control::connect(&p).is_ok()).unwrap_or(false)
}

/// Start the session unit with the policy's limits, unless it is already reachable.
/// A sill-owned copy of this binary and its `sill-shim`, taken once per process, for everything the
/// host later executes on a session's behalf (`<exe> launch` in the unit, the shim bound into the jail).
/// `current_exe()` may sit in a pinned folder (developing sill in sill): the agent can write there, and
/// the unit would re-read the file from disk at every session start (SPEC §4.11 #1). The copy is taken
/// when the person started the shelf, which is the moment they chose to run this build.
pub fn staged_exe() -> Result<PathBuf> {
    static STAGED: std::sync::OnceLock<std::result::Result<PathBuf, String>> = std::sync::OnceLock::new();
    STAGED
        .get_or_init(|| stage_exe().map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow::anyhow!("staging the launcher: {e}"))
}

fn stage_exe() -> Result<PathBuf> {
    let src = std::env::current_exe()?;
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"))
        .join("sill")
        .join("bin");
    std::fs::create_dir_all(&base)?;
    // one directory per shelf process: units started by an earlier shelf keep running their copy;
    // directories of shelves that are gone are removed here
    let me = std::process::id();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            if pid != me && !Path::new("/proc").join(name).exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let dir = base.join(me.to_string());
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join("sill");
    std::fs::copy(&src, &dst).with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    let shim = src.with_file_name("sill-shim");
    if shim.is_file() {
        std::fs::copy(&shim, dir.join("sill-shim"))?;
    }
    Ok(dst)
}

/// Whether `path` is inside one of `roots` (after resolving symlinks), for warning when the binary
/// about to run on the host sits in a folder some session's agent can write.
pub fn inside_any(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let p = path.canonicalize().ok()?;
    roots.iter().find(|r| r.canonicalize().map(|r| p.starts_with(r)).unwrap_or(false)).cloned()
}

pub fn ensure_started(opts: &StartOptions, home: &Path, exe: &Path) -> Result<PathBuf> {
    let sock = socket_path(&opts.session)?;
    if shim::Control::connect(&sock).is_ok() {
        return Ok(sock);
    }
    let policy = Policy::load(&opts.policy, home)?;
    let unit = unit_name(&opts.session);
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--unit", &unit, "--collect", "--quiet", "--same-dir"])
        .arg(format!("--property=CPUWeight={}", policy.limits.cpu_weight))
        .arg(format!("--property=IOWeight={}", policy.limits.io_weight))
        .arg(format!("--property=MemoryHigh={}", policy.limits.memory_high))
        .arg(format!("--property=MemoryMax={}", policy.limits.memory_max))
        .arg(format!("--property=TasksMax={}", policy.limits.tasks_max))
        .arg("--property=KillMode=control-group")
        // a stop must not hang on an interactive shell in the jail (bash ignores SIGTERM): SIGKILL
        // follows after a short grace; the agent itself was already asked to leave by the shim
        .arg("--property=TimeoutStopSec=5")
        .args(["--setenv=HOME", "--setenv=USER", "--setenv=TERM", "--setenv=LANG"])
        .arg("--")
        .arg(exe)
        .args(["launch", "--session", &opts.session, "--policy", &opts.policy])
        .arg("--cwd")
        .arg(&opts.cwd);
    if let Some(d) = &opts.state_dir {
        cmd.arg("--state-dir").arg(d);
    }
    if opts.unsafe_state {
        cmd.arg("--unsafe-state");
    }
    if opts.share_net {
        cmd.arg("--net");
    }
    for e in &opts.env {
        cmd.arg("--env").arg(e);
    }
    if let Some(s) = &opts.shim_bin {
        cmd.arg("--shim").arg(s);
    }
    if let Some(a) = &opts.agent {
        cmd.arg("--agent").arg(a);
    }
    for (host, inside) in &opts.binds {
        cmd.arg("--bind").arg(format!("{}={}", host.display(), inside.display()));
    }
    let st = cmd.status().context("run systemd-run (is the systemd user instance running?)")?;
    if !st.success() {
        bail!("systemd-run failed ({st})");
    }
    for _ in 0..50 {
        if shim::Control::connect(&sock).is_ok() {
            return Ok(sock);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("unit {unit} started but the shim socket did not appear; see journalctl --user -u {unit}")
}

pub fn stop(session: &str) -> Result<()> {
    if let Ok(sock) = socket_path(session)
        && let Ok(mut c) = shim::Control::connect(&sock)
    {
        let _ = c.kill(5000);
    }
    let unit = format!("{}.service", unit_name(session));
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if active {
        std::process::Command::new("systemctl").args(["--user", "stop", &unit]).status()?;
    }
    Ok(())
}
