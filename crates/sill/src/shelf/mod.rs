//! The shelf: a GTK4 window on the left edge, toggled by `sill toggle` (SPEC §3.1, §4.6).
//! One instance per desktop session (GApplication); `toggle` reaches it over D-Bus activation.

mod agents_page;
mod changes;
mod clamp;
mod conversation;
mod fonts;
mod herring;
mod hotkey;
mod icons;
mod lang_icons;
mod markdown;
mod processes;
mod session;
mod session_view;
mod settings;
mod spinner;
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

/// Wall-clock stamp for `SILL_TRACE` lines (shelf.log has no journal prefix).
pub fn trace_ts() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = d.as_secs() % 86400;
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, s / 60 % 60, s % 60, d.subsec_millis())
}

pub fn run(cwd: Option<PathBuf>, hidden: bool) -> Result<()> {
    let cwd = match cwd {
        Some(c) => std::path::absolute(c)?,
        None => std::env::current_dir()?,
    };
    fonts::init(); // before GTK's first font map
    let app = gtk::Application::builder().application_id(APP_ID).build();
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
        if sill_core::config::load().tray() {
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
            let _ = sill_core::config::update(|c| c.theme = t.id.to_string());
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
                    let _ = sill_core::config::update(|c| c.hotkey = Some(spec.to_string()));
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
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        // no shelf running: just save it
        let t = theme::by_id(name);
        return sill_core::config::update(|c| c.theme = t.id.to_string());
    }
    app.activate_action("set-theme", Some(&name.to_variant()));
    Ok(())
}

/// SIGTERM every other `sill shelf` process of this user (fallback for `restart`).
/// Send `sig` to every other `sill shelf` process.
fn kill_old_shelves_with(sig: rustix::process::Signal) {
    let me = std::process::id();
    let Ok(rd) = std::fs::read_dir("/proc") else { return };
    for e in rd.filter_map(Result::ok) {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me {
            continue;
        }
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        let is_shelf = args.first().map(|a| a.ends_with(b"/sill") || *a == b"sill").unwrap_or(false)
            && args.get(1).map(|a| *a == b"shelf").unwrap_or(false);
        if is_shelf && let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
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
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
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
            let probe = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
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
    if let Ok(store) = sill_core::store::Store::open_default()
        && let Ok(sessions) = store.sessions()
    {
        let pins: Vec<PathBuf> = sessions.iter().flat_map(|s| store.pins(&s.id).unwrap_or_default()).collect();
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
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        return sill_core::config::update(|c| c.hotkey = Some(spec.to_string()));
    }
    app.activate_action("set-hotkey", Some(&spec.to_variant()));
    Ok(())
}

/// `sill toggle`: ask the running shelf to show/hide itself.
pub fn toggle() -> Result<()> {
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(None::<&gio::Cancellable>)?;
    if !app.is_remote() {
        bail!("the shelf is not running (start it with `sill shelf`)");
    }
    app.activate_action("toggle", None);
    Ok(())
}
