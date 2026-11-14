//! sill-gui's own settings, `~/.config/sill/sill-gui.toml`: the window's modes and sizes. A file of
//! its own, not a table in `sill.toml`: the GTK shelf and the daemon write that file from their own
//! settings and would drop a table they do not know.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub shortcuts: crate::shortcuts::Bindings,
    /// Shelf mode's width, as a share of the monitor's (0.15–0.4). Unset = 35 %.
    pub shelf_width: Option<f32>,
    /// The mode the window was last in: `shelf`, `drawer` or `full`.
    pub mode: Option<String>,
    /// The working size (Drawer and Full), logical pixels, as the window last had it.
    pub work_width: Option<f32>,
    /// Window dimensions as fractions of the primary working area.
    pub window_width_share: Option<f32>,
    pub window_height_share: Option<f32>,
    pub work_height: Option<f32>,
    /// The drawer's width as last dragged, logical pixels.
    pub drawer_width: Option<f32>,
    /// The window stays above others. Unset = on, as the GTK shelf.
    pub always_on_top: Option<bool>,
    /// Where the window was left in Drawer and Full mode, device pixels (Shelf mode is always on
    /// its edge). Unset = the top-left of the working area.
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
    /// Interface scale, a multiplier on the desktop's own scaling: 1.0 is the desktop's normal
    /// size. Unset, unreadable or out of bounds = 1.0.
    pub interface_scale: Option<f32>,
}

/// Interface scale's presets in Settings; 1.0 is the desktop's normal size.
pub const SCALES: &[f32] = &[0.75, 0.9, 1.0, 1.1, 1.25, 1.5];
/// The bounds a scale is held to (a hand-edited file included).
const SCALE_BOUNDS: (f32, f32) = (0.5, 2.0);

impl Settings {
    pub fn always_on_top(&self) -> bool {
        self.always_on_top.unwrap_or(true)
    }

    /// Interface scale: the saved value, 1.0 when there is none or it is not a usable number.
    pub fn scale(&self) -> f32 {
        valid_scale(self.interface_scale.unwrap_or(1.0))
    }

    /// Shelf mode's share of the monitor's width.
    pub fn shelf_share(&self) -> f32 {
        self.shelf_width
            .filter(|s| s.is_finite())
            .map_or(crate::layout::SHELF_SHARE, |s| {
                s.clamp(0.15, crate::layout::SHELF_MAX)
            })
    }
}

/// The preset `step` places from `scale` (the nearest preset when it is between two), held to
/// the ends; step 0 is 100 %.
pub fn step_scale(scale: f32, step: i32) -> f32 {
    if step == 0 {
        return 1.0;
    }
    let at = (0..SCALES.len())
        .min_by(|a, b| {
            (SCALES[*a] - scale)
                .abs()
                .total_cmp(&(SCALES[*b] - scale).abs())
        })
        .unwrap_or(2) as i32;
    SCALES[(at + step).clamp(0, SCALES.len() as i32 - 1) as usize]
}

/// `scale` when it is a finite number within bounds, else 1.0.
pub fn valid_scale(scale: f32) -> f32 {
    if scale.is_finite() && (SCALE_BOUNDS.0..=SCALE_BOUNDS.1).contains(&scale) {
        scale
    } else {
        1.0
    }
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
    let mut settings: Settings = path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    if crate::shortcuts::validate(&settings.shortcuts).is_err() {
        settings.shortcuts.clear();
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
        let settings = Settings {
            shelf_width: Some(0.9),
            mode: Some("drawer".into()),
            work_width: Some(1200.0),
            ..Settings::default()
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.mode.as_deref(), Some("drawer"));
        assert_eq!(back.shelf_share(), 0.4, "a share past 40 % is no shelf");
        assert_eq!(Settings::default().shelf_share(), 0.35);
    }

    #[test]
    fn interface_scale_defaults_to_the_desktop_size_and_is_bounded() {
        assert_eq!(
            Settings::default().scale(),
            1.0,
            "an existing file keeps today's size"
        );
        let at = |v: f32| {
            Settings {
                interface_scale: Some(v),
                ..Settings::default()
            }
            .scale()
        };
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
        assert_eq!(
            step_scale(1.2, 1),
            1.5,
            "from between two, the nearest is the start"
        );
        assert_eq!(step_scale(1.25, 0), 1.0);
    }
}
