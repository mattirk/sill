//! The shared settings file `~/.config/sill/sill.toml` (SPEC §4.9): what the daemon and sill-gui
//! read, edited on sill-gui's Settings page through the daemon (`_sill/settings.*`). sill-gui's
//! own window settings are in `sill-gui.toml` beside it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Exact hostnames whose network refusal banners are hidden across sessions.
    pub network_quiet_hosts: Vec<String>,
    /// Colour theme id (`adwaita`, `dracula`, `nord`, `gruvbox-dark`, `iceberg-light`, …: sill-gui's `style::THEMES`).
    pub theme: String,
    /// The drawer's view follows the repository (the shown folder's, else the session's first
    /// pinned one): the diff view while it has changes, the tree while it is clean, switched again on
    /// every change to its files; the view button still switches by hand in between. Unset = off.
    pub drawer_follow_changes: Option<bool>,
    /// Backend page the herring opens: `changes` (default), `agents`, `settings`.
    pub default_page: Option<String>,
    /// Show the session timer automatically while the agent works. Unset = off.
    pub timer_auto: Option<bool>,
    /// Auto-revert (SPEC §4.4): a change the agent made outside any git repository and outside a
    /// pinned folder is reverted this many minutes after its last write, once the session is idle.
    /// 0 = never; unset = [`AUTO_REVERT_DEFAULT`].
    pub auto_revert_minutes: Option<u32>,
    /// A tray icon (StatusNotifierItem) while sill-gui runs: click shows or hides it, the menu
    /// quits. Unset = on; the panel must speak the protocol (SPEC §4.6).
    pub tray: Option<bool>,
    /// A new session starts with the pins of the latest session that had any (SPEC §3.2 Pins):
    /// the workspace carries over instead of being pinned again. Unset = off.
    pub remember_pins: Option<bool>,
    /// Every new session works in workspaces for its pinned repositories; the `+` menu's button
    /// beside the ghost makes the other kind (docs/parallel-coding.md §12). Unset = off.
    pub automatic_workspaces: Option<bool>,
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

/// Auto-revert when it was never set: a day, in minutes.
pub const AUTO_REVERT_DEFAULT: u32 = 1440;

/// The backend pages, in tab order: (id, title).
pub const PAGES: &[(&str, &str)] = &[("changes", "Changes"), ("agents", "Agents"), ("settings", "Settings")];

impl Config {
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

    pub fn automatic_workspaces(&self) -> bool {
        self.automatic_workspaces.unwrap_or(false)
    }

    pub fn remember_pins(&self) -> bool {
        self.remember_pins.unwrap_or(false)
    }

    /// Minutes before an unkept change outside git is reverted; 0 = never.
    pub fn auto_revert_minutes(&self) -> u32 {
        self.auto_revert_minutes.unwrap_or(AUTO_REVERT_DEFAULT)
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
    path().ok().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

/// A duration setting (`30d`, `2y`, `0`): seconds, or `default` when unset or unreadable.
fn age(s: Option<&str>, default: i64) -> i64 {
    s.map(str::trim).filter(|s| !s.is_empty()).and_then(sill_memory::parse_age).unwrap_or(default).max(0)
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
