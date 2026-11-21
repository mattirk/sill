//! How a session's `silli launch` process is started, limited, stopped and logged (SPEC §4.7,
//! docs/daemon.md §3.1 decision 10): a **child of the daemon**, in a cgroup of its own.
//!
//! - With a systemd user manager the child runs in a transient **scope** `silli-<id>.scope` under
//!   `silli.slice` (`systemd-run --user --scope`: systemd makes the cgroup, writes the policy's
//!   limits into it, and the command is exec'd in place — the pid is ours to wait on). The slice is
//!   the aggregate: `systemctl --user set-property silli.slice MemoryMax=…` caps every session at
//!   once. The daemon's own unit stays a plain cgroup, so it restarts — a delegated unit that had
//!   enabled controllers for session cgroups could never be entered again (cgroup v2's
//!   no-internal-processes rule), which is why the first cut of decision 10 was changed.
//! - Without one, the session cgroup is made by hand under the subtree `silli init` creates
//!   (SPEC §4.12); with neither there are no limits and the policy's `[limits] required` decides.
//! - Logs: one journald stream per session (`journalctl --user -t silli-<id>`) where journald is,
//!   files under `$XDG_RUNTIME_DIR/silli/<id>/` otherwise.
//! - Stop is the scope's stop (or `cgroup.kill`), or SIGKILL to bwrap when there is no cgroup.
//!
//! The daemon is the only process that starts sessions; `silli dev start` from a terminal uses the
//! same code. `SILLI_LAUNCHER=direct` skips the scopes, to try the no-systemd path on a host with it.
//! `SILLI_LAUNCHER=attach` is for the daemon's tests: no jail is launched at all, and a session
//! waits for a shim the test provides on its socket (`Hub::start_session`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::policy::Limits;

/// What this process found out about the host, once.
#[derive(Debug, Clone)]
pub struct Launcher {
    /// Sessions run in transient scopes made by the systemd user manager.
    pub scopes: bool,
    /// Without scopes: where session cgroups are made by hand; None = no limits possible here.
    pub cgroup_root: Option<PathBuf>,
    /// Which case, one line (the Settings page, the session note).
    pub reason: String,
    /// Session logs go to journald streams; files otherwise.
    pub journal: bool,
}

impl Launcher {
    /// Whether sessions get resource limits in this mode.
    pub fn has_limits(&self) -> bool {
        self.scopes || self.cgroup_root.is_some()
    }

    /// One line for the Settings page and `silli daemon status`.
    pub fn describe(&self) -> String {
        let logs = if self.journal { "logs in the journal (journalctl --user -t silli-<id>)" } else { "logs under $XDG_RUNTIME_DIR/silli/<id>/" };
        if self.scopes {
            return format!("sessions are children of the daemon in scopes silli-<id>.scope under silli.slice, limits as scope properties; {logs}");
        }
        match &self.cgroup_root {
            Some(c) => format!("sessions are children of the daemon, limits in cgroups under {} ({}); {}", c.display(), self.reason, logs),
            None => format!(
                "sessions are children of the daemon with no resource limits ({}): a busy agent can starve the host — `sudo silli init --delegate-cgroup`; {}",
                self.reason, logs
            ),
        }
    }
}

static L: OnceLock<Launcher> = OnceLock::new();

/// The launcher this process uses; probed on first call (nothing is written).
pub fn current() -> &'static Launcher {
    L.get_or_init(probe)
}

fn probe() -> Launcher {
    let journal = Path::new(JOURNAL_STREAM).exists() && std::os::unix::net::UnixStream::connect(JOURNAL_STREAM).is_ok();
    // `SILLI_LAUNCHER=direct` (`silli dev start --direct`): no scopes even where the user manager
    // answers — how the path of a host without systemd is tried on one with it
    let forced = std::env::var("SILLI_LAUNCHER").is_ok_and(|v| v == "direct");
    let systemd = if forced { Err("SILLI_LAUNCHER=direct".to_string()) } else { systemd_user_usable() };
    match systemd {
        Ok(()) => Launcher { scopes: true, cgroup_root: None, reason: "systemd user instance reachable".into(), journal },
        Err(why) => match delegated_subtree() {
            Some(root) => Launcher { scopes: false, cgroup_root: Some(root), reason: format!("{why}; silli init subtree"), journal },
            None => Launcher { scopes: false, cgroup_root: None, reason: format!("{why}; no silli init subtree"), journal },
        },
    }
}

/// The scope a session runs in (`silli-<id>.scope`).
pub fn scope_name(unit: &str) -> String {
    format!("{unit}.scope")
}

/// The `systemd-run --user --scope` prefix that puts a child in `silli-<id>.scope` under
/// `silli.slice` with the policy's limits; the command follows `--`.
pub fn scope_command(unit: &str, limits: &Limits) -> std::process::Command {
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--scope", "--quiet", "--collect", "--unit", unit, "--slice=silli.slice"])
        .arg(format!("--property=CPUWeight={}", limits.cpu_weight))
        .arg(format!("--property=IOWeight={}", limits.io_weight))
        .arg(format!("--property=MemoryHigh={}", limits.memory_high))
        .arg(format!("--property=MemoryMax={}", limits.memory_max))
        .arg(format!("--property=TasksMax={}", limits.tasks_max))
        // a stop must not hang on an interactive shell in the jail (bash ignores SIGTERM)
        .arg("--property=TimeoutStopSec=5")
        .arg("--");
    cmd
}

/// A scope of that name is loaded (active, or failed and not yet forgotten).
pub fn scope_loaded(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "cat", &scope_name(unit)])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stop a session's scope — kills its whole cgroup — and forget a failed one, so the name is free
/// for the next start. Ok(true) when a scope was there.
pub fn stop_scope(unit: &str) -> Result<bool> {
    let name = scope_name(unit);
    if !scope_loaded(unit) {
        return Ok(false);
    }
    let _ = std::process::Command::new("systemctl").args(["--user", "stop", "--quiet", &name]).stdin(std::process::Stdio::null()).status();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "reset-failed", "--quiet", &name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(true)
}

/// `systemd-run --user` works: the user manager answers and `$XDG_RUNTIME_DIR` is set. What
/// `silli init --user` and the client's daemon start use to choose a transient unit over a bare spawn.
pub fn systemd_user_usable() -> std::result::Result<(), String> {
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
            if matches!(state.as_str(), "running" | "degraded" | "starting" | "initializing" | "maintenance") {
                Ok(())
            } else {
                Err(format!("systemd user instance not reachable ({})", if state.is_empty() { "no answer" } else { &state }))
            }
        }
    }
}

/// journald's stream socket: a connection with a header is a log stream with its own identifier.
pub const JOURNAL_STREAM: &str = "/run/systemd/journal/stdout";

/// A journald stream for one session's output: `journalctl --user -t <identifier>` reads it.
/// `priority` is the syslog level of lines without a `<n>` prefix (6 = info; a ghost session logs
/// at 4, warning). The caller hands the stream to the child as stdout and stderr.
pub fn journal_stream(identifier: &str, priority: u8) -> Result<std::os::unix::net::UnixStream> {
    use std::io::Write;
    let mut s = std::os::unix::net::UnixStream::connect(JOURNAL_STREAM).context("journald stream socket")?;
    // identifier, unit id (none: journald takes the connecting cgroup's), priority, level prefix,
    // forward to syslog / kmsg / console
    s.write_all(format!("{identifier}\n\n{priority}\n0\n0\n0\n0\n").as_bytes())?;
    Ok(s)
}

/// Spawn `cmd` as a child of this process in its own session (no terminal hangup reaches it, and
/// it is nobody's job), stdin from /dev/null, stdout and stderr on `log`. The caller keeps the
/// `Child` and waits on it: the daemon's supervision.
pub fn spawn_child(mut cmd: std::process::Command, log: std::os::unix::net::UnixStream) -> Result<std::process::Child> {
    // Stdio takes an OwnedFd; a unix stream is one
    let out: std::os::fd::OwnedFd = log.try_clone()?.into();
    let err: std::os::fd::OwnedFd = log.into();
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::from(out)).stderr(std::process::Stdio::from(err));
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    cmd.spawn().context("spawn")
}

/// A session's log sink when there is no journald: the files in its runtime dir, as one stream
/// (a socketpair whose other end a thread copies into the file — so the child sees one fd kind
/// either way).
pub fn file_stream(path: &Path) -> Result<std::os::unix::net::UnixStream> {
    use std::io::{Read, Write};
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let mut file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut ours = ours;
    std::thread::Builder::new().name("silli-log-copy".into()).spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = ours.read(&mut buf) {
            if n == 0 || file.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    })?;
    Ok(theirs)
}

/// Wait until `pid` is gone, for a process that is not our child (an adopted session, docs/daemon.md
/// §3.6): a pidfd becomes readable at exit. Returns at once when the pid is already gone.
pub fn wait_pid_gone(pid: u32) -> Result<()> {
    let Some(p) = rustix::process::Pid::from_raw(pid as i32) else { bail!("pid {pid}") };
    let fd = match rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(e) => return Err(e).context("pidfd_open"),
    };
    let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e).context("poll pidfd"),
        }
    }
}

/// Root of the cgroup v2 hierarchy.
pub const CGROUP_FS: &str = "/sys/fs/cgroup";

/// The delegated subtree `silli init` creates (SPEC §4.12 step 4): `/sys/fs/cgroup/silli/<uid>`,
/// owned by that user with `cpu io memory pids` enabled in the parent's `subtree_control`.
pub fn delegated_subtree() -> Option<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let dir = Path::new(CGROUP_FS).join("silli").join(uid.to_string());
    let writable = std::fs::metadata(&dir).ok().map(|m| m.is_dir()).unwrap_or(false) && rustix::fs::access(&dir, rustix::fs::Access::WRITE_OK).is_ok();
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

/// Create (or reuse) the session cgroup `silli-<id>` under the delegated subtree, write the policy's
/// limits into it and return its path. Required limits fail closed if any controller write fails;
/// optional limits report unavailable controllers through `warnings`.
pub fn make_session_cgroup(root: &Path, session: &str, limits: &Limits, warnings: &mut Vec<String>) -> Result<PathBuf> {
    let dir = root.join(format!("silli-{session}"));
    if !dir.is_dir() {
        std::fs::create_dir(&dir).with_context(|| format!("create cgroup {}", dir.display()))?;
    }
    let mut set = |file: &str, value: String| {
        if let Err(e) = std::fs::write(dir.join(file), &value) {
            if limits.required {
                return Err(anyhow::anyhow!("required cgroup {file}={value} not applied ({e}); controller not delegated?"));
            }
            warnings.push(format!("cgroup {file}={value} not applied ({e}); controller not delegated?"));
        }
        Ok(())
    };
    set("cpu.weight", limits.cpu_weight.to_string())?;
    set("io.weight", format!("default {}", limits.io_weight))?;
    set("memory.high", cgroup_size(&limits.memory_high)?)?;
    set("memory.max", cgroup_size(&limits.memory_max)?)?;
    set("pids.max", limits.tasks_max.to_string())?;
    Ok(dir)
}

/// Kill everything in a session cgroup (`cgroup.kill`, Linux ≥ 5.14) and remove it. Ok(false)
/// when there is no such cgroup.
pub fn kill_session_cgroup(root: &Path, session: &str) -> Result<bool> {
    let dir = root.join(format!("silli-{session}"));
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

/// Whether the pid named in `file` is still alive. Says nothing about *what* it is: for a
/// session's own pid files use [`session_pid`].
pub fn pid_file_alive(file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let Ok(pid) = text.trim().parse::<u32>() else { return false };
    Path::new("/proc").join(pid.to_string()).is_dir()
}

/// Mark every descriptor above stderr close-on-exec; how many were not (docs/daemon.md §3.11).
/// Whatever this process inherited — a listening socket systemd passed, a terminal's or a
/// desktop's leftovers — then reaches no child it starts, and through `silli launch` no jail: bwrap
/// hands its own inherited descriptors on to the jail. Descriptors opened afterwards are
/// close-on-exec already (Rust and SQLite open them so); the few a child is meant to have are
/// handed over explicitly (the shim's listener, the seccomp filter). Before this the daemon's
/// systemd socket reached a session's launcher, which held it — unanswered — for a day.
pub fn cloexec_inherited() -> usize {
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else { return 0 };
    let fds: Vec<i32> = dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).filter(|fd| *fd > 2).collect();
    let mut marked = 0;
    for fd in fds {
        // SAFETY: only the descriptor's flags are read and set. A number that is no longer open —
        // the directory handle listed above — fails with EBADF and is skipped; one reused by
        // another thread meanwhile only becomes close-on-exec, which nothing here relies on.
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        if let Ok(flags) = rustix::io::fcntl_getfd(fd)
            && !flags.contains(rustix::io::FdFlags::CLOEXEC)
            && rustix::io::fcntl_setfd(fd, flags | rustix::io::FdFlags::CLOEXEC).is_ok()
        {
            marked += 1;
        }
    }
    marked
}

/// Which of a session's processes a pid file names (`launch.pid`, `bwrap.pid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProcess {
    /// `silli launch … --session <id>`.
    Launcher,
    /// The outer bwrap, whose arguments name the session's paths.
    Bwrap,
}

/// The live pid `file` names, when that process is this user's and its command line is the
/// session's `kind` of process; None otherwise. A pid file outlives its process when a jail dies
/// with nobody watching, and pids are reused — on a busy machine within days (this one wrapped past
/// 4 M in two) — so what a stale file names is never adopted, waited on or signalled
/// (docs/daemon.md §3.11). A zombie has an empty command line and counts as gone.
pub fn session_pid(file: &Path, session: &str, kind: SessionProcess) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(file).ok()?.trim().parse().ok()?;
    is_session_process(pid, session, kind).then_some(pid)
}

/// `pid` is this user's process and its command line is the session's `kind` of process.
pub fn is_session_process(pid: u32, session: &str, kind: SessionProcess) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if session.is_empty() {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
    let mine = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u32>().ok()))
        == Some(rustix::process::geteuid().as_raw());
    if !mine {
        return false;
    }
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    match kind {
        SessionProcess::Launcher => args.contains(&&b"launch"[..]) && args.windows(2).any(|w| w[0] == b"--session" && w[1] == session.as_bytes()),
        SessionProcess::Bwrap => {
            args.first().is_some_and(|a| Path::new(std::ffi::OsStr::from_bytes(a)).file_name() == Some(std::ffi::OsStr::new("bwrap")))
                && cmdline.windows(session.len()).any(|w| w == session.as_bytes())
        }
    }
}

/// SIGKILL to the session process `file` names and to its direct children — only when it is that
/// process ([`session_pid`]); true if a signal was sent.
pub fn kill_session_process(file: &Path, session: &str, kind: SessionProcess) -> bool {
    match session_pid(file, session, kind) {
        Some(pid) => kill_with_children(pid as i32),
        None => false,
    }
}

/// SIGKILL to `pid` and its direct children. bwrap is two processes: the outer one that waits,
/// and its child, pid 1 of the jail's namespace — the one whose death takes everything in the
/// jail with it.
fn kill_with_children(pid: i32) -> bool {
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
    #[test]
    fn required_limits_fail_on_controller_write_failure() {
        let root = std::env::temp_dir().join(format!("silli-cgroup-required-{}", crate::store::new_id()));
        let dir = root.join("silli-test");
        std::fs::create_dir_all(dir.join("cpu.weight")).unwrap();
        let mut limits = Limits::default();
        let mut warnings = vec![];
        assert!(make_session_cgroup(&root, "test", &limits, &mut warnings).is_err());
        limits.required = false;
        assert!(make_session_cgroup(&root, "test", &limits, &mut warnings).is_ok());
        assert_eq!(warnings.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

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

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("silli-launcher-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A launcher is recognised by its command line, for its own session only.
    #[test]
    fn a_session_process_is_known_by_its_command_line() {
        // A shell waiting on its own input: it starts no further process. (A loop of `sleep 1` did,
        // and where processes were short — a session's task limit, with other work running — the
        // failed fork ended the shell and with it this test.)
        let mut launcher = std::process::Command::new("sh")
            .args(["-c", "read _", "silli", "launch", "--session", "abcd1234"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut other = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        // `spawn` returns while the kernel is still putting the new program in place: for a moment
        // the child still shows this test's own command line (seen in about one parallel run in
        // eight). Wait for the shell to be there.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !is_session_process(launcher.id(), "abcd1234", SessionProcess::Launcher) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(is_session_process(launcher.id(), "abcd1234", SessionProcess::Launcher));
        assert!(!is_session_process(launcher.id(), "abcd12", SessionProcess::Launcher), "a prefix of the id is another session");
        assert!(!is_session_process(launcher.id(), "abcd1234", SessionProcess::Bwrap));
        assert!(!is_session_process(other.id(), "abcd1234", SessionProcess::Launcher));
        let _ = launcher.kill();
        let _ = launcher.wait();
        assert!(!is_session_process(launcher.id(), "abcd1234", SessionProcess::Launcher), "a reaped launcher is gone");
        let _ = other.kill();
        let _ = other.wait();
    }

    /// The hazard behind the check (docs/daemon.md §3.11): a jail died unwatched, its pid was
    /// reused by something else of the person's — that process is never adopted or signalled.
    #[test]
    fn a_stale_pid_file_naming_a_reused_pid_is_never_signalled() {
        let dir = scratch("reused");
        let mut bystander = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        for f in ["launch.pid", "bwrap.pid"] {
            std::fs::write(dir.join(f), bystander.id().to_string()).unwrap();
        }
        assert_eq!(session_pid(&dir.join("launch.pid"), "abcd1234", SessionProcess::Launcher), None);
        assert!(!kill_session_process(&dir.join("launch.pid"), "abcd1234", SessionProcess::Launcher));
        assert!(!kill_session_process(&dir.join("bwrap.pid"), "abcd1234", SessionProcess::Bwrap));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(bystander.try_wait().unwrap().is_none(), "an unrelated process was killed through a stale pid file");
        let _ = bystander.kill();
        let _ = bystander.wait();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
