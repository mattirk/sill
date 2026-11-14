//! Backend page "Settings" (SPEC §3.1): theme and width (saved to ~/.config/sill/sill.toml), plus
//! where things are.

use gtk4 as gtk;
use gtk::prelude::*;

use super::{theme, x11};

pub fn build() -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(12);

    fn row(label: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let l = gtk::Label::new(Some(label));
        l.add_css_class("heading");
        l.set_xalign(0.0);
        l.set_width_chars(10);
        b.append(&l);
        b.append(w);
        b
    }

    // theme
    let themes = theme::builtin();
    let names: Vec<&str> = themes.iter().map(|t| t.name).collect();
    let dropdown = gtk::DropDown::from_strings(&names);
    let current = theme::current().id;
    if let Some(i) = themes.iter().position(|t| t.id == current) {
        dropdown.set_selected(i as u32);
    }
    {
        let themes = themes.clone();
        dropdown.connect_selected_notify(move |d| {
            if let Some(t) = themes.get(d.selected() as usize) {
                theme::apply(t);
                let mut c = sill_core::config::load();
                c.theme = t.id.to_string();
                let _ = sill_core::config::save(&c);
            }
        });
    }
    root.append(&row("Theme", &dropdown));

    // width
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 0.8, 0.02);
    scale.set_value(x11::width_fraction());
    scale.set_hexpand(true);
    scale.set_draw_value(true);
    scale.set_digits(2);
    scale.connect_value_changed(|s| {
        x11::set_width_fraction(s.value());
        let mut c = sill_core::config::load();
        c.width = Some(s.value());
        let _ = sill_core::config::save(&c);
    });
    let hint = gtk::Label::new(Some("of the work area; applies the next time the shelf is shown"));
    hint.add_css_class("dim-label");
    let wbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
    wbox.set_hexpand(true);
    wbox.append(&scale);
    wbox.append(&hint);
    root.append(&row("Width", &wbox));

    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep.set_margin_top(8);
    root.append(&sep);

    let home = std::env::var("HOME").unwrap_or_default();
    let info: Vec<(String, String)> = vec![
        ("Config".into(), format!("{home}/.config/sill/sill.toml")),
        ("Policy".into(), "default — /etc/sill/policies/default.toml, then ~/.config/sill/policies/, then built-in".into()),
        ("Agents".into(), "built-in: claude, codex, gemini; overrides in ~/.config/sill/agents/*.toml".into()),
        ("Sessions".into(), format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/")),
        ("Units".into(), "systemctl --user list-units 'sill-*'; logs: journalctl --user -u sill-<id>".into()),
        ("Keys".into(), "Alt+Space show/hide · Escape back/hide · Enter send · Shift+Enter newline · F2 or double-click rename".into()),
    ];
    for (k, v) in info {
        let vl = gtk::Label::new(Some(&v));
        vl.set_xalign(0.0);
        vl.set_wrap(true);
        vl.set_selectable(true);
        vl.set_hexpand(true);
        root.append(&row(&k, &vl));
    }
    root
}
