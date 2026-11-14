//! Shelf settings file `~/.config/sill/sill.toml` (SPEC §4.9).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Colour theme id (`adwaita`, `dracula`, `solarized-dark`, `solarized-light`, `nord`).
    pub theme: String,
    /// Fraction of the work area the shelf takes (0.2–0.8).
    pub width: Option<f64>,
    /// Width of the file tree (the drawer) while it is open, logical pixels, out of the shelf's
    /// width; set by dragging the handle between the panes. Unset = 30 % of the shelf.
    pub drawer_width: Option<i32>,
    /// The drawer's view follows the repository (the shown folder's, else the session's first
    /// pinned one): the diff view while it has changes, the tree while it is clean, switched again on
    /// every change to its files; the view button still switches by hand in between. Unset = off.
    pub drawer_follow_changes: Option<bool>,
    /// Global show/hide key, GTK accelerator syntax (`<Alt>space`, `<Super>grave`, `F12`).
    pub hotkey: Option<String>,
    /// Backend page the "sill" button opens: `changes` (default), `files`, `agents`, `settings`.
    pub default_page: Option<String>,
    /// Show the session timer automatically while the agent works. Unset = off.
    pub timer_auto: Option<bool>,
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
    /// The terminal the shelf opens on the host (Ctrl+Shift+T, the shell button's right-click): a
    /// command line, `{cwd}` replaced by the directory (it is also the process's working directory).
    /// Unset = the first of [`HOST_TERMINALS`] found in `PATH`.
    pub host_terminal: Option<String>,
    /// A tray icon (StatusNotifierItem) while the shelf runs: click shows or hides it, the menu
    /// quits. Unset = on; the panel must speak the protocol (SPEC §4.6).
    pub tray: Option<bool>,
    /// A new session starts with the pins of the latest session that had any (SPEC §3.2 Pins):
    /// the workspace carries over instead of being pinned again. Unset = off.
    pub remember_pins: Option<bool>,
    /// Memory (docs/memory.md): when a session's tab closes, its agent is asked for a summary as its
    /// last turn (sill writes one from the facts when it can't be), kept with the session. Unset = on.
    pub memory: Option<bool>,
    /// How many earlier sessions a new agent is told about (docs/memory.md §2.3); unset = 3, 0 =
    /// none (search still works).
    pub memory_recall: Option<u32>,
    /// Ghost sessions read memory (docs/memory.md §3.2): the recall block and the jail's read-only
    /// snapshot, as an unpinned session gets them. They never write it either way. Unset = off.
    pub ghost_memory: Option<bool>,
    /// Compaction (docs/memory.md §2.4): this long after a session ended, its transcript loses the
    /// agent's thoughts, tool details and diffs (`30d`, `12h`, `2y`; unset = 30d, `0` = never).
    pub compact_after: Option<String>,
    /// This long after the end, the agent's text goes too: summary, notes, the person's prompts and
    /// the metadata remain (unset = 365d, `0` = never).
    pub keep_full_transcripts: Option<String>,
    /// Retention: this long after the end the session is deleted outright, as Delete does (unset
    /// or `0` = never — the point of memory is to keep).
    pub delete_after: Option<String>,
}

/// Terminals the shelf knows, in preference order: (program, command line). Programs that start
/// their shell in the inherited working directory need no flag; the others are told.
pub const HOST_TERMINALS: &[(&str, &str)] = &[
    ("x-terminal-emulator", "x-terminal-emulator"),
    ("gnome-terminal", "gnome-terminal --working-directory={cwd}"),
    ("kitty", "kitty --directory {cwd}"),
    ("alacritty", "alacritty --working-directory {cwd}"),
    ("wezterm", "wezterm start --cwd {cwd}"),
    ("konsole", "konsole --workdir {cwd}"),
    ("xfce4-terminal", "xfce4-terminal --working-directory={cwd}"),
    ("tilix", "tilix --working-directory={cwd}"),
    ("terminator", "terminator --working-directory={cwd}"),
    ("foot", "foot --working-directory={cwd}"),
    ("urxvt", "urxvt -cd {cwd}"),
    ("xterm", "xterm"),
];

/// Is `program` on `PATH`?
pub fn in_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

/// The known terminals that are installed, in preference order.
pub fn installed_terminals() -> Vec<(&'static str, &'static str)> {
    HOST_TERMINALS.iter().copied().filter(|(p, _)| in_path(p)).collect()
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
pub const PAGES: &[(&str, &str)] = &[("changes", "Changes"), ("agents", "Agents"), ("settings", "Settings")];

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

    /// Memory on unless turned off (docs/memory.md).
    pub fn memory(&self) -> bool {
        self.memory.unwrap_or(true)
    }

    /// The compaction durations in seconds; 0 = that step never runs.
    pub fn compact_after(&self) -> i64 {
        age(self.compact_after.as_deref(), 30 * 86_400)
    }
    pub fn keep_full_transcripts(&self) -> i64 {
        age(self.keep_full_transcripts.as_deref(), 365 * 86_400)
    }
    pub fn delete_after(&self) -> i64 {
        age(self.delete_after.as_deref(), 0)
    }
    /// Ghost sessions see memory (read-only) only when turned on; never when memory is off.
    pub fn ghost_memory(&self) -> bool {
        self.memory() && self.ghost_memory.unwrap_or(false)
    }

    pub fn memory_recall(&self) -> usize {
        self.memory_recall.unwrap_or(3).min(20) as usize
    }

    pub fn tray(&self) -> bool {
        self.tray.unwrap_or(true)
    }

    pub fn drawer_follow_changes(&self) -> bool {
        self.drawer_follow_changes.unwrap_or(false)
    }

    pub fn remember_pins(&self) -> bool {
        self.remember_pins.unwrap_or(false)
    }

    /// Minutes before an unkept change outside git is reverted; 0 = never.
    pub fn auto_revert_minutes(&self) -> u32 {
        self.auto_revert_minutes.unwrap_or(AUTO_REVERT_DEFAULT)
    }

    /// The factor applied to the desktop font: zoom × [`TEXT_SCALE_BASE`].
    pub fn text_scale(&self) -> f64 {
        self.zoom() * TEXT_SCALE_BASE
    }

    /// The host terminal's command line: as configured, else the first installed known one.
    pub fn host_terminal(&self) -> Option<String> {
        match self.host_terminal.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => Some(t.to_string()),
            _ => installed_terminals().first().map(|(_, cmd)| cmd.to_string()),
        }
    }

    /// A valid default page id.
    pub fn default_page(&self) -> &str {
        match self.default_page.as_deref() {
            Some(p) if PAGES.iter().any(|(id, _)| *id == p) => p,
            _ => "changes",
        }
    }
}

/// `~/.config/sill` (`$XDG_CONFIG_HOME/sill`): the settings file and the person's instructions.
pub fn dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
        .join("sill")
}

pub fn path() -> Result<PathBuf> {
    Ok(dir().join("sill.toml"))
}

pub fn load() -> Config {
    path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Load, change one thing, save. The shape every setting edit takes.
/// A duration setting (`30d`, `2y`, `0`): seconds, or `default` when unset or unreadable.
fn age(s: Option<&str>, default: i64) -> i64 {
    s.map(str::trim).filter(|s| !s.is_empty()).and_then(sill_memory::parse_age).unwrap_or(default).max(0)
}

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
