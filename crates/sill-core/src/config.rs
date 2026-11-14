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

pub fn save(c: &Config) -> Result<()> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, toml::to_string_pretty(c)?).with_context(|| format!("write {}", p.display()))
}
