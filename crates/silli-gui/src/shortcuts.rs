//! Editable window shortcuts. Editing/navigation keys remain owned by widgets. `Ctrl` in a
//! shortcut is the platform's command modifier ([`crate::keys::PRIMARY`]): Cmd on macOS, where
//! `Cmd` may be written too.
//!
//! A binding is a string in one of two forms. A written one, `Ctrl+Shift+T` (the defaults), is
//! matched by the character the key types, so it follows the keyboard layout. A recorded one,
//! `Alt+N @KeyN` (Settings records it from a key press, [`capture`]), names the physical key
//! after `@` and is matched by it: the key pressed while recording is the key that fires, also
//! where a modifier changes what the key types (Option+N on a Mac types `˜`). The part before
//! `@` is what the page shows.
use crate::{Message, layout};
use iced::keyboard::key::{Code, Named, Physical};
use iced::keyboard::{Key, Modifiers};

pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("new_session", "New session", "Ctrl+T"),
    ("close_session", "Close session", "Ctrl+W"),
    ("previous_tab", "Previous tab", "Ctrl+,"),
    ("next_tab", "Next tab", "Ctrl+."),
    ("terminal", "Toggle terminal", "Ctrl+Shift+T"),
    ("full", "Toggle full layout", "Ctrl+Shift+F"),
    ("shelf", "Toggle shelf layout", "Ctrl+Shift+D"),
    ("larger", "Increase interface scale", "Ctrl+Plus"),
    ("smaller", "Decrease interface scale", "Ctrl+-"),
    ("reset_scale", "Reset interface scale", "Ctrl+0"),
    ("tab_1", "Tab 1", "Ctrl+1"),
    ("tab_2", "Tab 2", "Ctrl+2"),
    ("tab_3", "Tab 3", "Ctrl+3"),
    ("tab_4", "Tab 4", "Ctrl+4"),
    ("tab_5", "Tab 5", "Ctrl+5"),
    ("tab_6", "Tab 6", "Ctrl+6"),
    ("tab_7", "Tab 7", "Ctrl+7"),
    ("tab_8", "Tab 8", "Ctrl+8"),
    ("tab_9", "Tab 9", "Ctrl+9"),
    ("host_shell", "Toggle host shell", "Ctrl+Shift+H"),
    ("find", "Find", "Ctrl+F"),
];
pub type Bindings = std::collections::BTreeMap<String, String>;
pub fn value(bindings: &Bindings, index: usize) -> &str {
    bindings.get(ACTIONS[index].0).map(String::as_str).unwrap_or(ACTIONS[index].2)
}

/// The other platform modifier, as a shortcut names it: Control on macOS (where `Ctrl` means
/// Cmd), Super elsewhere.
const OTHER: &str = if cfg!(target_os = "macos") { "Control" } else { "Super" };

/// The physical keys a shortcut can be recorded on: letters, digits, punctuation and function
/// keys. Editing and navigation keys stay with the widgets.
#[rustfmt::skip]
const CODES: &[Code] = &[
    Code::KeyA, Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyE, Code::KeyF, Code::KeyG, Code::KeyH, Code::KeyI,
    Code::KeyJ, Code::KeyK, Code::KeyL, Code::KeyM, Code::KeyN, Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR,
    Code::KeyS, Code::KeyT, Code::KeyU, Code::KeyV, Code::KeyW, Code::KeyX, Code::KeyY, Code::KeyZ,
    Code::Digit0, Code::Digit1, Code::Digit2, Code::Digit3, Code::Digit4, Code::Digit5, Code::Digit6, Code::Digit7,
    Code::Digit8, Code::Digit9,
    Code::Minus, Code::Equal, Code::BracketLeft, Code::BracketRight, Code::Backslash, Code::Semicolon, Code::Quote,
    Code::Backquote, Code::Comma, Code::Period, Code::Slash, Code::IntlBackslash,
    Code::F1, Code::F2, Code::F3, Code::F4, Code::F5, Code::F6, Code::F7, Code::F8, Code::F9, Code::F10, Code::F11,
    Code::F12,
];
fn is_function(code: Code) -> bool {
    CODES[CODES.len() - 12..].contains(&code)
}

#[derive(Debug, PartialEq, Eq)]
struct Chord {
    /// What the key types, lowercase; for a recorded chord only its label.
    key: String,
    /// The physical key of a recorded chord.
    code: Option<Code>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    other: bool,
}
impl Chord {
    /// The chord as a written one would say it: `=` and `+` are one key, with or without Shift.
    fn typed(&self) -> (&str, bool, bool, bool, bool) {
        let key = if self.key == "=" { "+" } else { &self.key };
        (key, self.ctrl, self.alt, self.shift && key != "+", self.other)
    }
    /// Whether one press could mean both.
    fn clashes(&self, with: &Chord) -> bool {
        self.typed() == with.typed()
            || (self.code.is_some() && self.code == with.code && (self.ctrl, self.alt, self.shift, self.other) == (with.ctrl, with.alt, with.shift, with.other))
    }
}
fn parse(value: &str) -> Result<Option<Chord>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let (written, code) = match value.rsplit_once('@') {
        Some((written, name)) => {
            let code = CODES.iter().copied().find(|code| format!("{code:?}").eq_ignore_ascii_case(name.trim()));
            (written, Some(code.ok_or_else(|| format!("Unknown key {:?}.", name.trim()))?))
        }
        None => (value, None),
    };
    let parts: Vec<_> = written.trim().split('+').collect();
    let mut chord = Chord { key: parts.last().unwrap().trim().to_lowercase(), code, ctrl: false, alt: false, shift: false, other: false };
    for modifier in &parts[..parts.len() - 1] {
        match modifier.trim().to_lowercase().as_str() {
            "ctrl" => chord.ctrl = true,
            "cmd" | "command" if cfg!(target_os = "macos") => chord.ctrl = true,
            "alt" => chord.alt = true,
            "shift" => chord.shift = true,
            other if other.eq_ignore_ascii_case(OTHER) => chord.other = true,
            _ => return Err(format!("Use {}, Alt, Shift or {OTHER} modifiers.", crate::keys::PRIMARY)),
        }
    }
    if chord.key == "plus" {
        chord.key = "+".into();
    }
    let modified = chord.ctrl || chord.alt || chord.other;
    match chord.code {
        Some(code) if !modified && !is_function(code) => {
            return Err(format!("Hold {}, Alt or {OTHER} with the key; only function keys work alone.", crate::keys::PRIMARY));
        }
        Some(_) => {}
        None => {
            if chord.key == "=" || chord.key == "+" {
                chord.key = "+".into();
                chord.shift = false;
            }
            if chord.key.chars().count() != 1 || !modified {
                return Err(format!("Use {} or Alt with a letter, number or punctuation; write Plus for +.", crate::keys::PRIMARY));
            }
        }
    }
    // These belong to text editing, terminal copy/paste, or existing navigation.
    if chord.ctrl && !chord.alt && !chord.other && ["a", "c", "v", "x", "z", "y", "s", "q"].contains(&chord.key.as_str()) {
        return Err("This shortcut is reserved for editing or application commands.".into());
    }
    Ok(Some(chord))
}
pub fn validate(bindings: &Bindings) -> Result<(), String> {
    let mut seen: Vec<(Chord, &str)> = Vec::new();
    for (i, (_, label, _)) in ACTIONS.iter().enumerate() {
        if let Some(chord) = parse(value(bindings, i)).map_err(|why| format!("{label}: {why}"))? {
            if let Some((_, other)) = seen.iter().find(|(c, _)| c.clashes(&chord)) {
                return Err(format!("{label} conflicts with {other}."));
            }
            seen.push((chord, *label));
        }
    }
    Ok(())
}

/// A binding as the page shows it: without the physical key, in the platform's words.
pub fn shown(value: &str) -> String {
    crate::keys::label(value.rsplit_once('@').map_or(value, |(written, _)| written).trim())
}

/// What a key press means to a field that records a shortcut.
#[derive(Debug, PartialEq, Eq)]
pub enum Capture {
    /// A modifier went down: the chord is not complete.
    Waiting,
    /// Escape: leave the binding as it was.
    Cancel,
    /// Backspace or Delete: the action gets no shortcut.
    Clear,
    /// The binding to store.
    Chord(String),
    /// A key no shortcut can be on, and why.
    Refused(String),
}

/// What the recorder makes of a press whatever is recorded: its own keys, and modifiers.
pub fn recorder_key(key: &Key, modifiers: Modifiers) -> Option<Capture> {
    Some(match key {
        Key::Named(Named::Escape) if modifiers.is_empty() => Capture::Cancel,
        Key::Named(Named::Backspace | Named::Delete) if modifiers.is_empty() => Capture::Clear,
        Key::Named(
            Named::Shift
            | Named::Control
            | Named::Alt
            | Named::AltGraph
            | Named::Super
            | Named::Meta
            | Named::Hyper
            | Named::CapsLock
            | Named::NumLock
            | Named::Fn
            | Named::FnLock,
        ) => Capture::Waiting,
        _ => return None,
    })
}

/// What a key types on this layout, where that can be written in a binding; else its name.
pub fn key_label(key: &Key, code: Code) -> String {
    match key {
        Key::Character(c) if c == "+" => "Plus".into(),
        Key::Character(c) if c.chars().count() == 1 && !c.contains(|c: char| c.is_whitespace() || c.is_control() || c == '@') => c.to_uppercase(),
        _ => format!("{code:?}"),
    }
}

/// Read a press for the recorder. `key` is the key without its modifiers (iced's `key`).
pub fn capture(key: &Key, physical: Physical, modifiers: Modifiers) -> Capture {
    if let Some(early) = recorder_key(key, modifiers) {
        return early;
    }
    let Physical::Code(code) = physical else {
        return Capture::Refused("This key is not known to the system by its place on the keyboard.".into());
    };
    if !CODES.contains(&code) {
        return Capture::Refused("Use a letter, number, punctuation or function key.".into());
    }
    let mut chord = String::new();
    let other = if cfg!(target_os = "macos") { modifiers.control() } else { modifiers.logo() };
    for (held, name) in [(modifiers.command(), "Ctrl"), (other, OTHER), (modifiers.alt(), "Alt"), (modifiers.shift(), "Shift")] {
        if held {
            chord.push_str(name);
            chord.push('+');
        }
    }
    chord.push_str(&key_label(key, code));
    Capture::Chord(format!("{chord} @{code:?}"))
}

/// What a press means with these bindings, for a caller that has no physical key.
#[cfg(test)]
pub fn message(bindings: &Bindings, key: &Key, modifiers: Modifiers) -> Option<Message> {
    message_at(bindings, key, Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified), modifiers)
}

pub fn message_at(bindings: &Bindings, key: &Key, physical: Physical, modifiers: Modifiers) -> Option<Message> {
    // the other platform modifier: Super elsewhere, Ctrl on macOS
    let other = if cfg!(target_os = "macos") { modifiers.control() } else { modifiers.logo() };
    let held = (modifiers.command(), modifiers.alt(), modifiers.shift(), other);
    let typed = match key {
        Key::Character(key) => Some(key.to_lowercase()),
        _ => None,
    };
    let code = match physical {
        Physical::Code(code) => Some(code),
        Physical::Unidentified(_) => None,
    };
    let pressed = |chord: &Chord| match (chord.code, code) {
        // a recorded chord is the physical key
        (Some(recorded), Some(code)) => recorded == code && held == (chord.ctrl, chord.alt, chord.shift, chord.other),
        // a press without one (some input methods): by what it types
        (Some(_), None) => typed.as_deref() == Some(&chord.key) && held == (chord.ctrl, chord.alt, chord.shift, chord.other),
        (None, _) => {
            let key = if typed.as_deref() == Some("=") { Some("+") } else { typed.as_deref() };
            key == Some(&chord.key) && (held.0, held.1, held.2 && key != Some("+"), held.3) == (chord.ctrl, chord.alt, chord.shift, chord.other)
        }
    };
    let index = (0..ACTIONS.len()).find(|i| parse(value(bindings, *i)).ok().flatten().as_ref().is_some_and(&pressed))?;
    Some(match index {
        0 => Message::NewSession,
        1 => Message::CloseCurrent,
        2 => Message::StepTab(-1),
        3 => Message::StepTab(1),
        4 => Message::ToggleTerminal,
        5 => Message::Layout(layout::Action::ToggleFull),
        6 => Message::Layout(layout::Action::ToggleShelf),
        7 => Message::ScaleStep(1),
        8 => Message::ScaleStep(-1),
        9 => Message::ScaleStep(0),
        19 => Message::ToggleHostTerminal,
        20 => Message::Find(crate::find::Event::Open),
        i => Message::SelectTab(i - 10),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remapping_removes_defaults_and_survives_serialization() {
        let mut settings = crate::settings::Settings::default();
        settings.shortcuts.insert("new_session".into(), "Alt+N".into());
        settings.shortcuts.insert("close_session".into(), String::new());
        let saved = toml::to_string(&settings).unwrap();
        let back: crate::settings::Settings = toml::from_str(&saved).unwrap();
        let key = |s: &str| Key::Character(s.into());
        assert!(matches!(message(&back.shortcuts, &key("n"), Modifiers::ALT), Some(Message::NewSession)));
        assert!(message(&back.shortcuts, &key("t"), Modifiers::COMMAND).is_none());
        assert!(message(&back.shortcuts, &key("w"), Modifiers::COMMAND).is_none());
        assert!(matches!(message(&Bindings::new(), &key("t"), Modifiers::COMMAND), Some(Message::NewSession)));
    }
    #[test]
    fn conflicts_include_defaults_and_scale_aliases() {
        let mut bindings = Bindings::new();
        assert!(validate(&bindings).is_ok());
        bindings.insert("new_session".into(), "ctrl+w".into());
        assert!(validate(&bindings).unwrap_err().contains("conflicts"));
        bindings.insert("close_session".into(), "Ctrl+T".into());
        assert!(validate(&bindings).is_ok(), "swapping together is allowed");
        bindings.insert("new_session".into(), "Ctrl+Shift+=".into());
        assert!(validate(&bindings).is_err());
        for value in ["T", "Ctrl+C", "Ctrl+Shift+V", "Ctrl+Left", "Bogus+T"] {
            bindings.insert("new_session".into(), value.into());
            assert!(validate(&bindings).is_err(), "{value}");
        }
    }
    #[test]
    fn a_recorded_chord_is_the_physical_key() {
        let press = |c: &str| Key::Character(c.into());
        // Option+N on a Mac types a dead tilde; the recorder is given the key without modifiers
        assert_eq!(capture(&press("n"), Physical::Code(Code::KeyN), Modifiers::ALT), Capture::Chord("Alt+N @KeyN".into()));
        assert_eq!(capture(&press("="), Physical::Code(Code::Equal), Modifiers::COMMAND | Modifiers::SHIFT), Capture::Chord("Ctrl+Shift+= @Equal".into()));
        assert_eq!(capture(&press("+"), Physical::Code(Code::Minus), Modifiers::COMMAND), Capture::Chord("Ctrl+Plus @Minus".into()));
        assert_eq!(capture(&Key::Named(Named::F5), Physical::Code(Code::F5), Modifiers::empty()), Capture::Chord("F5 @F5".into()));
        assert_eq!(capture(&Key::Unidentified, Physical::Code(Code::Equal), Modifiers::ALT), Capture::Chord("Alt+Equal @Equal".into()));
        assert_eq!(capture(&Key::Named(Named::Shift), Physical::Code(Code::ShiftLeft), Modifiers::SHIFT), Capture::Waiting);
        assert_eq!(capture(&Key::Named(Named::Escape), Physical::Code(Code::Escape), Modifiers::empty()), Capture::Cancel);
        assert_eq!(capture(&Key::Named(Named::Backspace), Physical::Code(Code::Backspace), Modifiers::empty()), Capture::Clear);
        assert!(matches!(capture(&Key::Named(Named::ArrowLeft), Physical::Code(Code::ArrowLeft), Modifiers::COMMAND), Capture::Refused(_)));

        let bindings = Bindings::from([("new_session".into(), "Alt+N @KeyN".into())]);
        assert!(validate(&bindings).is_ok());
        assert_eq!(shown("Alt+N @KeyN"), "Alt+N");
        // whatever the key types with the modifier held, the place decides
        assert!(matches!(message_at(&bindings, &press("˜"), Physical::Code(Code::KeyN), Modifiers::ALT), Some(Message::NewSession)));
        assert!(message_at(&bindings, &press("n"), Physical::Code(Code::KeyB), Modifiers::ALT).is_none());
        assert!(message_at(&bindings, &press("n"), Physical::Code(Code::KeyN), Modifiers::ALT | Modifiers::SHIFT).is_none());
        // a press that names no physical key falls back to what it types
        assert!(matches!(message(&bindings, &press("n"), Modifiers::ALT), Some(Message::NewSession)));
        // the default it replaced is free, the written defaults still follow the layout
        assert!(message_at(&bindings, &press("t"), Physical::Code(Code::KeyT), Modifiers::COMMAND).is_none());
        assert!(matches!(message_at(&bindings, &press("w"), Physical::Code(Code::Comma), Modifiers::COMMAND), Some(Message::CloseCurrent)));
    }
    #[test]
    fn recorded_chords_are_validated_against_written_ones() {
        let with = |value: &str| validate(&Bindings::from([("new_session".into(), value.to_string())]));
        assert!(with("F5 @F5").is_ok());
        assert!(with("Ctrl+Alt+Ö @Semicolon").is_ok());
        assert!(with("Ctrl+Shift+T @KeyT").unwrap_err().contains("conflicts"), "Toggle terminal has it");
        assert!(with("Ctrl+= @Equal").unwrap_err().contains("conflicts"), "the written Ctrl+Plus takes = too");
        assert!(with("T @KeyT").unwrap_err().contains("function keys"));
        assert!(with("Ctrl+C @KeyC").unwrap_err().contains("reserved"));
        assert!(with("Ctrl+T @Enter").unwrap_err().contains("Unknown key"));
        let both = Bindings::from([("new_session".into(), "Alt+N @KeyN".into()), ("close_session".into(), "Alt+N @KeyN".into())]);
        assert!(validate(&both).unwrap_err().contains("conflicts"));
    }
}
