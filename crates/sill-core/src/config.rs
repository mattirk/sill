//! Shelf settings file `~/.config/sill/sill.toml` (SPEC §4.9).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Colour theme id (`adwaita`, `dracula`, `solarized-dark`, `solarized-light`, `nord`).
    pub theme: String,
    /// Fraction of the work area the shelf takes (0.2–0.8).
    pub width: Option<f64>,
    /// Global show/hide key, GTK accelerator syntax (`<Alt>space`, `<Super>grave`, `F12`).
    pub hotkey: Option<String>,
    /// Backend page the "sill" button opens: `changes` (default), `files`, `agents`, `settings`.
    pub default_page: Option<String>,
    /// Zoom: a factor on the shelf's normal text size (1.0 = Normal, the default). The shelf's
    /// normal size is [`TEXT_SCALE_BASE`] × the desktop font — GTK scales that font for HiDPI first,
    /// so a factor means the same on a 1× and a 2× display. The Settings page offers
    /// [`TEXT_SCALES`]; any value in [`TEXT_SCALE_RANGE`] is accepted when edited by hand.
    pub text_scale: Option<f64>,
    /// Auto-revert (SPEC §4.4): a change the agent made outside any git repository and outside a
    /// pinned folder is reverted this many minutes after its last write, once the session is idle.
    /// 0 = never; unset = [`AUTO_REVERT_DEFAULT`].
    pub auto_revert_minutes: Option<u32>,
    /// Desktop access for new sessions (SPEC §4.2): give the jail the X11 display / the D-Bus session
    /// bus. Unset = the policy decides (`[jail] x11` / `dbus`). A session can override both in its
    /// session menu (stored per session).
    pub desktop_x11: Option<bool>,
    pub desktop_dbus: Option<bool>,
}

/// The per-session desktop choices (stored in `sessions.desktop`): (id, label).
pub const DESKTOP_CHOICES: &[(&str, &str)] = &[("", "default"), ("none", "no desktop"), ("x11", "X11"), ("x11+dbus", "X11 + D-Bus")];

/// What a session's jail gets: (x11, dbus); `None` = the policy decides. The session's own choice
/// wins, then the global default from `sill.toml`.
pub fn desktop_grants(session_desktop: &str, c: &Config) -> (Option<bool>, Option<bool>) {
    match session_desktop {
        "none" => (Some(false), Some(false)),
        "x11" => (Some(true), Some(false)),
        "x11+dbus" => (Some(true), Some(true)),
        _ => (c.desktop_x11, c.desktop_dbus),
    }
}

/// Auto-revert delays the Settings page offers: (minutes, label).
pub const AUTO_REVERT_CHOICES: &[(u32, &str)] =
    &[(0, "Never"), (10, "10 minutes"), (30, "30 minutes"), (60, "1 hour"), (240, "4 hours"), (1440, "1 day")];
pub const AUTO_REVERT_DEFAULT: u32 = 1440;

/// The backend pages, in tab order: (id, title).
pub const PAGES: &[(&str, &str)] = &[("changes", "Changes"), ("files", "Files"), ("agents", "Agents"), ("settings", "Settings")];

/// Text sizes the Settings page offers: (factor, label). Hand-edited values in between are kept.
pub const TEXT_SCALES: &[(f64, &str)] = &[
    (0.85, "Smaller"),
    (0.92, "Small"),
    (1.0, "Normal"),
    (1.15, "Large"),
    (1.3, "Larger"),
];

/// Accepted `text_scale` values; anything outside (or unparsable) falls back to 1.0 (Normal).
pub const TEXT_SCALE_RANGE: (f64, f64) = (0.5, 1.8);
/// The shelf's normal text size as a factor of the desktop font: what zoom 1.0 means
/// (2026-10-30: 1.15 × 1.10 — the former Large, then one more step up).
pub const TEXT_SCALE_BASE: f64 = 1.265;

impl Config {
    /// The zoom as set (1.0 = Normal), the value the Settings page shows and saves.
    pub fn zoom(&self) -> f64 {
        match self.text_scale {
            Some(s) if s.is_finite() && s >= TEXT_SCALE_RANGE.0 && s <= TEXT_SCALE_RANGE.1 => s,
            _ => 1.0,
        }
    }

    /// Minutes before an unkept change outside git is reverted; 0 = never.
    pub fn auto_revert_minutes(&self) -> u32 {
        self.auto_revert_minutes.unwrap_or(AUTO_REVERT_DEFAULT)
    }

    /// The factor applied to the desktop font: zoom × [`TEXT_SCALE_BASE`].
    pub fn text_scale(&self) -> f64 {
        self.zoom() * TEXT_SCALE_BASE
    }

    /// A valid default page id.
    pub fn default_page(&self) -> &str {
        match self.default_page.as_deref() {
            Some(p) if PAGES.iter().any(|(id, _)| *id == p) => p,
            _ => "changes",
        }
    }
}

pub fn path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    Ok(base.join("sill").join("sill.toml"))
}

pub fn load() -> Config {
    path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Load, change one thing, save. The shape every setting edit takes.
pub fn update(f: impl FnOnce(&mut Config)) -> Result<()> {
    let mut c = load();
    f(&mut c);
    save(&c)
}

pub fn save(c: &Config) -> Result<()> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, toml::to_string_pretty(c)?).with_context(|| format!("write {}", p.display()))
}
