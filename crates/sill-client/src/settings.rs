//! The Settings page's data, through public SFP only (`_sill/settings.*`, `_sill/pins.*`,
//! `_sill/memory.*`): what it reads in one go, the choices it offers and the defaults an unset
//! setting means. The choices and defaults are the daemon's (`sill-core`'s config), kept here so a
//! frontend needs no `sill-core`; a change there belongs here too.
use serde_json::{Value, json};
use sill_proto::admin::{SettingChange, Settings, SettingsPatch, Text};
use sill_proto::pins;
use sill_proto::sessions::HostPath;
use sill_proto::{Client, Empty};

/// One request: the method and its parameters.
pub type Request = (&'static str, Value);

/// Auto-revert, minutes: 0 never (`AUTO_REVERT_CHOICES`).
pub const AUTO_REVERT: &[(u32, &str)] = &[
    (0, "never"),
    (10, "10 minutes"),
    (30, "30 minutes"),
    (60, "1 hour"),
    (240, "4 hours"),
    (1440, "1 day"),
];
pub const AUTO_REVERT_DEFAULT: u32 = 1440;

/// Earlier sessions a new agent is told about.
pub const RECALL: &[(u32, &str)] = &[
    (0, "none"),
    (1, "1 session"),
    (3, "3 sessions"),
    (5, "5 sessions"),
    (10, "10 sessions"),
];
pub const RECALL_DEFAULT: u32 = 3;

/// Compaction and retention durations: the setting's text and its label, in days for ordering.
pub const DURATIONS: &[(&str, &str, u32)] = &[
    ("0", "never", 0),
    ("7d", "a week", 7),
    ("30d", "a month", 30),
    ("90d", "3 months", 90),
    ("180d", "6 months", 180),
    ("1y", "a year", 365),
    ("2y", "two years", 730),
];
/// Unset compaction, full-text and deletion durations, in days (0 = never).
pub const COMPACT_DEFAULT: u32 = 30;
pub const FULL_TEXT_DEFAULT: u32 = 365;
pub const DELETE_DEFAULT: u32 = 0;

/// The themes, by id and name, in order (sill-gui's `style::THEMES` has their palettes).
pub const THEMES: &[(&str, &str)] = &[
    ("adwaita", "Adwaita Dark"),
    ("adwaita-light", "Adwaita Light"),
    ("dracula", "Dracula"),
    ("solarized-dark", "Solarized Dark"),
    ("solarized-light", "Solarized Light"),
    ("nord", "Nord"),
    ("miasma", "Miasma"),
    ("gruvbox-dark", "Gruvbox Dark"),
    ("gruvbox-light", "Gruvbox Light"),
    ("iceberg-dark", "Iceberg Dark"),
    ("iceberg-light", "Iceberg Light"),
    ("sill-dark", "sill Dark"),
    ("sill-light", "sill Light"),
];

/// Everything the page shows.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub settings: Settings,
    pub instructions: String,
    pub global_pins: Vec<HostPath>,
    /// The time agents worked per UTC hour over the last `ACTIVE_DAYS` days; `None` when the
    /// daemon does not report it.
    pub active_hours: Option<Vec<sill_proto::admin::ActiveHour>>,
}

/// How far back the running time is read: a day more than a month of local days needs.
pub const ACTIVE_DAYS: u16 = 32;

/// Read the settings, the global instructions and the global pins.
pub fn read(client: &Client) -> anyhow::Result<Snapshot> {
    let settings: Settings = client.call("_sill/settings.get", Empty {})?;
    let instructions: Text = client.call("_sill/settings.instructions.get", Empty {})?;
    let mut global_pins = Vec::new();
    let mut after = None;
    loop {
        let page: pins::Page = client.call(
            "_sill/pins.list",
            pins::List {
                session_id: None,
                after: after.clone(),
                limit: Some(64),
            },
        )?;
        global_pins.extend(page.pins);
        anyhow::ensure!(
            page.next.is_none()
                || page.next.as_ref().map(|p| &p.bytes_base64)
                    != after.as_ref().map(|p: &HostPath| &p.bytes_base64),
            "pin listing did not advance"
        );
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let active_hours = client
        .has_capability("sill.usage")
        .then(|| {
            client.call::<sill_proto::admin::ActiveTime>(
                "_sill/usage.active_time",
                sill_proto::admin::ActiveTimeQuery { days: ACTIVE_DAYS },
            )
        })
        .and_then(Result::ok)
        .map(|time| time.hours);
    Ok(Snapshot {
        settings,
        instructions: instructions.text,
        global_pins,
        active_hours,
    })
}

/// One setting changed.
pub fn patch(change: SettingChange) -> Request {
    (
        "_sill/settings.patch",
        json!(SettingsPatch {
            changes: vec![change]
        }),
    )
}

pub fn set_instructions(text: &str) -> Request {
    (
        "_sill/settings.instructions.set",
        json!(Text { text: text.into() }),
    )
}

pub fn add_global_pin(path: HostPath) -> Request {
    ("_sill/pins.add", json!(pins::GlobalPin { path }))
}

pub fn remove_global_pin(path: HostPath) -> Request {
    ("_sill/pins.remove", json!(pins::GlobalPin { path }))
}

/// Compaction and retention now, by the durations set.
pub fn compact() -> Request {
    ("_sill/memory.compact", json!({}))
}

/// Delete every ended session (open tabs stay).
pub fn forget_ended() -> Request {
    ("_sill/memory.forget", json!({}))
}

/// A duration setting's days (`30d`, `1y`, `0`), or `default` when unset or unreadable.
pub fn days(value: Option<&str>, default: u32) -> u32 {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return default;
    };
    if value == "0" {
        return 0;
    }
    let (number, unit) = value.split_at(value.len() - 1);
    let Ok(n) = number.parse::<u32>() else {
        return default;
    };
    match unit {
        "d" => n,
        "w" => n * 7,
        "m" => n * 30,
        "y" => n * 365,
        _ => default,
    }
}

/// The choice nearest `value` in `choices` (a setting edited by hand may sit between them).
pub fn nearest<T>(choices: &[T], value: i64, of: impl Fn(&T) -> i64) -> usize {
    choices
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| (of(c) - value).abs())
        .map_or(0, |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_requests_and_choices_read_as_the_frontend_does() {
        let (method, params) = patch(SettingChange::Memory { value: Some(false) });
        assert_eq!(method, "_sill/settings.patch");
        assert_eq!(params, json!({"changes":[{"type":"memory","value":false}]}));
        assert_eq!(days(Some("1y"), 1), 365);
        assert_eq!(days(Some("0"), 30), 0);
        assert_eq!(days(None, 30), 30);
        assert_eq!(days(Some("soon"), 30), 30);
        // an hand-edited 45 days shows as the nearest choice, a month
        assert_eq!(
            DURATIONS[nearest(DURATIONS, 45, |d| d.2 as i64)].1,
            "a month"
        );
        assert_eq!(
            AUTO_REVERT[nearest(AUTO_REVERT, AUTO_REVERT_DEFAULT as i64, |c| c.0 as i64)].1,
            "1 day"
        );
    }
}
