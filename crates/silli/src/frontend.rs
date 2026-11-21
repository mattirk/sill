//! The desktop window, silli-gui: a binary of its own, installed beside `silli`. `silli gui` runs it,
//! `silli toggle` shows or hides it over its control socket, and `silli restart` restarts the daemon
//! and the window — under the `silli-gui` user service when that is installed.
use silli_proto::method;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// silli-gui: the debug build of the checkout this binary came from with `dev`, else the one
/// installed beside this binary, else whichever is on PATH.
pub fn gui_exe(dev: bool) -> PathBuf {
    if dev {
        let candidate = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/silli-gui");
        if let Ok(c) = candidate.canonicalize()
            && c.is_file()
        {
            return c;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|me| me.canonicalize().ok())
        .and_then(|me| me.parent().map(|dir| dir.join("silli-gui")))
        .filter(|beside| beside.is_file())
        .unwrap_or_else(|| PathBuf::from("silli-gui"))
}

/// A saved box selected for this command (`--box`, `SILLI_BOX`), when it is not this machine.
fn remote_box() -> Option<String> {
    std::env::var("SILLI_BOX").ok().filter(|name| !name.is_empty() && name != "local")
}

/// `silli gui [--hidden]`: silli-gui replaces this process, so the service's main process is the
/// window itself; a second start shows the running window and leaves (silli-gui's `control.rs`).
/// On a saved box (`silli --box NAME gui`, `silli box connect NAME`) the window gets a control socket
/// of its own, beside the local window's.
pub fn gui(hidden: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = gui_exe(false);
    let mut command = std::process::Command::new(&exe);
    if hidden {
        command.arg("--hidden");
    }
    if remote_box().is_some() {
        command.env("SILLI_GUI_SOCKET", gui_socket());
    }
    let error = command.exec();
    Err(error).with_context(|| format!("start {}", exe.display()))
}

/// `silli toggle`: show or hide the window.
pub fn toggle() -> Result<()> {
    gui_command("toggle 0")
}

/// silli-gui's control socket (`silli-gui`'s `control.rs`, the same path): `gui.sock` in the runtime
/// directory, `gui-<box>.sock` for a window on a saved box.
fn gui_socket() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::temp_dir().join(format!("silli-{}", rustix::process::getuid().as_raw())));
    let name = match remote_box() {
        Some(name) => format!("gui-{name}.sock"),
        None => "gui.sock".into(),
    };
    runtime.join("silli").join(name)
}

/// One command to the running silli-gui, and its answer.
fn gui_command(line: &str) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let path = gui_socket();
    let mut stream =
        std::os::unix::net::UnixStream::connect(&path).with_context(|| format!("silli-gui is not running ({}); `silli gui` starts it", path.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    writeln!(stream, "{line}")?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    match answer.trim() {
        "ok" => Ok(()),
        other => bail!("silli-gui answered: {other}"),
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

/// `silli restart`: with the `silli-gui` user service installed this stops the unit, ends any other
/// silli-gui (a `--dev` one, a hand-started one), restarts the daemon and starts the unit again (the
/// installed binaries); `--dev` instead stops the service and runs the checkout's debug build of
/// silli-gui on its own, for trying a build without installing it. The next daemon is started by
/// its first client, so a `--dev` window brings up the daemon of its own build (docs/daemon.md §3.6).
/// With `--jails`, the replacement daemon adopts running jails before rebuilding each one.
pub fn restart(hidden: bool, dev: bool, detached: bool, jails: bool) -> Result<()> {
    // run from a host shell of the window (the daemon's pty) or anywhere in the units' cgroups,
    // the stops below would take this command down before it starts anything — so the work is
    // handed to a copy outside: its own scope, its own session, its output in gui.log
    if !detached && inside_silli() {
        return restart_detached(hidden, dev, jails);
    }
    let unit_known = systemctl(&["cat", "silli-gui.service"]);
    if unit_known {
        systemctl(&["stop", "silli-gui.service"]);
    }
    stop_running_gui();
    let running = if jails {
        // The operator's own restart puts each jail back in the state it had; the daemon only
        // lets a frontend that shows permission state operate sessions using Desktop development
        // or Host control capabilities (admin.rs `frontend_gate`), so this command declares it.
        silli_proto::client::declare_capability("silli.permission_modes");
        // the operator's own restart: explicit, so `silli daemon autostart off` does not refuse it
        let client = silli_proto::Client::connect_or_start_explicit()?;
        let running: Vec<_> = client.list_sessions()?.into_iter().filter(|s| s.running && s.ended_at.is_none()).collect();
        running
    } else {
        Vec::new()
    };
    restart_daemon(dev)?;
    // one jail that cannot be started again does not keep the others, or the window, down
    let mut failed = Vec::new();
    if !running.is_empty() {
        let client = silli_proto::Client::connect_or_start_explicit()?;
        for session in running {
            let key = silli_proto::sessions::SessionKey { session_id: session.session_id.clone() };
            let restarted = client
                .call::<serde_json::Value>(method::SESSIONS_STOP, &key)
                .and_then(|_| client.call::<silli_proto::sessions::Started>(method::SESSIONS_START, &key));
            match restarted {
                Ok(_) => println!("jail {} restarted", session.session_id),
                Err(e) => {
                    eprintln!("jail {}: not restarted: {e:#}", session.session_id);
                    failed.push(session.session_id);
                }
            }
        }
    }
    let jails_done = || -> Result<()> {
        anyhow::ensure!(failed.is_empty(), "{} jail(s) not restarted: {}", failed.len(), failed.join(", "));
        Ok(())
    };
    if unit_known && !dev {
        if !systemctl(&["start", "silli-gui.service"]) {
            bail!("systemctl --user start silli-gui failed");
        }
        println!("silli-gui.service restarted (journalctl --user -u silli-gui)");
        return jails_done();
    }
    // no service, or --dev: silli-gui on its own, in a session of its own, its output in gui.log
    let exe = gui_exe(dev);
    // running a build from a pinned folder is running what the agent could have written there
    if let Ok(store) = silli_core::store::Store::open_default()
        && let Ok(sessions) = store.sessions()
    {
        let pins: Vec<PathBuf> = sessions.iter().flat_map(|s| store.pins(&s.id).unwrap_or_default()).collect();
        if let Some(p) = silli_core::session::inside_any(&exe, &pins) {
            eprintln!("warning: {} is inside the pinned folder {} — the agent can write there", exe.display(), p.display());
        }
    }
    println!("starting {}", exe.display());
    let mut cmd = std::process::Command::new(&exe);
    if hidden {
        cmd.arg("--hidden");
    }
    let log = silli_core::store::default_path()
        .ok()
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(p.with_file_name("gui.log")).ok());
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
    cmd.spawn().with_context(|| format!("start {}", exe.display()))?;
    jails_done()
}

/// Is this process a descendant of silli itself — in a host shell the daemon opened for the window
/// (marked with `SILLI_HOST_SHELL`) or anywhere in the daemon's or the window's unit cgroup?
fn inside_silli() -> bool {
    std::env::var_os("SILLI_HOST_SHELL").is_some()
        || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| c.contains("silli.service") || c.contains("silli-gui.service"))
}

/// Run `silli restart` again outside: in a transient scope of its own (not a unit's cgroup, so the
/// stops don't reach it), in a new session (no controlling pty to hang up when the shell goes),
/// logging to gui.log.
fn restart_detached(hidden: bool, dev: bool, jails: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = silli_core::store::default_path()?.with_file_name("gui.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    let mut args: Vec<std::ffi::OsString> = vec!["--user".into(), "--scope".into(), "--quiet".into(), "--".into()];
    args.extend(["setsid".into(), "-f".into(), exe.into_os_string(), "restart".into(), "--detached".into()]);
    if hidden {
        args.push("--hidden".into());
    }
    if dev {
        args.push("--dev".into());
    }
    if jails {
        args.push("--jails".into());
    }
    let st = std::process::Command::new("systemd-run")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .map_err(|e| anyhow::anyhow!("systemd-run (the restart must run outside silli's cgroups): {e}"))?;
    if !st.success() {
        bail!("systemd-run failed ({st}); run `silli restart` from a terminal that is not silli's");
    }
    println!("restarting outside silli; its output goes to {}", log_path.display());
    Ok(())
}

/// End this account's silli-gui windows: asked first, then made to.
fn stop_running_gui() {
    let uid = rustix::process::getuid().as_raw().to_string();
    let signal = |sig: &str| {
        std::process::Command::new("pkill")
            .args([sig, "-u", &uid, "-x", "silli-gui"])
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
            .args(["-u", &uid, "-x", "silli-gui"])
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

/// The daemon goes with the window (docs/daemon.md §3.1, §3.6): sessions stay — each is a scope of
/// its own — and the next daemon adopts them. Nothing starts it here: the new window's first call
/// does, from the window's own build (the dev build for `--dev`, the installed one otherwise), so a
/// window never talks to a daemon of another build. With the installed units, `--dev` also stops
/// `silli.socket` (a dev daemon binds the same path itself) and a plain restart starts it again —
/// unless autostart is off: then the socket stays stopped and the daemon is started here.
fn restart_daemon(dev: bool) -> Result<()> {
    let socket_unit = systemctl(&["cat", "silli.socket"]);
    if socket_unit {
        if ["silli.service", "silli.socket", "silli-gui.service"].iter().any(|u| crate::init::unit_stale(u)) {
            anyhow::ensure!(systemctl(&["daemon-reload"]), "could not reload user units");
        }
        anyhow::ensure!(systemctl(&["stop", "silli.socket", "silli.service"]), "could not stop daemon units");
    }
    // Unit files may exist while a bare daemon owns the socket. Stop that daemon too.
    if let Ok(client) = silli_proto::Client::connect() {
        client.call_within::<serde_json::Value>(silli_proto::method::DAEMON_SHUTDOWN, silli_proto::Empty::default(), silli_proto::client::QUICK_TIMEOUT)?;
    }
    systemctl(&["stop", "silli-daemon.service"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while silli_proto::Client::connect().is_ok() {
        anyhow::ensure!(std::time::Instant::now() < deadline, "old daemon did not stop within 10 seconds");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if dev {
        return Ok(());
    }
    if !silli_proto::autostart_enabled()? {
        // Autostart off (docs/daemon.md, Optional autostart): a started silli.socket would let any
        // client start the daemon again, so it stays stopped, and no client would start one
        // either — this restart, the operator's own, starts the daemon itself.
        drop(silli_proto::Client::connect_or_start_explicit()?);
    } else if socket_unit {
        anyhow::ensure!(systemctl(&["start", "silli.socket"]), "could not start silli.socket");
    }
    Ok(())
}
