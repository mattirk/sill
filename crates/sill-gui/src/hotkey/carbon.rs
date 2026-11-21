//! The hotkey on the Mac: Carbon's `RegisterEventHotKey`, the one way to hold a key system-wide
//! that needs no permission from the person (an event tap needs Accessibility). The system
//! takes the key before any program sees it and sends an event to this application's event
//! target, which the application's own run loop delivers on the main thread — where everything
//! here happens too: `set` is called from the window's update.
//!
//! A key the system refuses (another program registered it) is said on the Settings page; it is
//! not tried again by itself. A key macOS keeps for a shortcut of its own (Spotlight's, the
//! input sources') registers without complaint and never arrives.
use std::cell::Cell;
use std::ffi::c_void;

use super::{Report, pretty, read, validate};

/// `OSStatus`: 0 is no error.
type Status = i32;
/// `EventHandlerProcPtr`: the handler's call, the event, the data given at installation.
type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> Status;

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

/// `kEventClassKeyboard`, the four-character code `keyb`.
const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
/// `kEventHotKeyPressed`.
const HOT_KEY_PRESSED: u32 = 5;
/// `cmdKey`, `shiftKey`, `optionKey`, `controlKey` of Carbon's `Events.h`.
const CMD: u32 = 0x0100;
const SHIFT: u32 = 0x0200;
const OPTION: u32 = 0x0800;
const CONTROL: u32 = 0x1000;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        count: usize,
        types: *const EventTypeSpec,
        data: *mut c_void,
        out: *mut *mut c_void,
    ) -> Status;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: EventHotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> Status;
    fn UnregisterEventHotKey(hot_key: *mut c_void) -> Status;
}

type Reporter = Box<dyn Fn(Report) -> bool>;

/// The running hotkey: give it the spec, and again whenever the setting changes.
pub struct Hotkey {
    /// What the handler is given too: it lives as long as the program.
    report: &'static Reporter,
    /// The handler is installed (once, with the first spec).
    installed: Cell<bool>,
    /// The registered key's `EventHotKeyRef`, or null.
    held: Cell<*mut c_void>,
    last_status: Cell<Option<String>>,
}

/// `report` gets the presses and how the registration stands. Nothing is registered until
/// there is a spec.
pub fn start(report: impl Fn(Report) -> bool + Send + 'static) -> Hotkey {
    let report: Reporter = Box::new(report);
    Hotkey { report: Box::leak(Box::new(report)), installed: Cell::new(false), held: Cell::new(std::ptr::null_mut()), last_status: Cell::new(None) }
}

/// The system's call for a press of the registered key: there is one key, so any is ours.
extern "C" fn pressed(_call: *mut c_void, _event: *mut c_void, data: *mut c_void) -> Status {
    // SAFETY: `data` is the leaked reporter `set` gave at installation, never freed.
    let report = unsafe { &*(data as *const Reporter) };
    report(Report::Pressed(0));
    0
}

impl Hotkey {
    /// The key to hold from now on; an empty spec holds none. On the main thread.
    pub fn set(&self, spec: &str) {
        let held = self.held.replace(std::ptr::null_mut());
        if !held.is_null() {
            // SAFETY: the reference `RegisterEventHotKey` gave, unregistered once.
            unsafe { UnregisterEventHotKey(held) };
        }
        let status = if spec.trim().is_empty() {
            "No hotkey is set".to_string()
        } else {
            match self.register(spec) {
                Ok(()) => format!("{} shows and hides the window", pretty(spec)),
                Err(why) => format!("{}: {why:#}", pretty(spec)),
            }
        };
        let last = self.last_status.take();
        if last.as_ref() != Some(&status) {
            (self.report)(Report::Status(status.clone()));
        }
        self.last_status.set(Some(status));
    }

    fn register(&self, spec: &str) -> anyhow::Result<()> {
        validate(spec)?;
        let chord = read(spec)?;
        let mut modifiers = 0;
        for (held, mask) in [(chord.ctrl, CONTROL), (chord.shift, SHIFT), (chord.alt, OPTION), (chord.logo, CMD)] {
            if held {
                modifiers |= mask;
            }
        }
        // SAFETY: Carbon's event functions with their declared types, on the main thread. The
        // handler and its data live as long as the program; the out pointers are valid or null,
        // which the functions allow for a reference that is not wanted.
        unsafe {
            let target = GetApplicationEventTarget();
            if !self.installed.get() {
                let types = [EventTypeSpec { class: KEYBOARD, kind: HOT_KEY_PRESSED }];
                let data = self.report as *const Reporter as *mut c_void;
                let status = InstallEventHandler(target, pressed, types.len(), types.as_ptr(), data, std::ptr::null_mut());
                if status != 0 {
                    anyhow::bail!("macOS would not deliver hot keys (error {status})");
                }
                self.installed.set(true);
            }
            let mut held = std::ptr::null_mut();
            let id = EventHotKeyId { signature: u32::from_be_bytes(*b"sill"), id: 1 };
            let status = RegisterEventHotKey(u32::from(chord.mac), modifiers, id, target, 0, &mut held);
            if status != 0 {
                anyhow::bail!("macOS refused it (error {status}); another program may hold it");
            }
            self.held.set(held);
        }
        Ok(())
    }
}
