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

        // --- layers: top-level directories, then persistent mounts below them
        let real = mounts::real_mounts(&self.mounts);
        let mut layers: Vec<Layer> = Vec::new();
        let mut ro_binds: Vec<PathBuf> = Vec::new();
        let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
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
            if is_hidden(&path) {
                continue;
            }
            if !e.readable {
                warnings.push(format!("{} not readable, left out of the jail", path.display()));
                continue;
            }
            // a top-level dir that is a mount of a non-persistent fs (squashfs, fuse, …): read-only
            if self.mounts.iter().any(|m| m.mount_point == path) && !real.iter().any(|r| r.mount_point == path) {
                ro_binds.push(path);
                continue;
            }
            layers.push(self.layer_for(&path));
        }
        for m in &real {
            let top = m.mount_point.components().nth(1).map(|c| PathBuf::from("/").join(c.as_os_str()));
            let Some(top) = top else { continue };
            if m.mount_point == top {
                continue; // handled as a top-level entry above
            }
            if !layers.iter().any(|l| l.host == top) || is_hidden(&m.mount_point) {
                continue;
            }
            if is_under(&self.state_dir, &m.mount_point) {
                continue; // the sill state filesystem: never a layer, invisible to its parent overlay
            }
            layers.push(self.layer_for(&m.mount_point));
        }
        // parents before children (mount order)
        layers.sort_by(|a, b| a.host.cmp(&b.host));

        // --- state dir safety
        let state_layer = layers.iter().find(|l| is_under(&self.state_dir, &l.host));
        let state_on_submount = self.mounts.iter().any(|m| {
            m.mount_point != Path::new("/")
                && is_under(&self.state_dir, &m.mount_point)
                && !layers.iter().any(|l| l.host == m.mount_point)
        });
        if state_layer.is_some() && !state_on_submount {
            let msg = format!(
                "state dir {} lies inside overlaid tree {} on the same filesystem (SPEC §4.9: use the sill state filesystem)",
                self.state_dir.display(),
                state_layer.unwrap().host.display()
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
        env.insert("TERM".into(), std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()));
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
        for (target, name) in &symlinks {
            push(&mut a, &[&"--symlink", target, &format!("/{name}")]);
        }
        push(&mut a, &[&"--proc", &"/proc", &"--dev", &"/dev", &"--tmpfs", &"/tmp", &"--tmpfs", &"/run"]);
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
        for l in &layers {
            push(&mut a, &[&"--ro-bind", &l.host, &host_view(&l.host)]);
        }
        let mut host_masks: Vec<PathBuf> = Vec::new();
        for m in &self.mounts {
            if m.mount_point == Path::new("/") || layers.iter().any(|l| l.host == m.mount_point) {
                continue;
            }
            if layers.iter().any(|l| is_under(&m.mount_point, &l.host)) {
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

        Ok(JailPlan { argv: a, layers, masked, direct, warnings })
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
             80 25 0:40 / /home/me/.cache/doc rw - fuse.portal portal rw\n",
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
        assert_eq!(hosts, vec!["/etc", "/home", "/mnt", "/mnt/data", "/usr", "/var"]);
        assert_eq!(plan.layers[3].upper, PathBuf::from("/var/lib/sill/1000/sessions/x/upper/mnt%data"));
        let s = render(&plan.argv);
        assert!(s.contains("--tmpfs /.sill/host/var/lib/sill"));
        assert!(s.contains("--tmpfs /.sill/host/home/me/.cache/doc"));
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
