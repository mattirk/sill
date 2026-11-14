//! Global show/hide hotkey, grabbed directly on the X server (SPEC §4.6): no desktop-environment
//! shortcut needed, configurable in Settings. Spec syntax like GTK accelerators: `<Alt>space`,
//! `<Super>grave`, `F12`, `<Control>Shift_R`. Grabbed with and without NumLock/CapsLock.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, KeyButMask, ModMask};
use x11rb::rust_connection::RustConnection;

pub const DEFAULT: &str = "<Alt>space";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    keycode: u8,
    mods: ModMask,
}

/// Keys that are harmless to grab without a modifier: nothing types them.
const BARE_OK: &[&str] = &[
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15", "f16",
    "f17", "f18", "f19", "f20", "pause", "scroll_lock", "menu", "insert",
];

/// A binding must not steal a key people type: without a modifier only the BARE_OK keys are
/// allowed, and a plain modifier key is never a binding on its own.
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
    let is_mod_key = matches!(key.as_str(), "shift_l" | "shift_r" | "control_l" | "control_r" | "alt_l" | "alt_r" | "super_l" | "super_r" | "meta_l" | "meta_r");
    if is_mod_key {
        // grabbing a modifier key hides its release from other clients: they see it as held down
        bail!("a modifier key (Shift/Ctrl/Alt/Super) can't be the hotkey: other programs would see it as stuck");
    }
    if !has_mod && !BARE_OK.contains(&key.as_str()) {
        bail!("'{key}' without a modifier would be swallowed everywhere you type it; add Ctrl/Alt/Super or use an F-key");
    }
    Ok(())
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
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let per = map.keysyms_per_keycode as usize;
    for (i, chunk) in map.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            return Ok(Binding { keycode: min + i as u8, mods });
        }
    }
    bail!("key {rest:?} is not on this keyboard")
}

/// A few names beyond the plain Latin letters/digits (X keysym values).
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
        s if s.starts_with('f') && s[1..].parse::<u32>().map(|n| (1..=24).contains(&n)).unwrap_or(false) => {
            0xffbe + s[1..].parse::<u32>().unwrap() - 1
        }
        _ => return None,
    })
}

pub struct Hotkey {
    /// Rebind requests go to the grab thread: X key grabs belong to the connection that made them,
    /// so the thread that listens must also be the one that grabs.
    requests: Mutex<std::sync::mpsc::Sender<(String, std::sync::mpsc::Sender<Result<()>>)>>,
    wake: std::os::fd::OwnedFd,
}

impl Hotkey {
    /// Start the grab thread; `on_press` runs on the GTK main loop for every real press (X
    /// autorepeat is filtered out) with the X server time of the press, for focus requests.
    pub fn start(spec: &str, on_press: impl Fn(u32) + 'static) -> Result<(Self, Option<String>)> {
        let (conn, screen_num) = x11rb::connect(None).context("X11 connection for the hotkey")?;
        let root = conn.setup().roots[screen_num].root;
        let mut current: Option<Binding> = None;
        let mut error = None;
        match parse(spec, &conn) {
            Ok(b) => match grab(&conn, root, &b) {
                Ok(()) => current = Some(b),
                Err(e) => error = Some(format!("{e:#}")),
            },
            Err(e) => error = Some(format!("{e:#}")),
        }
        let (press_tx, press_rx) = async_channel::unbounded::<u32>();
        let (req_tx, req_rx) = std::sync::mpsc::channel::<(String, std::sync::mpsc::Sender<Result<()>>)>();
        let (wake_rd, wake_wr) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK)?;
        std::thread::Builder::new().name("sill-hotkey".into()).spawn(move || {
            use rustix::event::{PollFd, PollFlags, poll};
            let xsock = conn.stream();
            // X autorepeat shows up as a KeyRelease immediately followed by a KeyPress with the
            // same keycode and timestamp; a chord held a moment too long would otherwise toggle
            // twice (show, then hide) and look like a press that did nothing
            let mut last_release: Option<(u8, u32)> = None;
            loop {
                // drain what the server already delivered, then wait for the socket or a request
                while let Ok(Some(ev)) = conn.poll_for_event() {
                    match ev {
                        Event::KeyRelease(k) => last_release = Some((k.detail, k.time)),
                        Event::KeyPress(k) => {
                            let repeat = last_release == Some((k.detail, k.time));
                            last_release = None;
                            if repeat {
                                continue;
                            }
                            if let Some(b) = &current {
                                let state = ModMask::from(
                                    k.state.bits() & (KeyButMask::CONTROL | KeyButMask::SHIFT | KeyButMask::MOD1 | KeyButMask::MOD4).bits(),
                                );
                                if k.detail == b.keycode && state == b.mods {
                                    let _ = press_tx.send_blocking(k.time);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let mut fds = [PollFd::new(&xsock, PollFlags::IN), PollFd::new(&wake_rd, PollFlags::IN)];
                if poll(&mut fds, None).is_err() {
                    break;
                }
                if fds[1].revents().contains(PollFlags::IN) {
                    let mut buf = [0u8; 64];
                    let _ = rustix::io::read(&wake_rd, &mut buf);
                    while let Ok((spec, reply)) = req_rx.try_recv() {
                        let result = (|| -> Result<()> {
                            let b = parse(&spec, &conn)?;
                            let old = current.take();
                            if let Some(o) = &old {
                                let _ = ungrab(&conn, root, o);
                            }
                            if let Err(e) = grab(&conn, root, &b) {
                                if let Some(o) = old {
                                    let _ = grab(&conn, root, &o);
                                    current = Some(o);
                                }
                                return Err(e);
                            }
                            current = Some(b);
                            Ok(())
                        })();
                        let _ = reply.send(result);
                    }
                }
            }
        })?;
        gtk4::glib::spawn_future_local(async move {
            while let Ok(time) = press_rx.recv().await {
                on_press(time);
            }
        });
        Ok((Self { requests: Mutex::new(req_tx), wake: wake_wr }, error))
    }

    /// Replace the binding (Settings); the grab thread does the work on its own connection.
    pub fn rebind(&self, spec: &str) -> Result<()> {
        validate(spec)?;
        let (tx, rx) = std::sync::mpsc::channel();
        self.requests.lock().unwrap().send((spec.to_string(), tx)).map_err(|_| anyhow::anyhow!("hotkey thread gone"))?;
        let _ = rustix::io::write(&self.wake, b"r");
        rx.recv_timeout(std::time::Duration::from_secs(3)).map_err(|_| anyhow::anyhow!("hotkey thread did not answer"))?
    }
}

/// NumLock (Mod2) and CapsLock (Lock) variants so the key works regardless of those states.
fn variants(mods: ModMask) -> [ModMask; 4] {
    [mods, mods | ModMask::M2, mods | ModMask::LOCK, mods | ModMask::M2 | ModMask::LOCK]
}

fn grab(conn: &RustConnection, root: u32, b: &Binding) -> Result<()> {
    for m in variants(b.mods) {
        let cookie = conn.grab_key(true, root, m, b.keycode, GrabMode::ASYNC, GrabMode::ASYNC)?;
        cookie.check().map_err(|e| anyhow::anyhow!("grab refused ({e}); another program (Cinnamon shortcut?) already has it"))?;
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
