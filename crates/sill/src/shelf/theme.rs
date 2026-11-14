//! Colour themes (SPEC §3.1 Settings). A theme is a small palette; everything else derives from it:
//! libadwaita's named colours are overridden so stock widgets follow, and sill's own CSS and the
//! Markdown text tags use the same values. Built-in: Adwaita (system), Dracula, Solarized Dark,
//! Solarized Light, Nord. Chosen theme is saved in `~/.config/sill/sill.toml`.
//!
//! The text scale (`text_scale` in the same file) is applied here too: one `font-size` rule on the
//! toplevels, so every relative size in the stylesheet, the Markdown tags and the editor follow;
//! VTE sets its own font and takes the factor through [`on_change`] (see `session_view`).

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
    static LISTENERS: RefCell<Vec<Box<dyn Fn(&Theme)>>> = const { RefCell::new(Vec::new()) };
    static CURRENT: RefCell<Theme> = RefCell::new(by_id("adwaita"));
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static TEXT_SCALE: std::cell::Cell<f64> = const { std::cell::Cell::new(1.0) };
}

/// Called after every appearance change — theme or text scale — for widgets that can't be styled
/// by CSS (VTE, drawn icons).
pub fn on_change(f: impl Fn(&Theme) + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Box::new(f)));
}

/// GtkSourceView style scheme id for the theme: a generated `sill-<id>` scheme built from the palette
/// (see [`write_source_schemes`]); Adwaita keeps the stock schemes.
pub fn source_scheme(t: &Theme) -> String {
    if t.bg.is_empty() {
        if adw::StyleManager::default().is_dark() { "Adwaita-dark".into() } else { "Adwaita".into() }
    } else {
        format!("sill-{}", t.id)
    }
}

/// Write one GtkSourceView style scheme per palette theme into the user's state dir and return the
/// directory (added to the scheme manager's search path by the editor).
pub fn write_source_schemes() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    let dir = base.join("sill").join("styles");
    std::fs::create_dir_all(&dir).ok()?;
    for t in builtin().into_iter().filter(|t| !t.bg.is_empty()) {
        let xml = format!(
            r##"<?xml version="1.0" encoding="UTF-8"?>
<style-scheme id="sill-{id}" name="sill {name}" version="1.0">
  <style name="text"                  foreground="{fg}" background="{bg}"/>
  <style name="selection"             background="{accent}" foreground="{bg}"/>
  <style name="cursor"                foreground="{fg}"/>
  <style name="current-line"          background="{surface}"/>
  <style name="line-numbers"          foreground="{dim}" background="{bg}"/>
  <style name="current-line-number"   foreground="{accent}" background="{bg}" bold="true"/>
  <style name="bracket-match"         foreground="{accent}" bold="true"/>
  <style name="search-match"          background="{warning}" foreground="{bg}"/>
  <style name="def:comment"           foreground="{dim}" italic="true"/>
  <style name="def:shebang"           foreground="{dim}"/>
  <style name="def:doc-comment"       foreground="{dim}" italic="true"/>
  <style name="def:string"            foreground="{success}"/>
  <style name="def:special-char"      foreground="{warning}"/>
  <style name="def:number"            foreground="{warning}"/>
  <style name="def:boolean"           foreground="{warning}"/>
  <style name="def:constant"          foreground="{warning}"/>
  <style name="def:keyword"           foreground="{accent}" bold="true"/>
  <style name="def:statement"         foreground="{accent}"/>
  <style name="def:type"              foreground="{link}"/>
  <style name="def:builtin"           foreground="{link}"/>
  <style name="def:function"          foreground="{fg}"/>
  <style name="def:identifier"        foreground="{fg}"/>
  <style name="def:preprocessor"      foreground="{warning}"/>
  <style name="def:error"             foreground="{error}" underline="error"/>
  <style name="def:warning"           foreground="{warning}"/>
  <style name="def:note"              foreground="{accent}" bold="true"/>
  <style name="def:heading"           foreground="{accent}" bold="true"/>
  <style name="def:emphasis"          italic="true"/>
  <style name="def:strong-emphasis"   bold="true"/>
  <style name="def:link-text"         foreground="{link}" underline="single"/>
  <style name="def:link-destination"  foreground="{link}"/>
  <style name="def:inline-code"       foreground="{success}"/>
  <style name="def:list-marker"       foreground="{accent}"/>
  <style name="diff:added-line"       foreground="{success}"/>
  <style name="diff:removed-line"     foreground="{error}"/>
  <style name="diff:changed-line"     foreground="{warning}"/>
  <style name="diff:location"         foreground="{accent}"/>
  <style name="diff:diff-file"        foreground="{fg}" bold="true"/>
</style-scheme>
"##,
            id = t.id, name = t.name, fg = t.fg, bg = t.bg, accent = t.accent, surface = t.surface,
            dim = t.fg_dim, warning = t.warning, success = t.success, link = t.link, error = t.error,
        );
        let _ = std::fs::write(dir.join(format!("sill-{}.xml", t.id)), xml);
    }
    Some(dir)
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

/// The text scale in effect (1.0 = the desktop's font size).
pub fn text_scale() -> f64 {
    TEXT_SCALE.with(|s| s.get())
}

/// Change the text scale live: the stylesheet is rebuilt and the listeners run.
pub fn set_text_scale(scale: f64) {
    TEXT_SCALE.with(|s| s.set(scale));
    apply(&current());
}

/// The full stylesheet for a theme: text scale, named colours + sill's own rules.
pub fn stylesheet(t: &Theme) -> String {
    let mut s = String::new();
    s.push_str(&format!("@define-color sill_code_bg {};\n", t.code_bg));
    let scale = text_scale();
    if (scale - 1.0).abs() > 0.001 {
        // percentages are relative to the parent; at the toplevel that is the desktop font, so this
        // one rule scales everything that uses relative sizes (all of sill's own CSS does). A popover
        // resets to the default font unless told to follow its parent (100 %, not the factor again:
        // measured, the factor compounds there).
        s.push_str(&format!("window, tooltip {{ font-size: {:.0}%; }}\npopover {{ font-size: 100%; }}\n", scale * 100.0));
    }
    s.push_str(&named_colors(t));
    s.push_str(super::conversation::CSS);
    s.push('\n');
    s.push_str(super::tabs::CSS);
    if !t.bg.is_empty() {
        s.push_str(&format!(
            "\n.sill-tab {{ background: {surface}; }}\n\
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
    LISTENERS.with(|l| {
        for f in l.borrow().iter() {
            f(t);
        }
    });
}

/// Colours the Markdown text tags need (they can't use CSS variables).
pub struct TagColors {
    pub code_bg: String,
    pub link: String,
    /// Dimmed text (quotes); none for Adwaita, which keeps the stock colour.
    pub dim: Option<String>,
    /// Hairlines (rules, table header underline).
    pub rule: String,
}

pub fn tag_colors() -> TagColors {
    let t = current();
    TagColors {
        code_bg: t.code_bg.to_string(),
        link: t.link.to_string(),
        dim: if t.fg_dim.is_empty() { None } else { Some(t.fg_dim.to_string()) },
        rule: if t.border.is_empty() { "rgba(128,128,128,0.4)".to_string() } else { t.border.to_string() },
    }
}
