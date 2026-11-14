//! Session lifecycle on the host side (SPEC §4.7, docs/daemon.md §3.1): a session is a `sill launch`
//! process, a child of the daemon in a cgroup of its own (`launcher`), reachable through its shim
//! socket. Starting is idempotent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::policy::Policy;
use crate::shim;

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub session: String,
    pub policy: String,
    /// The session's pinned paths.
    pub pins: Vec<PathBuf>,
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
    /// A workspace session's repositories (`Store::workspace_mask`, §12).
    pub workspace: Vec<crate::agent_storage::WorkspaceBind>,
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

pub fn freeze_filesystem(mut resolution: Resolution, agent_id: &str, home: &Path, state_dir: &Path, pins: &[PathBuf], ghost: bool, workspace: &[crate::agent_storage::WorkspaceBind]) -> Result<Resolution> {
    let registry = crate::agents::load(home)?;
    let agent = registry.get(agent_id).context("unknown agent")?;
    let mut plan = crate::agent_storage::resolve(agent, &registry, &resolution.policy, state_dir, home, pins, ghost)?;
    plan.set_workspaces(workspace)?;
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
/// Resolve the base with the same overrides the launcher uses.
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
        // Model providers (docs/model-providers.md): each one's exact host and port, host-local
        // or not, for agents sill hands providers to. Part of the base, so a provider added or
        // removed makes a running jail outdated.
        if let Some(id) = agent {
            for endpoint in crate::providers::agent_endpoints(home, id)? {
                if !policy.network.allow_local.contains(&endpoint) { policy.network.allow_local.push(endpoint); }
            }
        }
    }
    Ok(policy)
}

// Launch records keep exact Linux path bytes, as the public HostPath DTOs do; the string form of
// early records is read too.
#[derive(Deserialize)]
#[serde(untagged)]
enum EncodedPath {
    Bytes(Vec<u8>),
    Text(PathBuf),
}
impl EncodedPath {
    fn into_path(self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        match self {
            Self::Bytes(bytes) => std::ffi::OsString::from_vec(bytes).into(),
            Self::Text(path) => path,
        }
    }
}
mod paths_bytes {
    use super::*;
    pub fn serialize<S: serde::Serializer>(paths: &[PathBuf], serializer: S) -> std::result::Result<S::Ok, S::Error> {
        paths.iter().map(|path| path.as_os_str().as_encoded_bytes()).collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<EncodedPath>::deserialize(deserializer)?.into_iter().map(EncodedPath::into_path).collect())
    }
}

/// What one jail build runs with: the session's base policy as resolved for its pins, then its
/// remembered grants (`apply_grants`) and its filesystem plan (`freeze_filesystem`). The launcher
/// gets exactly this, and a successful launch keeps it as its record (`policy_snapshot`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    /// Missing evidence requires a rebuild, never inference from current definitions.
    pub filesystem: Option<crate::agent_storage::Plan>,
    /// Pins used by this launch, independent of later store edits.
    #[serde(default, with = "paths_bytes")]
    pub launch_pins: Vec<PathBuf>,
    /// The base policy with the session grants this build applied.
    pub policy: Policy,
    /// Hash of the build's inputs (`input_fingerprint`): the base policy and the pinned folders.
    pub fingerprint: String,
    /// Remembered session capability choices this build consulted, with their outcome.
    #[serde(default)]
    pub session_permissions: Vec<crate::permissions::SessionChoice>,
    /// Session-only host approvals included in this build's allow-list.
    #[serde(default)]
    pub session_hosts: Vec<String>,
    /// Remembered session approvals this build's limits leave out (offline, or no additional
    /// hosts allowed), and why. They stay remembered and apply again when the limit goes.
    #[serde(default)]
    pub blocked_session_hosts: Vec<String>,
    #[serde(default)]
    pub session_hosts_reason: String,
}

/// The session's folder pins, canonical and distinct. A pin that no longer resolves fails the build.
fn pinned_folders(pins: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut folders = std::collections::BTreeSet::new();
    for pin in pins {
        let path = pin.canonicalize().with_context(|| format!("resolve pinned path {}", pin.display()))?;
        if path.is_dir() {
            folders.insert(path);
        }
    }
    Ok(folders.into_iter().collect())
}

/// Identifies what a build follows from: the base policy's content and the pinned folders.
pub fn input_fingerprint(base: &Policy, pins: &[PathBuf]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"future-secret-masks-v1");
    let base = serde_json::to_vec(base)?;
    hash.update((base.len() as u64).to_le_bytes());
    hash.update(&base);
    for folder in pinned_folders(pins)? {
        let name = folder.as_os_str().as_encoded_bytes();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Resolve once per jail build: the base policy for these pins, before the session's grants and
/// its filesystem plan.
pub fn resolve(base: &Policy, pins: &[PathBuf]) -> Result<Resolution> {
    Ok(Resolution {
        filesystem: None,
        launch_pins: pins.to_vec(),
        policy: base.clone(),
        fingerprint: input_fingerprint(base, pins)?,
        session_permissions: vec![],
        session_hosts: vec![],
        blocked_session_hosts: vec![],
        session_hosts_reason: String::new(),
    })
}

/// Apply remembered human grants after ghost restrictions, within the resolved limits. Grants
/// are not policy edits: the fingerprint stays, and the resolution records the choices and
/// hosts this build consulted.
pub fn apply_grants(mut resolution: Resolution, choices: &std::collections::BTreeMap<String, bool>, hosts: Vec<String>) -> Resolution {
    let policy = &mut resolution.policy;
    resolution.session_permissions = policy.permissions.apply_session(&mut policy.network, choices);
    if policy.network.mode != crate::policy::NetMode::None && policy.network.allow_additional_hosts {
        policy.network.allow.extend(hosts.iter().cloned());
        resolution.session_hosts = hosts;
    } else if !hosts.is_empty() {
        resolution.session_hosts_reason = if policy.network.mode == crate::policy::NetMode::None {
            "offline: the policy allows no network".into()
        } else {
            "the policy allows no additional hosts (network.allow_additional_hosts = false)".into()
        };
        resolution.blocked_session_hosts = hosts;
    }
    resolution
}

pub fn resolved_policy_with_grants(resolution: Resolution, session: &str) -> Result<Resolution> {
    let store = crate::store::Store::open(&crate::store::default_path()?)?;
    Ok(apply_grants(resolution, &store.permission_choices(session)?, store.network_hosts(session)?))
}

/// A stored session's next build before its session grants: its base policy for its jail pins
/// (none for a ghost).
pub fn next_build_base(row: &crate::store::Session, home: &Path, pins: &[PathBuf]) -> Result<Resolution> {
    let base = session_base_policy(&row.policy, home, row.ghost, Some(&row.agent))?;
    resolve(&base, if row.ghost { &[] } else { pins })
}

/// A stored session's next build, as the launcher resolves it.
pub fn next_build(row: &crate::store::Session, home: &Path, pins: &[PathBuf], workspace: &[crate::agent_storage::WorkspaceBind]) -> Result<Resolution> {
    let resolution = resolved_policy_with_grants(next_build_base(row, home, pins)?, &row.id)?;
    freeze_filesystem(resolution, &row.agent, home, &state_dir_path(rustix::process::getuid().as_raw(), &row.id)?, pins, row.ghost, workspace)
}

/// The launch no longer matches the remembered grants: a capability choice changed, or a host
/// grant it included was removed. Hosts approved since launch reach the running proxy live.
pub fn grants_outdated(launch: &Resolution, choices: &std::collections::BTreeMap<String, bool>, hosts: &[String]) -> bool {
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

pub fn policy_snapshot(session: &str) -> Result<Option<Resolution>> {
    match read_policy_snapshot(&snapshot_dir(session)?)? {
        Some(snapshot) => Ok(Some(snapshot)),
        None => read_policy_snapshot(&shim::runtime_dir(session)?.join("policy-snapshot")),
    }
}

/// Session deletion removes its launch records too; ghosts only use runtime storage.
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

fn read_policy_snapshot(dir: &Path) -> Result<Option<Resolution>> {
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
fn record_policy_snapshot(session: &str, resolution: &Resolution) -> Result<()> {
    write_policy_snapshot(&snapshot_dir(session)?, resolution)
}

fn write_policy_snapshot(dir: &Path, resolution: &Resolution) -> Result<()> {
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

/// The policy, the pinned folders or the remembered session grants changed since the recorded
/// launch. Compares the policy's content rather than modification times.
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

fn snapshot_outdated(snapshot: Result<Option<Resolution>>, base: &Policy, pins: &[PathBuf], agent_domains: &[String]) -> bool {
    match snapshot {
        Ok(Some(snapshot)) => {
            let fingerprint = |base: &Policy| input_fingerprint(base, pins);
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
        // No launch record, or a corrupt or unreadable one, is no evidence of what the jail runs
        // with: it never appears current.
        Ok(None) | Err(_) => true,
    }
}

/// A mask hides a path, not what Git keeps of it: a file that was ever committed (or staged) in
/// a repository the jail is given, as a pin or a workspace, stays readable there through the
/// object store (`git show`), whatever is laid over its path. That is a restriction the jail
/// cannot keep, so the build is refused and says so, instead of starting with a mask that hides
/// nothing. Checked at launch only: it reads the repository's history.
fn masks_git_cannot_undo(masks: &[PathBuf], pins: &[PathBuf], workspaces: &[PathBuf]) -> Result<()> {
    for mask in masks {
        let inside = |roots: &[PathBuf]| roots.iter().any(|r| mask != r && mask.starts_with(r));
        let repo = match workspaces.iter().find(|w| *mask != **w && mask.starts_with(w)) {
            Some(source) => Some(source.clone()),
            None if inside(pins) => mask.parent().and_then(crate::changes::repo_root),
            None => None,
        };
        let Some(repo) = repo else { continue };
        let Ok(relative) = mask.strip_prefix(&repo) else { continue };
        if relative.starts_with(".git") { continue; }
        let git = crate::changes::Git::open(&repo).with_context(|| format!("reading the repository of masked {}", mask.display()))?;
        let found = |args: &[&str]| -> Result<bool> {
            let out = git.cmd().arg("--literal-pathspecs").args(args).arg("--").arg(relative).output()?;
            anyhow::ensure!(out.status.success(), "git in {}: {}", repo.display(), String::from_utf8_lossy(&out.stderr).trim());
            Ok(!out.stdout.is_empty())
        };
        if found(&["ls-files", "-z"])? || found(&["log", "--all", "--reflog", "-1", "--format=%H"])? {
            bail!(
                "{} is masked by the policy, but Git has it in {}: what was committed there stays readable through the repository (git show), so the mask would hide nothing. \
                 Take it out of the repository and its history, or out of the policy's mask, or do not pin the repository",
                mask.display(), repo.display()
            );
        }
    }
    Ok(())
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
    // Serialize launch attempts and their frozen input file.
    static LAUNCHING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _launching = LAUNCHING.lock().unwrap_or_else(|e| e.into_inner());
    let sock = socket_path(&opts.session)?;
    if shim::answers(&sock) {
        return Ok(Started { socket: sock, child: None });
    }
    anyhow::ensure!(opts.agent.is_none() || opts.binds.is_empty(), "agent session bindings must come from the resolved filesystem plan");
    let base = session_base_policy(&opts.policy, home, opts.ghost, opts.agent.as_deref())?;
    let resolution = resolved_policy_with_grants(resolve(&base, &opts.pins)?, &opts.session)?;
    let resolution = if let Some(id) = &opts.agent {
        let state = opts.state_dir.clone().unwrap_or(state_dir_path(rustix::process::getuid().as_raw(), &opts.session)?);
        freeze_filesystem(resolution, id, home, &state, &opts.pins, opts.ghost, &opts.workspace)?
    } else {
        let registry = crate::agents::load(home)?;
        let state = opts.state_dir.clone().unwrap_or(state_dir_path(rustix::process::getuid().as_raw(), &opts.session)?);
        let mut resolution = resolution;
        resolution.filesystem = Some(crate::agent_storage::resolve_development(&registry, &resolution.policy, &state, home, &opts.binds)?);
        resolution
    };
    resolution.policy.permissions.validate_launch(&resolution.policy.network)?;
    if let Some(plan) = &resolution.filesystem {
        let paths = |list: Vec<&sill_proto::sessions::HostPath>| list.into_iter().map(|p| p.to_path()).collect::<Result<Vec<_>, _>>();
        masks_git_cannot_undo(
            &paths(plan.policy_paths.masks.iter().map(|m| &m.path).collect())?,
            &paths(plan.bindings.iter().filter(|b| b.declaration.is_none() && b.source == b.target).map(|b| &b.target).collect())?,
            &paths(plan.workspaces.iter().map(|w| &w.source).collect())?,
        )?;
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
    // Private host-side launch input: the child builds exactly what was resolved here.
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
            std::fs::create_dir_all(root.join("repo")).unwrap();
            Self(root)
        }
        fn repo(&self) -> PathBuf { self.0.join("repo") }
        fn snapshots(&self) -> PathBuf { self.0.join("snapshots") }
        fn resolve(&self, base: &Policy) -> Resolution {
            resolve(base, &[self.repo()]).unwrap()
        }
    }
    impl Drop for PolicyTestTree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn launch_pins_survive_later_pin_edits() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        let first = tree.resolve(&base);
        write_policy_snapshot(&tree.snapshots(), &first).unwrap();
        let changed = resolve(&base, &[]).unwrap();
        assert!(changed.launch_pins.is_empty());
        let saved = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(saved.launch_pins, vec![tree.repo()]);
        // A snapshot without pin evidence means none; never substitute today's settings.
        let mut without = serde_json::to_value(saved).unwrap();
        without.as_object_mut().unwrap().remove("launch_pins");
        let without: Resolution = serde_json::from_value(without).unwrap();
        assert!(without.launch_pins.is_empty());
    }

    /// Launches recorded while repositories could carry policy fragments (removed 2026-11-10)
    /// stay readable: Changes and the launch view of those sessions read them.
    #[test]
    fn a_launch_record_with_retired_fields_still_reads() {
        let tree = PolicyTestTree::new();
        let mut old = serde_json::to_value(tree.resolve(&Policy::builtin_default())).unwrap();
        let record = old.as_object_mut().unwrap();
        for retired in ["fragment_ro", "fragment_masks", "sources", "suggestions"] {
            record.insert(retired.into(), serde_json::json!([]));
        }
        std::fs::create_dir_all(tree.snapshots()).unwrap();
        std::fs::write(tree.snapshots().join("latest.json"), serde_json::to_vec(&old).unwrap()).unwrap();
        let read = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(read.launch_pins, vec![tree.repo()]);
    }

    #[test]
    fn policy_launch_snapshots_preserve_history_across_policy_and_pin_changes() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        let first = tree.resolve(&base);
        write_policy_snapshot(&tree.snapshots(), &first).unwrap();
        let snapshot = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(snapshot.fingerprint, first.fingerprint);
        assert!(!snapshot_outdated(Ok(Some(snapshot.clone())), &base, &[tree.repo()], &[]));
        let mut offline = base.clone();
        offline.network.mode = crate::policy::NetMode::None;
        assert!(snapshot_outdated(Ok(Some(snapshot)), &offline, &[tree.repo()], &[]), "the policy changed");
        let second = tree.resolve(&offline);
        write_policy_snapshot(&tree.snapshots(), &second).unwrap();
        let latest = read_policy_snapshot(&tree.snapshots()).unwrap().unwrap();
        assert_eq!(latest.policy.network.mode, crate::policy::NetMode::None);
        let archives: Vec<_> = std::fs::read_dir(tree.snapshots()).unwrap().flatten()
            .filter(|e| e.file_name() != "latest.json").collect();
        assert_eq!(archives.len(), 2);
        assert!(archives.into_iter().any(|entry| {
            let old: Resolution = serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            old.fingerprint == first.fingerprint && old.policy.network.mode == crate::policy::NetMode::Allow
        }));
        assert!(!snapshot_outdated(Ok(Some(latest.clone())), &offline, &[tree.repo()], &[]));
        // the pinned folders are an input too: one more, or none, is another build
        let other = tree.0.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(snapshot_outdated(Ok(Some(latest.clone())), &offline, &[tree.repo(), other], &[]), "a pin was added");
        assert!(snapshot_outdated(Ok(Some(latest)), &offline, &[], &[]), "the pin was removed");
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
    fn session_grants_apply_within_the_policys_limits_and_are_recorded_for_the_launch() {
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
        // A policy's prohibition blocks the remembered choice instead of being bypassed.
        let forbidding = Policy::parse("name='custom'\n[network]\nmode='allow'\n[permissions]\npublic_web='forbidden'").unwrap();
        let blocked = apply_grants(tree.resolve(&forbidding), &choices, hosts.clone());
        assert_eq!(blocked.policy.network.mode, crate::policy::NetMode::Allow);
        assert!(blocked.session_permissions[0].blocked.is_some());
        assert_eq!(blocked.session_hosts, hosts, "an approved host is not the public web");
        // Offline is offline: neither the choice nor the remembered host applies, and both stay recorded.
        let offline = Policy::parse("name='custom'\n[network]\nmode='none'").unwrap();
        let offline = apply_grants(tree.resolve(&offline), &choices, hosts.clone());
        assert!(offline.session_hosts.is_empty() && !offline.policy.network.allow.contains(&"example.com".to_string()));
        assert_eq!(offline.blocked_session_hosts, hosts);
        assert!(offline.session_hosts_reason.contains("offline") && offline.session_permissions[0].blocked.is_some());
        let decoded: Resolution = serde_json::from_slice(&serde_json::to_vec(&blocked).unwrap()).unwrap();
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
        // a link is refused even when it leads to a readable record
        std::fs::write(tree.repo().join("elsewhere.json"), serde_json::to_vec(&tree.resolve(&base)).unwrap()).unwrap();
        std::os::unix::fs::symlink(tree.repo().join("elsewhere.json"), tree.snapshots().join("latest.json")).unwrap();
        assert!(read_policy_snapshot(&tree.snapshots()).is_err());
    }

    /// A masked path inside a pinned or workspace repository: fine while Git never had it,
    /// refused once it was committed or staged, also after it was deleted again.
    #[test]
    fn a_mask_on_what_git_keeps_is_refused() {
        let tree = PolicyTestTree::new();
        let repo = tree.repo();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(repo.join("conf")).unwrap();
        std::fs::write(repo.join("tracked"), "t").unwrap();
        std::fs::write(repo.join("conf/.env"), "secret").unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        git(&["add", "tracked", ".gitignore"]);
        git(&["commit", "-qm", "one"]);
        let (env, pins) = (repo.join("conf/.env"), vec![repo.clone()]);
        // ignored and never committed: the mask is all there is to it; outside any pin nothing is asked
        masks_git_cannot_undo(&[env.clone()], &pins, &[]).unwrap();
        masks_git_cannot_undo(&[repo.join("tracked")], &[], &[]).unwrap();
        // a whole masked folder with nothing of Git's in it
        masks_git_cannot_undo(&[repo.join("conf")], &[], &pins).unwrap();
        // staged, then committed, then deleted: Git has it each time
        git(&["add", "-f", "conf/.env"]);
        assert!(masks_git_cannot_undo(&[env.clone()], &pins, &[]).unwrap_err().to_string().contains("Git has it"));
        git(&["commit", "-qm", "two"]);
        assert!(masks_git_cannot_undo(&[repo.join("conf")], &[], &pins).is_err(), "a workspace's repository, the folder masked");
        git(&["rm", "-q", "--cached", "conf/.env"]);
        git(&["commit", "-qm", "three"]);
        let error = masks_git_cannot_undo(&[env], &pins, &[]).unwrap_err().to_string();
        assert!(error.contains("Git has it") && error.contains(repo.to_str().unwrap()), "{error}");
    }

    /// Without a launch record there is no evidence of what a jail runs with, and a pinned
    /// folder that is gone cannot be built at all.
    #[test]
    fn a_jail_without_a_launch_record_is_never_current_and_a_missing_pin_fails_the_build() {
        let tree = PolicyTestTree::new();
        let base = Policy::builtin_default();
        assert!(snapshot_outdated(Ok(None), &base, &[tree.repo()], &[]));
        let error = resolve(&base, &[tree.0.join("gone")]).unwrap_err();
        assert!(format!("{error:#}").contains("resolve pinned path"), "{error:#}");
        // a pinned file is not a folder the jail binds: it changes nothing
        std::fs::write(tree.0.join("file"), "").unwrap();
        assert_eq!(resolve(&base, &[tree.repo(), tree.0.join("file")]).unwrap().fingerprint, tree.resolve(&base).fingerprint);
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
