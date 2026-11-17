//! `sill init` (SPEC §4.12): the one root step that sets a machine up for sill, idempotent, and
//! `sill init --user` for the per-account part. Everything an installer or a package `postinst`
//! does goes through here, so there is one place to read. `--dry-run` prints the plan only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use sill_core::jail::STATE_FS;
use sill_core::mounts;

#[derive(Args, Clone)]
pub struct InitArgs {
    /// print what would be done, change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// install prefix for the binaries, icons and the desktop file
    #[arg(long, default_value = "/usr/local")]
    pub prefix: PathBuf,
    /// per-user setup (no root): sill-gui autostart, hotkey hint
    #[arg(long)]
    pub user: bool,
    /// Install only the daemon and runtime; require systemd for account setup
    #[arg(long)]
    pub headless: bool,
    /// use an existing partition (a mount point) as the state filesystem instead of an image; it is
    /// bind-mounted at /var/lib/sill
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// size of the sparse state image created at /var/lib/sill.img
    #[arg(long, default_value = "32G")]
    pub state_size: String,
    /// delegate a cgroup subtree (/sys/fs/cgroup/sill/<uid>) even where a systemd user instance
    /// exists; done by default only without one
    #[arg(long)]
    pub delegate_cgroup: bool,
    /// only (re)do the cgroup delegation: what the boot hook runs
    #[arg(long)]
    pub cgroup_only: bool,
    /// leave binaries, policy, icons and units alone (a package put them in place already)
    #[arg(long)]
    pub no_copy: bool,
}

const UNIT_SETUP: &str = include_str!("../../../packaging/systemd/sill-state-setup.service");
const UNIT_MOUNT: &str = include_str!("../../../packaging/systemd/var-lib-sill.mount");
const UNIT_PERMS: &str = include_str!("../../../packaging/systemd/sill-state-perms.service");
const UNIT_GUI: &str = include_str!("../../../packaging/systemd/user/sill-gui.service");
const UNIT_DAEMON: &str = include_str!("../../../packaging/systemd/user/sill.service");
const SOCKET_DAEMON: &str = include_str!("../../../packaging/systemd/user/sill.socket");
const DESKTOP: &str = include_str!("../../../packaging/sill.desktop");
const ICON: &str = include_str!("../../../packaging/icons/sill.svg");

const IMAGE: &str = "/var/lib/sill.img";
const CGROUP_SILL: &str = "/sys/fs/cgroup/sill";
const CONTROLLERS: &[&str] = &["cpu", "io", "memory", "pids"];

/// Prints and (unless dry) performs steps; remembers what it did and skipped for the summary.
struct Init {
    dry: bool,
    did: Vec<String>,
    skipped: Vec<String>,
    problems: Vec<String>,
}

impl Init {
    fn done(&mut self, what: impl Into<String>) {
        let w = what.into();
        println!("{} {w}", if self.dry { "would:" } else { "did:  " });
        self.did.push(w);
    }
    fn skip(&mut self, what: impl Into<String>) {
        let w = what.into();
        println!("skip:  {w}");
        self.skipped.push(w);
    }
    fn problem(&mut self, what: impl Into<String>) {
        let w = what.into();
        println!("PROBLEM: {w}");
        self.problems.push(w);
    }
    fn ok(&self, what: impl AsRef<str>) {
        println!("ok:    {}", what.as_ref());
    }

    /// Write `content` to `path` (mode `mode`) unless it is already there unchanged.
    fn put_file(&mut self, path: &Path, content: &[u8], mode: u32) -> Result<bool> {
        if std::fs::read(path).map(|cur| cur == content).unwrap_or(false) {
            self.skip(format!("{} (unchanged)", path.display()));
            return Ok(false);
        }
        self.done(format!("write {}", path.display()));
        if !self.dry {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
            }
            let tmp = path.with_extension("sill-new");
            std::fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
            std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        }
        Ok(true)
    }

    fn run(&mut self, what: &str, prog: &str, args: &[&str]) -> Result<()> {
        self.done(format!("{what}: {prog} {}", args.join(" ")));
        if self.dry {
            return Ok(());
        }
        let st = std::process::Command::new(prog).args(args).status().with_context(|| format!("run {prog}"))?;
        if !st.success() {
            bail!("{prog} {} failed ({st})", args.join(" "));
        }
        Ok(())
    }
}

pub fn run(a: InitArgs) -> Result<()> {
    if a.user {
        return user_setup(&a);
    }
    let root = rustix::process::geteuid().is_root();
    if !root && !a.dry_run {
        bail!("sill init needs root (sudo sill init); `--dry-run` shows the plan without it, `--user` is the per-account part");
    }
    let mut i = Init { dry: a.dry_run, did: vec![], skipped: vec![], problems: vec![] };
    let has_systemd = Path::new("/run/systemd/system").is_dir();
    println!("sill init{} — prefix {}, {}", if a.dry_run { " (dry run)" } else { "" }, a.prefix.display(), if has_systemd { "systemd" } else { "no systemd" });

    if a.cgroup_only {
        delegate_cgroups(&mut i)?;
        return summary(&i);
    }

    check_prefix(&a.prefix)?;
    // 1. checks: reported, not fixed
    checks(&mut i);
    if a.headless && !i.problems.is_empty() {
        bail!("headless prerequisites failed; resolve the problems above before installation");
    }
    // 2. files
    if !a.no_copy {
        install_files(&mut i, &a.prefix, has_systemd, a.headless)?;
    } else {
        i.skip("binaries, policy, icons, units (--no-copy)".to_string());
    }
    // 3. state filesystem
    state_fs(&mut i, &a, has_systemd)?;
    // 4. cgroup delegation
    if !has_systemd || a.delegate_cgroup {
        delegate_cgroups(&mut i)?;
        boot_hook(&mut i, &a.prefix, has_systemd)?;
    } else {
        i.skip("cgroup delegation: the systemd user instance limits sessions (--delegate-cgroup forces it)".to_string());
    }
    summary(&i)?;
    if a.headless && !i.problems.is_empty() {
        bail!("headless installation incomplete; resolve the problems above");
    }
    if a.headless {
        println!("per user, once: sill init --user --headless");
        return Ok(());
    }
    println!("per user, once: sill init --user   (sill-gui autostart; then bind `sill toggle` to a key)");
    Ok(())
}

fn summary(i: &Init) -> Result<()> {
    println!();
    println!("{} step(s) {}, {} skipped, {} problem(s)", i.did.len(), if i.dry { "planned" } else { "done" }, i.skipped.len(), i.problems.len());
    for p in &i.problems {
        println!("  problem: {p}");
    }
    Ok(())
}

// --- 1. checks ----------------------------------------------------------------------------------

fn checks(i: &mut Init) {
    // bwrap: present, and not only the setuid build (a setuid bwrap refuses --disable-userns and
    // unprivileged user namespaces are then off anyway)
    match which("bwrap") {
        None => i.problem("bubblewrap not installed (apt install bubblewrap)"),
        Some(p) => {
            use std::os::unix::fs::PermissionsExt;
            let setuid = std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o4000 != 0).unwrap_or(false);
            if setuid {
                i.problem(format!("{} is setuid: the jail needs an unprivileged bwrap (user namespaces enabled, no setuid bit)", p.display()));
            } else {
                i.ok(format!("bwrap at {}", p.display()));
            }
        }
    }
    // kernel ≥ 6.2 (overlayfs in user namespaces: the CVE line before it, SPEC §4.10 #15)
    match kernel_version() {
        Some((maj, min)) if (maj, min) >= (6, 2) => i.ok(format!("kernel {maj}.{min}")),
        Some((maj, min)) => i.problem(format!("kernel {maj}.{min} is older than 6.2 (unprivileged overlayfs fixes)")),
        None => i.problem("kernel version not readable"),
    }
    // unprivileged user namespaces
    let max_ns = read_num("/proc/sys/user/max_user_namespaces").unwrap_or(1);
    let debian_knob = read_num("/proc/sys/kernel/unprivileged_userns_clone");
    let apparmor_knob = read_num("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    if max_ns == 0 {
        i.problem("user.max_user_namespaces = 0: unprivileged user namespaces are off");
    } else if debian_knob == Some(0) {
        i.problem("kernel.unprivileged_userns_clone = 0: enable it (sysctl -w kernel.unprivileged_userns_clone=1)");
    } else if apparmor_knob == Some(1) {
        i.problem("kernel.apparmor_restrict_unprivileged_userns = 1 (Ubuntu): bwrap needs an AppArmor profile or the knob off");
    } else {
        i.ok("unprivileged user namespaces enabled");
    }
    // cgroup v2
    if Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
        let c = std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").unwrap_or_default();
        let missing: Vec<&str> = CONTROLLERS.iter().copied().filter(|c0| !c.split_whitespace().any(|x| x == *c0)).collect();
        if missing.is_empty() {
            i.ok("cgroup v2 with cpu io memory pids");
        } else {
            i.problem(format!("cgroup v2 mounted but without {}: those limits will not apply", missing.join(" ")));
        }
    } else {
        i.problem("cgroup v2 is not mounted at /sys/fs/cgroup (unified hierarchy needed for limits)");
    }
    if which("mkfs.ext4").is_none() {
        i.problem("mkfs.ext4 not found (e2fsprogs): the state image cannot be created");
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).chain(["/usr/sbin", "/sbin"].map(PathBuf::from)).map(|d| d.join(name)).find(|p| p.is_file())
}

fn kernel_version() -> Option<(u32, u32)> {
    let u = rustix::system::uname();
    let rel = u.release().to_string_lossy();
    let mut parts = rel.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn read_num(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The built-in policy into `dir/default.toml`, kept if the admin edited it. The copy as shipped
/// sits beside it as `default.toml.dist` (the dpkg conffile idea): a file that still equals the
/// last shipped copy is not an edit and is replaced, so an upgrade reaches the host; one that
/// differs from both the shipped and the current built-in is the admin's and stays.
fn install_policy(i: &mut Init, dir: &Path) -> Result<()> {
    let policy = dir.join("default.toml");
    let dist = dir.join("default.toml.dist");
    let builtin = sill_core::policy::BUILTIN_DEFAULT;
    let current = std::fs::read_to_string(&policy).ok();
    let shipped = std::fs::read_to_string(&dist).ok();
    match current {
        Some(cur) if cur != builtin && shipped.as_deref() != Some(cur.as_str()) => {
            if shipped.is_none() {
                i.skip(format!("{} (differs from the built-in, kept; `rm` it to get the current one)", policy.display()));
            } else {
                i.skip(format!("{} (edited here, kept)", policy.display()));
            }
        }
        _ => {
            i.put_file(&policy, builtin.as_bytes(), 0o644)?;
        }
    }
    i.put_file(&dist, builtin.as_bytes(), 0o644)?;
    Ok(())
}

/// `<prefix>/bin/sill` is what the boot hook runs as root and the sill-gui unit per user, so the
/// prefix must be absolute and every directory above it root-owned and writable by root only
/// (security.md S12): a prefix under a home or a group-writable `/usr/local` would let whoever
/// writes there put code into every boot.
fn check_prefix(prefix: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !prefix.is_absolute() {
        bail!("--prefix {} must be absolute: it is written into units that run at boot", prefix.display());
    }
    let mut p = prefix.to_path_buf();
    loop {
        if let Ok(m) = std::fs::metadata(&p)
            && (m.uid() != 0 || m.mode() & 0o022 != 0)
        {
            bail!(
                "--prefix {}: {} is {} (uid {}, mode {:o}); the boot hook runs {}/bin/sill as root, so every directory above it must belong to root and be writable by root only",
                prefix.display(),
                p.display(),
                if m.uid() != 0 { "not owned by root" } else { "writable by group or others" },
                m.uid(),
                m.mode() & 0o7777,
                prefix.display()
            );
        }
        if !p.pop() {
            return Ok(());
        }
    }
}

// --- 2. files -----------------------------------------------------------------------------------

fn install_files(i: &mut Init, prefix: &Path, has_systemd: bool, headless: bool) -> Result<()> {
    let bin = prefix.join("bin");
    let me = std::env::current_exe()?.canonicalize()?;
    let src_dir = me.parent().context("executable has no parent")?.to_path_buf();
    // sill-gui is the desktop window: beside sill on a desktop, absent from a headless box
    for name in ["sill", "sill-shim", "sill-gui"] {
        if headless && name == "sill-gui" {
            continue;
        }
        let src = src_dir.join(name);
        let dst = bin.join(name);
        if !src.is_file() {
            if name == "sill-gui" {
                i.skip(format!("sill-gui not found next to this binary ({}): no desktop window installed", src.display()));
            } else {
                i.problem(format!("{} not found next to this binary ({})", name, src.display()));
            }
            continue;
        }
        if src.canonicalize().ok() == dst.canonicalize().ok() {
            i.skip(format!("{} (running from there)", dst.display()));
            continue;
        }
        let content = std::fs::read(&src)?;
        i.put_file(&dst, &content, 0o755)?;
    }
    install_policy(i, Path::new("/etc/sill/policies"))?;
    // icons and the desktop file
    if !headless {
        let share = prefix.join("share");
        i.put_file(&share.join("sill/sill.svg"), ICON.as_bytes(), 0o644)?;
        i.put_file(&share.join("icons/hicolor/scalable/apps/sill.svg"), ICON.as_bytes(), 0o644)?;
        let desktop = DESKTOP.replace("@PREFIX@", &prefix.display().to_string());
        if i.put_file(&share.join("applications/sill.desktop"), desktop.as_bytes(), 0o644)? && !i.dry {
            let _ = std::process::Command::new("update-desktop-database").arg("-q").arg(share.join("applications")).status();
        }
    }
    // sill-gui as a systemd user service (per user: sill init --user enables it)
    if has_systemd {
        if !headless {
            let unit = UNIT_GUI.replace("/usr/local/bin/sill", &bin.join("sill").display().to_string());
            i.put_file(Path::new("/etc/systemd/user/sill-gui.service"), unit.as_bytes(), 0o644)?;
        }
        // the daemon: socket-activated (docs/daemon.md §3.1); per user `sill init --user` enables the socket
        let unit = UNIT_DAEMON.replace("/usr/local/bin/sill", &bin.join("sill").display().to_string());
        i.put_file(Path::new("/etc/systemd/user/sill.service"), unit.as_bytes(), 0o644)?;
        i.put_file(Path::new("/etc/systemd/user/sill.socket"), SOCKET_DAEMON.as_bytes(), 0o644)?;
    }
    Ok(())
}

// --- 3. state filesystem ------------------------------------------------------------------------

fn state_fs(i: &mut Init, a: &InitArgs, has_systemd: bool) -> Result<()> {
    let state = Path::new(STATE_FS);
    let all = mounts::host_mounts()?;
    let mounted = mounts::is_mount_point(&all, state);
    if !i.dry {
        std::fs::create_dir_all(state)?;
    }
    match &a.state_dir {
        Some(dir) => {
            // a directory inside any overlaid tree would show up in every jail's merged view
            // (SPEC §4.9); a mount point of its own is the only safe shape
            let dir = std::path::absolute(dir)?;
            if !dir.is_dir() {
                bail!("--state-dir {}: not a directory", dir.display());
            }
            if !mounts::is_mount_point(&all, &dir) && dir != state {
                bail!(
                    "--state-dir {} is not a mount point: it would sit inside an overlaid tree (SPEC §4.9); give it its own partition or use the image",
                    dir.display()
                );
            }
            if mounted {
                i.skip(format!("{} already mounted", state.display()));
            } else if dir == state {
                i.skip(format!("{} is the state filesystem itself", state.display()));
            } else {
                i.run("bind-mount the state directory", "mount", &["--bind", &dir.display().to_string(), STATE_FS])?;
            }
            if has_systemd {
                let unit = bind_mount_unit(&dir);
                i.put_file(Path::new("/etc/systemd/system/var-lib-sill.mount"), unit.as_bytes(), 0o644)?;
                i.put_file(Path::new("/etc/systemd/system/sill-state-perms.service"), UNIT_PERMS.as_bytes(), 0o644)?;
                i.run("reload units", "systemctl", &["daemon-reload"])?;
                i.run("enable the mount", "systemctl", &["enable", "--now", "var-lib-sill.mount", "sill-state-perms.service"])?;
            } else {
                fstab_line(i, &format!("{} {} none bind,nosuid,nodev,nofail 0 0", dir.display(), STATE_FS))?;
            }
        }
        None => {
            let image = Path::new(IMAGE);
            if image.is_file() {
                i.skip(format!("{} exists", image.display()));
            } else {
                i.run("create the sparse image", "truncate", &["-s", &a.state_size, IMAGE])?;
                i.run("format it", "mkfs.ext4", &["-q", "-L", "sill-state", "-m", "0", "-E", "lazy_itable_init=1,lazy_journal_init=1", IMAGE])?;
                if !i.dry {
                    std::fs::set_permissions(image, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
                }
            }
            if has_systemd {
                i.put_file(Path::new("/etc/systemd/system/sill-state-setup.service"), UNIT_SETUP.as_bytes(), 0o644)?;
                i.put_file(Path::new("/etc/systemd/system/var-lib-sill.mount"), UNIT_MOUNT.as_bytes(), 0o644)?;
                i.put_file(Path::new("/etc/systemd/system/sill-state-perms.service"), UNIT_PERMS.as_bytes(), 0o644)?;
                i.run("reload units", "systemctl", &["daemon-reload"])?;
                if mounted {
                    i.skip(format!("{} already mounted", state.display()));
                    i.run("enable the units", "systemctl", &["enable", "var-lib-sill.mount", "sill-state-perms.service"])?;
                } else {
                    i.run("enable and mount", "systemctl", &["enable", "--now", "var-lib-sill.mount", "sill-state-perms.service"])?;
                }
            } else {
                fstab_line(i, &format!("{IMAGE} {STATE_FS} ext4 loop,noatime,nosuid,nodev,nofail 0 0"))?;
                if mounted {
                    i.skip(format!("{} already mounted", state.display()));
                } else {
                    i.run("mount the image", "mount", &[STATE_FS])?;
                }
            }
        }
    }
    // sticky and world-writable like /tmp, one 0700 directory per user inside
    i.done(format!("chmod 1777 {}; per-user 0700 dirs", state.display()));
    if !i.dry {
        std::fs::set_permissions(state, std::os::unix::fs::PermissionsExt::from_mode(0o1777))?;
        for (uid, gid, _, _) in regular_users() {
            // the parent is world-writable: a directory already there is taken over only when it
            // is root's (an earlier init) or the user's own — never one someone else put there, and
            // never through a symlink (security.md S5)
            let mine = state.join(uid.to_string());
            let created = match rustix::fs::mkdir(&mine, rustix::fs::Mode::from_raw_mode(0o700)) {
                Ok(()) => true,
                Err(rustix::io::Errno::EXIST) => false,
                Err(e) => return Err(e).with_context(|| format!("create {}", mine.display())),
            };
            let fd =
                rustix::fs::open(&mine, rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())
                    .with_context(|| format!("open {} (not through a symlink)", mine.display()))?;
            let st = rustix::fs::fstat(&fd)?;
            if !created && st.st_uid != uid && st.st_uid != 0 {
                bail!("{} belongs to uid {}, not uid {uid}: someone else put it there — remove it (or chown it) and run init again", mine.display(), st.st_uid);
            }
            rustix::fs::fchmod(&fd, rustix::fs::Mode::from_raw_mode(0o700))?;
            rustix::fs::fchown(&fd, Some(rustix::fs::Uid::from_raw(uid)), Some(rustix::fs::Gid::from_raw(gid)))?;
        }
    }
    Ok(())
}

fn bind_mount_unit(dir: &Path) -> String {
    format!(
        "# Binds the sill state directory at /var/lib/sill (sill init --state-dir, SPEC §4.9/§4.12).\n\
         [Unit]\nDescription=sill state filesystem\n\n\
         [Mount]\nWhat={}\nWhere={}\nType=none\nOptions=bind,nosuid,nodev\n\n\
         [Install]\nWantedBy=local-fs.target\n",
        dir.display(),
        STATE_FS
    )
}

fn fstab_line(i: &mut Init, line: &str) -> Result<()> {
    let fstab = Path::new("/etc/fstab");
    let cur = std::fs::read_to_string(fstab).unwrap_or_default();
    if cur.lines().any(|l| l.split_whitespace().nth(1) == Some(STATE_FS)) {
        i.skip(format!("/etc/fstab already has a {} entry", STATE_FS));
        return Ok(());
    }
    i.done(format!("append to /etc/fstab: {line}"));
    if !i.dry {
        let mut text = cur;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("# sill state filesystem (sill init)\n");
        text.push_str(line);
        text.push('\n');
        std::fs::write(fstab, text)?;
    }
    Ok(())
}

/// (uid, gid, name, home) of the human accounts: uid 1000–59999 with a login shell.
fn regular_users() -> Vec<(u32, u32, String, PathBuf)> {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else { return vec![] };
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let gid: u32 = f[3].parse().ok()?;
            let shell = f[6];
            if !(1000..60000).contains(&uid) || shell.ends_with("nologin") || shell.ends_with("/false") {
                return None;
            }
            Some((uid, gid, f[0].to_string(), PathBuf::from(f[5])))
        })
        .collect()
}

// --- 4. cgroup delegation -----------------------------------------------------------------------

/// `/sys/fs/cgroup/sill/<uid>` for every regular user, with cpu io memory pids enabled on the way
/// down. Not persistent by itself: [`boot_hook`] redoes it at boot where it can.
fn delegate_cgroups(i: &mut Init) -> Result<()> {
    let root = Path::new(sill_core::launcher::CGROUP_FS);
    if !root.join("cgroup.controllers").is_file() {
        i.problem("cgroup v2 is not mounted: no delegation");
        return Ok(());
    }
    let sill = Path::new(CGROUP_SILL);
    let enable = CONTROLLERS.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(" ");
    i.done(format!("enable {} in {}/cgroup.subtree_control", enable, root.display()));
    i.done(format!("create {} and enable the controllers below it", sill.display()));
    if !i.dry {
        // one controller at a time: a controller that is unavailable fails alone, not the line
        for c in CONTROLLERS {
            if let Err(e) = std::fs::write(root.join("cgroup.subtree_control"), format!("+{c}")) {
                i.problem(format!("{c} controller at the root: {e}"));
            }
        }
        if !sill.is_dir() {
            std::fs::create_dir(sill).with_context(|| format!("create {}", sill.display()))?;
        }
        for c in CONTROLLERS {
            if let Err(e) = std::fs::write(sill.join("cgroup.subtree_control"), format!("+{c}")) {
                i.problem(format!("{c} controller under {}: {e}", sill.display()));
            }
        }
    }
    let users = regular_users();
    for (uid, gid, name, _) in &users {
        let dir = sill.join(uid.to_string());
        i.done(format!("{} owned by {name}", dir.display()));
        if i.dry {
            continue;
        }
        if !dir.is_dir() {
            std::fs::create_dir(&dir)?;
        }
        let (u, g) = (Some(rustix::fs::Uid::from_raw(*uid)), Some(rustix::fs::Gid::from_raw(*gid)));
        rustix::fs::chown(&dir, u, g)?;
        // the files a delegatee writes: cgroup.procs (move in), cgroup.subtree_control (controllers
        // for its own children), cgroup.threads
        for f in ["cgroup.procs", "cgroup.subtree_control", "cgroup.threads"] {
            let _ = rustix::fs::chown(dir.join(f), u, g);
        }
        for c in CONTROLLERS {
            let _ = std::fs::write(dir.join("cgroup.subtree_control"), format!("+{c}"));
        }
    }
    if users.is_empty() {
        i.skip("no regular users (uid ≥ 1000 with a shell) to delegate to".to_string());
    }
    println!(
        "note:  a process can enter {}/<uid> only from inside it or as root (cgroup v2 common-ancestor rule). \
         systemd-logind puts logins there by itself; other init systems need a login hook sill does not install yet — \
         sessions then run without limits where the policy allows it (SPEC §4.7).",
        CGROUP_SILL
    );
    Ok(())
}

/// Redo the delegation at boot: a cron `@reboot` line or an OpenRC local.d script, whichever exists.
fn boot_hook(i: &mut Init, prefix: &Path, has_systemd: bool) -> Result<()> {
    let sill = prefix.join("bin/sill");
    if has_systemd {
        let unit = format!(
            "# Delegates /sys/fs/cgroup/sill/<uid> at boot (sill init --delegate-cgroup).\n\
             [Unit]\nDescription=sill cgroup delegation\nAfter=sysinit.target\n\n\
             [Service]\nType=oneshot\nExecStart={} init --cgroup-only\n\n\
             [Install]\nWantedBy=multi-user.target\n",
            sill.display()
        );
        i.put_file(Path::new("/etc/systemd/system/sill-cgroup.service"), unit.as_bytes(), 0o644)?;
        i.run("enable the boot hook", "systemctl", &["enable", "sill-cgroup.service"])?;
        return Ok(());
    }
    if Path::new("/etc/local.d").is_dir() {
        let script = format!("#!/bin/sh\n# sill cgroup delegation at boot (sill init)\nexec {} init --cgroup-only\n", sill.display());
        i.put_file(Path::new("/etc/local.d/sill-cgroup.start"), script.as_bytes(), 0o755)?;
        return Ok(());
    }
    if Path::new("/etc/cron.d").is_dir() {
        let line = format!("# sill cgroup delegation at boot (sill init)\n@reboot root {} init --cgroup-only\n", sill.display());
        i.put_file(Path::new("/etc/cron.d/sill-cgroup"), line.as_bytes(), 0o644)?;
        return Ok(());
    }
    i.problem(format!("no boot hook installed (no /etc/local.d, no /etc/cron.d): run `{} init --cgroup-only` after each boot", sill.display()));
    Ok(())
}

// --- 5. per user --------------------------------------------------------------------------------

/// The binary the autostart runs: a sill-owned copy of this one (with its `sill-shim` and
/// `sill-gui`) under `~/.local/state/sill/bin/autostart/`, which every jail masks — never
/// `current_exe()` itself, which may be a checkout's `target/debug/sill` in a pinned folder, where
/// an agent could rewrite what every login runs (security.md S12). `sill init --user` again
/// refreshes the copy; the system-wide unit (`sill init` as root) runs `<prefix>/bin/sill` and
/// needs none. Replaced by rename, so a window running the old copy keeps it.
fn stage_autostart(i: &mut Init, home: &Path, exe: &Path) -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state")).join("sill/bin/autostart");
    let dst = dir.join("sill");
    i.done(format!("copy {} to {} (the autostart runs the copy; `sill init --user` again refreshes it)", exe.display(), dst.display()));
    if i.dry {
        return Ok(dst);
    }
    std::fs::create_dir_all(&dir)?;
    for name in ["sill", "sill-shim", "sill-gui"] {
        let src = exe.with_file_name(name);
        if !src.is_file() {
            if name == "sill-gui" {
                i.skip(format!("sill-gui not found next to {}: the autostart looks for it on PATH", exe.display()));
            } else {
                i.problem(format!("{} not found next to {}: sessions need it", name, exe.display()));
            }
            continue;
        }
        let tmp = dir.join(format!(".{name}.part"));
        std::fs::copy(&src, &tmp).with_context(|| format!("copy {}", src.display()))?;
        std::fs::rename(&tmp, dir.join(name))?;
    }
    Ok(dst)
}

/// `systemctl --user cat <unit>` succeeds: a system-wide or private copy of the unit exists.
/// The user manager runs an older text of `unit` than the file on disk (systemd's own
/// `NeedDaemonReload`): a `sudo sill init` rewrote it — an upgrade — and only this user's
/// `systemctl --user daemon-reload` makes it current.
pub(crate) fn unit_stale(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "NeedDaemonReload", "--value", unit])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "yes")
}

fn unit_known(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "cat", unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The staged copy, made once per run however many units need it.
fn stage_once(i: &mut Init, staged: &mut Option<PathBuf>, home: &Path, exe: &Path) -> Result<PathBuf> {
    if let Some(p) = staged {
        return Ok(p.clone());
    }
    let p = stage_autostart(i, home, exe)?;
    *staged = Some(p.clone());
    Ok(p)
}

/// `sill init --user`: sill-gui's autostart for this account, nothing else. The hotkey is the
/// desktop's business: sill never edits anyone's shortcuts.
fn user_setup(a: &InitArgs) -> Result<()> {
    let mut i = Init { dry: a.dry_run, did: vec![], skipped: vec![], problems: vec![] };
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    let systemd = sill_core::launcher::systemd_user_usable();
    println!(
        "sill init --user{} — {}",
        if a.dry_run { " (dry run)" } else { "" },
        match &systemd {
            Ok(()) => "systemd user instance reachable: units".to_string(),
            Err(e) => format!("no systemd user instance ({e}): autostart file, daemon spawned by its first client"),
        }
    );
    if !sill_proto::autostart_enabled()? {
        println!("autostart disabled; `sill daemon autostart on` enables login and on-demand startup");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    if a.headless && systemd.is_err() {
        bail!("headless account setup requires a reachable systemd user manager; log in directly over SSH with libpam-systemd installed");
    }
    if systemd.is_ok() {
        let mut staged: Option<PathBuf> = None;
        if !a.headless {
            if !unit_known("sill-gui.service") {
                // no system-wide unit (cargo install, tarball without root): a private copy
                let run = stage_once(&mut i, &mut staged, &home, &exe)?;
                let unit = UNIT_GUI.replace("/usr/local/bin/sill", &run.display().to_string());
                i.put_file(&home.join(".config/systemd/user/sill-gui.service"), unit.as_bytes(), 0o644)?;
                i.run("reload user units", "systemctl", &["--user", "daemon-reload"])?;
            }
            let enabled = std::process::Command::new("systemctl")
                .args(["--user", "is-enabled", "--quiet", "sill-gui.service"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if enabled {
                i.skip("sill-gui.service already enabled".to_string());
            } else {
                i.run("start sill-gui with the session", "systemctl", &["--user", "enable", "--now", "sill-gui.service"])?;
            }
        }
        // the daemon's socket (docs/daemon.md §3.1): the first client starts sill.service
        if !unit_known("sill.socket") {
            let run = stage_once(&mut i, &mut staged, &home, &exe)?;
            let unit = UNIT_DAEMON.replace("/usr/local/bin/sill", &run.display().to_string());
            i.put_file(&home.join(".config/systemd/user/sill.service"), unit.as_bytes(), 0o644)?;
            i.put_file(&home.join(".config/systemd/user/sill.socket"), SOCKET_DAEMON.as_bytes(), 0o644)?;
            i.run("reload user units", "systemctl", &["--user", "daemon-reload"])?;
        } else if unit_stale("sill.service") {
            // the system-wide unit was rewritten by `sudo sill init` (an upgrade: Type=notify, the
            // watchdog, docs/daemon.md §3.7); this user's manager still runs the old text
            i.run("reload user units (sill.service changed)", "systemctl", &["--user", "daemon-reload"])?;
        }
        let socket_enabled =
            std::process::Command::new("systemctl").args(["--user", "is-enabled", "--quiet", "sill.socket"]).status().map(|s| s.success()).unwrap_or(false);
        if socket_enabled {
            i.skip("sill.socket already enabled".to_string());
        } else {
            i.run("listen for the daemon's clients", "systemctl", &["--user", "enable", "--now", "sill.socket"])?;
        }
        let stale = home.join(".config/autostart/sill.desktop");
        if stale.is_file() {
            i.done(format!("remove {} (the service starts sill-gui now)", stale.display()));
            if !i.dry {
                std::fs::remove_file(&stale)?;
            }
        }
    } else {
        let run = stage_autostart(&mut i, &home, &exe)?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=sill\nComment=sill's desktop window (hidden; `sill toggle` shows it)\n\
             Exec={} gui --hidden\nIcon=sill\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            run.display()
        );
        i.put_file(&home.join(".config/autostart/sill.desktop"), entry.as_bytes(), 0o644)?;
    }
    summary(&i)?;
    if a.headless {
        return Ok(());
    }
    let on_path = which("sill").and_then(|p| p.canonicalize().ok()) == exe.canonicalize().ok();
    println!(
        "hotkey: bind `{} toggle` to a key in your desktop's keyboard shortcuts (Alt+Space is the usual choice; \
         sill-gui's own grab, Settings → Window → Hotkey, works on X11)",
        if on_path { "sill".to_string() } else { exe.display().to_string() }
    );
    Ok(())
}

/// Configure both login startup and client fallback. Never operate on a selected remote box.
pub fn autostart(value: Option<&str>) -> Result<()> {
    if sill_proto::transport::selected()?.is_some() {
        bail!("autostart is a local account setting; run this command on the box itself");
    }
    let Some(value) = value else {
        println!("daemon autostart: {}", if sill_proto::autostart_enabled()? { "on" } else { "off" });
        return Ok(());
    };
    let path = sill_proto::autostart_disabled_path()?;
    if value == "on" {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // Reuse the installer so private units and non-systemd desktops follow the same path.
        let args = InitArgs {
            dry_run: false,
            prefix: "/usr/local".into(),
            user: true,
            headless: !std::env::current_exe()?.with_file_name("sill-gui").is_file(),
            state_dir: None,
            state_size: "32G".into(),
            delegate_cgroup: false,
            cgroup_only: false,
            no_copy: false,
        };
        user_setup(&args)?;
    } else {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, b"Disabled by sill daemon autostart off\n")?;
        let mut i = Init { dry: false, did: vec![], skipped: vec![], problems: vec![] };
        if sill_core::launcher::systemd_user_usable().is_ok() {
            if unit_known("sill-gui.service") {
                // Keep this window open; disable only its next login start.
                i.run("disable GUI login startup", "systemctl", &["--user", "disable", "sill-gui.service"])?;
            }
            if unit_known("sill.socket") {
                // An active socket would bypass the client's no-start preference.
                i.run("disable daemon socket activation", "systemctl", &["--user", "disable", "--now", "sill.socket"])?;
            }
        }
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
        match std::fs::remove_file(home.join(".config/autostart/sill.desktop")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        println!(
            "Autostart off. The window stays open; stopping socket activation may stop the daemon. Sessions keep running. Start manually with `sill daemon`."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> Init {
        Init { dry: false, did: vec![], skipped: vec![], problems: vec![] }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn headless_install_plan_excludes_desktop_artifacts() {
        let mut plan = Init { dry: true, did: vec![], skipped: vec![], problems: vec![] };
        install_files(&mut plan, Path::new("/usr/local"), true, true).unwrap();
        let actions = [plan.did, plan.skipped].concat().join("\n");
        assert!(actions.contains("/etc/systemd/user/sill.service"));
        assert!(actions.contains("/etc/systemd/user/sill.socket"));
        assert!(!actions.contains("sill-gui"));
        assert!(!actions.contains(".desktop"));
        assert!(!actions.contains(".svg"));
    }

    #[test]
    fn policy_install_cases() {
        let builtin = sill_core::policy::BUILTIN_DEFAULT;
        let old = "# an older shipped copy\n";
        let edited = "# the admin's own\n";
        let dir = std::env::temp_dir().join(format!("sill-policy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.as_path();
        let policy = dir.join("default.toml");
        let dist = dir.join("default.toml.dist");

        // fresh host: both written
        install_policy(&mut init(), dir).unwrap();
        assert_eq!(read(&policy), builtin);
        assert_eq!(read(&dist), builtin);

        // unchanged: nothing rewritten
        let mut i = init();
        install_policy(&mut i, dir).unwrap();
        assert!(i.did.is_empty(), "{:?}", i.did);

        // an older shipped copy, recorded as such: replaced
        std::fs::write(&policy, old).unwrap();
        std::fs::write(&dist, old).unwrap();
        install_policy(&mut init(), dir).unwrap();
        assert_eq!(read(&policy), builtin);
        assert_eq!(read(&dist), builtin);

        // the admin's edit, dist present: kept, dist brought up to date
        std::fs::write(&policy, edited).unwrap();
        std::fs::write(&dist, old).unwrap();
        let mut i = init();
        install_policy(&mut i, dir).unwrap();
        assert_eq!(read(&policy), edited);
        assert_eq!(read(&dist), builtin);
        assert!(i.skipped.iter().any(|s| s.contains("edited here")), "{:?}", i.skipped);

        // a differing file with no dist (a host from before .dist): kept, told how to upgrade
        std::fs::remove_file(&dist).unwrap();
        let mut i = init();
        install_policy(&mut i, dir).unwrap();
        assert_eq!(read(&policy), edited);
        assert_eq!(read(&dist), builtin);
        assert!(i.skipped.iter().any(|s| s.contains("`rm` it")), "{:?}", i.skipped);
        let _ = std::fs::remove_dir_all(dir);
    }
}
