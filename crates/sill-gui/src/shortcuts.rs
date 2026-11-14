//! Editable window shortcuts. Editing/navigation keys remain owned by widgets.
use crate::{Message, layout};
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
];
pub type Bindings = std::collections::BTreeMap<String, String>;
pub fn value<'a>(bindings: &'a Bindings, index: usize) -> &'a str {
    bindings
        .get(ACTIONS[index].0)
        .map(String::as_str)
        .unwrap_or(ACTIONS[index].2)
}
#[derive(Debug, PartialEq, Eq)]
struct Chord {
    key: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
}
fn parse(value: &str) -> Result<Option<Chord>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let parts: Vec<_> = value.trim().split('+').collect();
    let mut chord = Chord {
        key: parts.last().unwrap().trim().to_lowercase(),
        ctrl: false,
        alt: false,
        shift: false,
    };
    for modifier in &parts[..parts.len() - 1] {
        match modifier.trim().to_lowercase().as_str() {
            "ctrl" => chord.ctrl = true,
            "alt" => chord.alt = true,
            "shift" => chord.shift = true,
            _ => return Err("Use Ctrl, Alt or Shift modifiers.".into()),
        }
    }
    if chord.key == "plus" || chord.key == "=" {
        chord.key = "+".into();
        chord.shift = false;
    }
    if chord.key.chars().count() != 1 || (!chord.ctrl && !chord.alt) {
        return Err(
            "Use Ctrl or Alt with a letter, number or punctuation; write Plus for +.".into(),
        );
    }
    // These belong to text editing, terminal copy/paste, or existing navigation.
    if chord.ctrl
        && !chord.alt
        && ["a", "c", "v", "x", "z", "y", "s", "q"].contains(&chord.key.as_str())
    {
        return Err("This shortcut is reserved for editing or application commands.".into());
    }
    Ok(Some(chord))
}
pub fn validate(bindings: &Bindings) -> Result<(), String> {
    let mut seen = Vec::new();
    for (i, (_, label, _)) in ACTIONS.iter().enumerate() {
        if let Some(chord) = parse(value(bindings, i)).map_err(|why| format!("{label}: {why}"))? {
            if let Some((_, other)) = seen.iter().find(|(c, _)| c == &chord) {
                return Err(format!("{label} conflicts with {other}."));
            }
            seen.push((chord, *label));
        }
    }
    Ok(())
}
pub fn message(bindings: &Bindings, key: &Key, modifiers: Modifiers) -> Option<Message> {
    if modifiers.logo() {
        return None;
    }
    let Key::Character(key) = key else {
        return None;
    };
    let key = if key == "=" {
        "+".into()
    } else {
        key.to_lowercase()
    };
    let pressed = Chord {
        shift: modifiers.shift() && key != "+",
        key,
        ctrl: modifiers.control(),
        alt: modifiers.alt(),
    };
    let index = (0..ACTIONS.len())
        .find(|i| parse(value(bindings, *i)).ok().flatten().as_ref() == Some(&pressed))?;
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
        i => Message::SelectTab(i - 10),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remapping_removes_defaults_and_survives_serialization() {
        let mut settings = crate::settings::Settings::default();
        settings
            .shortcuts
            .insert("new_session".into(), "Alt+N".into());
        settings
            .shortcuts
            .insert("close_session".into(), String::new());
        let saved = toml::to_string(&settings).unwrap();
        let back: crate::settings::Settings = toml::from_str(&saved).unwrap();
        let key = |s: &str| Key::Character(s.into());
        assert!(matches!(
            message(&back.shortcuts, &key("n"), Modifiers::ALT),
            Some(Message::NewSession)
        ));
        assert!(message(&back.shortcuts, &key("t"), Modifiers::CTRL).is_none());
        assert!(message(&back.shortcuts, &key("w"), Modifiers::CTRL).is_none());
        assert!(matches!(
            message(&Bindings::new(), &key("t"), Modifiers::CTRL),
            Some(Message::NewSession)
        ));
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
}
