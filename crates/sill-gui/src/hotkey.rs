//! The global show/hide key, grabbed on the X server, made to keep working (the person,
//! 2026-11-05: an earlier grab was not reliable). Spec syntax as GTK accelerators: `<Control>Tab`,
//! `<Alt>space`, `<Super>grave`, `F12` — the daemon's shared `hotkey` setting. What it keeps:
//!
//! - **a lost X connection**: the thread connects again, and grabs again;
//! - **a refused grab** (another program, or the previous frontend not gone yet, holds the key)
//!   is tried again every few seconds, and the Settings page says so;
//! - **a changed keyboard mapping** (a layout switch, the desktop applying its keyboard settings
//!   after login) would leave the grab on a stale keycode; `MappingNotify` grabs it again;
//! - **key repeat**: a press is taken once — X's repeated press (same time as the release before
//!   it, or a press while the key is still down) and a second press within 250 ms are dropped;
//! - **the grab's own focus**: while the key is down the keyboard is grabbed, and a window brought
//!   up then could lose the focus again when the grab ends; a press is acted on when the key is
//!   released.
//!
//! The key is grabbed with and without NumLock and CapsLock.
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, KeyButMask, ModMask};
use x11rb::rust_connection::RustConnection;

/// The default, when the setting is unset.
pub const DEFAULT: &str = "<Alt>space";
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    keycode: u8,
    mods: ModMask,
}

/// Keys that are harmless to grab without a modifier: nothing types them.
const BARE_OK: &[&str] = &[
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "f13",
    "f14",
    "f15",
    "f16",
    "f17",
    "f18",
    "f19",
    "f20",
    "pause",
    "scroll_lock",
    "menu",
    "insert",
];

/// A binding must not steal a key people type: without a modifier only the keys above, and a
/// modifier key alone never (its release hidden from other programs, they would see it held).
pub fn validate(spec: &str) -> Result<()> {
    let mut rest = spec.trim();
    let mut has_mod = false;
    while let Some(r) = rest.strip_prefix('<') {
        let (_, after) = r.split_once('>').context("unterminated modifier")?;
        has_mod = true;
        rest = after;
    }
    let key = rest.trim().to_ascii_lowercase();
    if key.is_empty() {
        bail!("no key");
    }
    if matches!(
        key.as_str(),
        "shift_l"
            | "shift_r"
            | "control_l"
            | "control_r"
            | "alt_l"
            | "alt_r"
            | "super_l"
            | "super_r"
            | "meta_l"
            | "meta_r"
    ) {
        bail!(
            "a modifier key (Shift/Ctrl/Alt/Super) can't be the hotkey: other programs would see it as stuck"
        );
    }
    if !has_mod && !BARE_OK.contains(&key.as_str()) {
        bail!(
            "'{key}' without a modifier would be swallowed everywhere you type it; add Ctrl/Alt/Super or use an F-key"
        );
    }
    keysym_from_name(rest).with_context(|| format!("unknown key {rest:?}"))?;
    Ok(())
}

/// A spec as people read it: `<Control>Tab` → `Ctrl+Tab`.
pub fn pretty(spec: &str) -> String {
    let mut parts = Vec::new();
    let mut rest = spec.trim();
    while let Some(r) = rest.strip_prefix('<') {
        let Some((m, after)) = r.split_once('>') else {
            break;
        };
        parts.push(match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl".to_string(),
            "shift" => "Shift".into(),
            "alt" | "mod1" => "Alt".into(),
            "super" | "mod4" | "win" => "Super".into(),
            other => other.to_string(),
        });
        rest = after;
    }
    let key = rest.trim();
    let mut key = key.chars();
    parts.push(match key.next() {
        Some(first) => first.to_uppercase().chain(key).collect(),
        None => String::new(),
    });
    parts.join("+")
}

fn parse(spec: &str, conn: &RustConnection) -> Result<Binding> {
    validate(spec)?;
    let mut mods = ModMask::default();
    let mut rest = spec.trim();
    while let Some(r) = rest.strip_prefix('<') {
        let (m, after) = r.split_once('>').context("unterminated modifier")?;
        mods |= match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => ModMask::CONTROL,
            "shift" => ModMask::SHIFT,
            "alt" | "mod1" => ModMask::M1,
            "super" | "mod4" | "win" => ModMask::M4,
            other => bail!("unknown modifier <{other}>"),
        };
        rest = after;
    }
    let keysym = keysym_from_name(rest).with_context(|| format!("unknown key {rest:?}"))?;
    // read now, not cached: after a MappingNotify this is the new mapping
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let per = map.keysyms_per_keycode as usize;
    for (i, chunk) in map.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            return Ok(Binding {
                keycode: min + i as u8,
                mods,
            });
        }
    }
    bail!("key {rest:?} is not on this keyboard")
}

/// A few names beyond the plain Latin letters and digits (X keysym values).
fn keysym_from_name(name: &str) -> Option<u32> {
    let n = name.trim();
    if n.chars().count() == 1 {
        let c = n.chars().next()?;
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_lowercase() as u32);
        }
    }
    Some(match n.to_ascii_lowercase().as_str() {
        "space" => 0x0020,
        "grave" | "`" => 0x0060,
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "pause" => 0xff13,
        "scroll_lock" => 0xff14,
        "menu" => 0xff67,
        "insert" => 0xff63,
        "shift_l" => 0xffe1,
        "shift_r" => 0xffe2,
        "control_l" => 0xffe3,
        "control_r" => 0xffe4,
        "alt_l" => 0xffe9,
        "alt_r" => 0xffea,
        "super_l" => 0xffeb,
        "super_r" => 0xffec,
        s if s.starts_with('f')
            && s[1..]
                .parse::<u32>()
                .map(|n| (1..=24).contains(&n))
                .unwrap_or(false) =>
        {
            0xffbe + s[1..].parse::<u32>().unwrap() - 1
        }
        _ => return None,
    })
}

/// NumLock (Mod2) and CapsLock (Lock) variants, so the key works whatever their state.
fn variants(mods: ModMask) -> [ModMask; 4] {
    [
        mods,
        mods | ModMask::M2,
        mods | ModMask::LOCK,
        mods | ModMask::M2 | ModMask::LOCK,
    ]
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
}

impl Hotkey {
    pub fn set(&self, spec: &str) {
        let _ = self.specs.send(spec.to_string());
    }
}

/// Start the hotkey's thread; `report` gets its presses and how the grab stands (false: the
/// window is gone, and the thread with it). It grabs nothing until it has a spec.
pub fn start(report: impl Fn(Report) -> bool + Send + 'static) -> Hotkey {
    let (specs, receive) = mpsc::channel::<String>();
    let spawned = std::thread::Builder::new()
        .name("sill-gui-hotkey".into())
        .spawn(move || run(receive, report));
    if spawned.is_err() {
        eprintln!("sill-gui: the hotkey's thread could not start");
    }
    Hotkey { specs }
}

fn run(specs: mpsc::Receiver<String>, report: impl Fn(Report) -> bool) {
    let mut last_status = String::new();
    let say = |status: String, last: &mut String| -> bool {
        if *last == status {
            return true;
        }
        *last = status.clone();
        report(Report::Status(status))
    };
    if std::env::var_os("DISPLAY").is_none() {
        say(
            "no X11 display: bind `sill toggle` to a key in the desktop's keyboard settings".into(),
            &mut last_status,
        );
        return;
    }
    // the first spec: until the settings are read, nothing to grab
    let Ok(mut spec) = specs.recv() else { return };
    loop {
        let (conn, screen) = match x11rb::connect(None) {
            Ok(c) => c,
            Err(e) => {
                if !say(
                    format!("cannot reach the X server ({e}); trying again"),
                    &mut last_status,
                ) {
                    return;
                }
                std::thread::sleep(RETRY);
                continue;
            }
        };
        let root = conn.setup().roots[screen].root;
        match serve(
            &conn,
            root,
            &specs,
            &mut spec,
            &report,
            &mut last_status,
            &say,
        ) {
            Served::Gone => return,
            Served::Lost => {
                if !say(
                    "the X connection was lost; grabbing again".into(),
                    &mut last_status,
                ) {
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
    specs: &mpsc::Receiver<String>,
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
            let status = match parse(spec, conn).and_then(|b| grab(conn, root, &b).map(|()| b)) {
                Ok(b) => {
                    bound = Some(b);
                    format!("{} shows and hides the window", pretty(spec))
                }
                Err(e) => {
                    retry_at = Some(Instant::now() + RETRY);
                    format!("{}: {e:#} — trying again", pretty(spec))
                }
            };
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
                    let state = ModMask::from(
                        k.state.bits()
                            & (KeyButMask::CONTROL
                                | KeyButMask::SHIFT
                                | KeyButMask::MOD1
                                | KeyButMask::MOD4)
                                .bits(),
                    );
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
                    if matches!(events.peek(), Some(Event::KeyPress(p)) if p.detail == k.detail && p.time == k.time)
                    {
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
        // wait for the server, waking twice a second for a new spec or a retry
        let stream = conn.stream();
        let mut fds = [PollFd::new(stream, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 500_000_000,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(_)
                if fds[0]
                    .revents()
                    .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) =>
            {
                return Served::Lost;
            }
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
        assert!(validate("<Control>Tab").is_ok());
        assert!(validate("F12").is_ok());
        assert!(validate("a").is_err(), "a bare letter would be swallowed");
        assert!(
            validate("<Control>Control_L").is_err(),
            "a modifier key alone"
        );
        assert!(validate("<Alt>nosuchkey").is_err());
        assert_eq!(pretty("<Control>Tab"), "Ctrl+Tab");
        assert_eq!(pretty("<Super>grave"), "Super+Grave");
        assert_eq!(pretty("<Alt>space"), "Alt+Space");
    }
}
