//! Which desktop frontend this machine runs (the person, 2026-11-05): the GTK shelf, built into
//! this binary with the `shelf` feature, or sill-gui, a binary of its own installed beside it. The
//! two stay separate binaries; `frontend` in `sill.toml` picks one, and `sill shelf`, `sill toggle`
//! and `sill restart` go to it — so the service file stays the same and switching back is
//! `sill frontend gtk && sill restart`, without a rebuild.
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Gtk,
    Iced,
}

impl Frontend {
    pub fn name(self) -> &'static str {
        match self {
            Frontend::Gtk => "gtk",
            Frontend::Iced => "iced",
        }
    }
}

/// What this `sill` can run of the GTK shelf.
const HAS_GTK: bool = cfg!(feature = "shelf");

/// The frontend chosen: the setting, else the GTK shelf where this binary has it.
pub fn chosen() -> Frontend {
    match sill_core::config::load().frontend.as_deref() {
        Some("iced") => Frontend::Iced,
        Some("gtk") => Frontend::Gtk,
        _ if HAS_GTK => Frontend::Gtk,
        _ => Frontend::Iced,
    }
}

/// sill-gui: the debug build of the checkout this binary came from with `dev`, else the one
/// installed beside this binary, else whichever is on PATH.
pub fn gui_exe(dev: bool) -> PathBuf {
    if dev {
        let candidate =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/sill-gui");
        if let Ok(c) = candidate.canonicalize()
            && c.is_file()
        {
            return c;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|me| me.canonicalize().ok())
        .and_then(|me| me.parent().map(|dir| dir.join("sill-gui")))
        .filter(|beside| beside.is_file())
        .unwrap_or_else(|| PathBuf::from("sill-gui"))
}

/// `sill frontend [gtk|iced]`: show the choice, or change it.
pub fn set(name: Option<String>) -> Result<()> {
    let Some(name) = name else {
        let gui = gui_exe(false);
        println!("{}", chosen().name());
        println!(
            "available: gtk {} · iced {}",
            if HAS_GTK {
                "(in this binary)"
            } else {
                "(not in this binary)"
            },
            if gui.is_absolute() {
                format!("({})", gui.display())
            } else {
                "(sill-gui not installed beside sill)".into()
            }
        );
        return Ok(());
    };
    let frontend = match name.as_str() {
        "gtk" => Frontend::Gtk,
        "iced" => Frontend::Iced,
        other => bail!("unknown frontend {other:?}: gtk or iced"),
    };
    if frontend == Frontend::Gtk && !HAS_GTK {
        bail!("this sill was built without the GTK shelf (the `shelf` feature)");
    }
    if frontend == Frontend::Iced && !gui_exe(false).is_absolute() {
        eprintln!(
            "warning: sill-gui is not installed beside this sill; `sill shelf` will look for it on PATH"
        );
    }
    sill_core::config::update(|c| c.frontend = Some(frontend.name().into()))?;
    println!(
        "frontend: {} — `sill restart` switches to it",
        frontend.name()
    );
    Ok(())
}

/// `sill shelf`: the chosen frontend. sill-gui replaces this process, so the service's main
/// process is the window itself.
pub fn shelf(cwd: Option<PathBuf>, hidden: bool) -> Result<()> {
    match chosen() {
        Frontend::Iced => {
            use std::os::unix::process::CommandExt;
            let exe = gui_exe(false);
            let _ = cwd;
            let mut command = std::process::Command::new(&exe);
            if hidden {
                command.arg("--hidden");
            }
            let error = command.exec();
            Err(error).with_context(|| format!("start {}", exe.display()))
        }
        #[cfg(feature = "shelf")]
        Frontend::Gtk => crate::shelf::run(cwd, hidden),
        #[cfg(not(feature = "shelf"))]
        Frontend::Gtk => bail!("this sill has no GTK shelf; `sill frontend iced` runs sill-gui"),
    }
}

/// `sill toggle`: show or hide the chosen frontend's window.
pub fn toggle() -> Result<()> {
    match chosen() {
        Frontend::Iced => gui_command("toggle 0"),
        #[cfg(feature = "shelf")]
        Frontend::Gtk => crate::shelf::toggle(),
        #[cfg(not(feature = "shelf"))]
        Frontend::Gtk => bail!("this sill has no GTK shelf"),
    }
}

/// `sill restart`: the daemon and the chosen frontend, under the service when it is installed.
pub fn restart(hidden: bool, dev: bool, detached: bool) -> Result<()> {
    match chosen() {
        #[cfg(feature = "shelf")]
        Frontend::Gtk => crate::shelf::restart(hidden, dev, detached),
        #[cfg(not(feature = "shelf"))]
        Frontend::Gtk => bail!("this sill has no GTK shelf; `sill frontend iced` runs sill-gui"),
        Frontend::Iced => {
            let _ = (hidden, detached);
            restart_iced(dev)
        }
    }
}

/// sill-gui's control socket (`sill-gui`'s `control.rs`, the same path).
fn gui_socket() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("sill-{}", rustix::process::getuid().as_raw()))
        });
    runtime.join("sill").join("gui.sock")
}

/// One command to the running sill-gui, and its answer.
fn gui_command(line: &str) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let path = gui_socket();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("sill-gui is not running ({})", path.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    writeln!(stream, "{line}")?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    match answer.trim() {
        "ok" => Ok(()),
        other => bail!("sill-gui answered: {other}"),
    }
}

fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn restart_iced(dev: bool) -> Result<()> {
    let unit_known = systemctl(&["cat", "sill-shelf.service"]);
    if unit_known {
        systemctl(&["stop", "sill-shelf.service"]);
    }
    // a GTK shelf still running from before the switch goes too
    #[cfg(feature = "shelf")]
    crate::shelf::quit_running_shelf()?;
    stop_running_gui();
    restart_daemon(dev)?;
    if unit_known && !dev {
        if !systemctl(&["start", "sill-shelf.service"]) {
            bail!("systemctl --user start sill-shelf failed");
        }
        println!("sill-shelf.service restarted with sill-gui (journalctl --user -u sill-shelf)");
        return Ok(());
    }
    // no service, or --dev: sill-gui on its own, in a session of its own, its output in gui.log
    let exe = gui_exe(dev);
    println!("starting {}", exe.display());
    let mut cmd = std::process::Command::new(&exe);
    let log = sill_core::store::default_path().ok().and_then(|p| {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(p.with_file_name("gui.log"))
            .ok()
    });
    cmd.stdin(std::process::Stdio::null());
    match log {
        Some(f) => {
            cmd.stdout(f.try_clone()?);
            cmd.stderr(f);
        }
        None => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }
    // SAFETY: setsid between fork and exec only.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("start {}", exe.display()))?;
    Ok(())
}

/// End this account's sill-gui windows: asked first, then made to.
fn stop_running_gui() {
    let uid = rustix::process::getuid().as_raw().to_string();
    let signal = |sig: &str| {
        std::process::Command::new("pkill")
            .args([sig, "-u", &uid, "-x", "sill-gui"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !signal("-TERM") {
        return;
    }
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let running = std::process::Command::new("pgrep")
            .args(["-u", &uid, "-x", "sill-gui"])
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !running {
            return;
        }
    }
    signal("-KILL");
}

fn restart_daemon(dev: bool) -> Result<()> {
    #[cfg(feature = "shelf")]
    {
        crate::shelf::restart_daemon(dev);
        Ok(())
    }
    #[cfg(not(feature = "shelf"))]
    {
        let _ = dev;
        if let Ok(c) = sill_proto::Client::connect() {
            let _: serde_json::Value = c.call_within(
                sill_proto::method::DAEMON_SHUTDOWN,
                sill_proto::Empty::default(),
                sill_proto::client::QUICK_TIMEOUT,
            )?;
            drop(c);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = sill_proto::Client::connect_or_start()?;
        Ok(())
    }
}
