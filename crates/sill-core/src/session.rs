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
    /// Extra read-write binds (host, jail), see [`binds_for`].
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// A ghost session (docs/memory.md §3): `sill launch --ghost` tightens the policy
    /// (`Policy::ghostly`) and leaves the memory snapshot out unless Settings say otherwise.
    pub ghost: bool,
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
        .filter_map(|s| state_dir(uid, &s.id).ok())
        .map(|dir| ghost_agent_home(&dir, agent_id))
        .filter(|p| p.is_dir())
        .collect()
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

/// The host paths every *other* registry agent owns — its `home_paths` and `shared_files` (the
/// host's own `~/.codex/auth.json`, `~/.gemini/oauth_creds.json`), and whatever it adopted — that
/// exist on the host: masked in `agent`'s jail, so one agent never sees another's credentials
/// (SPEC §4.11 #8). With no current agent (`sill dev`), every agent's paths.
pub fn other_agents_paths(registry: &std::collections::BTreeMap<String, crate::agents::Agent>, agent: Option<&str>, home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for (id, a) in registry {
        if Some(id.as_str()) == agent {
            continue;
        }
        let own = a.home_paths.iter().chain(a.shared_files.iter()).map(|p| crate::paths::expand_home(p, home));
        for p in own.chain(adopted_binds(a, home).into_iter().map(|(_, inside)| inside)) {
            if p.exists() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
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

/// The policy a session's jail runs with: its own, tightened for a ghost (`Policy::ghostly`).
pub fn policy_of(s: &crate::store::Session, home: &Path) -> Result<Policy> {
    let mut p = Policy::load(&s.policy, home)?;
    if s.ghost {
        p.ghostly(home);
    }
    Ok(p)
}

/// `ghost` (docs/memory.md §3.2): the agent home is a copy in the session's state dir
/// ([`ghost_agent_home`], seeded with credential files only), adopted state is not bound (writes
/// there go to the overlay like any other) and pins are ignored — a ghost has none.
pub fn binds_for(
    agent: &crate::agents::Agent,
    policy: &Policy,
    state_dir: &Path,
    home: &Path,
    pins: &[PathBuf],
    ghost: bool,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    use crate::paths::{expand_home, expand_pattern, layer_name};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let mut out = Vec::new();
    // agent home: ~/.local/state/sill/agents/<id>/<basename of the path>. The agents' homes hold
    // credentials: 0700 from the `agents/` directory down, whatever the umask, on every start (a
    // home made before this was 0755/0664, SPEC §4.11 #8). A ghost's is a copy of it.
    let agents_base = if ghost {
        let copy = ghost_agent_home(state_dir, &agent.id);
        if !copy.exists() {
            let t0 = std::time::Instant::now();
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&copy)?;
            crate::credentials::seed(agent, &agent_home(home, &agent.id), &copy)?;
            eprintln!("sill: ghost agent home for {} copied in {} ms", agent.id, t0.elapsed().as_millis());
        }
        copy
    } else {
        agent_home(home, &agent.id)
    };
    let pins: &[PathBuf] = if ghost { &[] } else { pins };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&agents_base)?;
    for d in [agents_base.as_path()].into_iter().chain(agents_base.parent()) {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    for p in &agent.home_paths {
        let inside = expand_home(p, home);
        let name = inside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let owned = agents_base.join(&name);
        let file_path = inside.is_file() || agent.seed_files.iter().any(|seed| seed == p);
        if !owned.exists() {
            if !file_path {
                std::fs::DirBuilder::new().mode(0o700).create(&owned)?;
            } else {
                // a file path (~/.claude.json): seed from the host or start empty
                let content = if ghost { b"{}".to_vec() } else { std::fs::read(&inside).unwrap_or_else(|_| b"{}".to_vec()) };
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&owned)?;
                std::io::Write::write_all(&mut f, &content)?;
            }
            for seed in agent.seed_files.iter().filter(|_| !ghost) {
                let src = expand_home(seed, home);
                if let Ok(rel) = src.strip_prefix(&inside)
                    && owned.is_dir()
                    && src.is_file()
                    && let Ok(content) = std::fs::read(&src)
                {
                    // the seed is the login: written 0600, never through a link
                    let _ = crate::safe_fs::write_file_beneath(&owned, rel, &content, 0o600);
                }
            }
        }
        // bwrap needs the mount point to exist inside: a missing ~/.claude.json would be created as
        // a copy-up in the overlay, which is harmless (it is skipped by the ledger as a bind)
        out.push((owned, inside));
    }
    // adopted agent state (see `adopted_binds`): the agent's own files that landed in its home
    for (owned, inside) in if ghost { vec![] } else { adopted_binds(agent, home) } {
        if !out.iter().any(|(_, i)| *i == inside) {
            out.push((owned, inside));
        }
    }
    if ghost {
        // Registry agents have no curated home paths. Mount only private directories containing
        // their credential files, never the normal agent's adopted state.
        for rel in crate::credentials::files(agent, &agent_home(home, &agent.id))? {
            let parent = rel.parent().context("credential parent")?;
            let inside = home.join(parent);
            if !out.iter().any(|(_, p)| inside.starts_with(p)) {
                crate::safe_fs::open_dir_beneath(&agents_base, parent, true)?;
                out.push((agents_base.join(parent), inside));
            }
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
    let mut policy = effective_base_policy(name, home, ghost)?;
    if policy.network.mode != crate::policy::NetMode::None {
        if let Some(agent) = agent.and_then(|id| crate::agents::load(home).remove(id)) {
            policy.network.allow.extend(agent.domains);
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
pub fn next_build(row: &crate::store::Session, home: &Path, pins: &[PathBuf], ignore_roots: &[PathBuf]) -> Result<crate::policy_fragments::Resolution> {
    resolved_policy_with_grants(next_build_restrictions(row, home, pins, ignore_roots)?, &row.id)
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
    let base = session_base_policy(&opts.policy, home, opts.ghost, opts.agent.as_deref())?;
    let resolution = crate::policy_fragments::resolve(&base, &opts.pins, &opts.ignore_fragments)?;
    let resolution = resolved_policy_with_grants(resolution, &opts.session)?;
    resolution.policy.permissions.validate_launch(&resolution.policy.network)?;
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

    #[test]
    fn no_host_login_is_shared_into_jails_and_the_hosts_codex_login_stays_masked() {
        let registry: std::collections::BTreeMap<String, crate::agents::Agent> = crate::agents::builtin().into_iter().map(|a| (a.id.clone(), a)).collect();
        assert!(registry.values().all(|a| a.shared_files.is_empty()), "a built-in agent shares a host login file");
        let home = std::env::temp_dir().join(format!("sill-shared-login-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/auth.json"), "host login").unwrap();
        // the host's own Codex home (with its login) is hidden from every other agent's jail
        assert!(other_agents_paths(&registry, Some("claude"), &home).contains(&home.join(".codex")));
        // and Codex's own jail binds its sill-owned home there, never the host file over it
        let codex = &registry["codex"];
        assert!(!codex.shared_files.iter().any(|p| crate::paths::expand_home(p, &home) == home.join(".codex/auth.json")));
        let _ = std::fs::remove_dir_all(&home);
    }

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
