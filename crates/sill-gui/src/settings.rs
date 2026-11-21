//! sill-gui's own settings, `~/.config/sill/sill-gui.toml`: the window's modes and sizes, and the
//! Agents page's per-agent version choice. A file of its own, not a table in `sill.toml`: the
//! daemon writes that file from its own settings and would drop a table it does not know.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub settings_category: crate::settings_ui::Category,
    pub shortcuts: crate::shortcuts::Bindings,
    /// The global show/hide key (`hotkey.rs`): unset = its default, empty = none.
    pub hotkey: Option<String>,
    /// Stage edits for review in sessions started by choosing an agent.
    pub always_apply: bool,
    /// Shelf mode's width as a share of the main display's usable width (0.15–0.4), on displays
    /// without one of their own (`displays`). Unset = 35 %.
    pub shelf_width: Option<f32>,
    /// The mode the window was last in: `shelf`, `drawer` or `full`.
    pub mode: Option<String>,
    /// The display Drawer and Full mode were left on, by name (`DP-1`): where they come back.
    /// Unset = the main display.
    pub display: Option<String>,
    /// What each display keeps, by name: written as the person resizes and moves the window
    /// (`geometry.rs`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub displays: BTreeMap<String, DisplayPrefs>,
    /// The drawer's width as last dragged, logical pixels.
    pub drawer_width: Option<f32>,
    /// The window stays above others. Unset = on, except on the Mac, where sill is a normal
    /// window (the person, 2026-11-13).
    pub always_on_top: Option<bool>,
    /// Interface scale, a multiplier on the desktop's own scaling: 1.0 is the desktop's normal
    /// size. Unset, unreadable or out of bounds = 1.0.
    pub interface_scale: Option<f32>,
    /// Remote boxes pinned to the `+` menu, by their saved name (`boxes.json`), in pinning
    /// order. Shared by every window; a name no longer saved is not shown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pinned_boxes: Vec<String>,
}

/// One display's window: the Shelf's width and the free window's (Drawer and Full) size and place,
/// all shares of the display's usable area — device pixels over device pixels, so Interface scale
/// never enters. Unset = Shelf width's default; half the width, the full height, at the top-left.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayPrefs {
    pub shelf_width: Option<f32>,
    pub work_width: Option<f32>,
    pub work_height: Option<f32>,
    pub work_x: Option<f32>,
    pub work_y: Option<f32>,
}

/// Interface scale's presets in Settings; 1.0 is the desktop's normal size.
pub const SCALES: &[f32] = &[0.75, 0.9, 1.0, 1.1, 1.25, 1.5];
/// The bounds a scale is held to (a hand-edited file included).
const SCALE_BOUNDS: (f32, f32) = (0.5, 2.0);

impl Settings {
    pub fn always_on_top(&self) -> bool {
        self.always_on_top.unwrap_or(!cfg!(target_os = "macos"))
    }

    /// Interface scale: the saved value, 1.0 when there is none or it is not a usable number.
    pub fn scale(&self) -> f32 {
        valid_scale(self.interface_scale.unwrap_or(1.0))
    }

    /// Shelf mode's share of the main display's width, where the display has none of its own.
    pub fn shelf_share(&self) -> f32 {
        shelf_share(self.shelf_width).unwrap_or(crate::layout::SHELF_SHARE)
    }

    /// Shelf mode's share of `display`'s width: its own, else the default.
    pub fn shelf_share_on(&self, display: &str) -> f32 {
        self.displays.get(display).and_then(|prefs| shelf_share(prefs.shelf_width)).unwrap_or_else(|| self.shelf_share())
    }
}

/// A kept Shelf share, held to 15–40 % (a hand-edited file included); `None` when it is no number.
fn shelf_share(share: Option<f32>) -> Option<f32> {
    use crate::layout::{SHELF_MAX, SHELF_MIN};
    share.filter(|s| s.is_finite()).map(|s| s.clamp(SHELF_MIN, SHELF_MAX))
}

/// The preset `step` places from `scale` (the nearest preset when it is between two), held to
/// the ends; step 0 is 100 %.
pub fn step_scale(scale: f32, step: i32) -> f32 {
    if step == 0 {
        return 1.0;
    }
    let at = (0..SCALES.len()).min_by(|a, b| (SCALES[*a] - scale).abs().total_cmp(&(SCALES[*b] - scale).abs())).unwrap_or(2) as i32;
    SCALES[(at + step).clamp(0, SCALES.len() as i32 - 1) as usize]
}

/// `scale` when it is a finite number within bounds, else 1.0.
pub fn valid_scale(scale: f32) -> f32 {
    if scale.is_finite() && (SCALE_BOUNDS.0..=SCALE_BOUNDS.1).contains(&scale) { scale } else { 1.0 }
}

fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("sill").join("sill-gui.toml"))
}

/// The settings, or the defaults when there are none or they cannot be read.
pub fn load() -> Settings {
    let mut settings: Settings = path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|text| toml::from_str(&text).ok()).unwrap_or_default();
    if crate::shortcuts::validate(&settings.shortcuts).is_err() {
        settings.shortcuts.clear();
    }
    if settings.hotkey.as_deref().is_some_and(|spec| crate::hotkey::validate(spec).is_err()) {
        settings.hotkey = None;
    }
    settings
}

/// Write the settings; a failure is reported on stderr, the window goes on.
pub fn save(settings: &Settings) {
    let Some(path) = path() else { return };
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(settings).map_err(std::io::Error::other)?;
        // whole or not at all: write beside it, then move it into place
        let partial = path.with_extension("toml.partial");
        std::fs::write(&partial, text)?;
        std::fs::rename(&partial, &path)
    })();
    if let Err(error) = result {
        eprintln!("sill-gui: cannot save {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_and_the_shelf_share_stays_sensible() {
        let mut settings = Settings { shelf_width: Some(0.9), mode: Some("drawer".into()), display: Some("eDP-1".into()), ..Settings::default() };
        settings.displays.insert("DP-1".into(), DisplayPrefs { shelf_width: Some(0.3), work_width: Some(0.6), ..DisplayPrefs::default() });
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("[displays.DP-1]"), "{text}");
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.mode.as_deref(), Some("drawer"));
        assert_eq!(back.display.as_deref(), Some("eDP-1"));
        assert_eq!(back.displays, settings.displays);
        assert_eq!(back.shelf_share(), 0.4, "a share past 40 % is no shelf");
        assert_eq!(back.shelf_share_on("DP-1"), 0.3, "the display's own");
        assert_eq!(back.shelf_share_on("HDMI-1"), 0.4, "else the default");
        assert_eq!(Settings::default().shelf_share(), 0.35);
        // the single-monitor keys before 2026-11-07 are dropped, not read
        let old: Settings = toml::from_str("window_x = 300\nwindow_width_share = 0.5\nmode = \"full\"").unwrap();
        assert_eq!(old.mode.as_deref(), Some("full"));
        assert!(old.displays.is_empty());
        assert!(!toml::to_string_pretty(&old).unwrap().contains("window_x"));
    }

    #[test]
    fn interface_scale_defaults_to_the_desktop_size_and_is_bounded() {
        assert_eq!(Settings::default().scale(), 1.0, "an existing file keeps today's size");
        let at = |v: f32| Settings { interface_scale: Some(v), ..Settings::default() }.scale();
        assert_eq!(at(1.25), 1.25);
        for broken in [f32::NAN, f32::INFINITY, 0.0, -1.0, 9.0] {
            assert_eq!(at(broken), 1.0, "{broken}");
        }
        let back: Settings = toml::from_str("interface_scale = 1.1").unwrap();
        assert_eq!(back.scale(), 1.1);
        let wrong: Settings = toml::from_str("interface_scale = \"big\"").unwrap_or_default();
        assert_eq!(wrong.scale(), 1.0);
        assert!(SCALES.contains(&1.0) && SCALES.iter().all(|s| valid_scale(*s) == *s));
    }

    #[test]
    fn the_shortcuts_step_through_the_presets() {
        assert_eq!(step_scale(1.0, 1), 1.1);
        assert_eq!(step_scale(1.0, -1), 0.9);
        assert_eq!(step_scale(1.5, 1), 1.5, "held at the largest");
        assert_eq!(step_scale(0.75, -1), 0.75, "and the smallest");
        assert_eq!(step_scale(1.2, 1), 1.5, "from between two, the nearest is the start");
        assert_eq!(step_scale(1.25, 0), 1.0);
    }
}
