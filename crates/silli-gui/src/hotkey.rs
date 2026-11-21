//! The global show/hide key. A spec is what Settings records from a key press ([`capture`]):
//! `Alt+Space @Space`, `Super+\` @Backquote`, `F12 @F12` — modifiers, the key as the layout shows
//! it, and after `@` the physical key, which is what the system is asked for. Written by hand
//! (`SILLI_GUI_HOTKEY`) the physical key alone will do: `Ctrl+Alt+KeyT`. The setting is silli-gui's
//! own (`settings.rs`): a key belongs to the keyboard the window is at.
//!
//! Each system has its own way to hold a key, behind one face ([`start`], [`Hotkey::set`],
//! [`Report`]): a grab on the X server (`x11.rs`), Carbon's hot keys on the Mac (`carbon.rs`).
//! A held key never reaches the window, so while Settings records the key it is let go (an
//! empty spec).
use anyhow::{Context, Result, bail};
use iced::keyboard::key::{Code, Physical};
use iced::keyboard::{Key, Modifiers};

use crate::shortcuts::Capture;

#[cfg(target_os = "macos")]
mod carbon;
#[cfg(not(target_os = "macos"))]
mod x11;
#[cfg(target_os = "macos")]
pub use carbon::{Hotkey, start};
#[cfg(not(target_os = "macos"))]
pub use x11::{Hotkey, start};

/// The modifiers as this system's keyboards name them, in the order a spec lists them:
/// Control, the logo key, Alt, Shift.
const NAMES: [&str; 4] = if cfg!(target_os = "macos") { ["Control", "Cmd", "Option", "Shift"] } else { ["Ctrl", "Super", "Alt", "Shift"] };

/// The default, when the setting is unset.
pub const DEFAULT: &str = if cfg!(target_os = "macos") { "Option+Space @Space" } else { "Alt+Space @Space" };

/// What the hotkey thread tells the window.
#[derive(Debug, Clone)]
pub enum Report {
    /// The key was pressed, at this X server time (0 where there is none).
    Pressed(u32),
    /// How the grab stands, for the Settings page.
    Status(String),
}

/// The physical keys the hotkey can be on, each with its Linux input code (`KEY_*` in
/// `linux/input-event-codes.h`; an X server's keycode is that plus 8) and the Mac's virtual key
/// code (`kVK_*` in Carbon's `Events.h`), [`NONE`] where a Mac's keyboard has no such key.
/// The Mac's codes are of the ANSI keyboard; on an ISO one `Backquote` and `IntlBackslash`
/// may be each other's.
#[rustfmt::skip]
const KEYS: &[(Code, u8, u16)] = &[
    (Code::Digit1, 2, 0x12), (Code::Digit2, 3, 0x13), (Code::Digit3, 4, 0x14), (Code::Digit4, 5, 0x15),
    (Code::Digit5, 6, 0x17), (Code::Digit6, 7, 0x16), (Code::Digit7, 8, 0x1A), (Code::Digit8, 9, 0x1C),
    (Code::Digit9, 10, 0x19), (Code::Digit0, 11, 0x1D), (Code::Minus, 12, 0x1B), (Code::Equal, 13, 0x18),
    (Code::Backspace, 14, 0x33), (Code::Tab, 15, 0x30), (Code::KeyQ, 16, 0x0C), (Code::KeyW, 17, 0x0D),
    (Code::KeyE, 18, 0x0E), (Code::KeyR, 19, 0x0F), (Code::KeyT, 20, 0x11), (Code::KeyY, 21, 0x10),
    (Code::KeyU, 22, 0x20), (Code::KeyI, 23, 0x22), (Code::KeyO, 24, 0x1F), (Code::KeyP, 25, 0x23),
    (Code::BracketLeft, 26, 0x21), (Code::BracketRight, 27, 0x1E), (Code::Enter, 28, 0x24),
    (Code::KeyA, 30, 0x00), (Code::KeyS, 31, 0x01), (Code::KeyD, 32, 0x02), (Code::KeyF, 33, 0x03),
    (Code::KeyG, 34, 0x05), (Code::KeyH, 35, 0x04), (Code::KeyJ, 36, 0x26), (Code::KeyK, 37, 0x28),
    (Code::KeyL, 38, 0x25), (Code::Semicolon, 39, 0x29), (Code::Quote, 40, 0x27), (Code::Backquote, 41, 0x32),
    (Code::Backslash, 43, 0x2A), (Code::KeyZ, 44, 0x06), (Code::KeyX, 45, 0x07), (Code::KeyC, 46, 0x08),
    (Code::KeyV, 47, 0x09), (Code::KeyB, 48, 0x0B), (Code::KeyN, 49, 0x2D), (Code::KeyM, 50, 0x2E),
    (Code::Comma, 51, 0x2B), (Code::Period, 52, 0x2F), (Code::Slash, 53, 0x2C), (Code::Space, 57, 0x31),
    (Code::F1, 59, 0x7A), (Code::F2, 60, 0x78), (Code::F3, 61, 0x63), (Code::F4, 62, 0x76),
    (Code::F5, 63, 0x60), (Code::F6, 64, 0x61), (Code::F7, 65, 0x62), (Code::F8, 66, 0x64),
    (Code::F9, 67, 0x65), (Code::F10, 68, 0x6D), (Code::ScrollLock, 70, NONE), (Code::IntlBackslash, 86, 0x0A),
    (Code::F11, 87, 0x67), (Code::F12, 88, 0x6F), (Code::Home, 102, 0x73), (Code::ArrowUp, 103, 0x7E),
    (Code::PageUp, 104, 0x74), (Code::ArrowLeft, 105, 0x7B), (Code::ArrowRight, 106, 0x7C),
    (Code::End, 107, 0x77), (Code::ArrowDown, 108, 0x7D), (Code::PageDown, 109, 0x79),
    (Code::Insert, 110, 0x72), (Code::Delete, 111, 0x75), (Code::Pause, 119, NONE),
    (Code::ContextMenu, 127, 0x6E), (Code::F13, 183, 0x69), (Code::F14, 184, 0x6B), (Code::F15, 185, 0x71),
    (Code::F16, 186, 0x6A), (Code::F17, 187, 0x40), (Code::F18, 188, 0x4F), (Code::F19, 189, 0x50),
    (Code::F20, 190, 0x5A), (Code::F21, 191, NONE), (Code::F22, 192, NONE), (Code::F23, 193, NONE),
    (Code::F24, 194, NONE),
];

/// No such key on this system.
const NONE: u16 = u16::MAX;

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
    /// The key's virtual key code on a Mac, or [`NONE`].
    mac: u16,
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
    let &(code, input, mac) = KEYS.iter().find(|(code, ..)| format!("{code:?}").eq_ignore_ascii_case(name)).with_context(|| format!("unknown key {name:?}"))?;
    if cfg!(target_os = "macos") && mac == NONE {
        bail!("a Mac has no {name} key");
    }
    let mut chord = Chord { ctrl: false, alt: false, shift: false, logo: false, code, input, mac };
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => chord.ctrl = true,
            "alt" | "option" => chord.alt = true,
            "shift" => chord.shift = true,
            "super" | "win" | "cmd" => chord.logo = true,
            other => bail!("unknown modifier {other:?}"),
        }
    }
    Ok(chord)
}

/// A binding must not steal a key people type: without Control, Alt or the logo key only the
/// keys nothing types. An empty spec is no hotkey.
pub fn validate(spec: &str) -> Result<()> {
    if spec.trim().is_empty() {
        return Ok(());
    }
    let chord = read(spec)?;
    if !(chord.ctrl || chord.alt || chord.logo || bare_ok(chord.code)) {
        bail!("without {}, {} or {} this key would be swallowed everywhere you type it; hold one, or use a function key", NAMES[0], NAMES[2], NAMES[1]);
    }
    if cfg!(not(target_os = "macos")) && chord.ctrl && chord.alt && is_function(chord.code) {
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
    if !KEYS.iter().any(|(known, ..)| *known == code) {
        return Capture::Refused("This key can't be the hotkey.".into());
    }
    let mut chord = String::new();
    for (held, name) in [modifiers.control(), modifiers.logo(), modifiers.alt(), modifiers.shift()].into_iter().zip(NAMES) {
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
        assert_eq!(validate("Ctrl+Alt+F2 @F2").is_err(), cfg!(not(target_os = "macos")), "the virtual console's");
        assert!(validate("Option+Space @Space").is_ok() && validate("Cmd+Control+KeyK").is_ok(), "as a Mac names them");
        assert!(validate("Alt+X @NoSuchKey").is_err());
        assert!(validate("Hyper+Space @Space").is_err());
        assert_eq!(pretty("Super+` @Backquote"), "Super+`");
        // the label is only shown: the physical key is what is read
        assert_eq!(read("Alt+Ö @Semicolon").unwrap(), Chord { ctrl: false, alt: true, shift: false, logo: false, code: Code::Semicolon, input: 39, mac: 0x29 });
        // every key has one input code, with a keycode beside it, and one Mac code at most
        for (i, (code, input, mac)) in KEYS.iter().enumerate() {
            assert!(KEYS[..i].iter().all(|(c, n, m)| c != code && n != input && (*mac == NONE || m != mac)), "{code:?}");
            assert!(input.checked_add(8).is_some());
        }
    }

    #[test]
    fn a_press_is_recorded_as_the_physical_key() {
        use iced::keyboard::key::Named;
        let space = Key::Named(Named::Space);
        assert_eq!(capture(&space, Physical::Code(Code::Space), Modifiers::ALT), Capture::Chord(DEFAULT.into()));
        assert_eq!(
            capture(&Key::Character("`".into()), Physical::Code(Code::Backquote), Modifiers::LOGO),
            Capture::Chord(format!("{}+` @Backquote", NAMES[1]))
        );
        assert_eq!(capture(&Key::Named(Named::F12), Physical::Code(Code::F12), Modifiers::empty()), Capture::Chord("F12 @F12".into()));
        assert_eq!(capture(&Key::Named(Named::Tab), Physical::Code(Code::Tab), Modifiers::CTRL), Capture::Chord(format!("{}+Tab @Tab", NAMES[0])));
        assert_eq!(capture(&Key::Named(Named::Alt), Physical::Code(Code::AltLeft), Modifiers::ALT), Capture::Waiting);
        assert_eq!(capture(&Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::empty()), Capture::Cancel);
        assert_eq!(capture(&Key::Named(Named::Delete), Physical::Code(Code::Delete), Modifiers::empty()), Capture::Clear);
        assert!(matches!(capture(&Key::Character("a".into()), Physical::Code(Code::KeyA), Modifiers::SHIFT), Capture::Refused(_)));
        assert!(matches!(capture(&Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::ALT), Capture::Refused(_)));
    }
}
