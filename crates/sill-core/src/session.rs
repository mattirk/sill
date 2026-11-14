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
    /// Extra read-write binds (host, jail), see [`binds_for`].
    pub binds: Vec<(PathBuf, PathBuf)>,
}

/// The binds a session needs beyond the policy's mounts: the agent's sill-owned home paths and the
/// session-private scratch directories. Creates what is missing (seeding the agent home from the host
/// on first use, login files only).
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
    let agents_base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("agents")
        .join(&agent.id);
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
