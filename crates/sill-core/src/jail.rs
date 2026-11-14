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
    /// Extra arguments inserted before `--`.
    pub extra_args: Vec<OsString>,
    pub command: Vec<OsString>,
}

#[derive(Debug, Clone)]
pub struct RootEntry {
    pub name: String,
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
        let name = e.file_name().to_string_lossy().into_owned();
        let md = e.path().symlink_metadata()?;
        let symlink = if md.file_type().is_symlink() { Some(std::fs::read_link(e.path())?) } else { None };
        let readable = md.is_dir() && std::fs::read_dir(e.path()).is_ok();
        out.push(RootEntry { name, symlink, is_dir: md.is_dir(), readable });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

impl JailSpec {
    pub fn plan(&self) -> Result<JailPlan> {
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
        let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
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
            symlinks: &mut Vec<(PathBuf, String)>,
            expanded: &mut Vec<PathBuf>,
            warnings: &mut Vec<String>,
        ) {
            if is_hidden(path) || is_state_fs(path) {
                return;
            }
            let is_mount = spec.mounts.iter().any(|m| m.mount_point == path);
            if is_mount && !real.iter().any(|r| r.mount_point == path) {
                ro_binds.push(path.to_path_buf()); // non-persistent fs (squashfs, fuse): read-only
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
                        symlinks.push((t, child.to_string_lossy().trim_start_matches('/').to_string()));
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
                symlinks.push((target.clone(), e.name.clone()));
                continue;
            }
            if !e.is_dir || SKIP.contains(&e.name.as_str()) || FRESH.contains(&e.name.as_str()) {
                continue;
            }
            if RO_BIND.contains(&e.name.as_str()) {
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
        push(&mut a, &[&"--new-session", &"--cap-drop", &"ALL", &"--clearenv"]);

        // environment
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let home = self.home.to_string_lossy().into_owned();
        env.insert(
            "PATH".into(),
            format!("{home}/.local/bin:{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
        );
        env.insert("HOME".into(), home.clone());
        env.insert("USER".into(), self.user.clone());
        env.insert("LOGNAME".into(), self.user.clone());
        env.insert("SHELL".into(), "/bin/bash".into());
        // under the shelf's systemd unit TERM arrives empty, which leaves TUIs (login prompts) unable
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
            env.insert("NO_PROXY".into(), "".into());
            env.insert("no_proxy".into(), "".into());
        }
        env.extend(p.jail.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        for (k, v) in &env {
            push(&mut a, &[&"--setenv", k, v]);
        }

        // root symlinks, fresh dirs, /sys
        for (target, name) in symlinks.iter().filter(|(_, n)| !n.contains('/')) {
            push(&mut a, &[&"--symlink", target, &format!("/{name}")]);
        }
        push(&mut a, &[&"--proc", &"/proc", &"--dev", &"/dev", &"--tmpfs", &"/tmp", &"--tmpfs", &"/run"]);
        // expanded directories (contain a mount point): tmpfs, then their children below
        expanded.sort();
        for d in &expanded {
            push(&mut a, &[&"--tmpfs", d]);
        }
        for (target, name) in symlinks.iter().filter(|(_, n)| n.contains('/')) {
            push(&mut a, &[&"--symlink", target, &format!("/{name}")]);
        }
        push(&mut a, &[&"--dir", &format!("/run/user/{}", self.uid)]);
        // optional desktop access (policy [jail] x11 / dbus): the sockets and the variables that point at them
        if p.jail.x11 {
            if let Ok(display) = std::env::var("DISPLAY") {
                let num = display.trim_start_matches(':').split('.').next().unwrap_or("0").to_string();
                let sock = format!("/tmp/.X11-unix/X{num}");
                if Path::new(&sock).exists() {
                    push(&mut a, &[&"--ro-bind", &sock, &sock, &"--setenv", &"DISPLAY", &display]);
                }
                let xauth = std::env::var("XAUTHORITY").unwrap_or_else(|_| format!("{}/.Xauthority", self.home.display()));
                if Path::new(&xauth).is_file() {
                    push(&mut a, &[&"--ro-bind", &xauth, &xauth, &"--setenv", &"XAUTHORITY", &xauth]);
                }
            } else {
                warnings.push("policy asks for x11 but DISPLAY is not set".into());
            }
        }
        if p.jail.dbus {
            let bus = format!("/run/user/{}/bus", self.uid);
            if Path::new(&bus).exists() {
                push(&mut a, &[&"--bind", &bus, &bus, &"--setenv", &"DBUS_SESSION_BUS_ADDRESS", &format!("unix:path={bus}")]);
            }
        }
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

        // masks (both views), direct, ro
        let mut masked = Vec::new();
        for pat in &p.paths.mask {
            for path in expand_pattern(pat, &self.home) {
                let is_dir = path.is_dir();
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
        let mut direct = Vec::new();
        for pat in &p.paths.direct {
            for path in expand_pattern(pat, &self.home) {
                push(&mut a, &[&"--bind", &path, &path]);
                direct.push(path);
            }
        }
        for pat in &p.paths.ro {
            for path in expand_pattern(pat, &self.home) {
                push(&mut a, &[&"--ro-bind", &path, &path]);
            }
        }
        for (host, inside) in &self.binds {
            push(&mut a, &[&"--bind", host, inside]);
            direct.push(inside.clone());
        }

        if let Some(sock) = &self.proxy_socket {
            push(&mut a, &[&"--ro-bind", sock, &PROXY_SOCKET]);
        }
        a.extend(self.extra_args.iter().cloned());
        // bwrap's root tmpfs is writable by default; make it read-only now that every mount is in place
        push(&mut a, &[&"--remount-ro", &"/"]);
        push(&mut a, &[&"--chdir", &self.cwd]);
        a.push("--".into());
        if self.command.is_empty() {
            bail!("no command for the jail");
        }
        a.extend(self.command.iter().cloned());

        Ok(JailPlan { argv: a, layers, expanded, masked, direct, warnings })
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
                name: n.to_string(),
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
            extra_args: vec![],
            command: vec!["true".into()],
        }
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
