//! The global show/hide key, grabbed on the X server, made to keep working (the person,
//! 2026-11-05: an earlier grab was not reliable). A spec is what Settings records from a key
//! press ([`capture`]): `Alt+Space @Space`, `Super+\` @Backquote`, `F12 @F12` — modifiers, the key
//! as the layout shows it, and after `@` the physical key, which is what is grabbed. Written by
//! hand (`SILL_GUI_HOTKEY`) the physical key alone will do: `Ctrl+Alt+KeyT`. The setting is
//! sill-gui's own (`settings.rs`): a key belongs to the keyboard the window is at. What it keeps:
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
//! - **recording**: a grabbed key never reaches the window, so while Settings records the key the
//!   grab is let go (an empty spec), at once: the thread is woken through a pipe.
//!
//! The key is grabbed with and without NumLock and CapsLock.
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iced::keyboard::key::{Code, Physical};
use iced::keyboard::{Key, Modifiers};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, KeyButMask, ModMask};
use x11rb::rust_connection::RustConnection;

use crate::shortcuts::Capture;

/// The default, when the setting is unset.
pub const DEFAULT: &str = "Alt+Space @Space";
/// How long a refused grab waits before it is tried again.
const RETRY: Duration = Duration::from_secs(5);
/// A second press this soon after one is taken is a bounce, not a press.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What the hotkey thread tells the window.
#[derive(Debug, Clone)]
pub enum Report {
    /// The key was pressed, at this X server time.
    Pressed(u32),
    /// How the grab stands, for the Settings page.
    Status(String),
}

/// The physical keys the hotkey can be on, each with its Linux input code (`KEY_*` in
/// `linux/input-event-codes.h`); an X server's keycode is that plus 8.
#[rustfmt::skip]
const KEYS: &[(Code, u8)] = &[
    (Code::Digit1, 2), (Code::Digit2, 3), (Code::Digit3, 4), (Code::Digit4, 5), (Code::Digit5, 6), (Code::Digit6, 7),
    (Code::Digit7, 8), (Code::Digit8, 9), (Code::Digit9, 10), (Code::Digit0, 11), (Code::Minus, 12), (Code::Equal, 13),
    (Code::Backspace, 14), (Code::Tab, 15),
    (Code::KeyQ, 16), (Code::KeyW, 17), (Code::KeyE, 18), (Code::KeyR, 19), (Code::KeyT, 20), (Code::KeyY, 21),
    (Code::KeyU, 22), (Code::KeyI, 23), (Code::KeyO, 24), (Code::KeyP, 25), (Code::BracketLeft, 26),
    (Code::BracketRight, 27), (Code::Enter, 28),
    (Code::KeyA, 30), (Code::KeyS, 31), (Code::KeyD, 32), (Code::KeyF, 33), (Code::KeyG, 34), (Code::KeyH, 35),
    (Code::KeyJ, 36), (Code::KeyK, 37), (Code::KeyL, 38), (Code::Semicolon, 39), (Code::Quote, 40),
    (Code::Backquote, 41), (Code::Backslash, 43),
    (Code::KeyZ, 44), (Code::KeyX, 45), (Code::KeyC, 46), (Code::KeyV, 47), (Code::KeyB, 48), (Code::KeyN, 49),
    (Code::KeyM, 50), (Code::Comma, 51), (Code::Period, 52), (Code::Slash, 53), (Code::Space, 57),
    (Code::F1, 59), (Code::F2, 60), (Code::F3, 61), (Code::F4, 62), (Code::F5, 63), (Code::F6, 64), (Code::F7, 65),
    (Code::F8, 66), (Code::F9, 67), (Code::F10, 68), (Code::ScrollLock, 70), (Code::IntlBackslash, 86),
    (Code::F11, 87), (Code::F12, 88),
    (Code::Home, 102), (Code::ArrowUp, 103), (Code::PageUp, 104), (Code::ArrowLeft, 105), (Code::ArrowRight, 106),
    (Code::End, 107), (Code::ArrowDown, 108), (Code::PageDown, 109), (Code::Insert, 110), (Code::Delete, 111),
    (Code::Pause, 119), (Code::ContextMenu, 127),
    (Code::F13, 183), (Code::F14, 184), (Code::F15, 185), (Code::F16, 186), (Code::F17, 187), (Code::F18, 188),
    (Code::F19, 189), (Code::F20, 190), (Code::F21, 191), (Code::F22, 192), (Code::F23, 193), (Code::F24, 194),
];

/// Keys that are harmless to grab without a modifier: nothing types them.
fn bare_ok(code: Code) -> bool {
    is_function(code) || matches!(code, Code::Pause | Code::ScrollLock | Code::ContextMenu | Code::Insert)
}
fn is_function(code: Code) -> bool {
    let name = format!("{code:?}");
    name.strip_prefix('F').is_some_and(|n| n.parse::<u8>().is_ok())
}

/// A spec read: the modifiers held and the physical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    code: Code,
    /// The key's Linux input code.
    input: u8,
}

fn read(spec: &str) -> Result<Chord> {
    let spec = spec.trim();
    // the physical key after `@`, or (written by hand) in the key's own place
    let (written, named) = match spec.rsplit_once('@') {
        Some((written, name)) => (written.trim(), Some(name.trim())),
        None => (spec, None),
    };
    let mut parts: Vec<&str> = written.split('+').map(str::trim).collect();
    let key = parts.pop().unwrap_or_default();
    let name = named.unwrap_or(key);
    if name.is_empty() {
        bail!("no key");
    }
    let &(code, input) = KEYS.iter().find(|(code, _)| format!("{code:?}").eq_ignore_ascii_case(name)).with_context(|| format!("unknown key {name:?}"))?;
    let mut chord = Chord { ctrl: false, alt: false, shift: false, logo: false, code, input };
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => chord.ctrl = true,
            "alt" => chord.alt = true,
            "shift" => chord.shift = true,
            "super" | "win" | "cmd" => chord.logo = true,
            other => bail!("unknown modifier {other:?}"),
        }
    }
    Ok(chord)
}

/// A binding must not steal a key people type: without Ctrl, Alt or Super only the keys nothing
/// types. An empty spec is no hotkey.
pub fn validate(spec: &str) -> Result<()> {
    if spec.trim().is_empty() {
        return Ok(());
    }
    let chord = read(spec)?;
    if !(chord.ctrl || chord.alt || chord.logo || bare_ok(chord.code)) {
        bail!("without Ctrl, Alt or Super this key would be swallowed everywhere you type it; hold one, or use a function key");
    }
    if chord.ctrl && chord.alt && is_function(chord.code) {
        bail!("Ctrl+Alt with a function key switches the virtual console");
    }
    Ok(())
}

/// A spec as people read it: `Alt+Space @Space` → `Alt+Space`.
pub fn pretty(spec: &str) -> String {
    spec.rsplit_once('@').map_or(spec, |(written, _)| written).trim().to_string()
}

/// Read a press for the Settings page's recorder. `key` is the key without its modifiers.
pub fn capture(key: &Key, physical: Physical, modifiers: Modifiers) -> Capture {
    if let Some(early) = crate::shortcuts::recorder_key(key, modifiers) {
        return early;
    }
    let Physical::Code(code) = physical else {
        return Capture::Refused("This key is not known to the system by its place on the keyboard.".into());
    };
    if !KEYS.iter().any(|(known, _)| *known == code) {
        return Capture::Refused("This key can't be the hotkey.".into());
    }
    let mut chord = String::new();
    for (held, name) in [(modifiers.control(), "Ctrl"), (modifiers.logo(), "Super"), (modifiers.alt(), "Alt"), (modifiers.shift(), "Shift")] {
        if held {
            chord.push_str(name);
            chord.push('+');
        }
    }
    chord.push_str(&crate::shortcuts::key_label(key, code));
    let spec = format!("{chord} @{code:?}");
    match validate(&spec) {
        Ok(()) => Capture::Chord(spec),
        Err(why) => Capture::Refused(format!("{}: {why:#}.", pretty(&spec))),
    }
}

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
    let spawned = std::thread::Builder::new().name("sill-gui-hotkey".into()).spawn(move || run(receive, woken, report));
    if spawned.is_err() {
        eprintln!("sill-gui: the hotkey's thread could not start");
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
        say("no X11 display: bind `sill toggle` to a key in the desktop's keyboard settings".into(), &mut last_status);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_checked_and_read_as_people_say_them() {
        assert!(validate(DEFAULT).is_ok());
        assert!(validate("Ctrl+Tab @Tab").is_ok());
        assert!(validate("F12 @F12").is_ok());
        assert!(validate("").is_ok(), "no hotkey");
        assert!(validate("Ctrl+Alt+KeyT").is_ok(), "by hand, the physical key alone");
        assert!(validate("A @KeyA").is_err(), "a bare letter would be swallowed");
        assert!(validate("Shift+A @KeyA").is_err());
        assert!(validate("Ctrl+Alt+F2 @F2").is_err(), "the virtual console's");
        assert!(validate("Alt+X @NoSuchKey").is_err());
        assert!(validate("Hyper+Space @Space").is_err());
        assert_eq!(pretty("Super+` @Backquote"), "Super+`");
        // the label is only shown: the physical key is what is read
        assert_eq!(read("Alt+Ö @Semicolon").unwrap(), Chord { ctrl: false, alt: true, shift: false, logo: false, code: Code::Semicolon, input: 39 });
        // every key has one input code, and a keycode beside it
        for (i, (code, input)) in KEYS.iter().enumerate() {
            assert!(KEYS[..i].iter().all(|(c, n)| c != code && n != input), "{code:?}");
            assert!(input.checked_add(8).is_some());
        }
    }

    #[test]
    fn a_press_is_recorded_as_the_physical_key() {
        use iced::keyboard::key::Named;
        let space = Key::Named(Named::Space);
        assert_eq!(capture(&space, Physical::Code(Code::Space), Modifiers::ALT), Capture::Chord("Alt+Space @Space".into()));
        assert_eq!(capture(&Key::Character("`".into()), Physical::Code(Code::Backquote), Modifiers::LOGO), Capture::Chord("Super+` @Backquote".into()));
        assert_eq!(capture(&Key::Named(Named::F12), Physical::Code(Code::F12), Modifiers::empty()), Capture::Chord("F12 @F12".into()));
        assert_eq!(capture(&Key::Named(Named::Tab), Physical::Code(Code::Tab), Modifiers::CTRL), Capture::Chord("Ctrl+Tab @Tab".into()));
        assert_eq!(capture(&Key::Named(Named::Alt), Physical::Code(Code::AltLeft), Modifiers::ALT), Capture::Waiting);
        assert_eq!(capture(&Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::empty()), Capture::Cancel);
        assert_eq!(capture(&Key::Named(Named::Delete), Physical::Code(Code::Delete), Modifiers::empty()), Capture::Clear);
        assert!(matches!(capture(&Key::Character("a".into()), Physical::Code(Code::KeyA), Modifiers::SHIFT), Capture::Refused(_)));
        assert!(matches!(capture(&Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::ALT), Capture::Refused(_)));
    }
}
