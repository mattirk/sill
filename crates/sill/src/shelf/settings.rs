//! Backend page "Settings" (SPEC §3.1): theme and width (saved to ~/.config/sill/sill.toml), plus
//! where things are.

use std::path::PathBuf;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
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

    // the page the "sill" button opens
    let page_ids: Vec<&str> = sill_core::config::PAGES.iter().map(|(id, _)| *id).collect();
    let page_titles: Vec<&str> = sill_core::config::PAGES.iter().map(|(_, t)| *t).collect();
    let page_dd = gtk::DropDown::from_strings(&page_titles);
    let current_page = sill_core::config::load().default_page().to_string();
    if let Some(i) = page_ids.iter().position(|id| *id == current_page) {
        page_dd.set_selected(i as u32);
    }
    {
        let ids: Vec<String> = page_ids.iter().map(|s| s.to_string()).collect();
        page_dd.connect_selected_notify(move |d| {
            if let Some(id) = ids.get(d.selected() as usize) {
                let mut c = sill_core::config::load();
                c.default_page = Some(id.clone());
                let _ = sill_core::config::save(&c);
            }
        });
    }
    root.append(&row("Default page", &page_dd));

    // show/hide key: click the button, press the combination you want
    let current = sill_core::config::load().hotkey.unwrap_or_else(|| super::hotkey::DEFAULT.to_string());
    let hk = gtk::Button::with_label(&pretty(&current));
    hk.add_css_class("sill-hotkey");
    hk.set_tooltip_text(Some("Click, then press the key combination"));
    let hk_msg = gtk::Label::new(Some("Click the key and press the combination you want (Escape cancels). Something with a modifier plus a key is safest; a bare F-key also works."));
    hk_msg.add_css_class("dim-label");
    hk_msg.set_wrap(true);
    hk_msg.set_xalign(0.0);
    let capturing = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let capturing = capturing.clone();
        let b = hk.clone();
        hk.connect_clicked(move |_| {
            capturing.set(true);
            b.set_label("press keys…");
            b.add_css_class("sill-capturing");
        });
    }
    {
        let capturing = capturing.clone();
        let b = hk.clone();
        let msg = hk_msg.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            if !capturing.get() {
                return glib::Propagation::Proceed;
            }
            if key == gdk::Key::Escape {
                capturing.set(false);
                b.remove_css_class("sill-capturing");
                b.set_label(&pretty(&sill_core::config::load().hotkey.unwrap_or_else(|| super::hotkey::DEFAULT.to_string())));
                return glib::Propagation::Stop;
            }
            let Some(spec) = spec_from(key, state) else { return glib::Propagation::Stop };
            capturing.set(false);
            b.remove_css_class("sill-capturing");
            let result = super::window::HOTKEY.with(|h| h.borrow().as_ref().map(|hk| hk.rebind(&spec)));
            match result {
                Some(Ok(())) => {
                    let mut c = sill_core::config::load();
                    c.hotkey = Some(spec.clone());
                    let _ = sill_core::config::save(&c);
                    b.set_label(&pretty(&spec));
                    msg.set_text(&format!("bound to {}", pretty(&spec)));
                }
                Some(Err(err)) => {
                    b.set_label(&pretty(&sill_core::config::load().hotkey.unwrap_or_else(|| super::hotkey::DEFAULT.to_string())));
                    msg.set_text(&format!("{}: {err:#}", pretty(&spec)));
                }
                None => msg.set_text("hotkey grab is not active (no X11?)"),
            }
            glib::Propagation::Stop
        });
        hk.add_controller(keys);
    }
    let hkbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
    hkbox.set_hexpand(true);
    let hkrow = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    hkrow.append(&hk);
    hkbox.append(&hkrow);
    hkbox.append(&hk_msg);
    root.append(&row("Hotkey", &hkbox));

    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep.set_margin_top(8);
    root.append(&sep);

    let home = std::env::var("HOME").unwrap_or_default();
    let info: Vec<(String, String)> = vec![
        ("Config".into(), format!("{home}/.config/sill/sill.toml")),
        ("Policy".into(), "default — /etc/sill/policies/default.toml, then ~/.config/sill/policies/, then built-in".into()),
        ("Agents".into(), "the Agents page: install from the ACP registry, the gear edits command, environment, domains (~/.config/sill/agents/*.toml)".into()),
        ("Sessions".into(), format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/")),
        ("Launcher".into(), sill_core::launcher::current().describe()),
        (
            "Jails".into(),
            match sill_core::launcher::current().mode {
                sill_core::launcher::Mode::Unit => "systemctl --user list-units 'sill-*'; logs: journalctl --user -u sill-<id>".to_string(),
                sill_core::launcher::Mode::Direct => "pids and logs under $XDG_RUNTIME_DIR/sill/<id>/ ({bwrap,launch}.pid, {jail,proxy}.log)".to_string(),
            },
        ),
        ("Keys".into(), "hotkey show/hide · Escape sessions ⇄ backend · Enter send · Shift+Enter newline · Ctrl+T shell · F2 or double-click rename".into()),
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

/// GTK accelerator spec from a captured key press. Modifier keys alone are refused (they'd fire on
/// every shortcut that uses them); a modifier key *as the key* with another modifier held is allowed.
fn spec_from(key: gdk::Key, state: gdk::ModifierType) -> Option<String> {
    let name = key.name()?.to_string();
    let is_mod_key = matches!(
        name.as_str(),
        "Control_L" | "Control_R" | "Shift_L" | "Shift_R" | "Alt_L" | "Alt_R" | "Super_L" | "Super_R" | "Meta_L" | "Meta_R"
    );
    let mut spec = String::new();
    if state.contains(gdk::ModifierType::CONTROL_MASK) && !matches!(name.as_str(), "Control_L" | "Control_R") {
        spec.push_str("<Control>");
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) && !matches!(name.as_str(), "Shift_L" | "Shift_R") {
        spec.push_str("<Shift>");
    }
    if state.contains(gdk::ModifierType::ALT_MASK) && !matches!(name.as_str(), "Alt_L" | "Alt_R") {
        spec.push_str("<Alt>");
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) && !matches!(name.as_str(), "Super_L" | "Super_R") {
        spec.push_str("<Super>");
    }
    if is_mod_key {
        return None; // modifiers never end a combination (a grab on one leaves it "stuck" for other programs)
    }
    // letters: lower-case, as the X keysym lookup expects
    let key_name = if name.chars().count() == 1 { name.to_lowercase() } else { name };
    spec.push_str(&key_name);
    Some(spec)
}

/// Human form of a spec: `<Control><Alt>space` → `Ctrl+Alt+Space`.
fn pretty(spec: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut rest = spec;
    while let Some(r) = rest.strip_prefix('<') {
        let Some((m, after)) = r.split_once('>') else { break };
        out.push(match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl".into(),
            "shift" => "Shift".into(),
            "alt" | "mod1" => "Alt".into(),
            "super" | "mod4" | "win" => "Super".into(),
            other => other.to_string(),
        });
        rest = after;
    }
    let key = match rest.to_ascii_lowercase().as_str() {
        "space" => "Space".to_string(),
        "grave" => "`".to_string(),
        "shift_r" => "Right Shift".to_string(),
        "shift_l" => "Left Shift".to_string(),
        "control_r" => "Right Ctrl".to_string(),
        "control_l" => "Left Ctrl".to_string(),
        "return" => "Enter".to_string(),
        _ => {
            let mut c = rest.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    };
    out.push(key);
    out.join("+")
}

/// One agent's registry entry, editable: on/off, command, environment, domains.
/// Per-agent settings (offered, command, environment, domains), saved as the registry override.
/// Shown inline on the Agents page.
pub fn agent_editor(home: PathBuf, agent: sill_core::agents::Agent) -> gtk::Box {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_start(24);
    body.set_margin_top(4);
    body.set_margin_bottom(4);
    fn line(label: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let l = gtk::Label::new(Some(label));
        l.set_xalign(0.0);
        l.set_width_chars(12);
        b.append(&l);
        b.append(w);
        b
    }
    let enabled = gtk::Switch::new();
    enabled.set_active(agent.enabled);
    enabled.set_halign(gtk::Align::Start);
    body.append(&line("Offered", &enabled));
    let command = gtk::Entry::new();
    command.set_text(&agent.command.join(" "));
    command.set_hexpand(true);
    command.set_tooltip_text(Some("program and arguments, space separated; found on the jail's PATH"));
    body.append(&line("Command", &command));
    let env = gtk::TextView::new();
    env.set_monospace(true);
    env.buffer().set_text(&agent.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"));
    let env_scroll = gtk::ScrolledWindow::builder().child(&env).min_content_height(48).hexpand(true).build();
    env_scroll.add_css_class("frame");
    env_scroll.set_tooltip_text(Some("one KEY=VALUE per line, e.g. OPENAI_API_KEY=sk-…"));
    body.append(&line("Environment", &env_scroll));
    let domains = gtk::Entry::new();
    domains.set_text(&agent.domains.join(" "));
    domains.set_hexpand(true);
    domains.set_tooltip_text(Some("hosts the agent may reach, space separated; *.example.com matches subdomains"));
    body.append(&line("Domains", &domains));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let reset = gtk::Button::with_label("Reset to built-in");
    let msg = gtk::Label::new(None);
    msg.add_css_class("dim-label");
    msg.set_xalign(0.0);
    buttons.append(&save);
    buttons.append(&reset);
    buttons.append(&msg);
    body.append(&buttons);
    {
        let (home, base, msg) = (home.clone(), agent.clone(), msg.clone());
        let (enabled, command, env, domains) = (enabled.clone(), command.clone(), env.clone(), domains.clone());
        save.connect_clicked(move |_| {
            let mut a = base.clone();
            a.enabled = enabled.is_active();
            a.command = command.text().split_whitespace().map(str::to_string).collect();
            if a.command.is_empty() {
                msg.set_text("command is empty");
                return;
            }
            a.domains = domains.text().split_whitespace().map(str::to_string).collect();
            let b = env.buffer();
            let text = b.text(&b.start_iter(), &b.end_iter(), false);
            a.env = text
                .lines()
                .filter_map(|l| {
                    let l = l.trim();
                    if l.is_empty() || l.starts_with('#') {
                        return None;
                    }
                    l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                })
                .collect();
            match sill_core::agents::save_override(&home, &a) {
                Ok(p) => msg.set_text(&format!("saved {}", p.display().to_string().replace(&home.display().to_string(), "~"))),
                Err(e) => msg.set_text(&format!("save failed: {e:#}")),
            }
        });
    }
    {
        let (home, id, msg) = (home.clone(), agent.id.clone(), msg.clone());
        reset.connect_clicked(move |_| match sill_core::agents::remove_override(&home, &id) {
            Ok(true) => msg.set_text("override removed; reopen the page to see the built-in values"),
            Ok(false) => msg.set_text("no override to remove"),
            Err(e) => msg.set_text(&format!("reset failed: {e:#}")),
        });
    }
    body
}
