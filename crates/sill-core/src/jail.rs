//! Turn a policy and a session into a bubblewrap command line (SPEC §4.2).
//!
//! Layout inside the jail:
//! - `/` is bwrap's read-only tmpfs with the host's top-level symlinks (`/bin → usr/bin`, …);
//! - every top-level directory of the host and every persistent mount below one gets its own overlay
//!   (`--overlay-src D --overlay upper work D`), upper/work on the session's state dir;
//! - `/proc /dev /tmp /run` are fresh, `/sys` read-only;
//! - `/.sill/host/…` is the real tree read-only (per layer, sub-mounts masked) for revert and diffs;
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

use crate::mounts::{self, Mount};
use crate::paths::{expand_pattern, is_under, layer_name};
use crate::policy::{NetMode, Policy};

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
    /// Expanded policy paths from the immutable launch plan; None only for standalone CLI jails.
    pub policy_paths: Option<crate::agent_storage::PolicyPaths>,
    /// Targets of validated private agent bindings.
    pub agent_binds: Vec<(PathBuf, crate::agents::HomePathKind)>,
    /// Repository-fragment restrictions, applied after writable pins and other binds.
    /// Kept separate from base-policy paths so a writable pin can contain a fragment mask.
    pub fragment_ro: Vec<PathBuf>,
    pub fragment_masks: Vec<PathBuf>,
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

impl JailSpec {
    pub fn plan(&self) -> Result<JailPlan> {
        self.policy.permissions.validate_launch(&self.policy.network)?;
        let p = &self.policy;
        let mut warnings = Vec::new();
        let hidden: Vec<PathBuf> =
            p.paths.hidden_mounts.iter().map(|h| crate::paths::expand_home(h, &self.home)).collect();
        let is_hidden = |path: &Path| hidden.iter().any(|h| is_under(path, h));

        // --- layers: top-level directories, then persistent mounts below them. A directory that has a
        // mount point somewhere inside it can't be an overlay lower (the kernel refuses it), so it is
        // expanded: it becomes a tmpfs in the jail and each child is handled on its own (dirs → overlay
        // or expand again, symlinks → symlink, files → read-only bind).
        let real = mounts::real_mounts(&self.mounts);
        let mut layers: Vec<Layer> = Vec::new();
        let mut ro_binds: Vec<PathBuf> = Vec::new();
        let mut symlinks: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut expanded: Vec<PathBuf> = Vec::new();
        let has_submount = |path: &Path| {
            self.mounts.iter().any(|m| m.mount_point != path && is_under(&m.mount_point, path))
        };
        // the state fs and anything inside the session's own state dir: never overlaid, never visible
        let is_state_fs = |path: &Path| is_under(path, Path::new(STATE_FS)) || is_under(path, &self.state_dir);
        // returns true when the dir was handled (overlay, expansion or ro-bind)
        fn handle_dir(
            path: &Path,
            spec: &JailSpec,
            real: &[Mount],
            is_hidden: &dyn Fn(&Path) -> bool,
            has_submount: &dyn Fn(&Path) -> bool,
            is_state_fs: &dyn Fn(&Path) -> bool,
            layers: &mut Vec<Layer>,
            ro_binds: &mut Vec<PathBuf>,
            symlinks: &mut Vec<(PathBuf, PathBuf)>,
            expanded: &mut Vec<PathBuf>,
            warnings: &mut Vec<String>,
        ) {
            if is_hidden(path) || is_state_fs(path) {
                return;
            }
            let is_mount = spec.mounts.iter().any(|m| m.mount_point == path);
            // non-persistent fs (squashfs, fuse), or one overlayfs refuses as a lower (vfat):
            // read-only
            if is_mount && real.iter().find(|r| r.mount_point == path).is_none_or(|r| !mounts::overlayable(r)) {
                ro_binds.push(path.to_path_buf());
                return;
            }
            if !has_submount(path) {
                layers.push(spec.layer_for(path));
                return;
            }
            let Ok(rd) = std::fs::read_dir(path) else {
                warnings.push(format!("{} not readable, left out of the jail", path.display()));
                return;
            };
            expanded.push(path.to_path_buf());
            let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let child = e.path();
                let Ok(md) = child.symlink_metadata() else { continue };
                if md.file_type().is_symlink() {
                    if let Ok(t) = std::fs::read_link(&child) {
                        symlinks.push((t, child.strip_prefix("/").unwrap_or(&child).to_path_buf()));
                    }
                } else if md.is_dir() {
                    if std::fs::read_dir(&child).is_err() {
                        continue; // unreadable (e.g. root-only): absent in the jail
                    }
                    handle_dir(&child, spec, real, is_hidden, has_submount, is_state_fs, layers, ro_binds, symlinks, expanded, warnings);
                } else {
                    ro_binds.push(child);
                }
            }
        }
        for e in &self.root_entries {
            let path = PathBuf::from("/").join(&e.name);
            if let Some(target) = &e.symlink {
                symlinks.push((target.clone(), PathBuf::from(&e.name)));
                continue;
            }
            if !e.is_dir || SKIP.iter().any(|n| e.name == *n) || FRESH.iter().any(|n| e.name == *n) {
                continue;
            }
            if RO_BIND.iter().any(|n| e.name == *n) {
                ro_binds.push(path);
                continue;
            }
            if !e.readable {
                warnings.push(format!("{} not readable, left out of the jail", path.display()));
                continue;
            }
            handle_dir(&path, self, &real, &is_hidden, &has_submount, &is_state_fs, &mut layers, &mut ro_binds, &mut symlinks, &mut expanded, &mut warnings);
        }
        // parents before children (mount order)
        layers.sort_by(|a, b| a.host.cmp(&b.host));
        // Never let distinct host roots reuse an upper tree (/a/b and /a%b both flatten to a%b).
        let mut owners = BTreeMap::new();
        for layer in &layers {
            if let Some(previous) = owners.insert(layer.upper.clone(), layer.host.clone()) {
                if previous != layer.host { bail!("overlay state name collision between {} and {}; conflicting mount layouts are unsupported", previous.display(), layer.host.display()); }
            }
        }

        // --- state dir safety
        let state_in_tree = layers
            .iter()
            .map(|l| l.host.clone())
            .chain(expanded.iter().cloned())
            .find(|h| is_under(&self.state_dir, h));
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

        // --- argv
        let mut a: Vec<OsString> = Vec::new();
        let push = |a: &mut Vec<OsString>, items: &[&dyn AsRef<std::ffi::OsStr>]| {
            for i in items {
                a.push(i.as_ref().to_os_string());
            }
        };
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

        // environment
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
        for (k, v) in &env {
            push(&mut a, &[&"--setenv", k, v]);
        }

        // root symlinks, fresh dirs, /sys
        for (target, name) in symlinks.iter().filter(|(_, n)| n.components().count() == 1) {
            push(&mut a, &[&"--symlink", target, &Path::new("/").join(name)]);
        }
        push(&mut a, &[&"--proc", &"/proc", &"--dev", &"/dev", &"--tmpfs", &"/tmp", &"--tmpfs", &"/run"]);
        // expanded directories (contain a mount point): tmpfs, then their children below
        expanded.sort();
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

        // overlays
        for l in &layers {
            push(&mut a, &[&"--overlay-src", &l.host, &"--overlay", &l.upper, &l.work, &l.host]);
        }

        // host view: same layers read-only, every other mount below them masked
        push(&mut a, &[&"--dir", &HOST_VIEW]);
        let host_binds: Vec<PathBuf> = {
            // top-most of: expanded dirs and layers (an expanded dir's bind already covers its children)
            let mut v: Vec<PathBuf> = expanded.iter().cloned().chain(layers.iter().map(|l| l.host.clone())).collect();
            v.sort();
            let mut out: Vec<PathBuf> = Vec::new();
            for p in v {
                if !out.iter().any(|o| is_under(&p, o)) {
                    out.push(p);
                }
            }
            out
        };
        for h in &host_binds {
            push(&mut a, &[&"--ro-bind", h, &host_view(h)]);
        }
        let mut host_masks: Vec<PathBuf> = Vec::new();
        for m in &self.mounts {
            if m.mount_point == Path::new("/") || layers.iter().any(|l| l.host == m.mount_point) {
                continue;
            }
            if host_binds.iter().any(|h| is_under(&m.mount_point, h)) {
                host_masks.push(m.mount_point.clone());
            }
        }
        host_masks.sort();
        host_masks.dedup();
        for m in &host_masks {
            push(&mut a, &[&"--tmpfs", &host_view(m)]);
        }
        if self.unsafe_state {
            // the agent must not reach its own upper trees
            push(&mut a, &[&"--tmpfs", &self.state_dir, &"--tmpfs", &host_view(&self.state_dir)]);
        }

        // masks (both views), direct, ro. Sill's own files are masked whatever the policy says: its
        // configuration (policies, agent overrides — a session's mounts and allowed domains are made
        // of them) and its whole state directory — the session store, the staged binaries
        // (security.md S2), every agent's sill-owned home (`agents/`, the login scratch), the
        // daemon's lock, and whatever is put there next: an allow-list, so a new
        // file is hidden until someone decides otherwise. The current agent gets its own home bound
        // at `~/.claude` etc. from the host side, the others' stay out of sight (SPEC §4.11 #8); the
        // installed adapters (`tools/`) are bound back read-only below, the agent runs from there.
        let mut masked = Vec::new();
        let config_dir = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| self.home.join(".config"));
        let state_base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| self.home.join(".local/state"));
        let state_dir = state_base.join("sill");
        let own: Vec<PathBuf> = [config_dir.join("sill"), state_dir.clone()]
            .into_iter()
            // the user's journal is readable by ACL: every session's proxy lines and agent stderr (S15)
            .chain([PathBuf::from("/var/log/journal")])
            .chain(self.masks.iter().cloned())
            .filter(|p| p.exists())
            .collect();
        let policy_masks: Vec<(PathBuf, bool)> = match &self.policy_paths {
            Some(paths) => paths.masks.iter().map(|m| Ok((m.path.to_path()?, m.kind == crate::agents::HomePathKind::Directory))).collect::<Result<_>>()?,
            None => p.paths.mask.iter().flat_map(|pat| expand_pattern(pat, &self.home)).chain(own).map(|p| { let dir = p.is_dir(); (p, dir) }).collect(),
        };
        for (path, is_dir) in policy_masks {
            if masked.contains(&path) {
                continue;
            }
            {
                for target in [path.clone(), host_view(&path)] {
                    if is_dir {
                        push(&mut a, &[&"--tmpfs", &target]);
                    } else {
                        push(&mut a, &[&"--ro-bind", &"/dev/null", &target]);
                    }
                }
                masked.push(path);
            }
        }
        // Host agent originals are never reachable through the host view. A selected agent's
        // private binding may replace an agent mask, but never a policy secret mask.
        let mut agent_masked = Vec::new();
        let mut roots = self.agent_masks.clone();
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, kind) in roots {
            // Validate the nearest existing component; never follow an original-home symlink.
            let existing = path.ancestors().find(|p| p.symlink_metadata().is_ok()).context("agent root has no existing ancestor")?;
            let fd = rustix::fs::openat2(rustix::fs::CWD, existing, rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty(), rustix::fs::ResolveFlags::NO_SYMLINKS)
                .with_context(|| format!("host agent root {} must not contain symlinks", path.display()))?;
            if existing == path {
                let actual = rustix::fs::FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode);
                anyhow::ensure!(matches!((kind, actual), (crate::agents::HomePathKind::Directory, rustix::fs::FileType::Directory) | (crate::agents::HomePathKind::File, rustix::fs::FileType::RegularFile)), "host agent root has the wrong type: {}", path.display());
            }
            if agent_masked.iter().any(|p: &PathBuf| path.starts_with(p)) { continue; }
            // An absent root is masked in the jail's own view only (in case it appears on the host
            // later): the host view is read-only, so bwrap could not make a mount point there for
            // it (found 2026-11-07: `~/.gemini`, declared by the built-in gemini, never created).
            let targets: Vec<PathBuf> = if existing == path { vec![path.clone(), host_view(&path)] } else { vec![path.clone()] };
            for target in targets {
                if kind == crate::agents::HomePathKind::Directory { push(&mut a, &[&"--tmpfs", &target]); }
                else { push(&mut a, &[&"--ro-bind", &"/dev/null", &target]); }
            }
            agent_masked.push(path);
        }
        let mut mount_fds = Vec::new();
        // what the jail needs from sill's state, back over the mask: the adapters it runs
        let tools = state_dir.join("tools");
        if tools.is_dir() {
            for target in [tools.clone(), host_view(&tools)] {
                push(&mut a, &[&"--ro-bind", &tools, &target]);
            }
        }
        // a bind inside a mask puts the masked thing back, a bind over a mask buries it: neither
        // is mounted, whatever a policy, a pin or an agent override asks for (security.md S2)
        let mut direct = Vec::new();
        let direct_paths = match &self.policy_paths {
            Some(paths) => paths.direct.iter().map(|p| p.to_path()).collect::<Result<Vec<_>>>()?,
            None => p.paths.direct.iter().flat_map(|pat| expand_pattern(pat, &self.home)).collect(),
        };
        let ro_paths = match &self.policy_paths {
            Some(paths) => paths.read_only.iter().map(|p| p.to_path()).collect::<Result<Vec<_>>>()?,
            None => p.paths.ro.iter().flat_map(|pat| expand_pattern(pat, &self.home)).collect(),
        };
        for path in direct_paths {
            no_bind_across_mask(&path, &masked)?;
            no_bind_across_mask(&path, &agent_masked)?;
            push(&mut a, &[&"--bind", &path, &path]);
            direct.push(path);
        }
        for path in ro_paths {
            no_bind_across_mask(&path, &masked)?;
            no_bind_across_mask(&path, &agent_masked)?;
            push(&mut a, &[&"--ro-bind", &path, &path]);
        }
        for (host, inside) in &self.binds {
            if host == inside {
                // the host's own tree (a pin, a shared file): inside a mask it re-exposes, over a
                // mask it buries — neither is mounted
                no_bind_across_mask(inside, &masked)?;
                no_bind_across_mask(inside, &agent_masked)?;
                push(&mut a, &[&"--bind", host, inside]);
            } else {
                // sill's own tree in the host path's place (the session's private scratch, the
                // agent's sill-owned home): nothing of the host's below it is exposed, so a mask
                // beneath it is not buried but laid again on top — the masked path stays hidden
                // inside the bound tree too. Inside a mask it is still refused: the copy was
                // seeded from the host (2026-11-01: `~/.cache` scratch over a masked
                // `~/.cache/<other agent's path>` refused every jail on a machine with one).
                if let Some(m) = masked.iter().find(|m| is_under(inside, m)) {
                    no_bind_across_mask(inside, std::slice::from_ref(m))?;
                }
                if let Some((_, expected)) = self.agent_binds.iter().find(|(target, _)| target == inside) {
                    use rustix::fs::{openat2, fstat, FileType, OFlags, Mode, ResolveFlags, CWD};
                    let fd = openat2(CWD, host, OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW, Mode::empty(), ResolveFlags::NO_SYMLINKS)
                        .with_context(|| format!("open declared agent storage {}", host.display()))?;
                    let kind = FileType::from_raw_mode(fstat(&fd)?.st_mode);
                    anyhow::ensure!(matches!((expected, kind), (crate::agents::HomePathKind::Directory, FileType::Directory) | (crate::agents::HomePathKind::File, FileType::RegularFile)), "agent storage has the wrong type");
                    push(&mut a, &[&"--bind-fd", &fd.as_raw_fd().to_string(), inside]);
                    mount_fds.push(Arc::new(fd));
                } else {
                    if agent_masked.iter().any(|m| inside.starts_with(m)) {
                        no_bind_across_mask(inside, &agent_masked)?;
                    }
                    push(&mut a, &[&"--bind", host, inside]);
                    for m in agent_masked.iter().filter(|m| m.starts_with(inside)) {
                        if self.agent_masks.iter().any(|(p, kind)| p == m && *kind == crate::agents::HomePathKind::Directory) { push(&mut a, &[&"--tmpfs", m]); }
                        else { push(&mut a, &[&"--ro-bind", &"/dev/null", m]); }
                    }
                }
                for m in masked.iter().filter(|m| is_under(m, inside) && *m != inside) {
                    if m.is_dir() {
                        push(&mut a, &[&"--tmpfs", m]);
                    } else {
                        push(&mut a, &[&"--ro-bind", &"/dev/null", m]);
                    }
                }
            }
            direct.push(inside.clone());
        }

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
        masked.extend(agent_masked);
        mount_fds.extend(self.apply_workspace_binds(&mut a, &masked, &mut direct, &mut warnings)?);
        mount_fds.extend(self.apply_fragment_paths(&mut a, &mut masked)?);
        // bwrap's root tmpfs is writable by default; make it read-only now that every mount is in place
        push(&mut a, &[&"--remount-ro", &"/"]);
        push(&mut a, &[&"--chdir", &self.cwd]);
        a.push("--".into());
        if self.command.is_empty() {
            bail!("no command for the jail");
        }
        a.extend(self.command.iter().cloned());

        Ok(JailPlan { argv: a, mount_fds, layers, expanded, masked, direct, warnings })
    }

    /// A workspace session (docs/parallel-coding.md §12): each clone bound read-write at its
    /// repository's own path, after the pins (an ancestor pin is laid first) and before the
    /// repository's fragment rules, which then apply to the clone. The person's checkout is hidden
    /// in the read-only host view too. Its object store is borrowed read-only at
    /// `/.sill/borrowed/<n>`, and the jail sees the clone's `objects/info/alternates` naming that
    /// path (from memory, read-only), while on the host it names the real store: one clone, valid
    /// in both views. Every directory is opened without following symlinks and bound from that
    /// descriptor, so a path swapped after planning cannot bind another tree.
    fn apply_workspace_binds(&self, args: &mut Vec<OsString>, masked: &[PathBuf], direct: &mut Vec<PathBuf>, warnings: &mut Vec<String>) -> Result<Vec<Arc<OwnedFd>>> {
        use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
        let open = |p: &Path| -> Result<Arc<OwnedFd>> {
            let fd = openat2(CWD, p, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty(), ResolveFlags::NO_SYMLINKS)
                .with_context(|| format!("open {} (no symlinks)", p.display()))?;
            Ok(Arc::new(rustix::io::fcntl_dupfd_cloexec(&fd, 64)?))
        };
        let mut fds = Vec::new();
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
            let memfd = rustix::fs::memfd_create("sill-alternates", rustix::fs::MemfdFlags::CLOEXEC)?;
            rustix::io::write(&memfd, format!("{}\n", borrowed.display()).as_bytes())?;
            rustix::fs::seek(&memfd, rustix::fs::SeekFrom::Start(0))?;
            let memfd = Arc::new(rustix::io::fcntl_dupfd_cloexec(&memfd, 64)?);
            args.extend([OsString::from("--ro-bind-data"), memfd.as_raw_fd().to_string().into(), source.join(".git/objects/info/alternates").into_os_string()]);
            direct.push(source.clone());
            fds.extend([clone_fd, store, memfd]);
        }
        Ok(fds)
    }

    /// Where a host path really comes from inside this jail: under a workspace's repository, the
    /// same path in its clone.
    fn backing(&self, path: &Path) -> PathBuf {
        self.workspace_mask
            .iter()
            .find_map(|(source, clone, _)| path.strip_prefix(source).ok().map(|rel| clone.join(rel)))
            .unwrap_or_else(|| path.to_path_buf())
    }

    /// Apply repository restrictions last: a writable pin must never cover a mask or undo a
    /// read-only rule. The base policy's earlier masks remain authoritative too.
    fn apply_fragment_paths(&self, args: &mut Vec<OsString>, masked: &mut Vec<PathBuf>) -> Result<Vec<Arc<OwnedFd>>> {
        if self.fragment_ro.is_empty() && self.fragment_masks.is_empty() {
            return Ok(Vec::new());
        }
        let mut mount_fds = Vec::new();
        let restrictions: Vec<&PathBuf> = self.fragment_ro.iter().chain(&self.fragment_masks).collect();
        let overlaps = |a: &Path, b: &Path| is_under(a, b) || is_under(b, a);
        for path in &restrictions {
            if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                bail!("fragment restriction must be an absolute normalized path: {}", path.display());
            }
            // A symlink introduced after resolution must not redirect a restriction elsewhere.
            let canonical = path.canonicalize().with_context(|| format!("fragment path disappeared: {}", path.display()))?;
            if canonical != **path {
                bail!("fragment restriction changed or contains a symlink: {}", path.display());
            }
        }
        let check_bind = |host: &Path, inside: &Path| -> Result<()> {
            let source = host.canonicalize().unwrap_or_else(|_| host.to_path_buf());
            if source == inside && host == inside {
                return Ok(()); // ordinary identity pins are covered by the final restrictions
            }
            if restrictions.iter().any(|p| overlaps(&source, p) || overlaps(inside, p)) {
                bail!("bind {} -> {} overlaps a repository restriction at an alternate path; this mapping is unsupported",
                    host.display(), inside.display());
            }
            Ok(())
        };
        for (host, inside) in &self.binds {
            check_bind(host, inside)?;
        }
        for pat in self.policy.paths.direct.iter().chain(&self.policy.paths.ro) {
            for path in expand_pattern(pat, &self.home) {
                check_bind(&path, &path)?;
            }
        }
        if let Some(dir) = &self.memory_dir {
            check_bind(dir, Path::new(sill_memory::JAIL_DIR))?;
        }
        if let Some(dir) = &self.bridge_dir {
            check_bind(dir, Path::new(BRIDGE_DIR))?;
        }
        if let Some(sock) = &self.proxy_socket {
            check_bind(sock, Path::new(PROXY_SOCKET))?;
        }
        // The launcher currently adds only read-only binary binds. Do not permit opaque fd or
        // overlay arguments to smuggle another alias of a restricted repository into the jail.
        let mut extra = self.extra_args.as_slice();
        while !extra.is_empty() {
            if extra.len() < 3 || extra[0] != "--ro-bind" {
                bail!("unsupported extra mount arguments with repository path restrictions");
            }
            check_bind(Path::new(&extra[1]), Path::new(&extra[2]))?;
            extra = &extra[3..];
        }
        let mut readonly = self.fragment_ro.clone();
        readonly.sort();
        readonly.dedup();
        for path in &readonly {
            // A late bind must never unhide an earlier policy mask, including protected state.
            if let Some(m) = masked.iter().find(|m| is_under(path, m)) {
                bail!("fragment read-only path {} is inside masked {}", path.display(), m.display());
            }
            if self.policy.paths.hidden_mounts.iter().map(|p| crate::paths::expand_home(p, &self.home)).any(|p| overlaps(path, &p)) {
                bail!("fragment read-only path {} overlaps a hidden mount", path.display());
            }
            // Binding a broad root also replaces any nested writable pin mounts. Read-only bind
            // semantics are recursive in bubblewrap; applying parent then child stays read-only.
            // Open without following any symlink and retain the exact source inode. Reopening
            // a path in bubblewrap after planning could otherwise bind an unrelated host secret
            // substituted by a concurrent writer. Keep descriptor numbers away from shim fd 3.
            // inside a workspace the rule applies to the clone, which is what the jail has there
            let source = rustix::fs::openat2(rustix::fs::CWD, self.backing(path),
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(), rustix::fs::ResolveFlags::NO_SYMLINKS)
                .with_context(|| format!("open fragment read-only source {}", path.display()))?;
            let source = Arc::new(rustix::io::fcntl_dupfd_cloexec(&source, 64)?);
            args.extend([OsString::from("--ro-bind-fd"), source.as_raw_fd().to_string().into(), path.as_os_str().to_owned()]);
            mount_fds.push(source);
        }
        let mut final_masks: Vec<PathBuf> = masked.iter()
            .filter(|m| readonly.iter().any(|r| is_under(m, r)))
            .cloned().chain(self.fragment_masks.iter().cloned()).collect();
        final_masks.sort();
        final_masks.dedup();
        // A parent mask hides all children; attempting to create child mount points beneath an
        // already read-only /dev/null mask would be both unnecessary and invalid.
        let mut applied = Vec::<PathBuf>::new();
        for path in final_masks {
            if applied.iter().any(|p| is_under(&path, p)) {
                continue;
            }
            let is_dir = path.is_dir();
            for target in [path.clone(), host_view(&path)] {
                if is_dir {
                    args.extend([OsString::from("--tmpfs"), target.into_os_string()]);
                } else {
                    args.extend([OsString::from("--ro-bind"), OsString::from("/dev/null"), target.into_os_string()]);
                }
            }
            if !masked.contains(&path) {
                masked.push(path.clone());
            }
            applied.push(path);
        }
        Ok(mount_fds)
    }

    fn layer_for(&self, host: &Path) -> Layer {
        let name = layer_name(host);
        Layer {
            host: host.to_path_buf(),
            upper: self.state_dir.join("upper").join(&name),
            work: self.state_dir.join("work").join(&name),
        }
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
            fragment_ro: vec![],
            fragment_masks: vec![],
            memory_dir: None,
            bridge_dir: None,
            workspace_mask: vec![],
            seccomp_fd: None,
            extra_args: vec![],
            command: vec!["true".into()],
        }
    }

    struct FragmentTree(PathBuf);
    impl FragmentTree {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("sill-fragment-mount-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir_all(p.join("repo/nested")).unwrap();
            std::fs::create_dir_all(p.join("repo/secrets")).unwrap();
            std::fs::write(p.join("repo/secrets/key"), "secret").unwrap();
            Self(p.canonicalize().unwrap())
        }
        fn repo(&self) -> PathBuf { self.0.join("repo") }
    }
    impl Drop for FragmentTree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn fragment_restrictions_follow_writable_nested_pins_and_extra_mounts() {
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let child = repo.join("nested");
        let secret = repo.join("secrets");
        let mut s = spec();
        s.binds = vec![(repo.clone(), repo.clone()), (child.clone(), child.clone())];
        s.extra_args = vec!["--ro-bind".into(), "/bin/true".into(), "/.sill/test".into()];
        s.fragment_ro = vec![repo.clone()];
        s.fragment_masks = vec![secret.clone()];
        let plan = s.plan().unwrap();
        let find = |flag: &str, dest: &Path| plan.argv.windows(3).rposition(|a| a[0] == flag && a[2] == dest).unwrap();
        let ro = find("--ro-bind-fd", &repo);
        assert!(ro > find("--bind", &child));
        assert!(ro > find("--ro-bind", Path::new("/.sill/test")));
        let mask = plan.argv.windows(2).position(|a| a[0] == "--tmpfs" && a[1] == secret).unwrap();
        assert!(mask > ro);
        assert!(plan.argv.windows(2).any(|a| a[0] == "--tmpfs" && a[1] == host_view(&secret)));
        assert!(plan.masked.contains(&secret));
    }

    #[test]
    fn late_fragment_readonly_bind_restores_base_masks_in_both_views() {
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let secret = repo.join("secrets");
        let mut s = spec();
        s.masks = vec![secret.clone()];
        s.fragment_ro = vec![repo.clone()];
        let a = s.plan().unwrap().argv;
        let ro = a.windows(3).position(|a| a[0] == "--ro-bind-fd" && a[2] == repo).unwrap();
        for target in [&secret, &host_view(&secret)] {
            let mask = a.windows(2).rposition(|a| a[0] == "--tmpfs" && a[1] == *target).unwrap();
            assert!(mask > ro, "base mask must be restored after the broad read-only bind");
        }
        s.fragment_ro = vec![secret.join("key")];
        assert!(s.plan().unwrap_err().to_string().contains("inside masked"));
    }

    #[test]
    fn fragment_readonly_source_is_anchored_if_replaced_after_planning() {
        use std::os::unix::fs::MetadataExt;
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let protected = tree.0.join("protected");
        std::fs::create_dir(&protected).unwrap();
        std::fs::write(protected.join("key"), "not the repository").unwrap();
        let inode = std::fs::metadata(&repo).unwrap().ino();
        let mut s = spec();
        s.fragment_ro = vec![repo.clone()];
        s.masks = vec![protected.clone()];
        let plan = s.plan().unwrap();
        assert_eq!(plan.mount_fds.len(), 1);
        let fd = &plan.mount_fds[0];
        assert!(rustix::io::fcntl_getfd(fd.as_ref()).unwrap().contains(rustix::io::FdFlags::CLOEXEC));
        assert!(fd.as_raw_fd() >= 64);
        std::fs::rename(&repo, tree.0.join("original")).unwrap();
        std::os::unix::fs::symlink(&protected, &repo).unwrap();
        assert_eq!(rustix::fs::fstat(fd.as_ref()).unwrap().st_ino, inode);
        assert_ne!(rustix::fs::fstat(fd.as_ref()).unwrap().st_ino, std::fs::metadata(&repo).unwrap().ino());
        assert!(!plan.argv.windows(3).any(|a| a[0] == "--ro-bind" && a[1] == repo));
    }

    #[test]
    fn fragment_restrictions_reject_alternate_bind_sources_and_destinations() {
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let secret = repo.join("secrets");
        for (host, inside) in [
            (repo.clone(), tree.0.join("alias")),
            (secret.join("key"), tree.0.join("alias")),
            (tree.0.join("source"), repo.clone()),
            (tree.0.join("source"), secret.join("key")),
        ] {
            let mut s = spec();
            s.fragment_masks = vec![secret.clone()];
            s.binds = vec![(host.clone(), inside.clone())];
            assert!(s.plan().unwrap_err().to_string().contains("alternate path"));
            s.binds.clear();
            s.extra_args = vec!["--ro-bind".into(), host.into_os_string(), inside.into_os_string()];
            assert!(s.plan().unwrap_err().to_string().contains("alternate path"));
        }
        let mut s = spec();
        s.fragment_masks = vec![secret];
        s.extra_args = vec!["--args".into(), "10".into()];
        assert!(s.plan().unwrap_err().to_string().contains("unsupported extra mount"));
    }

    #[test]
    fn fragment_paths_reject_symlinks_missing_paths_and_hidden_mount_overlap() {
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let mut s = spec();
        std::os::unix::fs::symlink(repo.join("secrets"), repo.join("link")).unwrap();
        s.fragment_masks = vec![repo.join("link")];
        assert!(s.plan().unwrap_err().to_string().contains("symlink"));
        s.fragment_masks = vec![repo.join("missing")];
        assert!(s.plan().unwrap_err().to_string().contains("disappeared"));
        s.fragment_masks.clear();
        s.fragment_ro = vec![repo.clone()];
        s.policy.paths.hidden_mounts.push(repo.join("nested").to_str().unwrap().into());
        assert!(s.plan().unwrap_err().to_string().contains("hidden mount"));
    }

    #[test]
    fn fragment_parent_mask_covers_child_masks_and_file_masks_cover_both_views() {
        let tree = FragmentTree::new();
        let repo = tree.repo();
        let secret = repo.join("secrets");
        let key = secret.join("key");
        let mut s = spec();
        s.fragment_masks = vec![key.clone(), secret.clone()];
        let plan = s.plan().unwrap();
        assert!(plan.masked.contains(&secret));
        assert!(!plan.masked.contains(&key));
        s.fragment_masks = vec![key.clone()];
        let a = s.plan().unwrap().argv;
        for target in [&key, &host_view(&key)] {
            assert!(a.windows(3).any(|a| a[0] == "--ro-bind" && a[1] == "/dev/null" && a[2] == *target));
        }
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
        s.mounts = vec![Mount {mount_point:dir.join("a/b"),fstype:"ext4".into(),source:"test".into()}];
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
        let layer = s.layer_for(&exact);
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
    fn declared_agent_masks_cover_both_views_and_bind_a_held_private_source() {
        use crate::agents::HomePathKind::{Directory, File};
        let tree = FragmentTree::new();
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
        let tree = FragmentTree(root.canonicalize().unwrap());
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
    /// `/.sill/borrowed/0` and the jail's alternates file written from memory; then the
    /// repository's fragment rules, and `/` read-only last.
    #[test]
    fn a_workspace_clone_takes_the_repositorys_place() {
        let dir = std::env::temp_dir().join(format!("sill-jail-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (projects, source, clone) = (dir.join("projects"), dir.join("projects/src"), dir.join("ws/clone"));
        let objects = source.join(".git/objects");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(clone.join("sub")).unwrap();
        std::fs::create_dir_all(source.join("sub")).unwrap();
        let mut s = spec();
        s.binds = vec![(projects.clone(), projects.clone())];
        s.workspace_mask = vec![(source.clone(), clone.clone(), objects.clone())];
        s.fragment_ro = vec![source.join("sub")];
        let plan = s.plan().unwrap();
        let r = render(&plan.argv);
        let at = |needle: String| r.find(&needle).unwrap_or_else(|| panic!("{needle} not in {r}"));
        let pin = at(format!("--bind {} {} ", projects.display(), projects.display()));
        let bound = at(format!(" {} --tmpfs {} ", source.display(), host_view(&source).display()));
        let store = at(" /.sill/borrowed/0 ".into());
        let alternates = at(format!(" {} ", source.join(".git/objects/info/alternates").display()));
        let fragment = r.rfind(&format!(" {} ", source.join("sub").display())).unwrap();
        let root = at("--remount-ro / ".into());
        assert!(pin < bound && bound < store && store < alternates && alternates < fragment && fragment < root, "{r}");
        assert!(r.contains("--bind-fd ") && r.contains("--ro-bind-data "));
        assert!(plan.direct.contains(&source), "writes there are the clone's, not the overlay's");
        // the clone, the store, the alternates data, the fragment rule: all by descriptor
        assert_eq!(plan.mount_fds.len(), 4);
        // a store outside the checkout is refused
        s.workspace_mask = vec![(source.clone(), clone.clone(), dir.join("elsewhere"))];
        s.fragment_ro = vec![];
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
        let tree = FragmentTree(root.canonicalize().unwrap());
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
    /// again on top; the host's own tree over a mask is still refused, as is any bind inside one.
    #[test]
    fn own_tree_over_a_mask_is_remasked_host_tree_is_refused() {
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
        // the host's own tree over the same mask: refused
        let mut s = spec();
        s.masks = vec![dir.join("cache/other-agent")];
        s.binds = vec![(dir.join("cache"), dir.join("cache"))];
        assert!(s.plan().is_err());
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
        assert!(!hosts.contains(&"/var".to_string()) && !hosts.contains(&"/mnt".to_string()));
        assert!(!hosts.iter().any(|h| h.contains("sill")));
        assert!(plan.expanded.contains(&PathBuf::from("/var")) && plan.expanded.contains(&PathBuf::from("/mnt")));
        let s = render(&plan.argv);
        assert!(s.contains("--tmpfs /var "));
        assert!(s.contains("--tmpfs /.sill/host/var/lib/sill"));
        assert!(s.contains("--tmpfs /.sill/host/mnt/portal"));
        assert!(s.contains("--symlink usr/bin /bin"));
        assert!(s.contains("--disable-userns"));
        assert!(!s.contains("--share-net"));
        assert!(s.contains("--ro-bind /sys /sys"));
        assert!(s.ends_with("--chdir /home/me -- true"));
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
