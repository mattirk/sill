//! The hotkey on X11: the key grabbed on the root window, by a thread of its own that is made to
//! keep working (the person, 2026-11-05: an earlier grab was not reliable). What it keeps:
//!
//! - **a lost X connection**: the thread connects again, and grabs again;
//! - **a refused grab** (another program, or the previous frontend not gone yet, holds the key)
//!   is tried again every few seconds, and the Settings page says so;
//! - **a changed keyboard mapping** (a layout switch, the desktop applying its keyboard settings
//!   after login): the grab is on the physical key, which no layout moves; `MappingNotify` grabs
//!   it again all the same;
//! - **key repeat**: a press is taken once — X's repeated press (same time as the release before
//!   it, or a press while the key is still down) and a second press within 250 ms are dropped;
//! - **the grab's own focus**: while the key is down the keyboard is grabbed, and a window brought
//!   up then could lose the focus again when the grab ends; a press is acted on when the key is
//!   released;
//! - **recording**: while Settings records the key the grab is let go (an empty spec), at once:
//!   the thread is woken through a pipe.
//!
//! The key is grabbed with and without NumLock and CapsLock. An X keycode is the key's Linux
//! input code plus 8.
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, KeyButMask, ModMask};
use x11rb::rust_connection::RustConnection;

use super::{Report, pretty, read, validate};

/// How long a refused grab waits before it is tried again.
const RETRY: Duration = Duration::from_secs(5);
/// A second press this soon after one is taken is a bounce, not a press.
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    keycode: u8,
    mods: ModMask,
}

/// The grab a spec asks for on this server.
fn binding(spec: &str, conn: &RustConnection) -> Result<Binding> {
    validate(spec)?;
    let chord = read(spec)?;
    let mut mods = ModMask::default();
    for (held, mask) in [(chord.ctrl, ModMask::CONTROL), (chord.shift, ModMask::SHIFT), (chord.alt, ModMask::M1), (chord.logo, ModMask::M4)] {
        if held {
            mods |= mask;
        }
    }
    let setup = conn.setup();
    let keycode = chord.input.checked_add(8).filter(|keycode| (setup.min_keycode..=setup.max_keycode).contains(keycode));
    Ok(Binding { keycode: keycode.context("this X server has no such key")?, mods })
}

/// NumLock (Mod2) and CapsLock (Lock) variants, so the key works whatever their state.
fn variants(mods: ModMask) -> [ModMask; 4] {
    [mods, mods | ModMask::M2, mods | ModMask::LOCK, mods | ModMask::M2 | ModMask::LOCK]
}

fn grab(conn: &RustConnection, root: u32, b: &Binding) -> Result<()> {
    for m in variants(b.mods) {
        let cookie = conn.grab_key(true, root, m, b.keycode, GrabMode::ASYNC, GrabMode::ASYNC)?;
        if let Err(e) = cookie.check() {
            // a partial grab is no grab: let go of what was taken
            let _ = ungrab(conn, root, b);
            bail!("another program holds it ({e})");
        }
    }
    conn.flush()?;
    Ok(())
}

fn ungrab(conn: &RustConnection, root: u32, b: &Binding) -> Result<()> {
    for m in variants(b.mods) {
        conn.ungrab_key(b.keycode, root, m)?;
    }
    conn.flush()?;
    Ok(())
}

/// The running hotkey: give it the spec, and again whenever the setting changes.
pub struct Hotkey {
    specs: mpsc::Sender<String>,
    /// A byte here wakes the thread from its wait for the X server.
    wake: Option<OwnedFd>,
}

impl Hotkey {
    /// The key to hold from now on; an empty spec holds none.
    pub fn set(&self, spec: &str) {
        let _ = self.specs.send(spec.to_string());
        if let Some(wake) = &self.wake {
            let _ = rustix::io::write(wake, &[1]);
        }
    }
}

/// Start the hotkey's thread; `report` gets its presses and how the grab stands (false: the
/// window is gone, and the thread with it). It grabs nothing until it has a spec.
pub fn start(report: impl Fn(Report) -> bool + Send + 'static) -> Hotkey {
    let (specs, receive) = mpsc::channel::<String>();
    // without the pipe a new spec is seen at the thread's next wake, half a second at most
    let (woken, wake) = match rustix::pipe::pipe() {
        Ok((read, write)) => {
            // the programs the window starts do not get the pipe
            let _ = rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC);
            let _ = rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC);
            (Some(read), Some(write))
        }
        Err(_) => (None, None),
    };
    let spawned = std::thread::Builder::new().name("silli-gui-hotkey".into()).spawn(move || run(receive, woken, report));
    if spawned.is_err() {
        eprintln!("silli-gui: the hotkey's thread could not start");
    }
    Hotkey { specs, wake }
}

fn run(specs: mpsc::Receiver<String>, woken: Option<OwnedFd>, report: impl Fn(Report) -> bool) {
    let mut last_status = String::new();
    let say = |status: String, last: &mut String| -> bool {
        if *last == status {
            return true;
        }
        *last = status.clone();
        report(Report::Status(status))
    };
    if std::env::var_os("DISPLAY").is_none() {
        say("no X11 display: bind `silli toggle` to a key in the desktop's keyboard settings".into(), &mut last_status);
        return;
    }
    // the first spec: until the settings are read, nothing to grab
    let Ok(mut spec) = specs.recv() else { return };
    loop {
        let (conn, screen) = match x11rb::connect(None) {
            Ok(c) => c,
            Err(e) => {
                if !say(format!("cannot reach the X server ({e}); trying again"), &mut last_status) {
                    return;
                }
                std::thread::sleep(RETRY);
                continue;
            }
        };
        let root = conn.setup().roots[screen].root;
        match serve(&conn, root, (&specs, woken.as_ref()), &mut spec, &report, &mut last_status, &say) {
            Served::Gone => return,
            Served::Lost => {
                if !say("the X connection was lost; grabbing again".into(), &mut last_status) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

enum Served {
    /// The window is gone: so is the thread.
    Gone,
    /// The X connection broke: connect again.
    Lost,
}

fn serve(
    conn: &RustConnection,
    root: u32,
    (specs, woken): (&mpsc::Receiver<String>, Option<&OwnedFd>),
    spec: &mut String,
    report: &impl Fn(Report) -> bool,
    last_status: &mut String,
    say: &impl Fn(String, &mut String) -> bool,
) -> Served {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut bound: Option<Binding> = None;
    let mut regrab = true;
    let mut retry_at: Option<Instant> = None;
    // the key down (repeats are not presses), a press waiting for its release, the last one taken
    let mut held = false;
    let mut armed = false;
    let mut last_taken: Option<Instant> = None;
    let mut waited_for_repeat = false;
    loop {
        // a new spec from the settings
        loop {
            match specs.try_recv() {
                Ok(next) => {
                    if next != *spec {
                        *spec = next;
                        regrab = true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Served::Gone,
            }
        }
        if regrab || retry_at.is_some_and(|at| Instant::now() >= at) {
            regrab = false;
            retry_at = None;
            if let Some(old) = bound.take() {
                let _ = ungrab(conn, root, &old);
            }
            let status = if spec.trim().is_empty() {
                "No hotkey is set".to_string()
            } else {
                match binding(spec, conn).and_then(|b| grab(conn, root, &b).map(|()| b)) {
                    Ok(b) => {
                        bound = Some(b);
                        format!("{} shows and hides the window", pretty(spec))
                    }
                    Err(e) => {
                        retry_at = Some(Instant::now() + RETRY);
                        format!("{}: {e:#} — trying again", pretty(spec))
                    }
                }
            };
            (held, armed) = (false, false);
            if !say(status, last_status) {
                return Served::Gone;
            }
        }
        // what the server already sent; a release of our key last in it waits a moment for the
        // press X's repeat sends at the same time after it, so a repeat is not taken for a release
        let mut events = Vec::new();
        loop {
            match conn.poll_for_event() {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {
                    let our_release_last = matches!(
                        events.last(),
                        Some(Event::KeyRelease(k)) if bound.as_ref().is_some_and(|b| b.keycode == k.detail)
                    );
                    if our_release_last && !waited_for_repeat {
                        waited_for_repeat = true;
                        std::thread::sleep(Duration::from_millis(30));
                        continue;
                    }
                    waited_for_repeat = false;
                    break;
                }
                Err(_) => return Served::Lost,
            }
        }
        let mut events = events.into_iter().peekable();
        while let Some(event) = events.next() {
            match event {
                // the keyboard's mapping changed: the key may have another keycode now
                Event::MappingNotify(_) => regrab = true,
                Event::KeyPress(k) => {
                    let Some(b) = &bound else { continue };
                    let state = ModMask::from(k.state.bits() & (KeyButMask::CONTROL | KeyButMask::SHIFT | KeyButMask::MOD1 | KeyButMask::MOD4).bits());
                    if k.detail != b.keycode || state != b.mods || held {
                        continue;
                    }
                    held = true;
                    if last_taken.is_some_and(|t| t.elapsed() < DEBOUNCE) {
                        continue;
                    }
                    armed = true;
                }
                Event::KeyRelease(k) => {
                    if !bound.as_ref().is_some_and(|b| b.keycode == k.detail) {
                        continue;
                    }
                    // X's repeat: this release and a press of the key at the same time
                    if matches!(events.peek(), Some(Event::KeyPress(p)) if p.detail == k.detail && p.time == k.time) {
                        events.next();
                        continue;
                    }
                    held = false;
                    // the key is up and the grab over: now the window may take the focus
                    if std::mem::take(&mut armed) {
                        last_taken = Some(Instant::now());
                        if !report(Report::Pressed(k.time)) {
                            return Served::Gone;
                        }
                    }
                }
                _ => {}
            }
        }
        if regrab {
            continue;
        }
        // wait for the server or a new spec, waking twice a second for a retry
        let stream = conn.stream();
        let timeout = Timespec { tv_sec: 0, tv_nsec: 500_000_000 };
        let polled = match woken {
            Some(woken) => {
                let mut fds = [PollFd::new(stream, PollFlags::IN), PollFd::new(woken, PollFlags::IN)];
                let polled = poll(&mut fds, Some(&timeout));
                if fds[1].revents().contains(PollFlags::IN) {
                    let mut bytes = [0u8; 64];
                    let _ = rustix::io::read(woken, &mut bytes);
                }
                polled.map(|_| fds[0].revents())
            }
            None => {
                let mut fds = [PollFd::new(stream, PollFlags::IN)];
                poll(&mut fds, Some(&timeout)).map(|_| fds[0].revents())
            }
        };
        match polled {
            Ok(revents) if revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) => return Served::Lost,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Served::Lost,
        }
    }
}
