//! Colour themes (SPEC §3.1 Settings). A theme is a small palette; everything else derives from it:
//! the named colours (libadwaita's names, so sill's CSS reads as usual; libadwaita itself is not
//! used, 2026-11-01) and rules for the stock widgets come from it, and sill's own CSS and the
//! Markdown text tags use the same values. Every theme is complete — "Adwaita" is sill's copy of
//! that palette, not the desktop's — so a shelf looks the same on every machine whatever the GTK
//! theme, icon theme or dark/light preference there (2026-11-01). Built-in: Adwaita Dark, Adwaita
//! Light, Dracula, Solarized Dark, Solarized Light, Nord, Miasma, Gruvbox Dark, Gruvbox Light. Chosen theme is saved in
//! `~/.config/sill/sill.toml`.
//!
//! The text scale (`text_scale` in the same file) is applied here too: one `font-size` rule on the
//! toplevels, so every relative size in the stylesheet, the Markdown tags and the editor follow;
//! VTE sets its own font and takes the factor through [`on_change`] (see `session_view`).

use std::cell::RefCell;

use gtk4 as gtk;
use gtk::gdk;

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
        // libadwaita 1.4's palettes, fixed here so they don't move with the machine's libadwaita
        Theme {
            id: "adwaita", name: "Adwaita Dark", dark: true,
            bg: "#242424", surface: "#303030", code_bg: "#1e1e1e", fg: "#ffffff", fg_dim: "#9d9d9d",
            accent: "#78aeed", warning: "#ffa348", success: "#8ff0a4", error: "#ff7b63", link: "#78aeed",
            border: "#3d3d3d",
        },
        Theme {
            id: "adwaita-light", name: "Adwaita Light", dark: false,
            bg: "#fafafa", surface: "#ebebeb", code_bg: "#f0f0f0", fg: "#323232", fg_dim: "#8b8b8b",
            accent: "#1c71d8", warning: "#9c6e03", success: "#1b8553", error: "#c01c28", link: "#1c71d8",
            border: "#d8d8d8",
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
        // xero's Miasma (terminal palette from miasma.nvim's extras): muted earth tones — the
        // gold is the accent, the orange stands in for red (its red is a dark brown that would
        // not read as an error), the olive "blue" is left out
        Theme {
            id: "miasma", name: "Miasma", dark: true,
            bg: "#222222", surface: "#2b2b28", code_bg: "#1c1c1c", fg: "#c2c2b0", fg_dim: "#787866",
            accent: "#c9a554", warning: "#b36d43", success: "#5f875f", error: "#bb7744", link: "#d7c483",
            border: "#363632",
        },
        // Gruvbox Dark (morhetz's palette, terminalcolors.com's Alacritty file): bg0 page, bg1
        // surfaces, bg0 hard code, fg1 text, fg4 dim; the "bright" hues on the dark ground
        Theme {
            id: "gruvbox-dark", name: "Gruvbox Dark", dark: true,
            bg: "#282828", surface: "#3c3836", code_bg: "#1d2021", fg: "#ebdbb2", fg_dim: "#a89984",
            accent: "#83a598", warning: "#fabd2f", success: "#b8bb26", error: "#fb4934", link: "#8ec07c",
            border: "#504945",
        },
        // Gruvbox Light (morhetz's palette, terminalcolors.com's Alacritty file): bg0 hard page,
        // bg1 surfaces, bg0 soft code, fg1 text, fg4 dim; the "bright" (darker) hues on paper
        Theme {
            id: "gruvbox-light", name: "Gruvbox Light", dark: false,
            bg: "#fbf1c7", surface: "#ebdbb2", code_bg: "#f2e5bc", fg: "#3c3836", fg_dim: "#7c6f64",
            accent: "#076678", warning: "#b57614", success: "#79740e", error: "#9d0006", link: "#076678",
            border: "#d5c4a1",
        },
    ]
}

pub fn by_id(id: &str) -> Theme {
    builtin().into_iter().find(|t| t.id == id).unwrap_or_else(|| builtin().remove(0))
}

/// The faces the shelf is drawn in — sill's own, embedded (see `fonts.rs`), never the desktop's
/// font setting. `pt` is the text size at zoom 1; monospace text runs one point smaller.
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    pub sans: &'static str,
    pub mono: &'static str,
    pub pt: f64,
}

/// What ships: one pair, chosen 2026-11-01 — Source Sans 3 and Source Code Pro (Adobe, OFL 1.1).
pub const FONT: Font = Font { sans: "Source Sans 3", mono: "Source Code Pro", pt: 13.0 };

impl Font {
    /// The monospace size at zoom 1.
    pub fn mono_pt(&self) -> f64 {
        self.pt - 1.0
    }
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
/// (see [`write_source_schemes`]).
pub fn source_scheme(t: &Theme) -> String {
    format!("sill-{}", t.id)
}

/// Write one GtkSourceView style scheme per palette theme into the user's state dir and return the
/// directory (added to the scheme manager's search path by the editor).
pub fn write_source_schemes() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    let dir = base.join("sill").join("styles");
    std::fs::create_dir_all(&dir).ok()?;
    for t in builtin() {
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

/// The named colours sill's CSS uses (libadwaita's names), every one from the palette.
fn named_colors(t: &Theme) -> String {
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

/// The text scale in effect: zoom × [`sill_core::config::TEXT_SCALE_BASE`], the factor the
/// pixel-sized things (icons, Markdown sizes) follow.
pub fn text_scale() -> f64 {
    TEXT_SCALE.with(|s| s.get())
}

/// The zoom alone (1.0 = Normal): what the point sizes are multiplied by.
pub fn zoom() -> f64 {
    text_scale() / sill_core::config::TEXT_SCALE_BASE
}

pub fn font() -> Font {
    FONT
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
    // the faces and the size, absolute: sill's own font at sill's own size × zoom, whatever the
    // desktop's font setting; every relative size below hangs off this one rule
    let f = font();
    s.push_str(&format!("window, popover, tooltip {{ font-family: \"{}\"; font-size: {:.1}pt; }}\n", f.sans, f.pt * zoom()));
    s.push_str(&named_colors(t));
    s.push_str(&super::conversation::CSS.replace("font-family: monospace", &format!("font-family: \"{}\"", f.mono)));
    s.push('\n');
    s.push_str(super::tabs::CSS);
    s.push_str(super::settings::CSS);
    {
        s.push_str(&format!(
            "\n.sill-tab {{ background: {surface}; }}\n\
             .sill-tab.sill-ghost {{ opacity: 0.7; }}\n\
             .sill-tab.sill-ghost label {{ font-style: italic; }}\n\
             .sill-tab.selected {{ background: {bg}; }}\n\
             .sill-prompt-box {{ background: {surface}; border-top-color: {border}; }}\n\
             .dim-label {{ color: {dim}; }}\n\
             textview {{ background: transparent; color: {fg}; }}\n\
             textview text {{ background: transparent; color: {fg}; }}\n\
             window, window.background {{ background: {bg}; color: {fg}; }}\n\
             popover, popover.background {{ background: transparent; color: {fg}; }}\n\
             popover > contents, popover > arrow, tooltip > contents {{ background: {surface}; color: {fg}; border: 1px solid {border}; }}\n\
             entry {{ background: {surface}; color: {fg}; border-color: {border}; }}\n\
             list, listview, row, scrolledwindow, viewport {{ background: transparent; color: {fg}; }}\n\
             button {{ border: none; box-shadow: none; text-shadow: none; -gtk-icon-shadow: none; background-image: none; background-color: alpha({fg}, 0.08); color: {fg}; }}\n\
             button:checked, button:active {{ color: {fg}; }}\n\
             button:hover {{ background-color: alpha({fg}, 0.14); }}\n\
             button.flat, menubutton.flat > button, menubutton > button.flat {{ background: transparent; }}\n\
             button.flat:hover, menubutton.flat > button:hover, menubutton > button.flat:hover {{ background: alpha({fg}, 0.07); }}\n\
             button.suggested-action {{ background: {accent}; color: {accent_fg}; }}\n\
             button.destructive-action {{ background: {error}; color: {bg}; }}\n\
             switch {{ min-width: 2.2em; min-height: 1.1em; border: none; box-shadow: none; border-radius: 1em; background: alpha({fg}, 0.2); }}\n\
             switch:checked {{ background: {accent}; }}\n\
             switch > slider {{ min-width: 0.9em; min-height: 0.9em; margin: 0.1em; border: none; box-shadow: none; border-radius: 1em; background: {knob}; }}\n\
             switch:checked > slider {{ background: {accent_fg}; }}\n\
             switch > image {{ opacity: 0; }}\n\
             scrollbar, scrollbar trough {{ background: transparent; border: none; box-shadow: none; }}\n\
             scrollbar slider {{ min-width: 6px; min-height: 6px; margin: 2px; border: none; border-radius: 3px; background: alpha({fg}, 0.28); }}\n\
             scrollbar slider:hover, scrollbar slider:active {{ background: alpha({fg}, 0.5); }}\n\
             paned > separator {{ min-width: 1px; background: transparent; background-image: none; }}\n",
            surface = t.surface,
            bg = t.bg,
            border = t.border,
            dim = t.fg_dim,
            fg = t.fg,
            accent = t.accent,
            accent_fg = if t.dark { t.bg } else { "#ffffff" },
            knob = if t.dark { t.fg } else { "#ffffff" },
            error = t.error,
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
    // the theme decides dark or light; the desktop's preference never does (GTK's default theme has
    // a dark variant for the stock widgets sill's rules don't reach)
    if let Some(s) = gtk::Settings::default() {
        s.set_gtk_application_prefer_dark_theme(t.dark);
    }
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
    /// Dimmed text (quotes).
    pub dim: String,
    /// Hairlines (rules, table header underline).
    pub rule: String,
    /// Paragraph background of a block that differs from git HEAD (the viewer's Markdown preview).
    pub changed: String,
}

pub fn tag_colors() -> TagColors {
    let t = current();
    TagColors {
        code_bg: t.code_bg.to_string(),
        link: t.link.to_string(),
        dim: t.fg_dim.to_string(),
        rule: t.border.to_string(),
        changed: with_alpha(t.success, 0.18).unwrap_or_else(|| "rgba(46,194,126,0.18)".to_string()),
    }
}

/// `#rrggbb` at an alpha, as CSS `rgba(...)`; `None` for anything else (Adwaita's empty slots).
fn with_alpha(hex: &str, alpha: f32) -> Option<String> {
    let c = gtk4::gdk::RGBA::parse(hex).ok()?;
    Some(format!("rgba({},{},{},{alpha})", (c.red() * 255.0).round() as u8, (c.green() * 255.0).round() as u8, (c.blue() * 255.0).round() as u8))
}

