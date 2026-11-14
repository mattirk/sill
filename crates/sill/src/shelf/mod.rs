//! The shelf: a GTK4 window on the left edge, toggled by `sill toggle` (SPEC §3.1, §4.6).
//! One instance per desktop session (GApplication); `toggle` reaches it over D-Bus activation.

mod agents_page;
mod changes;
mod clamp;
pub mod client;
mod conversation;
mod diffstat;
mod fonts;
mod herring;
mod hotkey;
mod icons;
mod login_input;
mod markdown;
mod processes;
mod remote;
mod remote_store;
mod session;
mod session_page;
mod session_view;
mod settings;
mod spinner;
mod streams;
mod tabs;
mod theme;
mod tray;
mod tree;
mod viewer;
mod window;
mod x11;

use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Result, bail};
use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

pub const APP_ID: &str = "io.github.mattirk.sill";

fn application_id() -> String {
    match std::env::var("SILL_BOX").ok().filter(|name| !name.is_empty() && name != "local") {
        Some(name) => {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            name.hash(&mut hash);
            format!("{APP_ID}.box_{:016x}", hash.finish())
        }
        None => APP_ID.into(),
    }
}

/// Wall-clock stamp for `SILL_TRACE` lines (shelf.log has no journal prefix).
pub fn trace_ts() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs() % 86400;
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, s / 60 % 60, s % 60, d.subsec_millis())
}

pub fn run(cwd: Option<PathBuf>, hidden: bool) -> Result<()> {
    let cwd = match cwd {
        Some(c) => std::path::absolute(c)?,
        None if sill_proto::transport::selected()?.is_some() => remote::home()?,
        None => std::env::current_dir()?,
    };
    fonts::init(); // before GTK's first font map
    let app = gtk::Application::builder().application_id(application_id()).build();
    let cwd_for_activate = cwd.clone();
    app.connect_activate(move |app| {
        if let Some(w) = app.windows().first() {
            // a second `sill shelf`: just show the existing one
            window::show(&w, 0);
            return;
        }
        fonts::pin_rendering();
        let w = window::build(app, cwd_for_activate.clone());
        if std::env::var_os("SILL_TRACE").is_some() {
            // main-loop stall detector: anything that holds the GTK thread shows up here with its length
            let mut last = std::time::Instant::now();
            glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                let gap = last.elapsed();
                if gap > std::time::Duration::from_millis(150) {
                    eprintln!("{} main loop stalled {} ms", trace_ts(), gap.as_millis());
                }
                last = std::time::Instant::now();
                glib::ControlFlow::Continue
            });
        }
        if hidden {
            // keep the session warm, show on the first toggle
            w.set_visible(false);
        } else {
            w.present();
        }
        if remote::config::load().tray() {
            tray::start(app);
        }
    });
    let toggle = gio::SimpleAction::new("toggle", None);
    toggle.connect_activate({
        let app = app.clone();
        move |_, _| {
            if let Some(w) = app.windows().first() {
                window::toggle(w, 0);
            }
        }
    });
    app.add_action(&toggle);
    // `sill theme NAME`: switch the running shelf's theme (also what the Settings dropdown does)
    let set_theme = gio::SimpleAction::new("set-theme", Some(gtk::glib::VariantTy::STRING));
    set_theme.connect_activate(|_, param| {
        if let Some(id) = param.and_then(|p| p.str()) {
            let t = theme::by_id(id);
            theme::apply(&t);
            let _ = remote::config::update(|c| c.theme = t.id.to_string());
        }
    });
    app.add_action(&set_theme);
    // `sill hotkey SPEC`: rebind the show/hide key in the running shelf
    let set_hotkey = gio::SimpleAction::new("set-hotkey", Some(gtk::glib::VariantTy::STRING));
    set_hotkey.connect_activate(|_, param| {
        if let Some(spec) = param.and_then(|p| p.str()) {
            let r = window::HOTKEY.with(|h| h.borrow().as_ref().map(|hk| hk.rebind(spec)));
            match r {
                Some(Ok(())) => {
                    let _ = remote::config::update(|c| c.hotkey = Some(spec.to_string()));
                }
                Some(Err(e)) => eprintln!("sill: hotkey {spec}: {e:#}"),
                None => eprintln!("sill: hotkey grab not active"),
            }
        }
    });
    app.add_action(&set_hotkey);
    let quit = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        quit.connect_activate(move |_, _| app.quit());
    }
    app.add_action(&quit);
    // no command-line parsing by GTK: we were started by clap
    let code = app.run_with_args::<&str>(&[]);
    if code != glib_exit_ok() {
        bail!("shelf exited with {code:?}");
    }
    Ok(())
}

fn glib_exit_ok() -> gtk::glib::ExitCode {
    gtk::glib::ExitCode::SUCCESS
}

/// `sill theme NAME`: ask the running shelf to switch theme (saved for next start too).
pub fn set_theme(name: &str) -> Result<()> {
    let app = gio::Application::new(Some(&application_id()), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        // no shelf running: just save it
        let t = theme::by_id(name);
        return remote::config::update(|c| c.theme = t.id.to_string());
    }
    app.activate_action("set-theme", Some(&name.to_variant()));
    Ok(())
}

/// Resolve the frontend target from its initial environment and actual CLI arguments. Global
/// options may precede or follow the subcommand; `box connect NAME` selects NAME itself.
fn shelf_process_target(args: &[&[u8]], environ: &[u8]) -> Option<String> {
    let executable = args.first()?;
    if !(*executable == b"sill" || executable.ends_with(b"/sill")) { return None; }
    let mut target = environ.split(|b| *b == 0).find_map(|entry| entry.strip_prefix(b"SILL_BOX=")).unwrap_or(b"local");
    let mut positional = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = args[i];
        if arg == b"--box" {
            i += 1;
            target = *args.get(i)?;
        } else if let Some(name) = arg.strip_prefix(b"--box=") {
            target = name;
        } else if !arg.is_empty() {
            positional.push(arg);
        }
        i += 1;
    }
    match positional.as_slice() {
        [command, ..] if *command == b"shelf" => {},
        [command, action, name, ..] if *command == b"box" && *action == b"connect" => target = name,
        _ => return None,
    }
    let target = std::str::from_utf8(target).ok()?;
    Some(if target.is_empty() || target == "local" { "local".into() } else { target.into() })
}

/// Signal only this user's other frontends for the same box. An unreadable process environment
/// leaves its target unknown, so it is skipped rather than risking another frontend.
fn kill_old_shelves_with(sig: rustix::process::Signal) {
    use std::os::unix::fs::MetadataExt;
    let me = std::process::id();
    let uid = rustix::process::geteuid().as_raw();
    let target = std::env::var("SILL_BOX").ok().filter(|name| !name.is_empty()).unwrap_or_else(|| "local".into());
    let Ok(rd) = std::fs::read_dir("/proc") else { return };
    for e in rd.filter_map(Result::ok) {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me || !e.metadata().is_ok_and(|m| m.uid() == uid) { continue; }
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        let Ok(environ) = std::fs::read(e.path().join("environ")) else { continue };
        let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        if shelf_process_target(&args, &environ).as_deref() == Some(target.as_str())
            && let Some(p) = rustix::process::Pid::from_raw(pid as i32)
        {
            let _ = rustix::process::kill_process(p, sig);
        }
    }
}

fn kill_old_shelves() {
    kill_old_shelves_with(rustix::process::Signal::TERM);
}

/// Make sure no shelf holds the application id, whoever started it (the unit, `--dev`, a hand-run
/// `sill shelf`): ask it to quit over D-Bus, escalate to signals. Returns with the id free — every
/// `gio::Application` made here is dropped, so the next shelf to start becomes the primary one.
fn quit_running_shelf() -> Result<()> {
    // Ask the running shelf to quit over D-Bus. A shelf whose main loop is wedged owns the name
    // but answers nothing: registering against it times out, so that too means "kill it".
    let app = gio::Application::new(Some(&application_id()), gio::ApplicationFlags::empty());
    let remote = match app.register(None::<&gio::Cancellable>) {
        Ok(()) => app.is_remote(),
        Err(e) => {
            eprintln!("sill: the running shelf does not answer ({e}); terminating it");
            kill_old_shelves();
            true
        }
    };
    if remote {
        if app.is_registered() && app.is_remote() {
            app.activate_action("quit", None);
        }
        // wait for the old instance to release the application id; escalate if it doesn't
        let mut freed = false;
        for i in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let probe = gio::Application::new(Some(&application_id()), gio::ApplicationFlags::empty());
            if probe.register(None::<&gio::Cancellable>).is_ok() && !probe.is_remote() {
                freed = true;
                break;
            }
            if i == 20 {
                kill_old_shelves();
            }
            if i == 45 {
                kill_old_shelves_with(rustix::process::Signal::KILL);
            }
        }
        if !freed {
            bail!("the running shelf did not quit");
        }
    }
    drop(app);
    Ok(())
}

/// The daemon goes with the shelf (docs/daemon.md §3.1, §3.6): sessions stay — each is a scope of
/// its own — and the next daemon adopts them. Nothing starts it here: the new shelf's first call
/// does, from the shelf's own binary (the dev build for `--dev`, the installed one otherwise), so a
/// shelf never talks to a daemon of another build. With the installed units, `--dev` also stops
/// `sill.socket` (a dev daemon binds the same path itself) and a plain restart starts it again.
fn restart_daemon(dev: bool) {
    let systemctl = |args: &[&str]| {
        std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let socket_unit = systemctl(&["cat", "sill.socket"]);
    if socket_unit {
        // a deploy rewrote the units (`sudo sill init`): the user manager must read them again
        // before the restart, or the new daemon runs under the old text (docs/daemon.md §3.7)
        if ["sill.service", "sill.socket", "sill-shelf.service"].iter().any(|u| crate::init::unit_stale(u)) {
            systemctl(&["daemon-reload"]);
        }
        systemctl(&["stop", "sill.service"]);
        if dev {
            systemctl(&["stop", "sill.socket"]);
        }
    }
    if socket_unit && !dev {
        // the socket unit still listens: a connect from here would only activate a fresh daemon
        // to ask it to leave, and a second one after it (docs/daemon.md §3.9 item 8). The stop was
        // synchronous; the next client's connection starts the daemon that stays.
        systemctl(&["stop", "sill-daemon.service"]);
        return;
    }
    // a daemon a client started (transient unit sill-daemon, or bare): ask it to leave
    if let Ok(c) = sill_proto::Client::connect() {
        let _ = c.call::<serde_json::Value>(sill_proto::method::DAEMON_SHUTDOWN, sill_proto::Empty::default());
    }
    systemctl(&["stop", "sill-daemon.service"]);
    for _ in 0..40 {
        if sill_proto::Client::connect().is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if socket_unit && !dev {
        systemctl(&["start", "sill.socket"]);
    }
}

/// `sill restart`: with the `sill-shelf` user service installed this stops the unit, quits any other
/// shelf (a `--dev` one, a hand-started one — the unit's new instance would only hand off to it and
/// exit, leaving the old build on screen) and starts the unit again (the installed binary); `--dev`
/// instead stops the service and runs *this* executable detached, for trying a build without
/// installing it.
pub fn restart(hidden: bool, dev: bool, detached: bool) -> Result<()> {
    // run from the shelf's own host shell (a child of the shelf: the unit's cgroup, its pty), the
    // stop below would take this command down with the shelf before it starts anything — so the
    // work is handed to a copy outside: its own scope, its own session, its output in shelf.log
    if !detached && inside_shelf() {
        return restart_detached(hidden, dev);
    }
    let unit_known = std::process::Command::new("systemctl")
        .args(["--user", "cat", "sill-shelf.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if unit_known {
        let _ = std::process::Command::new("systemctl").args(["--user", "stop", "sill-shelf.service"]).status();
    }
    quit_running_shelf()?;
    restart_daemon(dev);
    if unit_known && !dev {
        let st = std::process::Command::new("systemctl").args(["--user", "start", "sill-shelf.service"]).status()?;
        if !st.success() {
            bail!("systemctl --user start sill-shelf failed");
        }
        println!("sill-shelf.service restarted (journalctl --user -u sill-shelf)");
        return Ok(());
    }
    // --dev means the debug build of the checkout this binary was built from, whichever copy of
    // `sill` ran this command (the installed one included); without it, this binary itself
    let mut exe = std::env::current_exe()?;
    if dev {
        let candidate = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/sill");
        if let Ok(c) = candidate.canonicalize()
            && c.is_file()
        {
            exe = c;
        }
    }
    // running a build from a pinned folder is running what the agent could have written there
    if let Ok(store) = remote_store::Store::open_default()
        && let Ok(sessions) = store.sessions()
    {
        let pins: Vec<PathBuf> = sessions.iter().flat_map(|s| store.pins(&s.id).unwrap_or_default()).chain(store.global_pins().unwrap_or_default()).collect();
        if let Some(p) = sill_core::session::inside_any(&exe, &pins) {
            eprintln!("warning: {} is inside the pinned folder {} — the agent can write there", exe.display(), p.display());
        }
    }
    println!("starting {}", exe.display());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("shelf");
    if hidden {
        cmd.arg("--hidden");
    }
    // a dev shelf has no journal: its output goes to the state dir
    let log = sill_core::store::default_path().ok().and_then(|p| {
        std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(p.with_file_name("shelf.log")).ok()
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
    cmd.spawn()?;
    println!("shelf restarted");
    Ok(())
}

/// Is this process a descendant of the running shelf — in its host shell (which the shelf marks
/// with `SILL_HOST_SHELL`) or anywhere in the user service's cgroup?
fn inside_shelf() -> bool {
    std::env::var_os("SILL_HOST_SHELL").is_some()
        || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| c.contains("sill-shelf.service"))
}

/// Run `sill restart` again outside the shelf: in a transient scope of its own (not the unit's
/// cgroup, so the stop doesn't reach it), in a new session (no controlling pty to hang up when the
/// shelf's terminal goes), logging to shelf.log.
fn restart_detached(hidden: bool, dev: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = sill_core::store::default_path()?.with_file_name("shelf.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    let mut args: Vec<std::ffi::OsString> = vec!["--user".into(), "--scope".into(), "--quiet".into(), "--".into()];
    args.extend(["setsid".into(), "-f".into(), exe.into_os_string(), "restart".into(), "--detached".into()]);
    if hidden {
        args.push("--hidden".into());
    }
    if dev {
        args.push("--dev".into());
    }
    let st = std::process::Command::new("systemd-run")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .map_err(|e| anyhow::anyhow!("systemd-run (the restart must run outside the shelf's cgroup): {e}"))?;
    if !st.success() {
        bail!("systemd-run failed ({st}); run `sill restart` from a terminal that is not the shelf's");
    }
    println!("restarting outside the shelf; its output goes to {}", log_path.display());
    Ok(())
}

/// `sill hotkey SPEC`: rebind the running shelf's show/hide key (and save it).
pub fn set_hotkey(spec: &str) -> Result<()> {
    hotkey::validate(spec)?;
    let app = gio::Application::new(Some(&application_id()), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        return remote::config::update(|c| c.hotkey = Some(spec.to_string()));
    }
    app.activate_action("set-hotkey", Some(&spec.to_variant()));
    Ok(())
}

/// `sill toggle`: ask the running shelf to show/hide itself.
pub fn toggle() -> Result<()> {
    let app = gio::Application::new(Some(&application_id()), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        bail!("the shelf is not running (start it with `sill shelf`)");
    }
    app.activate_action("toggle", None);
    Ok(())
}


#[cfg(test)]
mod process_scope_tests {
    use super::shelf_process_target;

    #[test]
    fn shelf_target_follows_global_options_and_initial_environment() {
        assert_eq!(shelf_process_target(&[b"/usr/bin/sill", b"shelf"], b"SILL_BOX=server\0TOKEN=never-print\0").as_deref(), Some("server"));
        assert_eq!(shelf_process_target(&[b"sill", b"--box", b"alpha", b"shelf"], b"").as_deref(), Some("alpha"));
        assert_eq!(shelf_process_target(&[b"sill", b"shelf", b"--box=beta"], b"SILL_BOX=alpha\0").as_deref(), Some("beta"));
        assert_eq!(shelf_process_target(&[b"sill", b"shelf", b"--box", b"local"], b"SILL_BOX=alpha\0").as_deref(), Some("local"));
    }

    #[test]
    fn box_connect_selects_its_named_frontend() {
        assert_eq!(shelf_process_target(&[b"sill", b"box", b"connect", b"beta"], b"SILL_BOX=alpha\0").as_deref(), Some("beta"));
        assert_eq!(shelf_process_target(&[b"sill", b"--box=alpha", b"box", b"connect", b"beta"], b"").as_deref(), Some("beta"));
        assert_eq!(shelf_process_target(&[b"sill", b"box", b"connect", b"local"], b"SILL_BOX=alpha\0").as_deref(), Some("local"));
    }

    #[test]
    fn other_sill_commands_and_executables_are_not_frontends() {
        assert!(shelf_process_target(&[b"sill", b"daemon"], b"").is_none());
        assert!(shelf_process_target(&[b"sill", b"box", b"add", b"beta"], b"").is_none());
        assert!(shelf_process_target(&[b"other", b"shelf"], b"").is_none());
        assert!(shelf_process_target(&[b"sill", b"--box"], b"").is_none());
    }
}
