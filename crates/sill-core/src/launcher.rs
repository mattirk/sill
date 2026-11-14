//! Launcher modes (SPEC §4.7): how a session's `sill launch` process is started, limited, stopped
//! and logged. The systemd user instance is the preferred launcher, not a prerequisite.
//!
//! - `unit`: `systemd-run --user --unit sill-<id> …`; limits are unit properties, logs go to the
//!   journal, stop is `systemctl --user stop`.
//! - `direct`: `<exe> launch …` detached (setsid, double fork, fds closed); limits through an own
//!   cgroup under a delegated subtree when one exists (`sill init`, SPEC §4.12), logs under
//!   `$XDG_RUNTIME_DIR/sill/<id>/`, stop is `cgroup.kill` or SIGKILL to bwrap.
//!
//! The mode is probed once per process ([`current`]); `SILL_LAUNCHER=unit|direct` forces it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::policy::Limits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Unit,
    Direct,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Unit => "unit",
            Mode::Direct => "direct",
        })
    }
}

/// What this process found out about the host, once.
#[derive(Debug, Clone)]
pub struct Launcher {
    pub mode: Mode,
    /// Why this mode (one line, for the Settings page and the session note).
    pub reason: String,
    /// `direct` only: the writable delegated cgroup subtree for this uid, if any.
    pub cgroup_root: Option<PathBuf>,
}

impl Launcher {
    /// Whether sessions get resource limits in this mode.
    pub fn has_limits(&self) -> bool {
        match self.mode {
            Mode::Unit => true,
            Mode::Direct => self.cgroup_root.is_some(),
        }
    }

    /// One line for the Settings page.
    pub fn describe(&self) -> String {
        match (self.mode, &self.cgroup_root) {
            (Mode::Unit, _) => format!("unit — sessions run as transient systemd user units ({})", self.reason),
            (Mode::Direct, Some(c)) => format!("direct — sessions run detached ({}); limits via cgroup {}", self.reason, c.display()),
            (Mode::Direct, None) => format!(
                "direct — sessions run detached ({}); no resource limits: a busy agent can starve the host (run `sudo sill init` to delegate a cgroup subtree)",
                self.reason
            ),
        }
    }
}

/// The launcher this process uses; probed on first call.
pub fn current() -> &'static Launcher {
    static L: OnceLock<Launcher> = OnceLock::new();
    L.get_or_init(probe)
}

fn probe() -> Launcher {
    let forced = std::env::var("SILL_LAUNCHER").ok();
    let unit_usable = unit_usable();
    let mode = match forced.as_deref() {
        Some("unit") => Mode::Unit,
        Some("direct") => Mode::Direct,
        _ => match unit_usable {
            Ok(()) => Mode::Unit,
            Err(_) => Mode::Direct,
        },
    };
    let reason = match (forced.as_deref(), &unit_usable) {
        (Some(f), _) if f == "unit" || f == "direct" => format!("SILL_LAUNCHER={f}"),
        (_, Ok(())) => "systemd user instance reachable".into(),
        (_, Err(e)) => e.clone(),
    };
    let cgroup_root = match mode {
        Mode::Unit => None,
        Mode::Direct => delegated_subtree(),
    };
    Launcher { mode, reason, cgroup_root }
}

/// `systemd-run --user` works when `$XDG_RUNTIME_DIR` is set and the user manager answers.
fn unit_usable() -> std::result::Result<(), String> {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err("XDG_RUNTIME_DIR is not set".into());
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return Err("systemd is not the init system".into());
    }
    let out = std::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Err(e) => Err(format!("systemctl not found ({e})")),
        Ok(o) => {
            let state = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // "running" and "degraded" both accept systemd-run; "offline"/"unknown"/empty do not
            if matches!(state.as_str(), "running" | "degraded" | "starting" | "initializing" | "maintenance") {
                Ok(())
            } else {
                Err(format!("systemd user instance not reachable ({})", if state.is_empty() { "no answer" } else { &state }))
            }
        }
    }
}

/// Root of the cgroup v2 hierarchy.
pub const CGROUP_FS: &str = "/sys/fs/cgroup";

/// The delegated subtree `sill init` creates (SPEC §4.12 step 4): `/sys/fs/cgroup/sill/<uid>`,
/// owned by that user with `cpu io memory pids` enabled in the parent's `subtree_control`.
pub fn delegated_subtree() -> Option<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let dir = Path::new(CGROUP_FS).join("sill").join(uid.to_string());
    let writable = std::fs::metadata(&dir).ok().map(|m| m.is_dir()).unwrap_or(false)
        && rustix::fs::access(&dir, rustix::fs::Access::WRITE_OK).is_ok();
    writable.then_some(dir)
}

/// Parse a size as systemd writes it (`8G`, `512M`, `12288K`, bytes, `max`/`infinity`) into what
/// cgroup files accept (bytes or `max`).
pub fn cgroup_size(s: &str) -> Result<String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("max") || s.eq_ignore_ascii_case("infinity") {
        return Ok("max".into());
    }
    let (num, mult): (&str, u64) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().with_context(|| format!("size {s:?}"))?;
    Ok(((v * mult as f64) as u64).to_string())
}

/// Create (or reuse) the session cgroup `sill-<id>` under the delegated subtree, write the policy's
/// limits into it and return its path. Files whose controller is not enabled are skipped with a
/// warning line returned in `warnings`.
pub fn make_session_cgroup(root: &Path, session: &str, limits: &Limits, warnings: &mut Vec<String>) -> Result<PathBuf> {
    let dir = root.join(format!("sill-{session}"));
    if !dir.is_dir() {
        std::fs::create_dir(&dir).with_context(|| format!("create cgroup {}", dir.display()))?;
    }
    let mut set = |file: &str, value: String| {
        if let Err(e) = std::fs::write(dir.join(file), &value) {
            warnings.push(format!("cgroup {file}={value} not applied ({e}); controller not delegated?"));
        }
    };
    set("cpu.weight", limits.cpu_weight.to_string());
    set("io.weight", format!("default {}", limits.io_weight));
    set("memory.high", cgroup_size(&limits.memory_high)?);
    set("memory.max", cgroup_size(&limits.memory_max)?);
    set("pids.max", limits.tasks_max.to_string());
    Ok(dir)
}

/// Kill everything in a session cgroup (`cgroup.kill`, Linux ≥ 5.14) and remove it. Ok(false)
/// when there is no such cgroup.
pub fn kill_session_cgroup(root: &Path, session: &str) -> Result<bool> {
    let dir = root.join(format!("sill-{session}"));
    if !dir.is_dir() {
        return Ok(false);
    }
    std::fs::write(dir.join("cgroup.kill"), "1").with_context(|| format!("cgroup.kill in {}", dir.display()))?;
    for _ in 0..50 {
        let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
        if procs.trim().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir(&dir);
    Ok(true)
}

/// Spawn `cmd` detached from this process: its own session (setsid), double-forked so nobody has
/// to reap it, stdin from /dev/null, stdout/stderr on `log`, every other fd closed (Rust marks
/// them close-on-exec). Returns after the intermediate child exited; the grandchild runs on.
pub fn spawn_detached(mut cmd: std::process::Command, log: std::fs::File) -> Result<()> {
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: only async-signal-safe calls (setsid, fork, _exit) between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            match libc_fork() {
                0 => Ok(()),          // grandchild: goes on to exec
                -1 => Err(std::io::Error::last_os_error()),
                _ => libc_exit(0),    // intermediate: gone at once, the grandchild is init's
            }
        });
    }
    let mut child = cmd.spawn().context("spawn")?;
    let st = child.wait()?;
    if !st.success() {
        bail!("detached start failed ({st})");
    }
    Ok(())
}

// Tiny libc shims: rustix has no fork (by design); these two are all that is needed here.
unsafe extern "C" {
    fn fork() -> i32;
    fn _exit(code: i32) -> !;
}
fn libc_fork() -> i32 {
    // SAFETY: called between fork and exec in `pre_exec`, where fork is allowed.
    unsafe { fork() }
}
fn libc_exit(code: i32) -> ! {
    // SAFETY: plain _exit.
    unsafe { _exit(code) }
}

/// Whether the pid named in `file` is still alive.
pub fn pid_file_alive(file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let Ok(pid) = text.trim().parse::<u32>() else { return false };
    Path::new("/proc").join(pid.to_string()).is_dir()
}

/// Send SIGKILL to the pid read from a file and to its direct children; true if a signal was
/// sent. bwrap is two processes: the outer one that waits, and its child, pid 1 of the jail's
/// namespace — the one whose death takes everything in the jail with it.
pub fn kill_pid_file(file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let Ok(pid) = text.trim().parse::<i32>() else { return false };
    let mut targets = children_of(pid);
    targets.push(pid);
    let mut sent = false;
    for p in targets {
        if let Some(p) = rustix::process::Pid::from_raw(p) {
            sent |= rustix::process::kill_process(p, rustix::process::Signal::KILL).is_ok();
        }
    }
    sent
}

/// Pids whose parent is `pid` (from `/proc/*/stat`).
fn children_of(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(child) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // "pid (comm) state ppid …": comm may contain spaces, so split after the last ')'
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { continue };
        let ppid = rest.split_whitespace().nth(1).and_then(|p| p.parse::<i32>().ok());
        if ppid == Some(pid) {
            out.push(child);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(cgroup_size("8G").unwrap(), (8u64 << 30).to_string());
        assert_eq!(cgroup_size("512M").unwrap(), (512u64 << 20).to_string());
        assert_eq!(cgroup_size("1.5G").unwrap(), ((1.5 * (1u64 << 30) as f64) as u64).to_string());
        assert_eq!(cgroup_size("12345").unwrap(), "12345");
        assert_eq!(cgroup_size("max").unwrap(), "max");
        assert!(cgroup_size("lots").is_err());
    }
}
