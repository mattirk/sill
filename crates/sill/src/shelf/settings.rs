//! Backend page "Settings" (SPEC §3.1): theme, text size, width, default page, hotkey (saved to
//! ~/.config/sill/sill.toml), plus where things are.
//!
//! Adding a parameter: a field in `sill_core::config::Config`, a row here built with [`row`] and
//! [`choice`] (or another widget), saved through `config::update`, and applied live if it can be.

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

    // theme
    let themes = theme::builtin();
    let names: Vec<&str> = themes.iter().map(|t| t.name).collect();
    let current = theme::current().id;
    let dropdown = choice(&names, themes.iter().position(|t| t.id == current), move |i| {
        if let Some(t) = themes.get(i) {
            theme::apply(t);
            let _ = sill_core::config::update(|c| c.theme = t.id.to_string());
        }
    });
    root.append(&row("Theme", &dropdown));

    // text size: steps from the table; a hand-edited value in between shows as the nearest step
    let scales = sill_core::config::TEXT_SCALES;
    let labels: Vec<&str> = scales.iter().map(|(_, l)| *l).collect();
    let current = sill_core::config::load().zoom();
    let nearest = scales
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (a.0 - current).abs().partial_cmp(&(b.0 - current).abs()).unwrap())
        .map(|(i, _)| i);
    let size_dd = choice(&labels, nearest, move |i| {
        if let Some((zoom, _)) = scales.get(i) {
            theme::set_text_scale(*zoom * sill_core::config::TEXT_SCALE_BASE);
            let _ = sill_core::config::update(|c| c.text_scale = Some(*zoom));
        }
    });
    root.append(&row("Text size", &size_dd));


    // auto-revert: unkept changes outside git and pins go away after this long
    let choices = sill_core::config::AUTO_REVERT_CHOICES;
    let labels: Vec<&str> = choices.iter().map(|(_, l)| *l).collect();
    let current = sill_core::config::load().auto_revert_minutes();
    let nearest = choices.iter().enumerate().min_by_key(|(_, (m, _))| (*m as i64 - current as i64).abs()).map(|(i, _)| i);
    let ar_dd = choice(&labels, nearest, move |i| {
        if let Some((m, _)) = choices.get(i) {
            let _ = sill_core::config::update(|c| c.auto_revert_minutes = Some(*m));
        }
    });
    ar_dd.set_tooltip_text(Some("What the agent changed outside git and outside pinned folders is reverted this long after its last write, once the session is idle. Keep in Changes stops the clock."));
    root.append(&row("Auto-revert", &ar_dd));

    // host terminal (Ctrl+Shift+H): a command line; empty = the first installed known terminal
    let term = gtk::Entry::new();
    term.set_hexpand(true);
    let cfg = sill_core::config::load();
    term.set_text(cfg.host_terminal.as_deref().unwrap_or(""));
    let detected = cfg.host_terminal().unwrap_or_else(|| "none found".into());
    term.set_placeholder_text(Some(&format!("auto: {detected}")));
    term.set_tooltip_text(Some(
        "Opened on the host with Ctrl+Shift+H or from the shell button's right-click menu, in the folder shown in the tree, else the session's first pinned folder, else home. {cwd} in the command line is replaced by that folder; it is the working directory anyway. Empty = the first installed terminal sill knows.",
    ));
    term.connect_changed(|e| {
        let t = e.text().trim().to_string();
        let _ = sill_core::config::update(|c| c.host_terminal = if t.is_empty() { None } else { Some(t) });
    });
    root.append(&row("Host terminal", &term));

    // desktop access for new sessions (SPEC §4.2): X11 / D-Bus — policy, on, off
    let home_path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let (pol_x11, pol_dbus) = sill_core::policy::Policy::load("default", &home_path).map(|p| (p.jail.x11, p.jail.dbus)).unwrap_or((false, false));
    let onoff = |b: bool| if b { "on" } else { "off" };
    let dbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    type Get = fn(&sill_core::config::Config) -> Option<bool>;
    type Set = fn(&mut sill_core::config::Config, Option<bool>);
    let items: [(&str, bool, Get, Set, &str); 2] = [
        ("X11", pol_x11, |c| c.desktop_x11, |c, v| c.desktop_x11 = v,
         "Give new sessions the X11 display: the agent can run and see GUI programs — and, as any X11 client, read keys and drive every window. A session's own menu (its tab glyph) can override this."),
        ("D-Bus", pol_dbus, |c| c.desktop_dbus, |c, v| c.desktop_dbus = v,
         "Give new sessions the D-Bus session bus: a full escape (systemd user units, keyring). A session's own menu (its tab glyph) can override this."),
    ];
    for (name, pol, get, set, tip) in items {
        let l = gtk::Label::new(Some(name));
        l.add_css_class("dim-label");
        let policy_label = format!("policy ({})", onoff(pol));
        let labels: Vec<&str> = vec![&policy_label, "on", "off"];
        let current = get(&sill_core::config::load());
        let dd = choice(&labels, Some(match current { None => 0, Some(true) => 1, Some(false) => 2 }), move |i| {
            let _ = sill_core::config::update(|c| set(c, match i { 1 => Some(true), 2 => Some(false), _ => None }));
        });
        dd.set_tooltip_text(Some(tip));
        dbox.append(&l);
        dbox.append(&dd);
    }
    root.append(&row("Desktop", &dbox));

    // standing instructions (SPEC §3.1): every agent gets them with its first prompt, whatever its type
    let instr = gtk::TextView::new();
    instr.set_wrap_mode(gtk::WrapMode::WordChar);
    instr.set_top_margin(4);
    instr.set_bottom_margin(4);
    instr.set_left_margin(6);
    instr.set_right_margin(6);
    instr.buffer().set_text(&sill_core::instructions::load_global());
    let instr_scroll = gtk::ScrolledWindow::builder().child(&instr).min_content_height(96).hexpand(true).build();
    instr_scroll.add_css_class("frame");
    instr_scroll.set_tooltip_text(Some(
        "Standing instructions for every agent, whatever its type, in Markdown: sent with the first prompt after the agent starts and again with the next prompt after an edit — not with every prompt. Sill puts its own line about the jail (managed processes) before them. Stored in ~/.config/sill/instructions.md, which the agent cannot see.",
    ));
    let instr_msg = gtk::Label::new(None);
    instr_msg.add_css_class("dim-label");
    instr_msg.set_xalign(0.0);
    {
        let msg = instr_msg.clone();
        instr.buffer().connect_changed(move |b| {
            let text = b.text(&b.start_iter(), &b.end_iter(), false);
            match sill_core::instructions::save_global(&text) {
                Ok(_) => msg.set_text(""),
                Err(e) => msg.set_text(&format!("save failed: {e:#}")),
            }
        });
    }
    let ibox = gtk::Box::new(gtk::Orientation::Vertical, 4);
    ibox.set_hexpand(true);
    ibox.append(&instr_scroll);
    ibox.append(&instr_msg);
    root.append(&row("Instructions", &ibox));

    // width
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 0.8, 0.02);
    scale.set_value(x11::width_fraction());
    scale.set_hexpand(true);
    scale.set_draw_value(true);
    scale.set_digits(2);
    scale.connect_value_changed(|s| {
        x11::set_width_fraction(s.value());
        let _ = sill_core::config::update(|c| c.width = Some(s.value()));
    });
    scale.set_tooltip_text(Some("Fraction of the primary monitor's work area; the file tree adds its own share while open. Applies the next time the shelf is shown."));
    root.append(&row("Width", &scale));

    // the page the "sill" button opens
    let page_ids: Vec<&str> = sill_core::config::PAGES.iter().map(|(id, _)| *id).collect();
    let page_titles: Vec<&str> = sill_core::config::PAGES.iter().map(|(_, t)| *t).collect();
    let current_page = sill_core::config::load().default_page().to_string();
    let page_dd = choice(&page_titles, page_ids.iter().position(|id| *id == current_page), move |i| {
        if let Some(id) = page_ids.get(i) {
            let _ = sill_core::config::update(|c| c.default_page = Some(id.to_string()));
        }
    });
    root.append(&row("Default page", &page_dd));

    // show/hide key: click the button, press the combination you want
    let current = sill_core::config::load().hotkey.unwrap_or_else(|| super::hotkey::DEFAULT.to_string());
    let hk = gtk::Button::with_label(&pretty(&current));
    hk.add_css_class("sill-hotkey");
    hk.set_tooltip_text(Some("Click, then press the combination you want (Escape cancels). Something with a modifier plus a key is safest; a bare F-key also works."));
    let hk_msg = gtk::Label::new(None);
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
                    let _ = sill_core::config::update(|c| c.hotkey = Some(spec.clone()));
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
        ("Instructions".into(), format!("{home}/.config/sill/instructions.md — the box above; per agent type and per session come later")),
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
        ("Keys".into(), "hotkey show/hide · Escape sessions ⇄ backend · Enter send · Shift+Enter newline · Ctrl+T shell · Ctrl+Shift+H host shell · Ctrl+Shift+T / W new / close tab · Ctrl+Shift+F tree · F2 or double-click rename".into()),
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

/// A settings row: heading label of fixed width, then the control.
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

/// A dropdown over fixed options; `on_select` gets the chosen index when the person changes it
/// (not for the initial selection).
fn choice(options: &[&str], selected: Option<usize>, on_select: impl Fn(usize) + 'static) -> gtk::DropDown {
    let dd = gtk::DropDown::from_strings(options);
    if let Some(i) = selected {
        dd.set_selected(i as u32);
    }
    dd.connect_selected_notify(move |d| on_select(d.selected() as usize));
    dd
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
