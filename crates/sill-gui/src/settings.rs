//! sill-gui's own settings, `~/.config/sill/sill-gui.toml`: the window's modes and sizes. A file of
//! its own, not a table in `sill.toml`: the GTK shelf and the daemon write that file from their own
//! settings and would drop a table they do not know.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Shelf mode's width, as a share of the monitor's (0.15–0.4). Unset = 35 %.
    pub shelf_width: Option<f32>,
    /// The mode the window was last in: `shelf`, `drawer` or `full`.
    pub mode: Option<String>,
    /// The working size (Drawer and Full), logical pixels, as the window last had it.
    pub work_width: Option<f32>,
    pub work_height: Option<f32>,
    /// The drawer's width as last dragged, logical pixels.
    pub drawer_width: Option<f32>,
    /// The window stays above others. Unset = on, as the GTK shelf.
    pub always_on_top: Option<bool>,
    /// Where the window was left in Drawer and Full mode, device pixels (Shelf mode is always on
    /// its edge). Unset = the top-left of the working area.
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
}

impl Settings {
    pub fn always_on_top(&self) -> bool {
        self.always_on_top.unwrap_or(true)
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

fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("sill").join("sill-gui.toml"))
}

/// The settings, or the defaults when there are none or they cannot be read.
pub fn load() -> Settings {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
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
}
