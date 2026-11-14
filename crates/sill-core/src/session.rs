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

/// Entries at the top of an agent's home that a ghost's copy leaves out: the agent's own record of
/// earlier sessions (transcripts, history, its memory), logs, telemetry and caches. What is left is
/// the login, the settings, plugins and skills — what the agent needs to start as itself.
const GHOST_SKIP: &[&str] = &[
    "projects", "sessions", "session-env", "shell-snapshots", "telemetry", "backups", "history.jsonl", "todos",
    "file-history", "paste-cache", "debug", "statsig", "cache", "tmp", "thread-writer-locks", "session_index.jsonl",
    "memories", "GEMINI.md",
];
/// Name prefixes left out the same way (Codex keeps its history in versioned SQLite files).
const GHOST_SKIP_PREFIX: &[&str] = &["logs_", "memories_", "thread_history_", "goals_", "queue_", "state_"];

fn ghost_skips(name: &str) -> bool {
    GHOST_SKIP.contains(&name) || GHOST_SKIP_PREFIX.iter().any(|p| name.starts_with(p))
}

/// Seed a ghost's agent home from the agent's sill-owned home: each top-level entry copied unless
/// [`ghost_skips`] says it is history. A missing source leaves the copy empty (the usual seeding
/// from the host then runs in [`binds_for`]).
fn seed_ghost_home(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(to)?;
    let Ok(rd) = std::fs::read_dir(from) else { return Ok(()) };
    for top in rd.filter_map(Result::ok) {
        let src = top.path();
        let dst = to.join(top.file_name());
        if src.is_dir() {
            std::fs::DirBuilder::new().mode(0o700).create(&dst)?;
            for e in std::fs::read_dir(&src)?.filter_map(Result::ok) {
                if !ghost_skips(&e.file_name().to_string_lossy()) {
                    copy_tree(&e.path(), &dst.join(e.file_name()))?;
                }
            }
        } else {
            copy_tree(&src, &dst)?;
            let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
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

/// `ghost` (docs/memory.md §3.2): the agent home is a copy in the session's state dir
/// ([`ghost_agent_home`], seeded once by [`seed_ghost_home`]), adopted state is not bound (writes
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
            seed_ghost_home(&agent_home(home, &agent.id), &copy)?;
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
        let host_is_dir = inside.is_dir();
        if !owned.exists() {
            if host_is_dir || !inside.exists() {
                std::fs::DirBuilder::new().mode(0o700).create(&owned)?;
            } else {
                // a file path (~/.claude.json): seed from the host or start empty
                let content = std::fs::read(&inside).unwrap_or_else(|_| b"{}".to_vec());
                let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&owned)?;
                std::io::Write::write_all(&mut f, &content)?;
            }
            for seed in &agent.seed_files {
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

/// Where the shelf puts files attached to a prompt: inside the session's private `~/.cache`
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
#[derive(Debug, Clone)]
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
    let sock = socket_path(&opts.session)?;
    if shim::Control::connect(&sock).is_ok() {
        return Ok(Started { socket: sock, child: None });
    }
    let policy = Policy::load(&opts.policy, home)?;
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
    for f in ["bwrap.pid", "launch.pid"] {
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
    cmd.args(launch_args(opts)).arg("--child");
    if let Some(root) = &launcher.cgroup_root {
        cmd.arg("--cgroup-root").arg(root);
    }
    cmd.current_dir(&opts.cwd);
    let mut child = crate::launcher::spawn_child(cmd, log)?;
    for _ in 0..50 {
        if shim::Control::connect(&sock).is_ok() {
            return Ok(Started { socket: sock, child: Some(child) });
        }
        if let Ok(Some(st)) = child.try_wait() {
            bail!("session ended before its shim socket appeared ({st}); see {}", log_hint(&opts.session));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!("session started but the shim socket did not appear in 5 s; see {}", log_hint(&opts.session))
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
    // a jail started before the daemon owned sessions runs as a service unit of its own: stop that
    // too, or closing its tab after an upgrade would leave it running
    if launcher.scopes {
        crate::launcher::stop_legacy_unit(&unit_name(session));
    }
    let killed_group = if launcher.scopes {
        crate::launcher::stop_scope(&unit_name(session)).unwrap_or(false)
    } else {
        match &launcher.cgroup_root {
            Some(root) => crate::launcher::kill_session_cgroup(root, session).unwrap_or(false),
            None => false,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A ghost's agent home: the login, settings, plugins and skills, not the agent's history.
    #[test]
    fn ghost_home_leaves_history_out() {
        let dir = std::env::temp_dir().join(format!("sill-ghost-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let from = dir.join("from");
        for d in [".claude/projects/-home-me/memory", ".claude/telemetry", ".claude/plugins/p", ".codex/sessions"] {
            std::fs::create_dir_all(from.join(d)).unwrap();
        }
        for (f, t) in [
            (".claude/.credentials.json", "login"),
            (".claude/settings.json", "{}"),
            (".claude/history.jsonl", "old prompts"),
            (".claude/projects/-home-me/memory/MEMORY.md", "remembered"),
            (".claude/plugins/p/x", "plugin"),
            (".claude.json", "{\"a\":1}"),
            (".codex/auth.json", "codex login"),
            (".codex/state_5.sqlite", "state"),
            (".codex/config.toml", "model = 'x'"),
        ] {
            std::fs::write(from.join(f), t).unwrap();
        }
        let to = dir.join("to");
        seed_ghost_home(&from, &to).unwrap();
        for kept in [".claude/.credentials.json", ".claude/settings.json", ".claude/plugins/p/x", ".claude.json", ".codex/auth.json", ".codex/config.toml"] {
            assert!(to.join(kept).exists(), "{kept} copied");
        }
        for gone in [".claude/history.jsonl", ".claude/projects", ".claude/telemetry", ".codex/sessions", ".codex/state_5.sqlite"] {
            assert!(!to.join(gone).exists(), "{gone} left out");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&to).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(to.join(".claude.json")).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
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
