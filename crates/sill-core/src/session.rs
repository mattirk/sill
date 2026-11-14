//! Session lifecycle on the host side (SPEC §4.7): a session is a `sill launch` process — a transient
//! systemd user unit where there is one, a detached process otherwise (`launcher`) — reachable through
//! its shim socket. Starting is idempotent.

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
    /// Desktop access overriding the policy's `[jail] x11` / `dbus` (`None` = policy), SPEC §4.2.
    pub x11: Option<bool>,
    pub dbus: Option<bool>,
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

/// Parents under `~` that many programs share: an agent's entry below them is adopted one level
/// down (`~/.config/<agent>`), never the parent itself.
const SHARED_PARENTS: &[&str] = &[".config", ".local", ".cache"];

/// Agent-owned paths that live in the sill-owned home without being listed in the registry:
/// whatever the agent (or its host-side login) has put there. Each is (owned path, path under `~`).
/// Top-level dot entries of the agent home map to `~/<name>`; entries one level below a shared
/// parent map to `~/.config/<name>` and so on.
pub fn adopted_binds(agent: &crate::agents::Agent, home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let base = agent_home(home, &agent.id);
    let curated: Vec<String> = agent
        .home_paths
        .iter()
        .filter_map(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&base) else { return out };
    for e in rd.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') || curated.contains(&name) {
            continue;
        }
        if SHARED_PARENTS.contains(&name.as_str()) {
            let parent = base.join(&name);
            let Ok(rd2) = std::fs::read_dir(&parent) else { continue };
            for c in rd2.filter_map(Result::ok) {
                let cname = c.file_name().to_string_lossy().into_owned();
                if name == ".local" && (cname == "share" || cname == "state" || cname == "bin") {
                    // one more level: ~/.local/share/<agent>
                    let Ok(rd3) = std::fs::read_dir(c.path()) else { continue };
                    for g in rd3.filter_map(Result::ok) {
                        let rel = Path::new(&name).join(&cname).join(g.file_name());
                        out.push((base.join(&rel), home.join(&rel)));
                    }
                    continue;
                }
                let rel = Path::new(&name).join(&cname);
                out.push((base.join(&rel), home.join(&rel)));
            }
        } else {
            out.push((base.join(&name), home.join(&name)));
        }
    }
    out
}

/// Every path under `~` that is the agent's own (registry `state_dirs` and `home_paths` plus the
/// adopted ones), as `~/…` strings for [`crate::changes::is_agent_state`].
pub fn state_paths(agent: &crate::agents::Agent, home: &Path) -> Vec<String> {
    let mut v: Vec<String> = agent.state_dirs.iter().chain(agent.home_paths.iter()).cloned().collect();
    for (_, inside) in adopted_binds(agent, home) {
        if let Ok(rel) = inside.strip_prefix(home) {
            v.push(format!("~/{}", rel.display()));
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Move new agent-private paths out of a session's upper tree into the sill-owned home, so they
/// persist across that agent's sessions, stay off the host and out of the ledger: the shallowest
/// *new* path under a dot entry of `~` (`~/.foo`, `~/.config/foo`, `~/.gemini/foo` when `~/.gemini`
/// already existed). Pinned folders are never touched. Returns what was adopted (paths under `~`);
/// the binds take effect when the session starts again.
pub fn adopt_agent_state(
    agent: &crate::agents::Agent,
    home: &Path,
    changes: &[crate::ledger::Change],
    pins: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    use crate::ledger::Kind;
    let base = agent_home(home, &agent.id);
    let already: Vec<PathBuf> = adopted_binds(agent, home).into_iter().map(|(_, i)| i).collect();
    let curated: Vec<PathBuf> = agent.home_paths.iter().map(|p| crate::paths::expand_home(p, home)).collect();
    let added: std::collections::HashSet<&Path> =
        changes.iter().filter(|c| c.kind == Kind::Added).map(|c| c.path.as_path()).collect();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for c in changes {
        let Ok(rel) = c.path.strip_prefix(home) else { continue };
        let comps: Vec<String> = rel.components().map(|x| x.as_os_str().to_string_lossy().into_owned()).collect();
        let Some(first) = comps.first() else { continue };
        if !first.starts_with('.') {
            continue;
        }
        if pins.iter().any(|p| c.path.starts_with(p)) || already.iter().chain(curated.iter()).any(|p| c.path.starts_with(p)) {
            continue;
        }
        // the shallowest new ancestor (including the path itself)
        let mut cur = home.to_path_buf();
        let mut pick = None;
        for comp in &comps {
            cur = cur.join(comp);
            if added.contains(cur.as_path()) {
                pick = Some(cur.clone());
                break;
            }
        }
        let Some(pick) = pick else { continue };
        // never adopt a shared parent itself (`~/.config` as a whole)
        if pick.parent() == Some(home) && SHARED_PARENTS.contains(&comps[0].as_str()) {
            continue;
        }
        if !candidates.contains(&pick) {
            candidates.push(pick);
        }
    }
    let mut adopted = Vec::new();
    for host_path in candidates {
        let Some(change) = changes.iter().find(|c| c.path == host_path) else { continue };
        let rel = host_path.strip_prefix(home).unwrap_or(&host_path);
        let dst = base.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        copy_tree(&change.upper, &dst)?;
        adopted.push(host_path);
    }
    Ok(adopted)
}

/// Copy a file or directory tree (regular files and directories only; overlay whiteouts and
/// special files are skipped), merging into an existing destination.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.filter_map(Result::ok) {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if meta.is_file() {
        std::fs::copy(src, dst)?;
    } else if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
    }
    Ok(())
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
    // adopted agent state (see `adopted_binds`): the agent's own files that landed in its home
    for (owned, inside) in adopted_binds(agent, home) {
        if !out.iter().any(|(_, i)| *i == inside) {
            out.push((owned, inside));
        }
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

/// A running jail was built before its policy file was last changed: mounts and the X11/D-Bus
/// grants are fixed at launch, so the policy on disk no longer describes it.
pub fn jail_outdated(session: &str, policy: &str, home: &Path) -> bool {
    let Ok(sock) = socket_path(session) else { return false };
    let Ok(started) = std::fs::metadata(&sock).and_then(|m| m.modified()) else { return false };
    let Some(path) = Policy::resolve_path(policy, home) else { return false };
    std::fs::metadata(path).and_then(|m| m.modified()).map(|changed| changed > started).unwrap_or(false)
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

/// The `launch` argument list a session needs, mode-independent.
fn launch_args(opts: &StartOptions) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = vec!["launch".into(), "--session".into(), opts.session.clone().into(), "--policy".into(), opts.policy.clone().into()];
    a.push("--cwd".into());
    a.push(opts.cwd.clone().into());
    if let Some(d) = &opts.state_dir {
        a.push("--state-dir".into());
        a.push(d.clone().into());
    }
    if opts.unsafe_state {
        a.push("--unsafe-state".into());
    }
    if opts.share_net {
        a.push("--net".into());
    }
    for e in &opts.env {
        a.push("--env".into());
        a.push(e.clone().into());
    }
    if let Some(s) = &opts.shim_bin {
        a.push("--shim".into());
        a.push(s.clone().into());
    }
    for (flag, v) in [("--x11", opts.x11), ("--dbus", opts.dbus)] {
        if let Some(on) = v {
            a.push(flag.into());
            a.push(if on { "on" } else { "off" }.into());
        }
    }
    if let Some(ag) = &opts.agent {
        a.push("--agent".into());
        a.push(ag.clone().into());
    }
    for (host, inside) in &opts.binds {
        a.push("--bind".into());
        a.push(format!("{}={}", host.display(), inside.display()).into());
    }
    a
}

/// Start the session unless it is already reachable, in the launcher mode this process probed
/// (SPEC §4.7): a transient systemd user unit with the policy's limits, or a detached `launch`
/// process that limits itself through the delegated cgroup subtree when there is one.
pub fn ensure_started(opts: &StartOptions, home: &Path, exe: &Path) -> Result<PathBuf> {
    let sock = socket_path(&opts.session)?;
    if shim::Control::connect(&sock).is_ok() {
        return Ok(sock);
    }
    let policy = Policy::load(&opts.policy, home)?;
    let launcher = crate::launcher::current();
    match launcher.mode {
        crate::launcher::Mode::Unit => start_unit(opts, &policy, exe)?,
        crate::launcher::Mode::Direct => start_direct(opts, &policy, exe, launcher)?,
    }
    for _ in 0..50 {
        if shim::Control::connect(&sock).is_ok() {
            return Ok(sock);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    match launcher.mode {
        crate::launcher::Mode::Unit => bail!(
            "unit {} started but the shim socket did not appear; see journalctl --user -u {}",
            unit_name(&opts.session),
            unit_name(&opts.session)
        ),
        crate::launcher::Mode::Direct => bail!(
            "session started but the shim socket did not appear; see {}",
            shim::runtime_dir(&opts.session)?.join("jail.log").display()
        ),
    }
}

fn start_unit(opts: &StartOptions, policy: &Policy, exe: &Path) -> Result<()> {
    let unit = unit_name(&opts.session);
    // a failed earlier start leaves the unit around ("already exists" from systemd-run, SPEC §4.11 #14)
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "reset-failed", "--quiet", &format!("{unit}.service")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
        .args(launch_args(opts));
    let st = cmd.status().context("run systemd-run (is the systemd user instance running?)")?;
    if !st.success() {
        bail!("systemd-run failed ({st})");
    }
    Ok(())
}

fn start_direct(opts: &StartOptions, policy: &Policy, exe: &Path, launcher: &crate::launcher::Launcher) -> Result<()> {
    if launcher.cgroup_root.is_none() && policy.limits.required {
        bail!(
            "no cgroup for the policy's limits ({}): the session would run unlimited. \
             `sudo sill init` delegates a subtree; or set [limits] required = false in the policy",
            launcher.reason
        );
    }
    let rt = shim::runtime_dir(&opts.session)?;
    std::fs::create_dir_all(&rt)?;
    // stale pid files of a start that failed earlier: whatever they name is gone or not ours
    for f in ["bwrap.pid", "launch.pid"] {
        let _ = std::fs::remove_file(rt.join(f));
    }
    let log = std::fs::File::create(rt.join("jail.log")).with_context(|| format!("create {}", rt.join("jail.log").display()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(launch_args(opts)).arg("--detached");
    if let Some(root) = &launcher.cgroup_root {
        cmd.arg("--cgroup-root").arg(root);
    }
    cmd.current_dir(&opts.cwd);
    crate::launcher::spawn_detached(cmd, log)
}

/// Stop a session the way it was started: the unit's cgroup, the delegated cgroup, or SIGKILL to
/// bwrap (pid 1 of the jail's namespace: everything in it dies with it).
pub fn stop(session: &str) -> Result<()> {
    if let Ok(sock) = socket_path(session)
        && let Ok(mut c) = shim::Control::connect(&sock)
    {
        let _ = c.kill(5000);
    }
    let unit = format!("{}.service", unit_name(session));
    let unit_active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &unit])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if unit_active {
        std::process::Command::new("systemctl").args(["--user", "stop", &unit]).status()?;
        return Ok(());
    }
    // direct mode (or a unit that is already gone): cgroup first, pid files otherwise
    let rt = shim::runtime_dir(session)?;
    let killed_group = match crate::launcher::delegated_subtree() {
        Some(root) => crate::launcher::kill_session_cgroup(&root, session).unwrap_or(false),
        None => false,
    };
    if !killed_group {
        crate::launcher::kill_pid_file(&rt.join("bwrap.pid"));
    }
    // the launcher exits when bwrap does; give it a moment, then make sure
    for _ in 0..20 {
        if !crate::launcher::pid_file_alive(&rt.join("launch.pid")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    crate::launcher::kill_pid_file(&rt.join("launch.pid"));
    for f in ["bwrap.pid", "launch.pid", "shim.sock", "proxy.sock"] {
        let _ = std::fs::remove_file(rt.join(f));
    }
    Ok(())
}
