//! Backend page "Settings" (SPEC §3.1): read-only for now — where things are and what applies.

use gtk4 as gtk;
use gtk::prelude::*;

pub fn build() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(12);
    let home = std::env::var("HOME").unwrap_or_default();
    let rows: Vec<(String, String)> = vec![
        ("Policy".into(), "default — /etc/sill/policies/default.toml, then ~/.config/sill/policies/, then built-in".into()),
        ("Agents".into(), "built-in: claude, codex, gemini; overrides in ~/.config/sill/agents/*.toml".into()),
        ("Sessions".into(), format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/")),
        ("Units".into(), "systemctl --user list-units 'sill-*'; logs: journalctl --user -u sill-<id>".into()),
        ("Hotkey".into(), "Alt+Space → sill toggle (Cinnamon custom shortcut); Escape hides".into()),
        ("Keys".into(), "Enter sends · Shift+Enter newline · Ctrl+` prompt/shell (M3)".into()),
    ];
    for (k, v) in rows {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let kl = gtk::Label::new(Some(&k));
        kl.add_css_class("heading");
        kl.set_xalign(0.0);
        kl.set_width_chars(10);
        let vl = gtk::Label::new(Some(&v));
        vl.set_xalign(0.0);
        vl.set_wrap(true);
        vl.set_selectable(true);
        vl.set_hexpand(true);
        row.append(&kl);
        row.append(&vl);
        root.append(&row);
    }
    root
}
