//! Colour themes (SPEC §3.1 Settings). A theme is a small palette; everything else derives from it:
//! libadwaita's named colours are overridden so stock widgets follow, and sill's own CSS and the
//! Markdown text tags use the same values. Built-in: Adwaita (system), Dracula, Solarized Dark,
//! Solarized Light, Nord. Chosen theme is saved in `~/.config/sill/sill.toml`.

use std::cell::RefCell;

use gtk4 as gtk;
use gtk::gdk;
use libadwaita as adw;

#[derive(Debug, Clone)]
pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    /// Window / page background.
    pub bg: &'static str,
    /// Slightly raised surfaces (tabs, user message, tool rows).
    pub surface: &'static str,
    /// Code blocks.
    pub code_bg: &'static str,
    pub fg: &'static str,
    pub fg_dim: &'static str,
    pub accent: &'static str,
    pub warning: &'static str,
    pub success: &'static str,
    pub error: &'static str,
    pub link: &'static str,
    pub border: &'static str,
}

pub fn builtin() -> Vec<Theme> {
    vec![
        Theme {
            id: "adwaita", name: "Adwaita (system)", dark: true,
            bg: "", surface: "", code_bg: "rgba(0,0,0,0.22)", fg: "", fg_dim: "",
            accent: "", warning: "", success: "", error: "", link: "#3584e4", border: "",
        },
        Theme {
            id: "dracula", name: "Dracula", dark: true,
            bg: "#282a36", surface: "#343746", code_bg: "#21222c", fg: "#f8f8f2", fg_dim: "#6272a4",
            accent: "#bd93f9", warning: "#ffb86c", success: "#50fa7b", error: "#ff5555", link: "#8be9fd",
            border: "#44475a",
        },
        Theme {
            id: "solarized-dark", name: "Solarized Dark", dark: true,
            bg: "#002b36", surface: "#073642", code_bg: "#00212b", fg: "#93a1a1", fg_dim: "#586e75",
            accent: "#268bd2", warning: "#b58900", success: "#859900", error: "#dc322f", link: "#2aa198",
            border: "#0a3f4d",
        },
        Theme {
            id: "solarized-light", name: "Solarized Light", dark: false,
            bg: "#fdf6e3", surface: "#eee8d5", code_bg: "#f3ecd8", fg: "#586e75", fg_dim: "#93a1a1",
            accent: "#268bd2", warning: "#b58900", success: "#859900", error: "#dc322f", link: "#2aa198",
            border: "#e0d9c4",
        },
        Theme {
            id: "nord", name: "Nord", dark: true,
            bg: "#2e3440", surface: "#3b4252", code_bg: "#272c36", fg: "#d8dee9", fg_dim: "#7b88a1",
            accent: "#88c0d0", warning: "#ebcb8b", success: "#a3be8c", error: "#bf616a", link: "#81a1c1",
            border: "#434c5e",
        },
    ]
}

pub fn by_id(id: &str) -> Theme {
    builtin().into_iter().find(|t| t.id == id).unwrap_or_else(|| builtin().remove(0))
}

thread_local! {
    static CURRENT: RefCell<Theme> = RefCell::new(by_id("adwaita"));
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

pub fn current() -> Theme {
    CURRENT.with(|c| c.borrow().clone())
}

/// libadwaita named-colour overrides for a palette theme (none for Adwaita itself).
fn named_colors(t: &Theme) -> String {
    if t.bg.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for (name, value) in [
        ("window_bg_color", t.bg),
        ("window_fg_color", t.fg),
        ("view_bg_color", t.bg),
        ("view_fg_color", t.fg),
        ("headerbar_bg_color", t.bg),
        ("headerbar_fg_color", t.fg),
        ("popover_bg_color", t.surface),
        ("popover_fg_color", t.fg),
        ("card_bg_color", t.surface),
        ("card_fg_color", t.fg),
        ("dialog_bg_color", t.surface),
        ("dialog_fg_color", t.fg),
        ("sidebar_bg_color", t.surface),
        ("sidebar_fg_color", t.fg),
        ("accent_bg_color", t.accent),
        ("accent_fg_color", if t.dark { t.bg } else { "#ffffff" }),
        ("accent_color", t.accent),
        ("warning_bg_color", t.warning),
        ("warning_fg_color", t.bg),
        ("warning_color", t.warning),
        ("success_bg_color", t.success),
        ("success_fg_color", t.bg),
        ("success_color", t.success),
        ("error_bg_color", t.error),
        ("error_fg_color", t.bg),
        ("error_color", t.error),
        ("destructive_bg_color", t.error),
        ("destructive_fg_color", t.bg),
        ("destructive_color", t.error),
        ("borders", t.border),
        ("dim_label_color", t.fg_dim),
    ] {
        s.push_str(&format!("@define-color {name} {value};\n"));
    }
    s
}

/// The full stylesheet for a theme: named colours + sill's own rules.
pub fn stylesheet(t: &Theme) -> String {
    let mut s = named_colors(t);
    s.push_str(super::conversation::CSS);
    s.push('\n');
    s.push_str(super::tabs::CSS);
    if !t.bg.is_empty() {
        s.push_str(&format!(
            "\n.sill-user {{ background: {surface}; }}\n\
             .sill-tool {{ background: {surface}; }}\n\
             .sill-tab {{ background: {surface}; }}\n\
             .sill-tab.selected {{ background: {bg}; }}\n\
             .sill-prompt-box {{ background: {surface}; border-top-color: {border}; }}\n\
             .dim-label {{ color: {dim}; }}\n\
             textview {{ background: transparent; color: {fg}; }}\n\
             textview text {{ background: transparent; color: {fg}; }}\n",
            surface = t.surface,
            bg = t.bg,
            border = t.border,
            dim = t.fg_dim,
            fg = t.fg,
        ));
    }
    s
}

/// Install (or replace) the theme's stylesheet and set the light/dark scheme.
pub fn apply(t: &Theme) {
    let display = gdk::Display::default().expect("display");
    PROVIDER.with(|p| {
        if let Some(old) = p.borrow_mut().take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        let css = gtk::CssProvider::new();
        css.load_from_string(&stylesheet(t));
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        *p.borrow_mut() = Some(css);
    });
    adw::StyleManager::default().set_color_scheme(if t.bg.is_empty() {
        adw::ColorScheme::Default
    } else if t.dark {
        adw::ColorScheme::ForceDark
    } else {
        adw::ColorScheme::ForceLight
    });
    CURRENT.with(|c| *c.borrow_mut() = t.clone());
    super::markdown::retheme();
}

/// Colours the Markdown text tags need (they can't use CSS variables).
pub struct TagColors {
    pub code_bg: String,
    pub link: String,
}

pub fn tag_colors() -> TagColors {
    let t = current();
    TagColors { code_bg: t.code_bg.to_string(), link: t.link.to_string() }
}
