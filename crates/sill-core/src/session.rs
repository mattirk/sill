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
