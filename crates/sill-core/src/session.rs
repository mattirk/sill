//! Session lifecycle on the host side (SPEC §4.7, docs/daemon.md §3.1): a session is a `sill launch`
//! process, a child of the daemon in a cgroup of its own (`launcher`), reachable through its shim
//! socket. Starting is idempotent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::policy::Policy;
use crate::shim;

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub session: String,
    pub policy: String,
    /// Pinned roots contributing repository policy fragments.
    pub pins: Vec<PathBuf>,
    /// Explicit human exceptions, valid for this launch only.
    pub ignore_fragments: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub unsafe_state: bool,
    pub share_net: bool,
    pub env: Vec<String>,
    pub shim_bin: Option<PathBuf>,
    /// Registry id of the agent the session runs: its domains join the proxy allow-list.
    pub agent: Option<String>,
    /// Explicit development-jail binds. Agent sessions resolve their bindings from declarations.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// A ghost session (docs/memory.md §3): `sill launch --ghost` tightens the policy
    /// (`Policy::ghostly`) and leaves the memory snapshot out unless Settings say otherwise.
    pub ghost: bool,
    /// A workspace session's source to hide (`Store::workspace_mask`).
    pub workspace: Option<crate::agent_storage::WorkspaceMask>,
}

/// The sill-owned home of an agent: `~/.local/state/sill/agents/<id>/`, holding the jail's copy of
/// each declared home path under its full relative path.
pub fn agent_home(home: &Path, agent_id: &str) -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/state"))
        .join("sill")
        .join("agents")
        .join(agent_id)
}

/// Where a ghost session's copy of its agent's home lives (docs/memory.md §3.2): in the session's
/// state dir, so it goes with the session's other state.
pub fn ghost_agent_home(state_dir: &Path, agent_id: &str) -> PathBuf {
    state_dir.join("agent-home").join(agent_id)
}

/// Existing copies of `agent_id`'s home across `sessions` (`ghost_agent_home`).
pub fn existing_ghost_homes(sessions: &[crate::store::Session], agent_id: &str) -> Vec<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    sessions
        .iter()
        .filter(|s| s.agent == agent_id)
        .filter_map(|s| state_dir_path(uid, &s.id).ok())
        .map(|dir| ghost_agent_home(&dir, agent_id))
        .filter(|p| p.is_dir())
        .collect()
}

/// Only recorded credential declarations authorize writes into an existing ghost.
pub fn ghost_credential_homes(sessions: &[crate::store::Session], agent_id: &str) -> Vec<(PathBuf, crate::credentials::Rules)> {
    sessions.iter().filter(|s| s.ghost && s.agent == agent_id).filter_map(|s| {
        let plan = recorded_filesystem(&s.id).ok()?;
        let path = plan.profile.to_path().ok()?;
        path.is_dir().then_some((path, plan.credentials))
    }).collect()
}

/// The policy a session's jail runs with: its own, tightened for a ghost (`Policy::ghostly`).
pub fn policy_of(s: &crate::store::Session, home: &Path) -> Result<Policy> {
    let mut p = Policy::load(&s.policy, home)?;
    if s.ghost {
        p.ghostly(home);
    }
    Ok(p)
}

/// Reading session state must not create directories or inspect today's ownership declarations.
pub fn recorded_filesystem(session: &str) -> Result<crate::agent_storage::Plan> {
    policy_snapshot(session)?.and_then(|s| s.filesystem)
        .context("filesystem plan not recorded; rebuild this session before inspecting or changing its files")
}

pub fn state_dir_path(uid: u32, session: &str) -> Result<PathBuf> {
    checked_id(session)?;
    Ok(Path::new(crate::jail::STATE_FS).join(uid.to_string()).join("sessions").join(session))
}

pub fn freeze_filesystem(mut resolution: crate::policy_fragments::Resolution, agent_id: &str, home: &Path, state_dir: &Path, pins: &[PathBuf], ghost: bool, workspace: Option<&crate::agent_storage::WorkspaceMask>) -> Result<crate::policy_fragments::Resolution> {
    let registry = crate::agents::load(home)?;
    let agent = registry.get(agent_id).context("unknown agent")?;
    let mut plan = crate::agent_storage::resolve(agent, &registry, &resolution.policy, state_dir, home, pins, ghost)?;
    plan.workspace = workspace.cloned();
    plan.add_fragments(&resolution.fragment_ro, &resolution.fragment_masks);
    resolution.filesystem = Some(plan);
    Ok(resolution)
}

/// Where the daemon puts files attached to a prompt: inside the session's private `~/.cache`
/// scratch (SPEC §3.2 attachments), so the jail sees them at the returned jail path and they go
/// with the session. `None` when the policy keeps no private `~/.cache`.
pub fn uploads_dir(policy: &Policy, state_dir: &Path, home: &Path) -> Option<(PathBuf, PathBuf)> {
    use crate::paths::{expand_home, layer_name};
    let cache = expand_home("~/.cache", home);
    if !policy.paths.private.iter().any(|p| expand_home(p, home) == cache) {
        return None;
    }
    let host = state_dir.join("private").join(layer_name(&cache)).join("sill").join("uploads");
    Some((host, cache.join("sill").join("uploads")))
}

/// A session id as `store::new_id` makes them (hex), with room for older or hand-made ones: letters,
/// digits, `-` and `_`, 1–64 of them — never empty, never a path. Every sink that joins an id into
/// a path or a unit name checks it: `sill.db` is a file in the person's state, so a row with a
/// crafted id must not reach `rm -rf` or a mount (security.md S16).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn checked_id(id: &str) -> Result<&str> {
    if valid_id(id) { Ok(id) } else { bail!("bad session id {id:?}") }
}

/// Where a session's upper trees live on the state filesystem (SPEC §4.9); created 0700 for the user.
pub fn state_dir(uid: u32, session: &str) -> Result<PathBuf> {
    let session = checked_id(session)?;
    let state_fs = Path::new(crate::jail::STATE_FS);
    let all = crate::mounts::host_mounts()?;
    if !crate::mounts::is_mount_point(&all, state_fs) {
        bail!("{} is not mounted (install the sill state units, SPEC §4.9)", state_fs.display());
    }
    // the state fs root is world-writable like /tmp: a `<uid>` directory someone else put there
    // first is not ours and is not used (security.md S5)
    let mine = state_fs.join(uid.to_string());
    let fd = crate::safe_fs::owned_dir(&mine, uid, true)?;
    rustix::fs::fchmod(&fd, rustix::fs::Mode::from_raw_mode(0o700))?;
    let sessions = mine.join("sessions");
    if sessions.exists() {
        crate::safe_fs::owned_dir(&sessions, uid, false)?;
    }
    Ok(sessions.join(session))
}

/// Remove a session's upper trees (needs a user namespace: overlayfs creates root-owned work dirs).
pub fn remove_state(uid: u32, session: &str) -> Result<()> {
    let dir = state_dir(uid, session)?; // checks the id
    if dir.is_dir() {
        let st = std::process::Command::new("unshare").args(["-Ur", "rm", "-rf"]).arg(&dir).status()?;
        if !st.success() {
            bail!("rm -rf {} failed", dir.display());
        }
    }
    Ok(())
}

/// `sill-<id>`: the session's scope (or cgroup) and journald stream identifier. Reduced to systemd's grammar so a crafted id names nothing but its own garbled self.
pub fn unit_name(session: &str) -> String {
    // a unit name is not a path, but systemd's grammar is narrower than ours: anything else is
    // reduced to what it allows, so a crafted id names no other unit than its own garbled self
    let safe: String = session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("sill-{safe}")
}

/// Where a session's log is (SPEC §4.7): its journald stream, or its files in the runtime dir.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LogSource {
    /// A journald stream identifier (`sill-<id>`).
    Journal(String),
    Files(Vec<PathBuf>),
    /// A managed process's output, read from the shim (docs/tasks.md §2.4).
    Proc { socket: PathBuf, name: String },
}

pub fn log_source(session: &str) -> LogSource {
    let file = shim::runtime_dir(session).map(|rt| rt.join("jail.log")).ok().filter(|f| f.is_file());
    if let Some(f) = file {
        // a ghost's, or any session's without journald
        LogSource::Files(vec![f])
    } else if crate::launcher::current().journal {
        LogSource::Journal(unit_name(session))
    } else {
        let rt = shim::runtime_dir(session).unwrap_or_default();
        LogSource::Files(vec![rt.join("jail.log")])
    }
}

/// The last `lines` of a log: `journalctl --user -t <identifier>` (precise times, no hostname), or
/// each file's tail under its name. Blocking: run it off the UI thread.
pub fn read_log(src: &LogSource, lines: usize) -> String {
    match src {
        LogSource::Journal(ident) => {
            let out = std::process::Command::new("journalctl")
                .args(["--user", "-t", ident, "-n", &lines.to_string(), "--no-pager", "--no-hostname", "-o", "short-precise"])
                .output();
            match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                Ok(o) => format!("journalctl: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("journalctl: {e}"),
            }
        }
        LogSource::Files(files) => {
            let mut out = String::new();
            for f in files {
                let text = std::fs::read_to_string(f).unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let tail: Vec<&str> = text.lines().rev().take(lines).collect();
                out.push_str(&format!("== {} ==\n", f.display()));
                for l in tail.into_iter().rev() {
                    out.push_str(l);
                    out.push('\n');
                }
                out.push('\n');
            }
            if out.is_empty() { "no log yet".to_string() } else { out }
        }
        LogSource::Proc { socket, name } => match shim::Control::connect(socket).and_then(|mut c| c.proc_tail(name, lines)) {
            Ok(t) if t.is_empty() => "no output yet".to_string(),
            Ok(t) => t,
            Err(e) => format!("{e:#}"),
        },
    }
}

pub fn socket_path(session: &str) -> Result<PathBuf> {
    Ok(shim::runtime_dir(session)?.join("shim.sock"))
}

/// A running jail was built before its policy file was last changed: mounts and permission
/// grants are fixed at launch, so the policy on disk no longer describes it.
/// Resolve the base with the same overrides the launcher uses, before repository restrictions.
pub fn effective_base_policy(name: &str, home: &Path, ghost: bool) -> Result<Policy> {
    let mut p = Policy::load(name, home)?;
    if ghost { p.ghostly(home); }
    Ok(p)
}

/// Freeze registry endpoints too: proxy behavior and the recorded policy must agree.
pub fn session_base_policy(name: &str, home: &Path, ghost: bool, agent: Option<&str>) -> Result<Policy> {
    let registry = crate::agents::load(home)?;
    let mut policy = effective_base_policy(name, home, ghost)?;
    if policy.network.mode != crate::policy::NetMode::None {
        if let Some(agent) = agent.and_then(|id| registry.get(id)) {
            policy.network.allow.extend(agent.domains.clone());
        }
    }
    Ok(policy)
}

/// Apply remembered human grants after repository and ghost restrictions, within the resolved
/// limits. Grants are not policy source edits: the source fingerprint stays, and the resolution
/// records the choices and hosts this build consulted.
pub fn apply_grants(mut resolution: crate::policy_fragments::Resolution, choices: &std::collections::BTreeMap<String, bool>, hosts: Vec<String>) -> crate::policy_fragments::Resolution {
    let policy = &mut resolution.policy;
    resolution.session_permissions = policy.permissions.apply_session(&mut policy.network, choices);
    if policy.network.mode != crate::policy::NetMode::None && policy.network.allow_additional_hosts {
        policy.network.allow.extend(hosts.iter().cloned());
        resolution.session_hosts = hosts;
    } else if !hosts.is_empty() {
        resolution.session_hosts_reason = if policy.network.mode == crate::policy::NetMode::None {
            "offline: the policy or a repository restriction allows no network".into()
        } else {
            "the policy allows no additional hosts (network.allow_additional_hosts = false)".into()
        };
        resolution.blocked_session_hosts = hosts;
    }
    resolution
}

pub fn resolved_policy_with_grants(resolution: crate::policy_fragments::Resolution, session: &str) -> Result<crate::policy_fragments::Resolution> {
    let store = crate::store::Store::open(&crate::store::default_path()?)?;
    Ok(apply_grants(resolution, &store.permission_choices(session)?, store.network_hosts(session)?))
}

/// A stored session's next build before its session grants: base policy,
/// then repository fragments of its jail pins (none for a ghost).
pub fn next_build_restrictions(row: &crate::store::Session, home: &Path, pins: &[PathBuf], ignore_roots: &[PathBuf]) -> Result<crate::policy_fragments::Resolution> {
    let base = session_base_policy(&row.policy, home, row.ghost, Some(&row.agent))?;
    crate::policy_fragments::resolve(&base, if row.ghost { &[] } else { pins }, ignore_roots)
}

/// A stored session's next build, as the launcher resolves it.
pub fn next_build(row: &crate::store::Session, home: &Path, pins: &[PathBuf], ignore_roots: &[PathBuf], workspace: Option<&crate::agent_storage::WorkspaceMask>) -> Result<crate::policy_fragments::Resolution> {
    let resolution = resolved_policy_with_grants(next_build_restrictions(row, home, pins, ignore_roots)?, &row.id)?;
    freeze_filesystem(resolution, &row.agent, home, &state_dir_path(rustix::process::getuid().as_raw(), &row.id)?, pins, row.ghost, workspace)
}

/// The launch no longer matches the remembered grants: a capability choice changed, or a host
/// grant it included was removed. Hosts approved since launch reach the running proxy live.
pub fn grants_outdated(launch: &crate::policy_fragments::Resolution, choices: &std::collections::BTreeMap<String, bool>, hosts: &[String]) -> bool {
    let launched: std::collections::BTreeMap<_, _> = launch.session_permissions.iter().map(|c| (c.capability.clone(), c.enabled)).collect();
    launched != *choices || launch.session_hosts.iter().any(|host| !hosts.contains(host))
}

fn snapshot_dir(session: &str) -> Result<PathBuf> {
    if !valid_id(session) { bail!("not a session id"); }
    Ok(crate::store::default_path()?.with_file_name("policy-snapshots").join(session))
}

const MAX_POLICY_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// Never infer mounted pins from mutable settings: a snapshot without pin evidence means none,
/// and leaving its overlay writes visible is safer than hiding recoverable work.
pub fn launch_pins(session: &str) -> Result<Vec<PathBuf>> {
    Ok(policy_snapshot(session)?.map(|s| s.launch_pins).unwrap_or_default())
}

pub fn policy_snapshot(session: &str) -> Result<Option<crate::policy_fragments::Resolution>> {
    match read_policy_snapshot(&snapshot_dir(session)?)? {
        Some(snapshot) => Ok(Some(snapshot)),
        None => read_policy_snapshot(&shim::runtime_dir(session)?.join("policy-snapshot")),
    }
}

/// Session deletion removes policy sources and history too; ghosts only use runtime storage.
pub fn forget_policy_snapshots(session: &str) -> Result<()> {
    for dir in [snapshot_dir(session)?, shim::runtime_dir(session)?.join("policy-snapshot")] {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn read_policy_snapshot(dir: &Path) -> Result<Option<crate::policy_fragments::Resolution>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join("latest.json");
    let file = match std::fs::OpenOptions::new().read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("open launch policy snapshot"),
    };
    if !file.metadata()?.is_file() { bail!("launch policy snapshot is not a regular file"); }
    let mut bytes = Vec::new();
    file.take(MAX_POLICY_SNAPSHOT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_POLICY_SNAPSHOT_BYTES { bail!("launch policy snapshot exceeds 4 MiB"); }
    Ok(Some(serde_json::from_slice(&bytes).context("read launch policy snapshot")?))
}

/// Record only a successful launch; retain older builds without reconstructing from live files.
fn record_policy_snapshot(session: &str, resolution: &crate::policy_fragments::Resolution) -> Result<()> {
    write_policy_snapshot(&snapshot_dir(session)?, resolution)
}

fn write_policy_snapshot(dir: &Path, resolution: &crate::policy_fragments::Resolution) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let _owner = crate::safe_fs::owned_dir(dir, rustix::process::getuid().as_raw(), false)?;
    let bytes = serde_json::to_vec_pretty(resolution)?;
    if bytes.len() > MAX_POLICY_SNAPSHOT_BYTES { bail!("launch policy snapshot exceeds 4 MiB"); }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let mut archive = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
        .open(dir.join(format!("{stamp}-{:032x}.json", rand::random::<u128>())))?;
    archive.write_all(&bytes)?;
    archive.sync_all()?;
    let temp = dir.join(format!("latest-{:032x}.tmp", rand::random::<u128>()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, dir.join("latest.json"))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

/// Policy sources, pins or remembered session grants changed since the recorded launch. Compare
/// source content, including absent fragments and pins, rather than modification times.
/// `agent_domains` are the agent's network defaults now (part of `base`): one added since the
/// launch does not make the jail outdated — it only widens, and applies at the next start
/// (2026-11-06, "Save for future sessions" asked every running session of the agent to restart).
/// A removed one still does.
pub fn jail_outdated_with_grants(session: &str, base: &Policy, pins: &[PathBuf], store: &crate::store::Store, agent_domains: &[String]) -> bool {
    let snapshot = policy_snapshot(session);
    if let Ok(Some(launch)) = &snapshot {
        let (Ok(choices), Ok(hosts)) = (store.permission_choices(session), store.network_hosts(session)) else { return true };
        if grants_outdated(launch, &choices, &hosts) { return true; }
    }
    snapshot_outdated(snapshot, base, pins, agent_domains)
}

fn snapshot_outdated(snapshot: Result<Option<crate::policy_fragments::Resolution>>, base: &Policy, pins: &[PathBuf], agent_domains: &[String]) -> bool {
    match snapshot {
        Ok(Some(snapshot)) => {
            let fingerprint = |base: &Policy| crate::policy_fragments::input_fingerprint(base, pins);
            if fingerprint(base).map(|current| current == snapshot.fingerprint).unwrap_or(false) {
                return false;
            }
            // Only agent defaults added since the launch? Then the base without them is the
            // launch's. The launch's own allow-list, less its session grants, is what it had.
            let launched: Vec<&String> = snapshot.policy.network.allow.iter()
                .filter(|h| !snapshot.session_hosts.contains(h)).collect();
            let added: Vec<&String> = agent_domains.iter().filter(|d| !launched.contains(d)).collect();
            if added.is_empty() { return true; }
            let mut without = base.clone();
            without.network.allow.retain(|h| !added.contains(&h));
            fingerprint(&without).map(|current| current != snapshot.fingerprint).unwrap_or(true)
        }
        // A jail without a snapshot has no evidence that it applied the fragments.
        Ok(None) => crate::policy_fragments::resolve(base, pins, &[])
            .map(|current| !current.sources.is_empty()).unwrap_or(true),
        // Missing evidence must not make corrupt or unreadable snapshots appear current.
        Err(_) => true,
    }
}

pub fn is_running(session: &str) -> bool {
    socket_path(session).map(|p| shim::Control::connect(&p).is_ok()).unwrap_or(false)
}

/// Start the session unit with the policy's limits, unless it is already reachable.
/// A sill-owned copy of this binary and its `sill-shim`, taken once per process, for everything the
/// host later executes on a session's behalf (`<exe> launch` in the unit, the shim bound into the jail).
/// `current_exe()` may sit in a pinned folder (developing sill in sill): the agent can write there, and
/// the unit would re-read the file from disk at every session start (SPEC §4.11 #1). The copy is taken
/// when the daemon started, which is the moment the person chose to run this build.
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
    // one directory per daemon process: units started by an earlier daemon keep running their copy;
    // directories of daemons that are gone are removed here
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
    if let Some(ag) = &opts.agent {
        a.push("--agent".into());
        a.push(ag.clone().into());
    }
    if opts.ghost {
        a.push("--ghost".into());
    }
    for (host, inside) in &opts.binds {
        a.push("--bind".into());
        a.push(format!("{}={}", host.display(), inside.display()).into());
    }
    a
}

/// What `ensure_started` found or made: the shim socket, and the child when it started one (the
/// daemon keeps it and waits on it; an already reachable session has none).
pub struct Started {
    pub socket: PathBuf,
    pub child: Option<std::process::Child>,
}

/// Start the session unless it is already reachable (SPEC §4.7, docs/daemon.md §3.1): `sill launch`
/// as a child of this process, in a session cgroup under the launcher's root when there is one,
/// its output on a journald stream (info level; ghost sessions will log at warning) or in `jail.log`.
pub fn ensure_started(opts: &StartOptions, home: &Path, exe: &Path) -> Result<Started> {
    // Serialize launch attempts, including one-build exceptions and their frozen input file.
    static LAUNCHING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _launching = LAUNCHING.lock().unwrap_or_else(|e| e.into_inner());
    let sock = socket_path(&opts.session)?;
    if shim::answers(&sock) {
        return Ok(Started { socket: sock, child: None });
    }
    anyhow::ensure!(opts.agent.is_none() || opts.binds.is_empty(), "agent session bindings must come from the resolved filesystem plan");
    let base = session_base_policy(&opts.policy, home, opts.ghost, opts.agent.as_deref())?;
    let resolution = crate::policy_fragments::resolve(&base, &opts.pins, &opts.ignore_fragments)?;
    let resolution = resolved_policy_with_grants(resolution, &opts.session)?;
    let resolution = if let Some(id) = &opts.agent {
        let state = opts.state_dir.clone().unwrap_or(state_dir_path(rustix::process::getuid().as_raw(), &opts.session)?);
        freeze_filesystem(resolution, id, home, &state, &opts.pins, opts.ghost, opts.workspace.as_ref())?
    } else {
        let registry = crate::agents::load(home)?;
        let state = opts.state_dir.clone().unwrap_or(state_dir_path(rustix::process::getuid().as_raw(), &opts.session)?);
        let mut resolution = resolution;
        let mut plan = crate::agent_storage::resolve_development(&registry, &resolution.policy, &state, home, &opts.binds)?;
        plan.add_fragments(&resolution.fragment_ro, &resolution.fragment_masks);
        resolution.filesystem = Some(plan);
        resolution
    };
    resolution.policy.permissions.validate_launch(&resolution.policy.network)?;
    if let Some(plan) = &resolution.filesystem {
        plan.prepare()?;
    }
    let serialized_resolution = serde_json::to_vec_pretty(&resolution)?;
    if serialized_resolution.len() > MAX_POLICY_SNAPSHOT_BYTES { bail!("launch policy snapshot exceeds 4 MiB"); }
    let policy = &resolution.policy;
    let launcher = crate::launcher::current();
    if !launcher.has_limits() && policy.limits.required {
        bail!(
            "no cgroup for the policy's limits ({}): the session would run unlimited. \
             `sudo sill init --delegate-cgroup`, or set [limits] required = false in the policy",
            launcher.reason
        );
    }
    let unit = unit_name(&opts.session);
    // a scope left over from a jail that is gone or unusable (the socket did not answer): its name
    // must be free; stopping it kills whatever lingers in its cgroup
    if launcher.scopes && crate::launcher::stop_scope(&unit).unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let rt = shim::runtime_dir(&opts.session)?;
    std::fs::create_dir_all(&rt)?;
    // stale pid files of a start that failed earlier: whatever they name is gone or not ours
    for f in ["bwrap.pid", "launch.pid", "launch-error"] {
        let _ = std::fs::remove_file(rt.join(f));
    }
    // a ghost's log — the proxy's host names included — stays in its runtime dir (RAM) and goes
    // with the session, never into the journal (docs/memory.md §3.4)
    let log = if launcher.journal && !opts.ghost {
        crate::launcher::journal_stream(&unit_name(&opts.session), 6)?
    } else {
        crate::launcher::file_stream(&rt.join("jail.log"))?
    };
    let mut cmd = if launcher.scopes {
        let mut c = crate::launcher::scope_command(&unit, &policy.limits);
        c.arg(exe);
        c
    } else {
        std::process::Command::new(exe)
    };
    // Private host-side launch input: the child does not reread agent-writable fragments.
    let resolved_path = rt.join("launch-policy.json");
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600)
            .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(&resolved_path)?;
        file.write_all(&serialized_resolution)?;
    }
    cmd.args(launch_args(opts)).arg("--resolved-policy").arg(&resolved_path).arg("--child");
    if let Some(root) = &launcher.cgroup_root {
        cmd.arg("--cgroup-root").arg(root);
    }
    cmd.current_dir(&opts.cwd);
    let mut child = crate::launcher::spawn_child(cmd, log)?;
    // until the shim answers: a jail that fails at once makes the launcher retry and then exit,
    // which is a failed start, not a running session (2026-11-06: a start reported success
    // while every bubblewrap attempt failed)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if shim::answers(&sock) {
            let recorded = if opts.ghost {
                write_policy_snapshot(&rt.join("policy-snapshot"), &resolution)
            } else { record_policy_snapshot(&opts.session, &resolution) };
            if let Err(error) = recorded {
                let _ = stop(&opts.session);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.context("recording launch policy; stopped unrecorded jail"));
            }
            let _ = std::fs::remove_file(&resolved_path);
            return Ok(Started { socket: sock, child: Some(child) });
        }
        if let Ok(Some(st)) = child.try_wait() {
            // the launcher reports a jail that failed and exits itself: its own status says little
            let status = if st.success() { String::new() } else { format!(" ({st})") };
            // the launcher's own reason when it failed before the jail ran (a test desktop that
            // would not start), bounded
            let reason = std::fs::read_to_string(rt.join("launch-error"))
                .map(|e| format!(": {}", e.chars().take(600).collect::<String>().trim()))
                .unwrap_or_default();
            bail!("the jail could not start{status}{reason}; see {}", log_hint(&opts.session));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = stop(&opts.session);
    let _ = child.kill();
    let _ = child.wait();
    bail!("the jail did not answer within 10 s; see {}", log_hint(&opts.session))
}

/// Where to look when a session misbehaves.
pub fn log_hint(session: &str) -> String {
    match log_source(session) {
        LogSource::Journal(ident) => format!("journalctl --user -t {ident}"),
        LogSource::Files(files) => files.first().map(|f| f.display().to_string()).unwrap_or_default(),
        LogSource::Proc { .. } => String::new(),
    }
}

/// Stop a session: ask the shim to end the agent, then stop its scope or kill the session cgroup
/// (everything in the jail) — or, with no cgroup, SIGKILL to bwrap, pid 1 of the jail's namespace.
pub fn stop(session: &str) -> Result<()> {
    if let Ok(sock) = socket_path(session)
        && let Ok(mut c) = shim::Control::connect(&sock)
    {
        let _ = c.kill(5000);
    }
    let rt = shim::runtime_dir(session)?;
    let launcher = crate::launcher::current();
    let killed_group = if launcher.scopes {
        crate::launcher::stop_scope(&unit_name(session)).unwrap_or(false)
    } else {
        match &launcher.cgroup_root {
            Some(root) => crate::launcher::kill_session_cgroup(root, session).unwrap_or(false),
            None => false,
        }
    };
    // what the pid files name is signalled only while it is still the session's (a jail that died
    // unwatched leaves them behind, and pids are reused: docs/daemon.md §3.11)
    use crate::launcher::SessionProcess::{Bwrap, Launcher};
    if !killed_group {
        crate::launcher::kill_session_process(&rt.join("bwrap.pid"), session, Bwrap);
    }
    // the launcher exits when bwrap does; give it a moment, then make sure
    for _ in 0..20 {
        if crate::launcher::session_pid(&rt.join("launch.pid"), session, Launcher).is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    crate::launcher::kill_session_process(&rt.join("launch.pid"), session, Launcher);
    for f in ["bwrap.pid", "launch.pid", "shim.sock", "proxy.sock"] {
        let _ = std::fs::remove_file(rt.join(f));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PolicyTestTree(PathBuf);
    impl PolicyTestTree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("sill-policy-snapshot-{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(root.join("repo/.sill")).unwrap();
            Self(root)
        }
        fn repo(&self) -> PathBuf { self.0.join("repo") }
        fn snapshots(&self) -> PathBuf { self.0.join("snapshots") }
        fn fragment(&self, text: &str) { std::fs::write(self.repo().join(".sill/policy.toml"), text).unwrap(); }
        fn resolve(&self, base: &Policy) -> crate::policy_fragments::Resolution {
            crate::policy_fragments::resolve(base, &[self.repo()], &[]).unwrap()
        }
    }
    impl Drop for PolicyTestTree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn launch_pins_survive_edits_without_policy_fragments() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        let first = tree.resolve(&base);
        assert!(first.sources.is_empty());
        write_policy_snapshot(&tree.snapshots(), &first).unwrap();
        let changed = crate::policy_fragments::resolve(&base, &[], &[]).unwrap();
        assert!(changed.launch_pins.is_empty());
        let saved = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(saved.launch_pins, vec![tree.repo()]);
        // A snapshot without pin evidence means none; never substitute today's settings.
        let mut without = serde_json::to_value(saved).unwrap();
        without.as_object_mut().unwrap().remove("launch_pins");
        let without: crate::policy_fragments::Resolution = serde_json::from_value(without).unwrap();
        assert!(without.launch_pins.is_empty());
    }

    #[test]
    fn policy_launch_snapshots_preserve_history_and_source_text_across_edits() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        tree.fragment("[network]\nallow=['example.com']");
        let first = tree.resolve(&base);
        write_policy_snapshot(&tree.snapshots(), &first).unwrap();
        let snapshot = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(snapshot.fingerprint, first.fingerprint);
        assert!(!snapshot_outdated(Ok(Some(snapshot.clone())), &base, &[tree.repo()], &[]));
        tree.fragment("[network]\nmode='none'");
        assert!(snapshot_outdated(Ok(Some(snapshot)), &base, &[tree.repo()], &[]));
        let second = tree.resolve(&base);
        write_policy_snapshot(&tree.snapshots(), &second).unwrap();
        let latest = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(latest.policy.network.mode, crate::policy::NetMode::None);
        let archives: Vec<_> = std::fs::read_dir(tree.snapshots()).unwrap().flatten()
            .filter(|e| e.file_name() != "latest.json").collect();
        assert_eq!(archives.len(), 2);
        assert!(archives.into_iter().any(|entry| {
            let old: crate::policy_fragments::Resolution = serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            old.fingerprint == first.fingerprint && old.sources[0].text.contains("example.com")
        }));
        std::fs::remove_file(tree.repo().join(".sill/policy.toml")).unwrap();
        assert!(snapshot_outdated(Ok(Some(latest)), &base, &[tree.repo()], &[]));
    }

    /// An agent default added after the launch leaves the jail current (it only widens); a
    /// removed one, or another change alongside an added one, makes it outdated.
    #[test]
    fn added_agent_defaults_do_not_outdate_a_running_jail() {
        let tree = PolicyTestTree::new();
        let with = |domains: &[&str]| {
            let mut base = Policy::builtin_default();
            base.network.allow.extend(domains.iter().map(|d| d.to_string()));
            base
        };
        let launch = tree.resolve(&with(&["api.agent.example"]));
        // a session grant at launch that is later saved as an agent default stays current too
        let mut launch = apply_grants(launch, &Default::default(), vec!["saved.example".into()]);
        launch.fingerprint = tree.resolve(&with(&["api.agent.example"])).fingerprint;
        let domains = |d: &[&str]| d.iter().map(|d| d.to_string()).collect::<Vec<_>>();
        let pins = [tree.repo()];
        let now = ["api.agent.example", "saved.example", "new.example"];
        assert!(!snapshot_outdated(Ok(Some(launch.clone())), &with(&now), &pins, &domains(&now)), "added");
        assert!(snapshot_outdated(Ok(Some(launch.clone())), &with(&[]), &pins, &[]), "removed");
        let mut changed = with(&now);
        changed.network.quiet.push("other.example".into());
        assert!(snapshot_outdated(Ok(Some(launch)), &changed, &pins, &domains(&now)), "added and something else");
    }

    #[test]
    fn session_grants_apply_after_repository_restrictions_and_are_recorded_for_the_launch() {
        let tree = PolicyTestTree::new();
        let base = Policy::parse("name='custom'\n[network]\nmode='allow'").unwrap();
        let choices: std::collections::BTreeMap<String, bool> = [("public_web".to_string(), true)].into();
        let hosts = vec!["example.com".to_string()];
        let launch = apply_grants(tree.resolve(&base), &choices, hosts.clone());
        assert_eq!(launch.policy.network.mode, crate::policy::NetMode::All);
        assert_eq!(launch.session_hosts, hosts);
        assert!(!grants_outdated(&launch, &choices, &hosts));
        // Hosts approved while running are live; removed hosts or changed choices need a rebuild.
        assert!(!grants_outdated(&launch, &choices, &["example.com".into(), "later.example.com".into()]));
        assert!(grants_outdated(&launch, &choices, &[]));
        assert!(grants_outdated(&launch, &Default::default(), &hosts));
        // A repository prohibition blocks the remembered choice instead of being bypassed.
        tree.fragment("[permissions]\npublic_web='forbidden'");
        let blocked = apply_grants(tree.resolve(&base), &choices, hosts.clone());
        assert_eq!(blocked.policy.network.mode, crate::policy::NetMode::Allow);
        assert!(blocked.session_permissions[0].blocked.is_some());
        tree.fragment("[network]\nmode='none'");
        let offline = apply_grants(tree.resolve(&base), &choices, hosts);
        assert!(offline.session_hosts.is_empty() && !offline.policy.network.allow.contains(&"example.com".to_string()));
        let decoded: crate::policy_fragments::Resolution = serde_json::from_slice(&serde_json::to_vec(&blocked).unwrap()).unwrap();
        assert_eq!(decoded.session_permissions, blocked.session_permissions);
    }

    #[test]
    fn corrupt_or_unsafe_policy_snapshot_is_stale_and_never_reconstructed() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        assert!(read_policy_snapshot(&tree.snapshots()).unwrap().is_none());
        write_policy_snapshot(&tree.snapshots(), &tree.resolve(&base)).unwrap();
        std::fs::write(tree.snapshots().join("latest.json"), "corrupt").unwrap();
        assert!(snapshot_outdated(read_policy_snapshot(&tree.snapshots()), &base, &[tree.repo()], &[]));
        std::fs::remove_file(tree.snapshots().join("latest.json")).unwrap();
        std::os::unix::fs::symlink(tree.repo().join(".sill/policy.toml"), tree.snapshots().join("latest.json")).unwrap();
        assert!(read_policy_snapshot(&tree.snapshots()).is_err());
    }

    #[test]
    fn jail_without_snapshot_needs_rebuild_for_valid_or_invalid_repository_fragment() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        assert!(!snapshot_outdated(Ok(None), &base, &[tree.repo()], &[]));
        tree.fragment("[network]\nmode='none'");
        assert!(snapshot_outdated(Ok(None), &base, &[tree.repo()], &[]));
        tree.fragment("malformed [");
        assert!(snapshot_outdated(Ok(None), &base, &[tree.repo()], &[]));
    }

    #[test]
    fn ignored_fragment_exception_survives_snapshot_but_not_next_build() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        tree.fragment("[paths]\ndirect=['.']");
        let ignored = crate::policy_fragments::resolve(&base, &[tree.repo()], &[tree.repo()]).unwrap();
        write_policy_snapshot(&tree.snapshots(), &ignored).unwrap();
        let snapshot = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert!(snapshot.sources[0].ignored);
        assert!(snapshot.sources[0].text.contains("direct"));
        assert!(!snapshot_outdated(Ok(Some(snapshot)), &base, &[tree.repo()], &[]));
        assert!(crate::policy_fragments::resolve(&base, &[tree.repo()], &[]).is_err());
    }

    #[test]
    fn ids_at_the_sinks() {
        assert!(valid_id(&crate::store::new_id()));
        assert!(valid_id("a1b2c3d4") && valid_id("old-style_id"));
        for bad in ["", "../x", "a/b", ".", "..", "a b", &"x".repeat(65)] {
            assert!(!valid_id(bad), "{bad:?}");
            assert!(state_dir(1000, bad).is_err(), "{bad:?}");
            assert!(crate::shim::runtime_dir(bad).is_err(), "{bad:?}");
        }
        assert_eq!(unit_name("../etc"), "sill-___etc");
    }

    /// A detached session's log: each file's tail under its name, empty or missing files skipped.
    #[test]
    fn read_log_tails_the_files() {
        let dir = std::env::temp_dir().join(format!("sill-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jail = dir.join("jail.log");
        std::fs::write(&jail, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        std::fs::write(dir.join("proxy.log"), "").unwrap();
        let src = LogSource::Files(vec![jail.clone(), dir.join("proxy.log"), dir.join("missing.log")]);
        let text = read_log(&src, 3);
        assert_eq!(text, format!("== {} ==\nline 8\nline 9\nline 10\n\n", jail.display()));
        assert_eq!(read_log(&LogSource::Files(vec![dir.join("missing.log")]), 3), "no log yet");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
