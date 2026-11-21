//! The hotkey on Windows: `RegisterHotKey`. A hot key belongs to the thread that registered it
//! and arrives as a message on that thread's queue, so one thread of its own holds the key and
//! reads its queue; `set` hands it the spec and wakes it with a message.
//!
//! A key Windows refuses (another program registered it) is said on the Settings page; it is
//! not tried again by itself.
use std::sync::mpsc;

use iced::keyboard::key::Code;

use super::{Report, pretty, read, validate};

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

/// `MSG`.
#[repr(C)]
struct Message {
    window: *mut std::ffi::c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
}

#[link(name = "user32", kind = "raw-dylib")]
unsafe extern "system" {
    fn RegisterHotKey(window: *mut std::ffi::c_void, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(window: *mut std::ffi::c_void, id: i32) -> i32;
    fn GetMessageW(message: *mut Message, window: *mut std::ffi::c_void, first: u32, last: u32) -> i32;
    fn PeekMessageW(message: *mut Message, window: *mut std::ffi::c_void, first: u32, last: u32, remove: u32) -> i32;
    fn PostThreadMessageW(thread: u32, message: u32, wparam: usize, lparam: isize) -> i32;
    fn MapVirtualKeyW(code: u32, kind: u32) -> u32;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

const WM_HOTKEY: u32 = 0x0312;
/// `WM_APP`: a new spec waits in the channel.
const WM_SPEC: u32 = 0x8000;
const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;
/// Holding the key down sends one press, not a stream of them.
const MOD_NOREPEAT: u32 = 0x4000;
/// `MAPVK_VSC_TO_VK_EX`.
const SCAN_CODE_TO_KEY: u32 = 3;
const ID: i32 = 1;

/// The running hotkey: give it the spec, and again whenever the setting changes.
pub struct Hotkey {
    specs: mpsc::Sender<String>,
    thread: u32,
}

impl Hotkey {
    /// The key to hold from now on; an empty spec holds none.
    pub fn set(&self, spec: &str) {
        if self.specs.send(spec.to_string()).is_ok() {
            // SAFETY: posts a message without pointers to the hotkey thread's queue
            unsafe { PostThreadMessageW(self.thread, WM_SPEC, 0, 0) };
        }
    }
}

/// `report` gets the presses and how the registration stands. Nothing is registered until
/// there is a spec.
pub fn start(report: impl Fn(Report) -> bool + Send + 'static) -> Hotkey {
    let (specs, waiting) = mpsc::channel::<String>();
    let (ready, thread) = mpsc::channel();
    let _ = std::thread::Builder::new().name("silli-gui-hotkey".into()).spawn(move || {
        // SAFETY: plain calls on this thread's own message queue, with a valid MSG to fill
        unsafe {
            let mut message: Message = std::mem::zeroed();
            // a thread has a queue once it asks for a message: made before the id is given out
            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, 0);
            let _ = ready.send(GetCurrentThreadId());
            let mut last = None;
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                match message.message {
                    WM_HOTKEY => {
                        if !report(Report::Pressed(0)) {
                            break;
                        }
                    }
                    WM_SPEC => {
                        // the newest spec is the one that counts
                        let Some(spec) = waiting.try_iter().last() else { continue };
                        UnregisterHotKey(std::ptr::null_mut(), ID);
                        let status = if spec.trim().is_empty() {
                            "No hotkey is set".to_string()
                        } else {
                            match register(&spec) {
                                Ok(()) => format!("{} shows and hides the window", pretty(&spec)),
                                Err(why) => format!("{}: {why:#}", pretty(&spec)),
                            }
                        };
                        if last.as_ref() != Some(&status) && !report(Report::Status(status.clone())) {
                            break;
                        }
                        last = Some(status);
                    }
                    _ => {}
                }
            }
            UnregisterHotKey(std::ptr::null_mut(), ID);
        }
    });
    Hotkey { specs, thread: thread.recv().unwrap_or(0) }
}

/// Register `spec` for this thread.
fn register(spec: &str) -> anyhow::Result<()> {
    validate(spec)?;
    let chord = read(spec)?;
    let key = virtual_key(chord.code, chord.input).ok_or_else(|| anyhow::anyhow!("this keyboard has no such key"))?;
    let mut modifiers = MOD_NOREPEAT;
    for (held, mask) in [(chord.ctrl, MOD_CONTROL), (chord.shift, MOD_SHIFT), (chord.alt, MOD_ALT), (chord.logo, MOD_WIN)] {
        if held {
            modifiers |= mask;
        }
    }
    // SAFETY: registers for this thread (no window), with plain values
    if unsafe { RegisterHotKey(std::ptr::null_mut(), ID, modifiers, key) } == 0 {
        anyhow::bail!("Windows refused it; another program may hold it");
    }
    Ok(())
}

/// The virtual key Windows wants for a physical key. The keys of the typing block are found by
/// their scan code, which a Linux input code is for them, through the keyboard's layout; the
/// others have codes of their own.
fn virtual_key(code: Code, input: u8) -> Option<u32> {
    let name = format!("{code:?}");
    if let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        return Some(0x70 + number - 1);
    }
    Some(match code {
        Code::Home => 0x24,
        Code::End => 0x23,
        Code::PageUp => 0x21,
        Code::PageDown => 0x22,
        Code::ArrowLeft => 0x25,
        Code::ArrowUp => 0x26,
        Code::ArrowRight => 0x27,
        Code::ArrowDown => 0x28,
        Code::Insert => 0x2D,
        Code::Delete => 0x2E,
        Code::Pause => 0x13,
        Code::ScrollLock => 0x91,
        Code::ContextMenu => 0x5D,
        _ => {
            // SAFETY: a lookup in the current keyboard layout
            let key = unsafe { MapVirtualKeyW(u32::from(input), SCAN_CODE_TO_KEY) };
            if key == 0 {
                return None;
            }
            key
        }
    })
}
