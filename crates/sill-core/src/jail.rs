//! Turn a policy and a session into a bubblewrap command line (SPEC §4.2).
//!
//! Layout inside the jail:
//! - `/` is bwrap's read-only tmpfs with the host's top-level symlinks (`/bin → usr/bin`, …);
//! - every top-level directory of the host and every persistent mount below one gets its own overlay
//!   (`--overlay-src D --overlay upper work D`), upper/work on the session's state dir;
//! - `/proc /dev /tmp /run` are fresh, `/sys` read-only;
//! - `/.sill/host/…` is the real tree read-only (an overlay per layer, no bind of a host directory,
//!   so no mount made later reaches it) for revert and diffs;
//! - masked paths are tmpfs/`/dev/null` in both views; `direct` paths are bound through; `ro` read-only.
//!
//! Nothing here runs bwrap: [`JailSpec::plan`] returns the argv and the layer list, [`prepare`] creates
//! the upper/work directories. The caller (CLI, unit launcher) execs it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::agent_storage::{AgentMask, PolicyPaths};
use crate::agents::HomePathKind;
use crate::mounts::{self, Mount};
use crate::paths::{expand_pattern, is_under, layer_name};
use crate::policy::{NetMode, Policy};

/// Every mount of the host view read-only, after the last mask and bind laid in it (`laid` is
/// the argv from the host view on): its overlays, its tmpfs directories and the tmpfs masks laid
/// over them are writable until then, and the shim's Revert copies from here, so a file an agent
/// could put in a mask would be restored as the host's. A mount below a later one is out of
/// reach and has no path left to remount; one under a later mount at the same path is that mount.
fn seal_host_view(a: &mut Vec<OsString>, laid: usize, layers: &[Layer], expanded: &[PathBuf]) {
    // (position, path, a tmpfs) of what was laid in the view after its own layout
    let later: Vec<(usize, PathBuf, bool)> = (laid..a.len())
        .filter(|&i| Path::new(&a[i]).starts_with(HOST_VIEW) && i > 0)
        .map(|i| (i, PathBuf::from(&a[i]), a[i - 1] == "--tmpfs"))
        .collect();
    let mut seal: Vec<PathBuf> = Vec::new();
    for d in expanded.iter().chain(layers.iter().map(|l| &l.host)) {
        let at = host_view(d);
        if !later.iter().any(|(_, c, _)| *c != at && at.starts_with(c)) {
            seal.push(at);
        }
    }
    for (i, at, tmpfs) in &later {
        if *tmpfs && !later.iter().any(|(j, c, _)| j > i && c != at && at.starts_with(c)) && !seal.contains(at) {
            seal.push(at.clone());
        }
    }
    for at in seal {
        push(a, &[&"--remount-ro", &at]);
    }
}

/// Directories that are never overlaid: the jail gets fresh ones.
const FRESH: &[&str] = &["proc", "dev", "tmp", "run"];
/// Read-only bind instead of overlay.
const RO_BIND: &[&str] = &["sys"];
/// Skipped entirely.
const SKIP: &[&str] = &["lost+found"];
/// Where the host tree is visible read-only inside the jail.
pub const HOST_VIEW: &str = "/.sill/host";
/// The sill state filesystem (SPEC §4.9): never a layer, never visible — it holds every session's
/// upper trees.
pub const STATE_FS: &str = "/var/lib/sill";
/// Where the session proxy's socket is bound inside the jail.
pub const PROXY_SOCKET: &str = "/.sill/proxy.sock";
/// Host control's bridge directory in the jail (docs/permission-modes.md): the daemon listens on
/// `host.sock` in it only while the session's Host control is active. The directory is bound, not
/// the socket, so a socket made later (by a restarted daemon) shows in a running jail.
pub const BRIDGE_DIR: &str = "/.sill/bridge";
pub const BRIDGE_SOCKET: &str = "host.sock";
/// Under [`BRIDGE_DIR`]: the session's host processes for the jail's `sill-proc` (`processes.json`
/// and `<id>.log`), kept after revocation; and `view/`, the pictures Host control's `view` took.
pub const BRIDGE_HOST_DIR: &str = "host";
/// Loopback port the shim forwards to the proxy socket.
pub const PROXY_PORT: u16 = 3128;

#[derive(Debug, Clone)]
pub struct JailSpec {
    pub policy: Policy,
    pub home: PathBuf,
    pub user: String,
    pub uid: u32,
    /// Session state dir; `upper/` and `work/` are created below it.
    pub state_dir: PathBuf,
    /// Allow a state dir that lies inside an overlaid tree (development only; SPEC §4.9).
    pub unsafe_state: bool,
    pub cwd: PathBuf,
    /// Session environment (applied after the policy's `jail.env`).
    pub env: BTreeMap<String, String>,
    /// Keep the host network namespace. Only until the proxy exists (SPEC §4.8); ignored when the
    /// policy says `none`.
    pub share_net: bool,
    /// Host mount table (`mounts::host_mounts()`), injectable for tests.
    pub mounts: Vec<Mount>,
    /// Top-level entries of `/` (name, symlink target if any, is_dir), injectable for tests.
    pub root_entries: Vec<RootEntry>,
    /// Host path of the session proxy's Unix socket (SPEC §4.8). Bound at `/.sill/proxy.sock`; the
    /// environment points HTTP(S)_PROXY at `127.0.0.1:<proxy_port>`, which the shim forwards.
    pub proxy_socket: Option<PathBuf>,
    pub proxy_port: u16,
    /// Extra read-write binds (host path, jail path): agent home, session-private scratch. Applied
    /// after the overlays and masks; nothing below them is recorded.
    pub binds: Vec<(PathBuf, PathBuf)>,
    /// Additional masks for standalone CLI jails. Missing paths are skipped.
    pub masks: Vec<PathBuf>,
    /// Original host agent roots, separate from policy masks.
    pub agent_masks: Vec<(PathBuf, crate::agents::HomePathKind)>,
    /// Expanded policy paths from the immutable launch plan; None only for standalone CLI jails,
    /// whose policy is expanded when the plan is made.
    pub policy_paths: Option<PolicyPaths>,
    /// Targets of validated private agent bindings.
    pub agent_binds: Vec<(PathBuf, crate::agents::HomePathKind)>,
    /// The memory snapshot's directory (docs/memory.md §2.3), bound read-only at
    /// `sill_memory::JAIL_DIR` for the jail's `sill-memory`; None = no memory in this jail.
    pub memory_dir: Option<PathBuf>,
    /// Host path of the Host control bridge directory, bound read-only at [`BRIDGE_DIR`].
    pub bridge_dir: Option<PathBuf>,
    /// A workspace session's repositories (docs/parallel-coding.md §12): (the person's checkout,
    /// the clone bound in its place, the checkout's object store to borrow read-only).
    pub workspace_mask: Vec<(PathBuf, PathBuf, PathBuf)>,
    /// An inheritable fd holding the seccomp program (`seccomp::memfd`), given to bwrap as
    /// `--seccomp <fd>` when the policy's `jail.seccomp` is on (security.md S18); None = no filter.
    pub seccomp_fd: Option<i32>,
    /// Extra arguments inserted before `--`.
    pub extra_args: Vec<OsString>,
    pub command: Vec<OsString>,
}

#[derive(Debug, Clone)]
pub struct RootEntry {
    pub name: OsString,
    pub symlink: Option<PathBuf>,
    pub is_dir: bool,
    /// The user can list it (overlayfs needs a readable lower; `/root` usually isn't).
    pub readable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// Host path (top-level dir or mount point) this overlay covers.
    pub host: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
}

#[derive(Debug, Clone)]
pub struct JailPlan {
    pub argv: Vec<OsString>,
    /// Anchored repository sources, held until bubblewrap consumes --ro-bind-fd.
    /// Descriptors stay CLOEXEC in the parent; use command() when executing this plan.
    pub mount_fds: Vec<Arc<OwnedFd>>,
    pub layers: Vec<Layer>,
    /// Directories that contain a mount point and were split into per-child mounts.
    pub expanded: Vec<PathBuf>,
    pub masked: Vec<PathBuf>,
    pub direct: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn read_root_entries() -> Result<Vec<RootEntry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/").context("read /")? {
        let e = e?;
        let name = e.file_name();
        let md = e.path().symlink_metadata()?;
        let symlink = if md.file_type().is_symlink() { Some(std::fs::read_link(e.path())?) } else { None };
        let readable = md.is_dir() && std::fs::read_dir(e.path()).is_ok();
        out.push(RootEntry { name, symlink, is_dir: md.is_dir(), readable });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A folder bound into the jail in its own place: a pin, or a workspace's clone at its
/// repository's path (`backing` is where its files really are; `borrowed` the object store's
/// place in the jail, for a workspace).
struct Bound {
    at: PathBuf,
    backing: PathBuf,
    borrowed: Option<PathBuf>,
}

/// The jail's `objects/info/alternates` of a workspace at `source`: the borrowed store's path,
/// from memory, read-only over the clone's own file.
fn alternates(args: &mut Vec<OsString>, source: &Path, borrowed: &Path) -> Result<Arc<OwnedFd>> {
    let memfd = rustix::fs::memfd_create("sill-alternates", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::io::write(&memfd, format!("{}\n", borrowed.display()).as_bytes())?;
    rustix::fs::seek(&memfd, rustix::fs::SeekFrom::Start(0))?;
    let memfd = Arc::new(rustix::io::fcntl_dupfd_cloexec(&memfd, 64)?);
    args.extend([OsString::from("--ro-bind-data"), memfd.as_raw_fd().to_string().into(), source.join(".git/objects/info/alternates").into_os_string()]);
    Ok(memfd)
}

/// Refuse a mount at `inside` that lies under a masked path (it would expose what the mask hides)
/// or above one (it would bury the mask under a real tree).
fn no_bind_across_mask(inside: &Path, masked: &[PathBuf]) -> Result<()> {
    if let Some(m) = masked.iter().find(|m| is_under(inside, m) || is_under(m, inside)) {
        anyhow::bail!(
            "{}: not mounted — it would {} the masked {} (a pin, a `direct`/`ro` path or an agent's files may not cross a `mask`)",
            inside.display(),
            if is_under(inside, m) { "sit inside" } else { "cover" },
            m.display()
        );
    }
    Ok(())
}

fn push(a: &mut Vec<OsString>, items: &[&dyn AsRef<std::ffi::OsStr>]) {
    a.extend(items.iter().map(|i| i.as_ref().to_os_string()));
}

/// A mask in one view: a tmpfs over a directory, `/dev/null` over a file.
fn push_mask(a: &mut Vec<OsString>, target: &Path, is_dir: bool) {
    if is_dir {
        push(a, &[&"--tmpfs", &target]);
    } else {
        push(a, &[&"--ro-bind", &"/dev/null", &target]);
    }
}

fn open_no_symlinks(path: &Path, flags: rustix::fs::OFlags) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
    openat2(CWD, path, OFlags::PATH | OFlags::CLOEXEC | flags, Mode::empty(), ResolveFlags::NO_SYMLINKS)
}

fn kind_matches(expected: HomePathKind, actual: rustix::fs::FileType) -> bool {
    use rustix::fs::FileType;
    matches!((expected, actual), (HomePathKind::Directory, FileType::Directory) | (HomePathKind::File, FileType::RegularFile))
}

/// How the host's tree is laid into a jail: which directories get an overlay, and what the rest
/// becomes. It follows from the mount table and the root's entries alone, so the launcher and a
/// later reader of the session's upper trees agree as long as the mounts do.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Parents before children (mount order).
    pub layers: Vec<Layer>,
    /// Directories that contain a mount point and were split into per-child mounts.
    pub expanded: Vec<PathBuf>,
    /// Bound read-only: `/sys`, non-persistent mounts, files of an expanded directory.
    ro_binds: Vec<PathBuf>,
    /// (target, path in the jail without its leading `/`).
    symlinks: Vec<(PathBuf, PathBuf)>,
    pub warnings: Vec<String>,
}

/// Top-level directories, then persistent mounts below them. A directory that has a mount point
/// somewhere inside it can't be an overlay lower (the kernel refuses it), so it is expanded: it
/// becomes a tmpfs in the jail and each child is handled on its own (dirs → overlay or expand
/// again, symlinks → symlink, files → read-only bind).
pub fn layout(policy: &Policy, home: &Path, state_dir: &Path, mounts: &[Mount], root_entries: &[RootEntry]) -> Result<Layout> {
    struct Walk<'a> {
        mounts: &'a [Mount],
        real: Vec<Mount>,
        hidden: Vec<PathBuf>,
        state_dir: &'a Path,
        out: Layout,
    }
    impl Walk<'_> {
        fn dir(&mut self, path: &Path) {
            // hidden mounts; the state fs and anything inside the session's own state dir: never
            // overlaid, never visible
            if self.hidden.iter().any(|h| is_under(path, h)) || is_under(path, Path::new(STATE_FS)) || is_under(path, self.state_dir) {
                return;
            }
            let is_mount = self.mounts.iter().any(|m| m.mount_point == path);
            // non-persistent fs (squashfs, fuse), or one overlayfs refuses as a lower (vfat):
            // read-only
            if is_mount && self.real.iter().find(|r| r.mount_point == path).is_none_or(|r| !mounts::overlayable(r)) {
                self.out.ro_binds.push(path.to_path_buf());
                return;
            }
            if !self.mounts.iter().any(|m| m.mount_point != path && is_under(&m.mount_point, path)) {
                self.out.layers.push(layer_for(self.state_dir, path));
                return;
            }
            let Ok(rd) = std::fs::read_dir(path) else {
                self.out.warnings.push(format!("{} not readable, left out of the jail", path.display()));
                return;
            };
            self.out.expanded.push(path.to_path_buf());
            let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let child = e.path();
                let Ok(md) = child.symlink_metadata() else { continue };
                if md.file_type().is_symlink() {
                    if let Ok(t) = std::fs::read_link(&child) {
                        self.out.symlinks.push((t, child.strip_prefix("/").unwrap_or(&child).to_path_buf()));
                    }
                } else if md.is_dir() {
                    if std::fs::read_dir(&child).is_err() {
                        continue; // unreadable (e.g. root-only): absent in the jail
                    }
                    self.dir(&child);
                } else {
                    self.out.ro_binds.push(child);
                }
            }
        }
    }
    let mut hidden: Vec<PathBuf> = policy.paths.hidden_mounts.iter().map(|h| crate::paths::expand_home(h, home)).collect();
    let mut warnings = Vec::new();
    // The state filesystem at another mount point is the state filesystem: hidden like it. One
    // that holds the home or the root cannot be hidden; the launch says so instead.
    for alias in mounts::state_aliases(mounts, Path::new(STATE_FS)) {
        if alias == Path::new("/") || is_under(home, &alias) {
            warnings.push(format!(
                "the sill state filesystem is also mounted at {}, which holds your files and cannot be hidden: every session's files are readable in this jail. Give sill a state filesystem of its own.",
                alias.display()
            ));
        } else {
            hidden.push(alias);
        }
    }
    let mut walk = Walk {
        mounts,
        real: mounts::real_mounts(mounts),
        hidden,
        state_dir,
        out: Layout { warnings, ..Layout::default() },
    };
    for e in root_entries {
        let path = PathBuf::from("/").join(&e.name);
        if let Some(target) = &e.symlink {
            walk.out.symlinks.push((target.clone(), PathBuf::from(&e.name)));
            continue;
        }
        if !e.is_dir || SKIP.iter().any(|n| e.name == *n) || FRESH.iter().any(|n| e.name == *n) {
            continue;
        }
        if RO_BIND.iter().any(|n| e.name == *n) {
            walk.out.ro_binds.push(path);
            continue;
        }
        if !e.readable {
            walk.out.warnings.push(format!("{} not readable, left out of the jail", path.display()));
            continue;
        }
        walk.dir(&path);
    }
    let mut out = walk.out;
    out.layers.sort_by(|a, b| a.host.cmp(&b.host));
    out.expanded.sort();
    // Never let distinct host roots reuse an upper tree (/a/b and /a%b both flatten to a%b).
    let mut owners = BTreeMap::new();
    for layer in &out.layers {
        if let Some(previous) = owners.insert(layer.upper.clone(), layer.host.clone())
            && previous != layer.host
        {
            bail!("overlay state name collision between {} and {}; conflicting mount layouts are unsupported", previous.display(), layer.host.display());
        }
    }
    Ok(out)
}

fn layer_for(state_dir: &Path, host: &Path) -> Layer {
    let name = layer_name(host);
    Layer { host: host.to_path_buf(), upper: state_dir.join("upper").join(&name), work: state_dir.join("work").join(&name) }
}

/// What the masks laid in a jail were, for the binds that follow them.
struct Masked {
    /// The policy's and sill's own, with whether each is a directory.
    policy: Vec<(PathBuf, bool)>,
    /// The agents' original host data.
    agents: Vec<PathBuf>,
}

impl Masked {
    fn policy_paths(&self) -> Vec<PathBuf> {
        self.policy.iter().map(|(path, _)| path.clone()).collect()
    }
}

impl JailSpec {
    pub fn plan(&self) -> Result<JailPlan> {
        self.policy.permissions.validate_launch(&self.policy.network)?;
        let p = &self.policy;
        let Layout { layers, expanded, ro_binds, symlinks, mut warnings } = layout(p, &self.home, &self.state_dir, &self.mounts, &self.root_entries)?;
        self.check_state_dir(&layers, &expanded, &mut warnings)?;
        let paths = match &self.policy_paths {
            Some(paths) => paths.clone(),
            None => self.expanded_policy_paths()?,
        };

        let mut a: Vec<OsString> = Vec::new();
        push(&mut a, &[&"bwrap", &"--unshare-all"]);
        if self.share_net && p.network.mode != NetMode::None {
            push(&mut a, &[&"--share-net"]);
            warnings.push("network shared with the host (no proxy yet)".into());
        }
        if !p.jail.allow_userns {
            // bwrap checks for the explicit flag even though --unshare-all implies it
            push(&mut a, &[&"--unshare-user", &"--disable-userns"]);
        }
        // the jail never outlives its launcher: without it there is no proxy, no test desktop and
        // nobody waiting on the jail (the launcher spawns bwrap from the thread that waits on it)
        push(&mut a, &[&"--die-with-parent", &"--new-session", &"--cap-drop", &"ALL", &"--clearenv"]);
        if p.jail.seccomp && let Some(fd) = self.seccomp_fd {
            push(&mut a, &[&"--seccomp", &fd.to_string()]);
        }
        for (k, v) in &self.environment() {
            push(&mut a, &[&"--setenv", k, v]);
        }

        // root symlinks, fresh dirs, /sys
        for (target, name) in symlinks.iter().filter(|(_, n)| n.components().count() == 1) {
            push(&mut a, &[&"--symlink", target, &Path::new("/").join(name)]);
        }
        push(&mut a, &[&"--proc", &"/proc", &"--dev", &"/dev", &"--tmpfs", &"/tmp", &"--tmpfs", &"/run"]);
        if p.permissions.gpu == crate::permissions::Rule::On {
            let gpu = crate::gpu::probe();
            let node = gpu.node.ok_or_else(|| anyhow::anyhow!("{}", gpu.missing))?;
            push(&mut a, &[&"--dev-bind", &node, &node]);
        }
        // expanded directories (contain a mount point): tmpfs, then their children below
        for d in &expanded {
            push(&mut a, &[&"--tmpfs", d]);
        }
        for (target, name) in symlinks.iter().filter(|(_, n)| n.components().count() > 1) {
            push(&mut a, &[&"--symlink", target, &Path::new("/").join(name)]);
        }
        push(&mut a, &[&"--dir", &format!("/run/user/{}", self.uid)]);
        for r in &ro_binds {
            push(&mut a, &[&"--ro-bind", r, r]);
        }
        for l in &layers {
            push(&mut a, &[&"--overlay-src", &l.host, &"--overlay", &l.upper, &l.work, &l.host]);
        }
        self.lay_host_view(&mut a, &layers, &expanded, &ro_binds, &symlinks);
        let host_view_laid = a.len();

        let sill_own = crate::paths::sill_own(&self.home)?;
        let masked = Masked { policy: self.lay_policy_masks(&mut a, &paths)?, agents: self.lay_agent_masks(&mut a)? };
        let mut mount_fds = Vec::new();
        // what the jail needs from sill's state, back over the mask: the adapters it runs
        let tools = crate::paths::sill_state(&self.home).join("tools");
        if tools.is_dir() {
            for target in [tools.clone(), host_view(&tools)] {
                push(&mut a, &[&"--ro-bind", &tools, &target]);
            }
        }
        let ro_paths = paths.read_only.iter().map(|p| p.to_path()).collect::<Result<Vec<_>>>()?;
        let mut direct = self.lay_binds(&mut a, &paths, &ro_paths, &masked, &sill_own, &mut mount_fds)?;

        if let Some(sock) = &self.proxy_socket {
            push(&mut a, &[&"--ro-bind", sock, &PROXY_SOCKET]);
        }
        // the memory snapshot: its directory, so a snapshot renamed into it later shows too
        if let Some(dir) = self.memory_dir.as_ref().filter(|d| d.is_dir()) {
            push(&mut a, &[&"--ro-bind", dir, &sill_memory::JAIL_DIR]);
        }
        if let Some(dir) = self.bridge_dir.as_ref().filter(|d| d.is_dir()) {
            push(&mut a, &[&"--ro-bind", dir, &BRIDGE_DIR]);
        }
        a.extend(self.extra_args.iter().cloned());
        let policy_masked = masked.policy_paths();
        let mut all_masked = policy_masked.clone();
        all_masked.extend(masked.agents.iter().cloned());
        let (fds, mut bound) = self.apply_workspace_binds(&mut a, &all_masked, &mut direct, &mut warnings)?;
        mount_fds.extend(fds);
        bound.extend(self.binds.iter().filter(|(host, inside)| host == inside).map(|(_, inside)| Bound { at: inside.clone(), backing: inside.clone(), borrowed: None }));
        mount_fds.extend(self.apply_rules_inside_binds(&mut a, &policy_masked, &ro_paths, &masked.agents, &bound, &mut warnings)?);
        seal_host_view(&mut a, host_view_laid, &layers, &expanded);
        // bwrap's root tmpfs is writable by default; make it read-only now that every mount is in place
        push(&mut a, &[&"--remount-ro", &"/"]);
        push(&mut a, &[&"--chdir", &self.cwd]);
        a.push("--".into());
        if self.command.is_empty() {
            bail!("no command for the jail");
        }
        a.extend(self.command.iter().cloned());

        Ok(JailPlan { argv: a, mount_fds, layers, expanded, masked: all_masked, direct, warnings })
    }

    /// The upper trees must not be reachable through a layer of the jail they belong to.
    fn check_state_dir(&self, layers: &[Layer], expanded: &[PathBuf], warnings: &mut Vec<String>) -> Result<()> {
        let state_in_tree = layers.iter().map(|l| &l.host).chain(expanded).find(|h| is_under(&self.state_dir, h));
        let state_on_submount = self.mounts.iter().any(|m| {
            m.mount_point != Path::new("/")
                && is_under(&self.state_dir, &m.mount_point)
                && !layers.iter().any(|l| l.host == m.mount_point)
        });
        if let Some(tree) = state_in_tree
            && !state_on_submount
        {
            let msg = format!(
                "state dir {} lies inside overlaid tree {} on the same filesystem (SPEC §4.9: use the sill state filesystem)",
                self.state_dir.display(),
                tree.display()
            );
            if self.unsafe_state { warnings.push(msg) } else { bail!(msg) }
        }
        Ok(())
    }

    /// The policy's path rules expanded now, for a jail without a frozen filesystem plan (the
    /// `sill jail` command): its masks, sill's own files and the other agents' paths that exist.
    fn expanded_policy_paths(&self) -> Result<PolicyPaths> {
        use sill_proto::sessions::HostPath;
        let paths = &self.policy.paths;
        let own = crate::paths::sill_own(&self.home)?.into_iter().chain(self.masks.iter().cloned()).filter(|p| p.exists());
        let expand = |patterns: &[String]| patterns.iter().flat_map(|pat| expand_pattern(pat, &self.home)).map(|p| HostPath::from_path(&p)).collect();
        Ok(PolicyPaths {
            masks: paths.mask.iter().map(|pat| crate::paths::mask_root(pat, &self.home)).chain(own)
                .map(|p| AgentMask { kind: if p.is_file() { HomePathKind::File } else { HomePathKind::Directory }, path: HostPath::from_path(&p) })
                .collect(),
            direct: expand(&paths.direct),
            read_only: expand(&paths.ro),
        })
    }

    fn environment(&self) -> BTreeMap<String, String> {
        let p = &self.policy;
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let home = self.home.to_string_lossy().into_owned();
        env.insert(
            "PATH".into(),
            // /.sill/bin: `sill-proc` (docs/tasks.md §2.4), the shim under that name
            format!("/.sill/bin:{home}/.local/bin:{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
        );
        env.insert("HOME".into(), home.clone());
        env.insert("USER".into(), self.user.clone());
        env.insert("LOGNAME".into(), self.user.clone());
        env.insert("SHELL".into(), "/bin/bash".into());
        // under the daemon's systemd unit TERM arrives empty, which leaves TUIs (login prompts) unable
        // to draw: an empty value counts as unset
        env.insert("TERM".into(), std::env::var("TERM").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "xterm-256color".into()));
        if let Ok(lang) = std::env::var("LANG") {
            env.insert("LANG".into(), lang);
        }
        env.insert("XDG_RUNTIME_DIR".into(), format!("/run/user/{}", self.uid));
        env.insert("SILL_SESSION".into(), "1".into());
        if self.proxy_socket.is_some() && !(self.share_net && p.network.mode != NetMode::None) {
            let url = format!("http://127.0.0.1:{}", self.proxy_port);
            for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"] {
                env.insert(k.into(), url.clone());
            }
            // the jail has its own network namespace: its loopback is the jail's own (a dev server
            // the agent started), never the host's, so it stays direct; the proxy refuses loopback
            for k in ["NO_PROXY", "no_proxy"] {
                env.insert(k.into(), "localhost,127.0.0.1,::1".into());
            }
            // Node's fetch (undici) ignores the variables above unless asked: Gemini CLI's API calls
            // then went direct, which the jail cannot, and its prompts hung (2026-11-03)
            env.insert("NODE_USE_ENV_PROXY".into(), "1".into());
        }
        env.extend(p.jail.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        // Last, over anything a policy or agent sets: the buses are dead addresses (never unset:
        // nothing autolaunches one) and no host bus socket is shared. Desktop development's
        // private bus is the one the shim starts inside the jail (`sill-shim --bus`).
        env.insert("DBUS_SYSTEM_BUS_ADDRESS".into(), "disabled:".into());
        env.insert(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            if p.permissions.private_bus == crate::permissions::Rule::On { crate::bus::address(self.uid) } else { "disabled:".into() },
        );
        env
    }

    /// The host view: the jail's own layout once more, read-only and without the session's
    /// writes. Nothing in it is a bind of a host directory: a bind stays a slave of the host's
    /// mount, so a filesystem mounted on the host later would arrive in it with its own flags,
    /// writable (audit A3). A layer is an overlay of the directory itself, which shows no mount
    /// below it, now or later; a directory that holds mount points is a tmpfs with its children
    /// laid one by one, as in the jail. What the jail leaves out is absent here too.
    fn lay_host_view(&self, a: &mut Vec<OsString>, layers: &[Layer], expanded: &[PathBuf], ro_binds: &[PathBuf], symlinks: &[(PathBuf, PathBuf)]) {
        push(a, &[&"--dir", &HOST_VIEW]);
        for d in expanded {
            push(a, &[&"--tmpfs", &host_view(d)]);
        }
        for (target, name) in symlinks.iter().filter(|(_, n)| n.components().count() > 1) {
            push(a, &[&"--symlink", target, &Path::new(HOST_VIEW).join(name)]);
        }
        // the plain files of an expanded directory; a mount bound read-only in the jail (/sys, a
        // FUSE or FAT filesystem) is a recursive bind and stays out
        for f in ro_binds {
            if f.parent().is_some_and(|parent| expanded.iter().any(|d| d == parent)) && !mounts::is_mount_point(&self.mounts, f) {
                push(a, &[&"--ro-bind", f, &host_view(f)]);
            }
        }
        // an overlay whose writes would go to a tmpfs of bwrap's own; `seal_host_view` makes it
        // read-only once the masks are in it (bwrap's read-only overlay wants a second source)
        for l in layers {
            push(a, &[&"--overlay-src", &l.host, &"--tmp-overlay", &host_view(&l.host)]);
        }
        if self.unsafe_state {
            // the agent must not reach its own upper trees
            push(a, &[&"--tmpfs", &self.state_dir, &"--tmpfs", &host_view(&self.state_dir)]);
        }
    }

    /// The policy's masks in both views. Sill's own files are among them whatever the policy
    /// says: its configuration (policies, agent overrides — a session's mounts and allowed domains
    /// are made of them) and its whole state directory — the session store, the staged binaries
    /// (security.md S2), every agent's sill-owned home (`agents/`, the login scratch), the
    /// daemon's lock, and whatever is put there next: an allow-list, so a new file is hidden until
    /// someone decides otherwise. The current agent gets its own home bound at `~/.claude` etc.
    /// from the host side, the others' stay out of sight (SPEC §4.11 #8); the installed adapters
    /// (`tools/`) are bound back read-only after this, the agent runs from there.
    fn lay_policy_masks(&self, a: &mut Vec<OsString>, paths: &PolicyPaths) -> Result<Vec<(PathBuf, bool)>> {
        let mut masks: Vec<(PathBuf, bool)> = paths.masks.iter().map(|m| Ok((m.path.to_path()?, m.kind == HomePathKind::Directory))).collect::<Result<_>>()?;
        masks.sort_by(|a, b| a.0.cmp(&b.0));
        let mut masked: Vec<(PathBuf, bool)> = Vec::new();
        let mut host_masks: Vec<PathBuf> = Vec::new();
        for (path, is_dir) in masks {
            if masked.iter().any(|(m, _)| path.starts_with(m)) { continue; }
            anyhow::ensure!(path.is_absolute() && path != Path::new("/"), "mask must have an absolute, non-root literal prefix: {}", path.display());
            // The host view is read-only: for an absent target hide its nearest existing
            // ancestor, rather than omit the mask and expose files created later.
            let existing = path.ancestors().find(|p| p.symlink_metadata().is_ok()).context("mask has no existing ancestor")?;
            open_no_symlinks(existing, rustix::fs::OFlags::empty())
                .with_context(|| format!("mask {} must not contain symlinks", path.display()))?;
            push_mask(a, &path, is_dir);
            host_masks.push(existing.to_path_buf());
            masked.push((path, is_dir));
        }
        host_masks.sort();
        let mut hidden: Vec<PathBuf> = Vec::new();
        for path in host_masks {
            if hidden.iter().any(|m| path.starts_with(m)) { continue; }
            push_mask(a, &host_view(&path), path.is_dir());
            hidden.push(path);
        }
        Ok(masked)
    }

    /// Host agent originals are never reachable through the host view. A selected agent's
    /// private binding may replace an agent mask, but never a policy secret mask.
    fn lay_agent_masks(&self, a: &mut Vec<OsString>) -> Result<Vec<PathBuf>> {
        let mut agent_masked: Vec<PathBuf> = Vec::new();
        let mut roots = self.agent_masks.clone();
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, kind) in roots {
            // Validate the nearest existing component; never follow an original-home symlink.
            let existing = path.ancestors().find(|p| p.symlink_metadata().is_ok()).context("agent root has no existing ancestor")?;
            let fd = open_no_symlinks(existing, rustix::fs::OFlags::empty())
                .with_context(|| format!("host agent root {} must not contain symlinks", path.display()))?;
            if existing == path {
                let actual = rustix::fs::FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode);
                anyhow::ensure!(kind_matches(kind, actual), "host agent root has the wrong type: {}", path.display());
            }
            if agent_masked.iter().any(|p| path.starts_with(p)) { continue; }
            // An absent root is masked in the jail's own view only (in case it appears on the host
            // later): the host view is read-only, so bwrap could not make a mount point there for
            // it (found 2026-11-07: `~/.gemini`, declared by the built-in gemini, never created).
            push_mask(a, &path, kind == HomePathKind::Directory);
            if existing == path {
                push_mask(a, &host_view(&path), kind == HomePathKind::Directory);
            }
            agent_masked.push(path);
        }
        Ok(agent_masked)
    }

    /// The policy's `direct` and `ro` paths, then the spec's binds; returns what is bound through
    /// to the host or to sill's own trees. A bind inside a mask puts the masked thing back, a bind
    /// over a mask buries it: neither is mounted, whatever a policy or an agent override asks for
    /// (security.md S2). A pin is the one bind that may hold a path the policy masks: the mask is
    /// laid again after it (`apply_rules_inside_binds`). Inside a mask, or over sill's own files
    /// or another agent's, a pin is refused like the rest.
    fn lay_binds(&self, a: &mut Vec<OsString>, paths: &PolicyPaths, ro_paths: &[PathBuf], masked: &Masked, sill_own: &[PathBuf], mount_fds: &mut Vec<Arc<OwnedFd>>) -> Result<Vec<PathBuf>> {
        let policy_masked = masked.policy_paths();
        let agent_masked = &masked.agents;
        let mut direct = Vec::new();
        for path in paths.direct.iter().map(|p| p.to_path()) {
            let path = path?;
            no_bind_across_mask(&path, &policy_masked)?;
            no_bind_across_mask(&path, agent_masked)?;
            push(a, &[&"--bind", &path, &path]);
            direct.push(path);
        }
        for path in ro_paths {
            no_bind_across_mask(path, &policy_masked)?;
            no_bind_across_mask(path, agent_masked)?;
            push(a, &[&"--ro-bind", path, path]);
        }
        for (host, inside) in &self.binds {
            if host == inside {
                // the host's own tree (a pin, a shared file): inside a mask it re-exposes
                if let Some(m) = policy_masked.iter().find(|m| is_under(inside, m)) {
                    no_bind_across_mask(inside, std::slice::from_ref(m))?;
                }
                no_bind_across_mask(inside, sill_own)?;
                no_bind_across_mask(inside, agent_masked)?;
                push(a, &[&"--bind", host, inside]);
            } else {
                // sill's own tree in the host path's place (the session's private scratch, the
                // agent's sill-owned home): nothing of the host's below it is exposed, so a mask
                // beneath it is not buried but laid again on top — the masked path stays hidden
                // inside the bound tree too. Only a declared private agent profile may sit
                // below a wildcard's conservative parent mask; scratch and host copies cannot.
                let agent_bind = self.agent_binds.iter().find(|(target, _)| target == inside);
                for m in policy_masked.iter().filter(|m| is_under(inside, m)) {
                    if agent_bind.is_none() || !crate::agent_storage::private_under_wildcard_mask(&self.policy, &self.home, inside, m) {
                        no_bind_across_mask(inside, std::slice::from_ref(m))?;
                    }
                }
                if let Some((_, expected)) = agent_bind {
                    no_bind_across_mask(inside, sill_own)?;
                    no_bind_across_mask(inside, &self.masks)?;
                    let fd = open_no_symlinks(host, rustix::fs::OFlags::NOFOLLOW)
                        .with_context(|| format!("open declared agent storage {}", host.display()))?;
                    let actual = rustix::fs::FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode);
                    anyhow::ensure!(kind_matches(*expected, actual), "agent storage has the wrong type");
                    push(a, &[&"--bind-fd", &fd.as_raw_fd().to_string(), inside]);
                    mount_fds.push(Arc::new(fd));
                } else {
                    if agent_masked.iter().any(|m| inside.starts_with(m)) {
                        no_bind_across_mask(inside, agent_masked)?;
                    }
                    push(a, &[&"--bind", host, inside]);
                    for m in agent_masked.iter().filter(|m| m.starts_with(inside)) {
                        let is_dir = self.agent_masks.iter().any(|(p, kind)| p == m && *kind == HomePathKind::Directory);
                        push_mask(a, m, is_dir);
                    }
                }
                for (m, is_dir) in masked.policy.iter().filter(|(m, _)| is_under(m, inside) && m != inside) {
                    push_mask(a, m, *is_dir);
                }
            }
            direct.push(inside.clone());
        }
        Ok(direct)
    }

    /// A workspace session (docs/parallel-coding.md §12): each clone bound read-write at its
    /// repository's own path, after the pins (an ancestor pin is laid first). The person's
    /// checkout is hidden in the read-only host view too. Its object store is borrowed read-only at
    /// `/.sill/borrowed/<n>`, and the jail sees the clone's `objects/info/alternates` naming that
    /// path (from memory, read-only), while on the host it names the real store: one clone, valid
    /// in both views. Every directory is opened without following symlinks and bound from that
    /// descriptor, so a path swapped after planning cannot bind another tree.
    fn apply_workspace_binds(&self, args: &mut Vec<OsString>, masked: &[PathBuf], direct: &mut Vec<PathBuf>, warnings: &mut Vec<String>) -> Result<(Vec<Arc<OwnedFd>>, Vec<Bound>)> {
        use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
        let open = |p: &Path| -> Result<Arc<OwnedFd>> {
            let fd = openat2(CWD, p, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty(), ResolveFlags::NO_SYMLINKS)
                .with_context(|| format!("open {} (no symlinks)", p.display()))?;
            Ok(Arc::new(rustix::io::fcntl_dupfd_cloexec(&fd, 64)?))
        };
        let mut fds = Vec::new();
        let mut bound = Vec::new();
        for (n, (source, clone, objects)) in self.workspace_mask.iter().enumerate() {
            if !source.is_absolute() || !clone.is_absolute() || !objects.starts_with(source) || objects == source {
                bail!("workspace {}: the clone must be absolute and the object store inside the checkout", source.display());
            }
            if let Some(m) = masked.iter().find(|m| is_under(source, m) || is_under(clone, m)) {
                warnings.push(format!("workspace {} is hidden by {}; not bound", source.display(), m.display()));
                continue;
            }
            let clone_fd = open(clone)?;
            args.extend([OsString::from("--bind-fd"), clone_fd.as_raw_fd().to_string().into(), source.as_os_str().to_owned()]);
            args.extend([OsString::from("--tmpfs"), host_view(source).into_os_string()]);
            let borrowed = PathBuf::from(format!("/.sill/borrowed/{n}"));
            let store = open(objects)?;
            args.extend([OsString::from("--ro-bind-fd"), store.as_raw_fd().to_string().into(), borrowed.clone().into_os_string()]);
            let memfd = alternates(args, source, &borrowed)?;
            direct.push(source.clone());
            fds.extend([clone_fd, store, memfd]);
            bound.push(Bound { at: source.clone(), backing: clone.clone(), borrowed: Some(borrowed) });
        }
        Ok((fds, bound))
    }

    /// The policy's `mask` and `ro` for paths inside a pinned folder or a workspace's repository
    /// (security.md, "Policy path rules inside a pinned folder"). The pin's or the clone's bind
    /// covers what was laid before it, so these rules are laid again here, last: read-only paths
    /// bound from what the jail has there (the clone, in a workspace), then the masks.
    ///
    /// Every directory between the bound folder and a rule's path is first bound onto itself. A
    /// mount point cannot be renamed or removed, so the agent cannot move a rule's path away for
    /// the next build to miss: the way to it stays where the policy names it. Sources are opened
    /// without following symlinks and bound by descriptor.
    fn apply_rules_inside_binds(&self, args: &mut Vec<OsString>, masks: &[PathBuf], ro: &[PathBuf], agent_masked: &[PathBuf], bound: &[Bound], warnings: &mut Vec<String>) -> Result<Vec<Arc<OwnedFd>>> {
        use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
        // the innermost bound folder a path lies in, and where its files really are
        let home_of = |p: &Path| bound.iter().filter(|b| is_under(p, &b.at)).max_by_key(|b| b.at.components().count());
        let backing = |b: &Bound, p: &Path| b.backing.join(p.strip_prefix(&b.at).unwrap_or(Path::new("")));
        let between = |b: &Bound, p: &Path| -> Vec<PathBuf> {
            p.ancestors().skip(1).take_while(|d| is_under(d, &b.at) && *d != b.at).map(Path::to_path_buf).collect()
        };
        // path → read-only; BTreeMap order lays a folder before what is inside it
        let mut binds: BTreeMap<PathBuf, bool> = BTreeMap::new();
        let mut lay: Vec<(PathBuf, bool)> = Vec::new();
        for m in masks {
            let Some(b) = home_of(m).filter(|b| b.at != *m) else { continue };
            // what the jail has at the path decides: nothing there, nothing of the host's to hide
            let mut target = m.clone();
            if b.borrowed.is_none() {
                while backing(b, &target).symlink_metadata().is_err() && target != b.at {
                    target = target.parent().context("mask has no parent")?.to_path_buf();
                }
            }
            if target != *m {
                warnings.push(format!("absent mask {} hides {} inside the live pin so future host files stay hidden", m.display(), target.display()));
            }
            let m = &target;
            let Ok(meta) = backing(b, m).symlink_metadata() else { continue };
            if !meta.is_dir() && !meta.is_file() {
                warnings.push(format!("masked {} is neither a file nor a folder in {}; not masked there", m.display(), b.at.display()));
                continue;
            }
            lay.push((m.clone(), meta.is_dir()));
            for d in between(b, m) {
                binds.entry(d).or_insert(false);
            }
        }
        for p in ro {
            let Some(b) = home_of(p) else { continue };
            binds.insert(p.clone(), true);
            for d in between(b, p) {
                binds.entry(d).or_insert(false);
            }
        }
        // a folder on the way to a rule keeps the rule above it: read-only inside a read-only path
        let read_only: Vec<PathBuf> = binds.iter().filter(|(_, ro)| **ro).map(|(p, _)| p.clone()).collect();
        let mut fds = Vec::new();
        let mut again = Vec::new();
        for (path, _) in &binds {
            let b = home_of(path).expect("a path inside a bound folder");
            // binding a folder again shows the host's own tree below it: never over sill's own
            // tree in a host path's place, a workspace or another agent's files
            let buried = self.binds.iter().filter(|(host, inside)| host != inside).map(|(_, inside)| inside)
                .chain(bound.iter().filter(|w| w.borrowed.is_some() && w.at != *path).map(|w| &w.at))
                .chain(agent_masked)
                .find(|t| is_under(t, path));
            if let Some(t) = buried {
                bail!("{}: a policy `mask` or `ro` path inside a pinned folder would cover {}; not mounted", path.display(), t.display());
            }
            let source = backing(b, path);
            let fd = match openat2(CWD, &source, OFlags::PATH | OFlags::CLOEXEC, Mode::empty(), ResolveFlags::NO_SYMLINKS) {
                Ok(fd) => Arc::new(rustix::io::fcntl_dupfd_cloexec(&fd, 64)?),
                // a workspace's clone is the agent's to arrange: what is not there is not protected
                Err(_) if b.borrowed.is_some() => {
                    warnings.push(format!("{} is not in the workspace at {}; the policy's rule for it is not applied", path.display(), b.at.display()));
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("open {} (no symlinks) for the policy's rule inside a pinned folder", source.display())),
            };
            let flag = if read_only.iter().any(|r| is_under(path, r)) { "--ro-bind-fd" } else { "--bind-fd" };
            args.extend([OsString::from(flag), fd.as_raw_fd().to_string().into(), path.as_os_str().to_owned()]);
            fds.push(fd);
            // the clone's own alternates file shows again below a folder bound here
            again.extend(bound.iter().filter(|w| w.borrowed.is_some() && is_under(&w.at.join(".git/objects/info/alternates"), path)).map(|w| w.at.clone()));
        }
        again.sort();
        again.dedup();
        for w in bound.iter().filter(|w| again.contains(&w.at)) {
            fds.push(alternates(args, &w.at, w.borrowed.as_ref().expect("a workspace"))?);
        }
        lay.sort();
        let mut laid: Vec<PathBuf> = Vec::new();
        for (path, is_dir) in lay {
            if laid.iter().any(|p| is_under(&path, p)) {
                continue;
            }
            if is_dir {
                args.extend([OsString::from("--tmpfs"), path.clone().into_os_string()]);
            } else {
                args.extend([OsString::from("--ro-bind"), OsString::from("/dev/null"), path.clone().into_os_string()]);
            }
            laid.push(path);
        }
        Ok(fds)
    }

}

impl JailPlan {
    /// Prepare bubblewrap with just this plan's source descriptors inherited in the child.
    /// Keeping CLOEXEC set in the parent avoids leaking sources to unrelated agent spawns.
    pub fn command(&self) -> std::process::Command {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(&self.argv[0]);
        command.args(&self.argv[1..]);
        let fds = self.mount_fds.clone();
        // SAFETY: fcntl is async-signal-safe; descriptors stay owned by the command closure.
        unsafe {
            command.pre_exec(move || {
                for fd in &fds {
                    rustix::io::fcntl_setfd(fd.as_ref(), rustix::io::FdFlags::empty())?;
                }
                Ok(())
            });
        }
        command
    }
}

pub fn host_view(path: &Path) -> PathBuf {
    Path::new(HOST_VIEW).join(path.strip_prefix("/").unwrap_or(path))
}

/// Create the upper/work directories for every layer.
pub fn prepare(plan: &JailPlan) -> Result<()> {
    for l in &plan.layers {
        std::fs::create_dir_all(&l.upper).with_context(|| format!("create {}", l.upper.display()))?;
        std::fs::create_dir_all(&l.work).with_context(|| format!("create {}", l.work.display()))?;
    }
    Ok(())
}

/// Shell-quoted rendering of the argv for display.
pub fn render(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| {
            let s = a.to_string_lossy();
            if s.is_empty() || s.contains([' ', '"', '\'', '$', '*', '?', '[']) {
                format!("'{}'", s.replace('\'', "'\\''"))
            } else {
                s.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JailSpec {
        let mounts = mounts::parse_mountinfo(
            "25 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
             30 25 0:5 / /dev rw - devtmpfs udev rw\n\
             31 25 0:6 / /proc rw - proc proc rw\n\
             40 25 8:1 / /boot rw - ext4 /dev/sda1 rw\n\
             41 40 8:3 / /boot/efi rw - vfat /dev/sda3 rw\n\
             50 25 0:30 / /tmp rw - tmpfs tmpfs rw\n\
             60 25 8:16 / /mnt/data rw - ext4 /dev/sdb1 rw\n\
             70 25 7:0 / /var/lib/sill rw - ext4 /dev/loop0 rw\n\
             80 25 0:40 / /mnt/portal rw - fuse.portal portal rw\n",
        );
        let root_entries = ["bin", "boot", "dev", "etc", "home", "lib", "mnt", "proc", "run", "sys", "tmp", "usr", "var"]
            .iter()
            .map(|n| RootEntry {
                name: (*n).into(),
                symlink: matches!(*n, "bin" | "lib").then(|| PathBuf::from(format!("usr/{n}"))),
                is_dir: !matches!(*n, "bin" | "lib"),
                readable: !matches!(*n, "bin" | "lib"),
            })
            .collect();
        JailSpec {
            policy: Policy::builtin_default(),
            home: PathBuf::from("/home/me"),
            user: "me".into(),
            uid: 1000,
            state_dir: PathBuf::from("/var/lib/sill/1000/sessions/x"),
            unsafe_state: false,
            cwd: PathBuf::from("/home/me"),
            env: Default::default(),
            share_net: false,
            mounts,
            root_entries,
            proxy_socket: None,
            proxy_port: PROXY_PORT,
            binds: vec![],
            agent_masks: vec![],
            policy_paths: None,
            agent_binds: vec![],
            masks: vec![],
            memory_dir: None,
            bridge_dir: None,
            workspace_mask: vec![],
            seccomp_fd: None,
            extra_args: vec![],
            command: vec!["true".into()],
        }
    }

    /// A scratch directory of a test's own, removed with it.
    struct TempTree(PathBuf);
    impl TempTree {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("sill-jail-tree-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir_all(&p).unwrap();
            Self(p.canonicalize().unwrap())
        }
    }
    impl Drop for TempTree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn distinct_mount_roots_cannot_alias_an_overlay_state_directory() {
        let dir = std::env::temp_dir().join(format!("sill-layer-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::create_dir_all(dir.join("a%b")).unwrap();
        let mut s = spec();
        // A temporary mount's children are expanded exactly as a host /mnt tree is.
        s.root_entries = vec![RootEntry { name: dir.strip_prefix("/").unwrap().as_os_str().to_os_string(), symlink: None, is_dir: true, readable: true }];
        s.mounts = vec![Mount {mount_point:dir.join("a/b"),fstype:"ext4".into(),source:"test".into(),device:String::new(),root:"/".into()}];
        assert!(s.plan().unwrap_err().to_string().contains("overlay state name collision"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn non_utf8_mount_and_bind_arguments_remain_exact() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let exact = PathBuf::from(OsString::from_vec(b"/pin/\xff".to_vec()));
        let mut s = spec();
        s.binds.push((exact.clone(), exact.clone()));
        let plan = s.plan().unwrap();
        assert!(plan.argv.windows(3).any(|args| args[0] == "--bind" && args[1] == exact && args[2] == exact));
        let layer = layer_for(&s.state_dir, &exact);
        assert_eq!(layer.upper.file_name().unwrap().as_bytes(), b"pin%\xff");
        s.root_entries.push(RootEntry { name: OsString::from_vec(b"link\xff".to_vec()), symlink: Some(exact.clone()), is_dir: false, readable: false });
        let plan = s.plan().unwrap();
        assert!(plan.argv.windows(3).any(|args| args[0] == "--symlink" && args[1] == exact && args[2].as_bytes() == b"/link\xff"));
    }

    /// Behind the proxy, Node's own `fetch` is told to use it (Gemini CLI went direct and hung),
    /// and the jail's own loopback stays direct.
    #[test]
    fn node_uses_the_proxy_and_loopback_stays_in_the_jail() {
        let mut s = spec();
        s.proxy_socket = Some(PathBuf::from("/run/user/1000/sill/x/proxy.sock"));
        let r = render(&s.plan().unwrap().argv);
        assert!(r.contains("--setenv NODE_USE_ENV_PROXY 1"), "{r}");
        assert!(r.contains("--setenv HTTPS_PROXY http://127.0.0.1:"), "{r}");
        assert!(r.contains("--setenv NO_PROXY localhost,127.0.0.1,::1"), "{r}");
    }

    /// Without D-Bus the jail gets a dead address, not an unset one: nothing autolaunches a bus.
    #[test]
    fn no_dbus_is_a_dead_address_not_an_autolaunch() {
        let s = spec();
        let r = render(&s.plan().unwrap().argv);
        assert!(r.contains("--setenv DBUS_SESSION_BUS_ADDRESS disabled:"), "{r}");
        assert!(r.contains("--setenv DBUS_SYSTEM_BUS_ADDRESS disabled:"), "{r}");
        assert!(!r.contains("unix:path="), "{r}");
        assert!(!r.contains(".X11-unix") && !r.contains("XAUTHORITY"), "{r}");
    }

    #[test]
    fn gpu_access_is_explicit_and_render_only() {
        let mut s = spec();
        assert!(!render(&s.plan().unwrap().argv).contains("/dev/dri"));
        s.policy.permissions.gpu = crate::permissions::Rule::On;
        if let Some(node) = crate::gpu::probe().node {
            let r = render(&s.plan().unwrap().argv);
            assert!(r.contains(&format!("--dev-bind {0} {0}", node.display())));
            assert!(!r.contains("/dev/dri/card"));
        } else {
            assert!(s.plan().unwrap_err().to_string().contains("render node"));
        }
        s.policy.permissions.gpu = crate::permissions::Rule::Forbidden;
        assert!(!render(&s.plan().unwrap().argv).contains("/dev/dri"));
    }

    /// With the private bus the session address is the jail's own socket; the system bus stays
    /// dead, and a policy's environment cannot point the agent at the person's bus.
    #[test]
    fn the_private_bus_is_the_jail_s_own_address() {
        let mut s = spec();
        s.policy.permissions.private_bus = crate::permissions::Rule::On;
        s.policy.jail.env.insert("DBUS_SESSION_BUS_ADDRESS".into(), "unix:path=/run/user/1000/sill-host-bus".into());
        let r = render(&s.plan().unwrap().argv);
        assert!(r.contains("--setenv DBUS_SESSION_BUS_ADDRESS unix:path=/run/user/1000/bus "), "{r}");
        assert!(!r.contains("sill-host-bus"), "{r}");
        assert!(r.contains("--setenv DBUS_SYSTEM_BUS_ADDRESS disabled:"), "{r}");
    }

    #[test]
    fn memory_snapshot_is_bound_read_only() {
        let dir = std::env::temp_dir().join(format!("sill-jail-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = spec();
        s.memory_dir = Some(dir.clone());
        assert!(!render(&s.plan().unwrap().argv).contains(sill_memory::JAIL_DIR), "absent: not bound");
        std::fs::create_dir_all(&dir).unwrap();
        let r = render(&s.plan().unwrap().argv);
        assert!(r.contains(&format!("--ro-bind {} {}", dir.display(), sill_memory::JAIL_DIR)), "{r}");
        s.memory_dir = None;
        assert!(!render(&s.plan().unwrap().argv).contains(sill_memory::JAIL_DIR));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_secrets_have_masks_before_they_exist() {
        let tree = std::env::temp_dir().join(format!("sill-future-mask-{}", std::process::id()));
        std::fs::create_dir_all(tree.join("config")).unwrap();
        let mut s = spec();
        s.policy.paths.mask = vec![format!("{}/config/*/credentials*", tree.display()), format!("{}/absent/token", tree.display())];
        let plan = s.plan().unwrap();
        assert!(plan.masked.contains(&tree.join("config")));
        assert!(plan.masked.contains(&tree.join("absent/token")));
        let r = render(&plan.argv);
        assert!(r.contains(&format!("--tmpfs {}", host_view(&tree).display())), "absent host-view paths hide the existing parent: {r}");
        // Creating a new matching file cannot change the chosen mask root.
        std::fs::create_dir_all(tree.join("config/new-app")).unwrap();
        std::fs::write(tree.join("config/new-app/credentials.json"), "secret").unwrap();
        assert_eq!(plan.masked, s.plan().unwrap().masked);
        std::fs::remove_dir_all(tree).unwrap();
    }

    #[test]
    fn extra_masks_are_applied_once_and_only_when_present() {
        let dir = std::env::temp_dir().join(format!("sill-jail-masks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(dir.join(".claude.json"), "{}").unwrap();
        let mut s = spec();
        s.masks = vec![dir.join(".codex"), dir.join(".claude.json"), dir.join(".gemini"), dir.join(".codex")];
        let plan = s.plan().unwrap();
        let r = render(&plan.argv);
        assert!(r.contains(&format!("--tmpfs {} ", dir.join(".codex").display())));
        assert!(r.contains(&format!("--ro-bind /dev/null {} ", dir.join(".claude.json").display())));
        assert!(!r.contains(".gemini"), "a path that does not exist is not masked");
        assert_eq!(plan.masked.iter().filter(|m| **m == dir.join(".codex")).count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn private_config_binding_follows_parent_masks_without_exposing_host_config() {
        use crate::agents::HomePathKind::Directory;
        let tree = TempTree::new();
        let mut s = spec();
        s.home = tree.0.join("home");
        let original = s.home.join(".config/opencode");
        let source = tree.0.join("private/opencode");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        s.policy.paths.mask = vec!["~/.config/*/credentials*".into()];
        s.agent_masks = vec![(original.clone(), Directory)];
        s.agent_binds = vec![(original.clone(), Directory)];
        s.binds = vec![(source.clone(), original.clone())];
        let plan = s.plan().unwrap();
        let a = &plan.argv;
        let mask = a.windows(2).position(|a| a[0] == "--tmpfs" && a[1] == s.home.join(".config")).unwrap();
        assert!(a.windows(2).any(|a| a[0] == "--tmpfs" && a[1] == host_view(&s.home.join(".config"))));
        let bind = a.windows(3).position(|a| a[0] == "--bind-fd" && a[2] == original).unwrap();
        assert!(bind > mask);
        s.agent_binds.clear();
        assert!(s.plan().is_err(), "scratch cannot override a parent mask");
        s.agent_binds = vec![(original.clone(), Directory)];
        s.policy.paths.mask.push("~/.config/opencode".into());
        assert!(s.plan().is_err(), "explicit masks still win");
    }

    #[test]
    fn declared_agent_masks_cover_both_views_and_bind_a_held_private_source() {
        use crate::agents::HomePathKind::{Directory, File};
        let tree = TempTree::new();
        let original = tree.0.join("home/.codex");
        let source = tree.0.join("private/.codex");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let mut s = spec();
        s.agent_masks = vec![(original.clone(), Directory), (tree.0.join("home/.claude.json"), File)];
        s.agent_binds = vec![(original.clone(), Directory)];
        s.binds = vec![(source.clone(), original.clone())];
        let plan = s.plan().unwrap();
        let a = &plan.argv;
        let mask = a.windows(2).position(|a| a[0] == "--tmpfs" && a[1] == original).unwrap();
        assert!(a.windows(2).any(|a| a[0] == "--tmpfs" && a[1] == host_view(&original)));
        let bind = a.windows(3).position(|a| a[0] == "--bind-fd" && a[2] == original).unwrap();
        assert!(bind > mask);
        assert!(plan.masked.contains(&tree.0.join("home/.claude.json")), "absent originals must stay hidden if later created on the host");
        let fd: i32 = a[bind + 1].to_str().unwrap().parse().unwrap();
        assert!(plan.mount_fds.iter().any(|held| held.as_raw_fd() == fd));
        std::fs::rename(&source, source.with_extension("saved")).unwrap();
        std::os::unix::fs::symlink(&original, &source).unwrap();
        assert_eq!(std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap(), source.with_extension("saved"));
        assert!(s.plan().is_err());
        std::fs::remove_file(&source).unwrap();
        std::fs::rename(source.with_extension("saved"), &source).unwrap();
        s.policy.paths.mask.push(original.display().to_string());
        assert!(s.plan().is_err(), "private agent source may never override a policy secret mask");
        s.policy.paths.mask.clear();
        s.binds = vec![(original.clone(), original)];
        assert!(s.plan().is_err(), "a pin cannot expose the host agent original");
    }

    #[test]
    #[ignore = "host only: requires bubblewrap user namespaces and overlayfs"]
    fn real_agent_storage_keeps_host_cargo_readable_across_restarts() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
            .join(format!("agent-storage-jail-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&root).unwrap();
        let tree = TempTree(root.canonicalize().unwrap());
        let home = tree.0.join("home");
        let put = |path: PathBuf, value: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        };
        put(home.join(".cargo/registry/src/host-marker"), "host-cache");
        put(home.join(".cargo/credentials.toml"), "fixture-secret");
        put(home.join(".codex/host-original"), "private-host-profile");
        put(home.join(".claude/host-original"), "private-host-profile");
        put(home.join(".local/state/sill/agents/codex/.cargo/stray"), "unused");
        put(home.join(".local/state/sill/agents/codex/.codex/saved"), "saved-settings");
        let registry = crate::agents::builtin().into_iter().map(|a| (a.id.clone(), a)).collect::<BTreeMap<_, _>>();
        for agent in ["codex", "claude", "codex", "claude"] {
            let mut s = spec();
            s.home = home.clone();
            s.cwd = home.clone();
            s.uid = rustix::process::getuid().as_raw();
            s.state_dir = tree.0.join(format!("state-{agent}"));
            s.unsafe_state = true;
            s.mounts = crate::mounts::host_mounts().unwrap();
            s.root_entries = read_root_entries().unwrap();
            s.command = vec!["/bin/sh".into(), "-eu".into(), "-c".into(), r#"
test "$(cat "$1/.cargo/registry/src/host-marker")" = host-cache
test "$(cat "/.sill/host$1/.cargo/registry/src/host-marker")" = host-cache
test ! -s "$1/.cargo/credentials.toml"
test ! -s "/.sill/host$1/.cargo/credentials.toml"
test ! -e "$1/.codex/host-original"
test ! -e "$1/.claude/host-original"
test ! -e "/.sill/host$1/.codex/host-original"
test ! -e "/.sill/host$1/.claude/host-original"
mkdir -p "$1/.cargo/new-child"
printf session-cache > "$1/.cargo/new-child/download"
test ! -e "/.sill/host$1/.cargo/new-child/download"
if test "$2" = codex; then test "$(cat "$1/.codex/saved")" = saved-settings; fi
"#.into(), "storage-fixture".into(), home.clone().into(), agent.into()];
            let storage = crate::agent_storage::resolve(&registry[agent], &registry, &s.policy, &s.state_dir, &home, &[], false).unwrap();
            storage.prepare().unwrap();
            storage.apply(&mut s).unwrap();
            let plan = s.plan().unwrap();
            prepare(&plan).unwrap();
            let output = plan.command().output().unwrap();
            assert!(output.status.success(), "{agent}: {}", String::from_utf8_lossy(&output.stderr));
            assert!(!home.join(".cargo/new-child").exists());
            assert_eq!(std::fs::read_to_string(home.join(".cargo/registry/src/host-marker")).unwrap(), "host-cache");
        }
    }

    /// A workspace session (docs/parallel-coding.md §12): after an ancestor pin, the clone is bound
    /// at the repository's path, the checkout hidden in the host view, its object store borrowed at
    /// `/.sill/borrowed/0` and the jail's alternates file written from memory; `/` read-only last.
    #[test]
    fn a_workspace_clone_takes_the_repositorys_place() {
        let dir = std::env::temp_dir().join(format!("sill-jail-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (projects, source, clone) = (dir.join("projects"), dir.join("projects/src"), dir.join("ws/clone"));
        let objects = source.join(".git/objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(&clone).unwrap();
        let mut s = spec();
        s.binds = vec![(projects.clone(), projects.clone())];
        s.workspace_mask = vec![(source.clone(), clone.clone(), objects.clone())];
        let plan = s.plan().unwrap();
        let r = render(&plan.argv);
        let at = |needle: String| r.find(&needle).unwrap_or_else(|| panic!("{needle} not in {r}"));
        let pin = at(format!("--bind {} {} ", projects.display(), projects.display()));
        let bound = at(format!(" {} --tmpfs {} ", source.display(), host_view(&source).display()));
        let store = at(" /.sill/borrowed/0 ".into());
        let alternates = at(format!(" {} ", source.join(".git/objects/info/alternates").display()));
        let root = at("--remount-ro / ".into());
        assert!(pin < bound && bound < store && store < alternates && alternates < root, "{r}");
        assert!(r.contains("--bind-fd ") && r.contains("--ro-bind-data "));
        assert!(plan.direct.contains(&source), "writes there are the clone's, not the overlay's");
        // the clone, the store, the alternates data: all by descriptor
        assert_eq!(plan.mount_fds.len(), 3);
        // a store outside the checkout is refused
        s.workspace_mask = vec![(source.clone(), clone.clone(), dir.join("elsewhere"))];
        assert!(s.plan().is_err());
        // a repository the policy already hides stays hidden, with a warning
        s.workspace_mask = vec![(source.clone(), clone.clone(), objects.clone())];
        s.masks = vec![projects.clone()];
        s.binds = vec![];
        let plan = s.plan().unwrap();
        assert!(!render(&plan.argv).contains("--bind-fd"));
        assert!(plan.warnings.iter().any(|w| w.contains("hidden by")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A pin may hold paths the policy masks or makes read-only (security.md, "Policy path rules
    /// inside a pinned folder"): the rules are laid again after the pin, the folders on the way to
    /// them bound onto themselves so that they cannot be renamed away. Inside a mask, or over
    /// sill's own files, a pin is still refused.
    #[test]
    fn policy_rules_inside_a_pin_are_laid_again_after_it() {
        let tree = TempTree::new();
        let repo = tree.0.join("repo");
        std::fs::create_dir_all(repo.join("conf/deep")).unwrap();
        std::fs::create_dir_all(repo.join("secrets")).unwrap();
        std::fs::create_dir_all(repo.join("vendor/lib")).unwrap();
        std::fs::write(repo.join("conf/deep/.env"), "secret").unwrap();
        let (env, secrets, lib) = (repo.join("conf/deep/.env"), repo.join("secrets"), repo.join("vendor/lib"));
        let mut s = spec();
        s.masks = vec![env.clone(), secrets.clone()];
        s.policy.paths.ro = vec![lib.display().to_string()];
        s.binds = vec![(repo.clone(), repo.clone())];
        let plan = s.plan().unwrap();
        let a = &plan.argv;
        let at = |flag: &str, dest: &Path| a.windows(3).rposition(|w| w[0] == flag && w[2] == dest).unwrap_or_else(|| panic!("{flag} {} not in {}", dest.display(), render(a)));
        let pin = at("--bind", &repo);
        // the way to each rule, top-down, then the read-only path, all by descriptor after the pin
        let (conf, deep, vendor) = (at("--bind-fd", &repo.join("conf")), at("--bind-fd", &repo.join("conf/deep")), at("--bind-fd", &repo.join("vendor")));
        let ro = at("--ro-bind-fd", &lib);
        assert!(pin < conf && conf < deep && pin < vendor && vendor < ro, "{}", render(a));
        // the masks last, over everything bound before them
        let file = a.windows(3).rposition(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == env).unwrap();
        let dir = a.windows(2).rposition(|w| w[0] == "--tmpfs" && w[1] == secrets).unwrap();
        assert!(file > deep && file > ro && dir > ro, "{}", render(a));
        assert_eq!(plan.mount_fds.len(), 4);
        assert!(plan.mount_fds.iter().all(|fd| fd.as_raw_fd() >= 64 && rustix::io::fcntl_getfd(fd.as_ref()).unwrap().contains(rustix::io::FdFlags::CLOEXEC)));
        // a folder on the way to a rule inside a read-only path stays read-only
        s.policy.paths.ro = vec![repo.join("vendor").display().to_string(), lib.display().to_string()];
        let a = s.plan().unwrap().argv;
        assert!(a.windows(3).any(|w| w[0] == "--ro-bind-fd" && w[2] == repo.join("vendor")) && !a.windows(3).any(|w| w[0] == "--bind-fd" && w[2] == repo.join("vendor")));
        // sill's own tree in a host path's place below such a folder would be buried: refused
        s.binds.push((tree.0.join("private"), repo.join("vendor/lib/cache")));
        std::fs::create_dir_all(tree.0.join("private")).unwrap();
        assert!(s.plan().unwrap_err().to_string().contains("would cover"));
        s.binds.pop();
        // a pin inside a mask re-exposes it, and one over sill's own files is never mounted
        s.binds = vec![(secrets.clone(), secrets.clone())];
        assert!(s.plan().unwrap_err().to_string().contains("sit inside"));
        let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| s.home.join(".local/state"));
        let over = state.parent().unwrap().to_path_buf();
        s.binds = vec![(over.clone(), over)];
        assert!(s.plan().unwrap_err().to_string().contains("cover the masked"));
    }

    /// The same rules in a workspace: they apply to the clone, which is what the jail has at the
    /// repository's path. What the clone does not hold is not masked; a read-only repository keeps
    /// the jail's alternates file.
    #[test]
    fn policy_rules_inside_a_workspace_apply_to_its_clone() {
        let tree = TempTree::new();
        let (source, clone) = (tree.0.join("src"), tree.0.join("ws/clone"));
        let objects = source.join(".git/objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(source.join("conf")).unwrap();
        std::fs::create_dir_all(clone.join("conf")).unwrap();
        std::fs::create_dir_all(clone.join(".git/objects/info")).unwrap();
        for dir in [&source, &clone] {
            std::fs::write(dir.join("conf/.env"), "secret").unwrap();
        }
        std::fs::write(source.join("only-in-the-checkout"), "x").unwrap();
        let (env, absent) = (source.join("conf/.env"), source.join("only-in-the-checkout"));
        let mut s = spec();
        s.masks = vec![env.clone(), absent.clone()];
        s.workspace_mask = vec![(source.clone(), clone.clone(), objects.clone())];
        let plan = s.plan().unwrap();
        let a = &plan.argv;
        let bound = a.windows(3).position(|w| w[0] == "--bind-fd" && w[2] == source).unwrap();
        let conf = a.windows(3).rposition(|w| w[0] == "--bind-fd" && w[2] == source.join("conf")).unwrap();
        let mask = a.windows(3).rposition(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == env).unwrap();
        assert!(bound < conf && conf < mask, "{}", render(a));
        // the folder on the way is the clone's, not the checkout's
        let fd: i32 = a[conf + 1].to_str().unwrap().parse().unwrap();
        assert_eq!(std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap(), clone.join("conf"));
        assert_eq!(a.windows(3).filter(|w| w[2] == absent).count(), 1, "masked where the host has it, before the clone, and not again");
        // the whole repository read-only: the clone bound again, the jail's alternates over it
        s.masks.clear();
        s.policy.paths.ro = vec![source.display().to_string()];
        let plan = s.plan().unwrap();
        let a = &plan.argv;
        let ro = a.windows(3).rposition(|w| w[0] == "--ro-bind-fd" && w[2] == source).unwrap();
        let alternates = a.windows(3).rposition(|w| w[0] == "--ro-bind-data" && w[2] == source.join(".git/objects/info/alternates")).unwrap();
        assert!(bound < ro && ro < alternates, "{}", render(a));
        let fd: i32 = a[ro + 1].to_str().unwrap().parse().unwrap();
        assert_eq!(std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap(), clone);
    }

    /// The real thing, in bubblewrap: a workspace made by `workspace::create`, its repository inside
    /// a writable ancestor pin. Inside, the repository's path is the clone: its files, its Git
    /// working through the borrowed store; the person's uncommitted file is not there, in either
    /// view; the store and the jail's alternates are read-only; the rest of the pin is writable.
    #[test]
    #[ignore = "host only: requires bubblewrap user namespaces and overlayfs"]
    fn real_workspace_jail_puts_the_clone_at_the_repositorys_path() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
            .join(format!("workspace-jail-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&root).unwrap();
        let tree = TempTree(root.canonicalize().unwrap());
        let home = tree.0.join("home");
        let projects = home.join("projects");
        let source = projects.join("src");
        std::fs::create_dir_all(&source).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&source, &["init", "-q"]);
        std::fs::write(source.join("tracked.txt"), "tracked\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "one"]);
        std::fs::write(source.join("uncommitted.txt"), "the person's\n").unwrap();
        let src = crate::workspace::inspect(&source).unwrap().unwrap();
        let ws = home.join(".local/share/sill/workspaces/src/try-ws1");
        crate::workspace::create(&src, "ws1", "try", &ws, "op").unwrap();
        let host_alternates = std::fs::read_to_string(ws.join(".git/objects/info/alternates")).unwrap();
        let before = walk(&source.join(".git/objects"));
        let mut s = spec();
        s.home = home.clone();
        s.cwd = source.clone();
        s.uid = rustix::process::getuid().as_raw();
        s.state_dir = tree.0.join("state");
        s.unsafe_state = true;
        s.mounts = crate::mounts::host_mounts().unwrap();
        s.root_entries = read_root_entries().unwrap();
        s.binds = vec![(projects.clone(), projects.clone())];
        s.workspace_mask = vec![(source.clone(), ws.clone(), source.join(".git/objects"))];
        s.command = vec!["/bin/sh".into(), "-eu".into(), "-c".into(), r#"
S=$1; P=$2
test "$(cat "$S/tracked.txt")" = tracked
test ! -e "$S/uncommitted.txt"
test ! -e "/.sill/host$S/uncommitted.txt"
test "$(cat "$S/.git/objects/info/alternates")" = /.sill/borrowed/0
if echo x > "$S/.git/objects/info/alternates" 2>/dev/null; then echo "alternates writable"; exit 1; fi
if touch /.sill/borrowed/0/x 2>/dev/null; then echo "the store is writable"; exit 1; fi
touch "$P/other-file"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
git -C "$S" log --oneline | grep -q one
echo two > "$S/tracked.txt"
git -C "$S" -c user.name=t -c user.email=t@t commit -qam two
git -C "$S" fsck --full
"#.into(), "workspace-fixture".into(), source.clone().into(), projects.clone().into()];
        let plan = s.plan().unwrap();
        prepare(&plan).unwrap();
        let output = plan.command().output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(projects.join("other-file").exists(), "the ancestor pin is writable");
        assert_eq!(std::fs::read_to_string(source.join("tracked.txt")).unwrap(), "tracked\n", "the person's checkout untouched");
        assert_eq!(std::fs::read_to_string(source.join("uncommitted.txt")).unwrap(), "the person's\n");
        assert_eq!(std::fs::read_to_string(ws.join("tracked.txt")).unwrap(), "two\n", "the commit is the clone's");
        assert_eq!(git(&ws, &["log", "-1", "--format=%s"]), "two");
        assert_eq!(std::fs::read_to_string(ws.join(".git/objects/info/alternates")).unwrap(), host_alternates);
        assert_eq!(walk(&source.join(".git/objects")), before, "the source's store is unchanged");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() { out.extend(walk(&e.path())); } else { out.push(e.path()); }
        }
        out.sort();
        out
    }

    /// A sill-owned bind (private scratch, agent home) over a masked path is mounted, the mask laid
    /// again on top, and so it is after a pin of the host's own tree; any bind inside a mask is
    /// refused.
    #[test]
    fn a_tree_over_a_mask_is_remasked_and_none_is_bound_inside_one() {
        let dir = std::env::temp_dir().join(format!("sill-jail-remask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache/other-agent")).unwrap();
        std::fs::create_dir_all(dir.join("scratch")).unwrap();
        let mut s = spec();
        s.masks = vec![dir.join("cache/other-agent")];
        s.binds = vec![(dir.join("scratch"), dir.join("cache"))];
        let r = render(&s.plan().unwrap().argv);
        let bind = format!("--bind {} {} ", dir.join("scratch").display(), dir.join("cache").display());
        let remask = format!("--tmpfs {} ", dir.join("cache/other-agent").display());
        assert!(r.contains(&bind), "the scratch is bound");
        let (b, m) = (r.find(&bind).unwrap(), r.rfind(&remask).unwrap());
        assert!(m > b, "the mask is laid again after the bind");
        // the host's own tree (a pin) over the same mask: bound, the mask laid again after it
        let mut s = spec();
        s.masks = vec![dir.join("cache/other-agent")];
        s.binds = vec![(dir.join("cache"), dir.join("cache"))];
        let r = render(&s.plan().unwrap().argv);
        let pin = format!("--bind {0} {0} ", dir.join("cache").display());
        assert!(r.rfind(&remask).unwrap() > r.find(&pin).unwrap(), "{r}");
        // sill's own tree inside a mask: refused
        let mut s = spec();
        s.masks = vec![dir.join("cache")];
        s.binds = vec![(dir.join("scratch"), dir.join("cache/other-agent"))];
        assert!(s.plan().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_bind_may_cross_a_mask() {
        let masked = vec![PathBuf::from("/home/me/.ssh"), PathBuf::from("/home/me/.config/x/credentials")];
        assert!(no_bind_across_mask(Path::new("/home/me/.ssh/id_rsa"), &masked).is_err()); // inside: re-exposes
        assert!(no_bind_across_mask(Path::new("/home/me"), &masked).is_err()); // over: buries
        assert!(no_bind_across_mask(Path::new("/home/me/.ssh"), &masked).is_err());
        assert!(no_bind_across_mask(Path::new("/home/me/.sshx"), &masked).is_ok());
        assert!(no_bind_across_mask(Path::new("/home/me/proj"), &masked).is_ok());
        assert!(no_bind_across_mask(Path::new("/home/me/.config/x"), &masked).is_err());
    }

    #[test]
    fn layers_follow_mounts_and_hidden() {
        let plan = spec().plan().unwrap();
        let hosts: Vec<_> = plan.layers.iter().map(|l| l.host.to_string_lossy().into_owned()).collect();
        // /boot hidden by the default policy; /mnt/data is a nested persistent mount; /var/lib/sill is a
        // sub-mount of /var and therefore not a layer (invisible to the overlay, masked in the host view)
        // /mnt and /var contain mount points → expanded (tmpfs + children); their real sub-mounts are
        // their own layers; the state fs never is. The fake root_entries have no real /mnt or /var
        // children on this machine's filesystem, so only the mounts themselves show up as layers.
        assert!(hosts.contains(&"/etc".to_string()) && hosts.contains(&"/home".to_string()) && hosts.contains(&"/usr".to_string()));
        assert!(!hosts.contains(&"/mnt".to_string()), "/mnt holds mounts, must be expanded");
        // (/mnt/data would be a layer on a machine where that directory exists; the test host has none)
        assert!(!hosts.contains(&"/var".to_string()));
        // the state fs itself, not any path with "sill" in its name: /mnt is read from the real
        // machine, and inside Sill's VM it has a real /mnt/sill-state, which this fake mount table
        // does not name as the state disk
        assert!(!hosts.iter().any(|h| Path::new(h).starts_with("/var/lib/sill")), "{hosts:?}");
        assert!(plan.expanded.contains(&PathBuf::from("/var")) && plan.expanded.contains(&PathBuf::from("/mnt")));
        let s = render(&plan.argv);
        assert!(s.contains("--tmpfs /var "));
        assert!(s.contains("--tmpfs /.sill/host/var ") && s.contains("--tmpfs /.sill/host/mnt "));
        assert!(!s.contains("/.sill/host/var/lib/sill") && !s.contains("/.sill/host/mnt/portal"), "what the jail leaves out is not in the host view: {s}");
        assert!(s.contains("--symlink usr/bin /bin"));
        assert!(s.contains("--disable-userns"));
        assert!(!s.contains("--share-net"));
        assert!(s.contains("--ro-bind /sys /sys"));
        assert!(s.ends_with("--chdir /home/me -- true"));
    }

    /// Audit A3: a bind of a host directory is a slave of the host's mount and receives what is
    /// mounted there later, writable. The host view holds none.
    #[test]
    fn the_host_view_binds_no_host_directory() {
        let plan = spec().plan().unwrap();
        let a: Vec<String> = plan.argv.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert!(!plan.layers.is_empty() && !plan.expanded.is_empty());
        for (i, arg) in a.iter().enumerate().filter(|(_, arg)| arg.starts_with(HOST_VIEW)) {
            let binds_a_directory = matches!(a[i - 2].as_str(), "--ro-bind" | "--bind" | "--dev-bind") && Path::new(&a[i - 1]).is_dir();
            assert!(!binds_a_directory || a[i - 1].ends_with("/tools"), "{} {} {arg}", a[i - 2], a[i - 1]);
        }
        let last_mount = a.iter().rposition(|arg| arg.starts_with("--") && arg != "--remount-ro" && arg != "--chdir" && arg != "--").unwrap();
        for layer in &plan.layers {
            let at = host_view(&layer.host).to_string_lossy().into_owned();
            let host = layer.host.to_string_lossy().into_owned();
            assert!(a.windows(4).any(|w| w[0] == "--overlay-src" && w[1] == host && w[2] == "--tmp-overlay" && w[3] == at), "{at}");
            let sealed = a.windows(2).rposition(|w| w[0] == "--remount-ro" && w[1] == at).unwrap_or_else(|| panic!("{at} stays writable"));
            assert!(sealed > last_mount, "{at} is read-only before the last mount");
        }
        for dir in &plan.expanded {
            let at = host_view(dir).to_string_lossy().into_owned();
            assert!(a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == at) && a.windows(2).any(|w| w[0] == "--remount-ro" && w[1] == at), "{at}");
        }
    }

    /// A layer under a mask of the host view has no path there: remounting it would fail the launch.
    #[test]
    fn a_host_view_mount_under_a_mask_is_not_remounted() {
        let (layers, expanded) = ([layer_for(Path::new("/s"), Path::new("/home/me/.config")), layer_for(Path::new("/s"), Path::new("/etc"))], [PathBuf::from("/home/me")]);
        let mut a: Vec<OsString> = ["--tmpfs", "/.sill/host/home/me", "--tmpfs", "/.sill/host/etc"].iter().map(OsString::from).collect();
        seal_host_view(&mut a, 0, &layers, &expanded);
        let r = render(&a[4..]);
        assert_eq!(r, "--remount-ro /.sill/host/home/me --remount-ro /.sill/host/etc");
    }

    /// The masks of the host view are tmpfs: writable, and Revert copies from there.
    #[test]
    fn every_mask_of_the_host_view_is_read_only() {
        let mut a: Vec<OsString> = ["--tmpfs", "/.sill/host/home/me/.ssh", "--ro-bind", "/dev/null", "/.sill/host/home/me/.netrc", "--tmpfs", "/.sill/host/home/me/.config/x", "--tmpfs", "/.sill/host/home/me/.config"]
            .iter().map(OsString::from).collect();
        let n = a.len();
        seal_host_view(&mut a, 0, &[layer_for(Path::new("/s"), Path::new("/home"))], &[]);
        assert_eq!(render(&a[n..]), "--remount-ro /.sill/host/home --remount-ro /.sill/host/home/me/.ssh --remount-ro /.sill/host/home/me/.config");
        // a real plan: every tmpfs laid in the view is sealed after the last mount, or lies under a later one
        let plan = spec().plan().unwrap();
        let a: Vec<String> = plan.argv.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        let masks: Vec<(usize, &String)> = a.windows(2).enumerate().filter(|(_, w)| w[0] == "--tmpfs" && w[1].starts_with(HOST_VIEW)).map(|(i, w)| (i + 1, &w[1])).collect();
        assert!(!masks.is_empty());
        for (i, at) in masks {
            let sealed = a.windows(2).any(|w| w[0] == "--remount-ro" && w[1] == *at);
            let covered = a.iter().enumerate().any(|(j, c)| j > i && c != at && c.starts_with(HOST_VIEW) && Path::new(at).starts_with(c) && a[j - 1] != "--remount-ro");
            assert!(sealed || covered, "{at} stays writable");
        }
    }

    #[test]
    fn the_state_filesystem_is_hidden_at_every_mount_point_it_has() {
        // Sill.app's VM: the state disk at /mnt/sill-state, bound to /var/lib/sill
        let mut s = spec();
        s.mounts = mounts::parse_mountinfo(
            "25 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
             60 25 254:32 / /mnt/sill-state rw - ext4 /dev/vdc rw\n\
             61 25 8:16 / /mnt/data rw - ext4 /dev/sdb1 rw\n\
             70 25 254:32 / /var/lib/sill rw - ext4 /dev/vdc rw\n",
        );
        let plan = s.plan().unwrap();
        assert!(!plan.layers.iter().any(|l| l.host.starts_with("/mnt/sill-state")), "the state disk is never a layer");
        let r = render(&plan.argv);
        assert!(!r.contains("--overlay-src /mnt/sill-state"), "{r}");
        assert!(!r.contains("/.sill/host/mnt/sill-state") && !r.contains("/.sill/host/var/lib/sill"), "{r}");
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        // another filesystem on another device is not taken for it
        assert_eq!(mounts::state_aliases(&s.mounts, Path::new(STATE_FS)), [PathBuf::from("/mnt/sill-state")]);

        // a state filesystem that is also the home's cannot be hidden: the launch says so
        s.mounts = mounts::parse_mountinfo(
            "25 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
             60 25 8:5 / /home rw - ext4 /dev/sda5 rw\n\
             70 25 8:5 / /var/lib/sill rw - ext4 /dev/sda5 rw\n",
        );
        let plan = s.plan().unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("also mounted at /home")), "{:?}", plan.warnings);
        assert!(plan.layers.iter().any(|l| l.host == Path::new("/home")));

        // a bind of a subdirectory shows another tree: not an alias
        let mounts = mounts::parse_mountinfo(
            "25 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
             60 25 8:5 / /data rw - ext4 /dev/sda5 rw\n\
             70 25 8:5 /sill /var/lib/sill rw - ext4 /dev/sda5 rw\n",
        );
        assert!(mounts::state_aliases(&mounts, Path::new(STATE_FS)).is_empty());
    }

    #[test]
    fn a_filesystem_overlayfs_refuses_is_bound_read_only() {
        // a policy that does not hide /boot, on a machine whose /boot is vfat
        let mut s = spec();
        s.mounts = mounts::parse_mountinfo(
            "25 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
             40 25 8:1 / /boot rw - vfat /dev/sda1 rw\n\
             70 25 7:0 / /var/lib/sill rw - ext4 /dev/loop0 rw\n",
        );
        s.policy.paths.hidden_mounts.clear();
        let plan = s.plan().unwrap();
        assert!(!plan.layers.iter().any(|l| l.host == Path::new("/boot")), "vfat cannot be an overlay lower");
        let r = render(&plan.argv);
        assert!(r.contains("--ro-bind /boot /boot"), "{r}");
        assert!(!r.contains("--overlay-src /boot"));
    }

    #[test]
    fn seccomp_flag_follows_policy_and_fd() {
        let mut s = spec();
        assert!(!render(&s.plan().unwrap().argv).contains("--seccomp"), "no fd: no flag");
        s.seccomp_fd = Some(7);
        let r = render(&s.plan().unwrap().argv);
        assert!(r.contains("--seccomp 7"), "{r}");
        assert!(r.find("--seccomp").unwrap() < r.find("--overlay").unwrap_or(usize::MAX), "before the mounts");
        s.policy.jail.seccomp = false;
        assert!(!render(&s.plan().unwrap().argv).contains("--seccomp"), "policy off: no flag");
    }

    #[test]
    fn state_dir_inside_layer_is_refused() {
        let mut s = spec();
        s.state_dir = PathBuf::from("/home/me/.local/state/sill/x");
        assert!(s.plan().is_err());
        s.unsafe_state = true;
        let plan = s.plan().unwrap();
        assert!(!plan.warnings.is_empty());
        assert!(render(&plan.argv).contains("--tmpfs /home/me/.local/state/sill/x"));
    }
}
