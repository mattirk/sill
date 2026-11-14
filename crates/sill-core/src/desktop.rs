//! The managed test desktop of Desktop development (docs/permission-modes.md): a Weston
//! compositor with its own Xwayland, run by the session's launcher outside the jail, whose
//! Wayland socket and X11 display are the only display the jail gets.
//!
//! Weston is a trusted host helper, narrowly configured: headless (it never connects to the
//! person's display; they watch it in sill-gui, from pictures the daemon takes with [`capture`]),
//! only the `weston.ini` written here (a plain background, no locking, no animations: smaller,
//! fewer pictures), its own runtime directory and home (nothing of the person's
//! compositor, fonts or cursor settings, and nothing agent-writable), a fixed `PATH`, software
//! rendering at the person's screen scale, and the debug extension so that its clients can take
//! screenshots (`weston-screenshooter`) with no debug scope advertised (a protocol dump would show
//! one client another's input). It lives in the session's scope, under its limits, and goes with
//! the jail: the launcher stops it, and it dies with the launcher. A desktop that cannot start
//! fails the launch; nothing ever falls back to the real display.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// The oldest Weston with `--no-config` and `--xwayland` (10.0, 2022). Tested: 15.0.1.
const MIN_MAJOR: u32 = 10;
/// The desktop's size in pixels (an output of this size; a window this large when shown).
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 800;
/// The Wayland socket's name in the helper's runtime directory and in the jail's.
pub const WAYLAND_SOCKET: &str = "wayland-0";
/// The test desktop's `weston.ini`: a plain background (it compresses; Weston's default texture
/// does not), the clock panel, no screen locking and no animations (fewer changed pictures).
const CONFIG: &str = "[core]\nidle-time=0\n\n[shell]\nbackground-color=0xff22262e\npanel-position=top\nlocking=false\nstartup-animation=none\nclose-animation=none\nfocus-animation=none\n";
/// How long Weston may take to answer on its socket.
const READY: Duration = Duration::from_secs(10);

/// What this machine offers for a test desktop. Probed once per process.
#[derive(Debug, Clone)]
pub struct Probe {
    pub weston: Option<PathBuf>,
    pub xwayland: Option<PathBuf>,
    pub version: Option<String>,
    /// Why there is no test desktop; empty when there is one.
    pub missing: String,
}

impl Probe {
    pub fn available(&self) -> bool {
        self.missing.is_empty()
    }
}

pub(crate) fn find(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            use std::os::unix::fs::PermissionsExt;
            candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn probe_now() -> Probe {
    let weston = find("weston");
    let xwayland = find("Xwayland");
    let version = weston.as_ref().and_then(|w| {
        let out = Command::new(w).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        out.status.success().then(|| text.trim().strip_prefix("weston ").unwrap_or(text.trim()).to_string())
    });
    let major = version.as_deref().and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    let missing = match (&weston, &version, major, &xwayland) {
        (None, ..) => "Weston is not installed (the test desktop needs weston and Xwayland)".to_string(),
        (Some(_), None, ..) => "Weston did not report its version".to_string(),
        (_, Some(v), Some(m), _) if m < MIN_MAJOR => format!("Weston {v} is too old (at least {MIN_MAJOR} is needed)"),
        (_, Some(v), None, _) => format!("unrecognised Weston version {v}"),
        (_, _, _, None) => "Xwayland is not installed (X11 programs need it)".to_string(),
        _ => String::new(),
    };
    Probe { weston, xwayland, version, missing }
}

/// This machine's test desktop support, probed once per process (a newly installed Weston is seen
/// by a daemon started after it).
pub fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(probe_now)
}

/// The scale the desktop renders at: the person's screen's, so that what runs in it is the size
/// of their other programs, and sharp, when sill-gui shows it at its own size. From
/// `SILL_TEST_DESKTOP_SCALE` (1–4), else the X resources' `Xft.dpi` (96 per step) of the display
/// the daemon was started in, else `GDK_SCALE`, else 1.
pub fn scale() -> u32 {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let clamp = |n: u32| n.clamp(1, 4);
    if let Some(n) = var("SILL_TEST_DESKTOP_SCALE").and_then(|v| v.parse::<u32>().ok()) {
        return clamp(n);
    }
    if var("DISPLAY").is_some()
        && let Some(dpi) = xft_dpi()
    {
        return clamp(((dpi / 96.0).round() as u32).max(1));
    }
    var("GDK_SCALE").and_then(|v| v.parse::<u32>().ok()).map_or(1, clamp)
}

/// `Xft.dpi` from `xrdb -query` (a read of the display's resource database, two seconds at most).
fn xft_dpi() -> Option<f64> {
    let mut child = Command::new(find("xrdb")?)
        .arg("-query")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().ok()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut()?, &mut out).ok()?;
    out.lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|dpi| *dpi > 0.0)
}

/// A running test desktop. Dropping it stops Weston (and its Xwayland and shell) and removes its
/// runtime directory.
pub struct Desktop {
    child: Child,
    pub dir: PathBuf,
    pub wayland_socket: PathBuf,
    /// The X11 display number Weston listens on (`/tmp/.X11-unix/X<n>`), served by its Xwayland.
    pub x11_display: u32,
    pub x11_socket: PathBuf,
    pub scale: u32,
}

impl Desktop {
    /// Start a test desktop, headless at `scale`, with its runtime directory at `dir` (made
    /// afresh, 0700).
    pub fn start(dir: &Path, scale: u32) -> Result<Self> {
        let probe = probe();
        if !probe.available() {
            bail!("no test desktop: {}", probe.missing);
        }
        let weston = probe.weston.clone().expect("probed");
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing the old {}", dir.display())),
        }
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let log = dir.join("weston.log");
        // the desktop's whole configuration, written here: never the person's, never the agent's
        let config = dir.join("weston.ini");
        std::fs::write(&config, CONFIG).context("writing the test desktop's configuration")?;
        let mut cmd = Command::new(&weston);
        cmd.arg("--backend=headless")
            .arg("--renderer=pixman")
            .arg(format!("--width={WIDTH}"))
            .arg(format!("--height={HEIGHT}"))
            .arg(format!("--scale={scale}"))
            .arg(format!("--socket={WAYLAND_SOCKET}"))
            .arg("--xwayland")
            .arg(format!("--config={}", config.display()))
            // the desktop's own clients may capture it (weston-screenshooter, which needs the debug
            // extension) but subscribe to no debug scope: "proto" would hand any client every
            // other client's protocol traffic, keystrokes typed into the window included
            .arg("--debug")
            .arg("--debug-scopes=none")
            .arg("--idle-time=0")
            .arg(format!("--log={}", log.display()));
        // nothing of the launcher's environment: no display, bus or settings of the person's
        cmd.env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", dir)
            .env("XDG_RUNTIME_DIR", dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"));
        if let Ok(lang) = std::env::var("LANG") {
            cmd.env("LANG", lang);
        }
        // what it says before its log starts (a display it cannot open) goes to a file beside it
        let stderr = std::fs::File::create(dir.join("stderr.log")).context("creating the test desktop's error log")?;
        cmd.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            // SAFETY: prctl only, between fork and exec.
            unsafe {
                cmd.pre_exec(|| {
                    // gone with the launcher, even if it is killed outright (no scope to stop)
                    if libc_prctl_pdeathsig() != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().with_context(|| format!("starting {}", weston.display()))?;
        let wayland_socket = dir.join(WAYLAND_SOCKET);
        let deadline = Instant::now() + READY;
        let x11_display = loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!("the test desktop ended at once ({status}): {}", tail(&log));
            }
            let answers = std::os::unix::net::UnixStream::connect(&wayland_socket).is_ok();
            if answers && let Some(n) = x11_display(&log) {
                break n;
            }
            if Instant::now() > deadline {
                let _ = stop_group(&mut child);
                bail!("the test desktop did not start within {} s: {}", READY.as_secs(), tail(&log));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let x11_socket = PathBuf::from(format!("/tmp/.X11-unix/X{x11_display}"));
        // Weston made it a moment ago: it must be our own socket, not something left in /tmp
        let ours = {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            std::fs::symlink_metadata(&x11_socket)
                .is_ok_and(|m| m.file_type().is_socket() && m.uid() == rustix::process::getuid().as_raw())
        };
        if !ours {
            let _ = stop_group(&mut child);
            bail!("the test desktop's X11 socket {} is not its own", x11_socket.display());
        }
        Ok(Desktop { child, dir: dir.to_path_buf(), wayland_socket, x11_display, x11_socket, scale })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Weston is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The environment a program in the jail needs to reach this desktop.
    pub fn jail_env(&self) -> Vec<(String, String)> {
        vec![
            ("WAYLAND_DISPLAY".into(), WAYLAND_SOCKET.into()),
            ("DISPLAY".into(), format!(":{}", self.x11_display)),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
        ]
    }

    /// Host path → jail path of the desktop's two sockets.
    pub fn jail_binds(&self, uid: u32) -> Vec<(PathBuf, PathBuf)> {
        vec![
            (self.wayland_socket.clone(), PathBuf::from(format!("/run/user/{uid}/{WAYLAND_SOCKET}"))),
            (self.x11_socket.clone(), self.x11_socket.clone()),
        ]
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = stop_group(&mut self.child);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A picture of a running test desktop, encoded for reading.
pub struct Picture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The SHA-256 of its pixels, hex: an unchanged desktop has the same revision.
    pub revision: String,
}

/// A capture's pixels and their revision, before any encoding.
pub struct Shot {
    raw: crate::desktop_capture::Raw,
    pub revision: String,
}

impl Shot {
    pub fn width(&self) -> u32 {
        self.raw.width
    }
    pub fn height(&self) -> u32 {
        self.raw.height
    }
    /// The PNG, for a frontend that reads this picture.
    pub fn picture(self) -> Result<Picture> {
        let png = crate::desktop_capture::png(&self.raw)?;
        Ok(Picture { png, width: self.raw.width, height: self.raw.height, revision: self.revision })
    }
}

/// Take a picture of the test desktop whose runtime directory is `dir`, as a client of its own
/// socket, outside the jail: its programs reach the compositor, never this process.
pub fn capture(dir: &Path) -> Result<Shot> {
    let socket = dir.join(WAYLAND_SOCKET);
    if !socket.exists() {
        bail!("no test desktop is running");
    }
    let raw = crate::desktop_capture::capture(&socket)?;
    // of the pixels as they came: an unchanged desktop costs a copy and this hash, no conversion
    let revision = crate::desktop_capture::revision(&raw);
    Ok(Shot { raw, revision })
}

/// SIGTERM to Weston's process group (it takes its Xwayland and shell down and removes its X11
/// socket and lock), SIGKILL after two seconds.
fn stop_group(child: &mut Child) -> Result<()> {
    use rustix::process::{Pid, Signal, kill_process_group};
    let Some(pid) = Pid::from_raw(child.id() as i32) else { return Ok(()) };
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let _ = kill_process_group(pid, Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            let _ = kill_process_group(pid, Signal::KILL); // stragglers of the group
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = kill_process_group(pid, Signal::KILL);
    let _ = child.wait();
    Ok(())
}

fn libc_prctl_pdeathsig() -> i32 {
    match rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM)) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// The display Weston's Xwayland module listens on, from its log ("xserver listening on display :N").
fn x11_display(log: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .find_map(|line| line.split("xserver listening on display :").nth(1))
        .and_then(|rest| rest.trim().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok())
}

/// The last lines of Weston's log, or of what it wrote before logging started.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let text = if text.trim().is_empty() {
        std::fs::read_to_string(log.with_file_name("stderr.log")).unwrap_or_default()
    } else {
        text
    };
    let lines: Vec<&str> = text.lines().rev().take(6).collect();
    let tail: Vec<&str> = lines.into_iter().rev().collect();
    if tail.is_empty() { "no log".into() } else { tail.join(" | ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_x11_display_comes_from_weston_s_log() {
        let dir = std::env::temp_dir().join(format!("sill-desktop-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("weston.log");
        std::fs::write(&log, "[22:40:42.258] Loading module 'xwayland.so'\n[22:40:42.260] xserver listening on display :7\n").unwrap();
        assert_eq!(x11_display(&log), Some(7));
        std::fs::write(&log, "[22:40:42.258] nothing yet\n").unwrap();
        assert_eq!(x11_display(&log), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
