//! Backend page "Settings" (SPEC §3.1): theme, text size, width, default page, hotkey (saved to
//! ~/.config/sill/sill.toml), plus where things are.
//!
//! The page is sections — Appearance, Shelf, Sessions, Instructions, Memory, then Where things
//! are folded — each a title over a card of rows (label and a short dim line on the left, the
//! control on the right; wide controls under their label), held to a reading width.
//!
//! Adding a parameter: a field in `sill_core::config::Config`, a row here in the section it belongs
//! to, built with [`row`] or [`wide`] and [`choice`] (or another widget), saved through
//! `config::update`, and applied live if it can be.

use std::path::PathBuf;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use super::{theme, x11};

/// Width the page is held to, logical px at text scale 1: the cards stay a column on a wide shelf.
const MEASURE: f64 = 680.0;

/// The page's own rules: section titles, cards, rows. Colours by libadwaita's names, so they
/// follow the theme (see `theme::named_colors`).
pub const CSS: &str = r#"
.sill-settings-title { color: @accent_color; font-weight: bold; font-size: 0.85em; margin-left: 2px; }
.sill-settings-card { background: @card_bg_color; }
.sill-settings-row { padding: 8px 12px; }
.sill-settings-next { border-top: 1px solid alpha(@view_fg_color, 0.08); }
.sill-settings-sub { font-size: 0.85em; }
.sill-settings-card scrolledwindow.frame { background: @view_bg_color; border: 1px solid @borders; }
.sill-settings dropdown > button { min-height: 0; padding: 3px 8px; }
.sill-settings-info { font-size: 0.9em; }
"#;

pub fn build() -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 18);
    root.add_css_class("sill-settings");
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(12);
    root.set_margin_bottom(16);

    // ---- Appearance: theme, text size, width
    let (sec, card) = section("Appearance");

    let themes = theme::builtin();
    let names: Vec<&str> = themes.iter().map(|t| t.name).collect();
    let current = theme::current().id;
    let dropdown = choice(&names, themes.iter().position(|t| t.id == current), move |i| {
        if let Some(t) = themes.get(i) {
            theme::apply(t);
            let _ = sill_core::config::update(|c| c.theme = t.id.to_string());
        }
    });
    row(&card, "Theme", None, &dropdown);

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
    row(&card, "Text size", None, &size_dd);

    // width: share of the monitor with the tree open
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 0.8, 0.02);
    scale.set_value(x11::width_fraction());
    scale.set_hexpand(true);
    scale.set_draw_value(true);
    scale.set_digits(2);
    scale.connect_value_changed(|s| {
        x11::set_width_fraction(s.value());
        let _ = sill_core::config::update(|c| c.width = Some(s.value()));
    });
    scale.set_tooltip_text(Some("Fraction of the primary monitor's work area for the shelf with the file tree open (drag the line between them for the tree's share); with the tree closed the shelf is that much narrower. Applies the next time the shelf is shown."));
    wide(&card, "Width", Some("Share of the monitor, with the file tree open"), &scale);
    root.append(&sec);

    // ---- Shelf: hotkey, default page, tray
    let (sec, card) = section("Shelf");

    // show/hide key: click the button, press the combination you want; the outcome on the dim line
    let current = sill_core::config::load().hotkey.unwrap_or_else(|| super::hotkey::DEFAULT.to_string());
    let hk = gtk::Button::with_label(&pretty(&current));
    hk.add_css_class("sill-hotkey");
    hk.set_tooltip_text(Some("Click, then press the combination you want (Escape cancels). Something with a modifier plus a key is safest; a bare F-key also works."));
    let hk_msg = row(&card, "Hotkey", Some("Shows or hides the shelf — click, then press the keys"), &hk);
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

    // the page the "sill" button opens
    let page_ids: Vec<&str> = sill_core::config::PAGES.iter().map(|(id, _)| *id).collect();
    let page_titles: Vec<&str> = sill_core::config::PAGES.iter().map(|(_, t)| *t).collect();
    let current_page = sill_core::config::load().default_page().to_string();
    let page_dd = choice(&page_titles, page_ids.iter().position(|id| *id == current_page), move |i| {
        if let Some(id) = page_ids.get(i) {
            let _ = sill_core::config::update(|c| c.default_page = Some(id.to_string()));
        }
    });
    row(&card, "Default page", Some("What the sill button opens"), &page_dd);

    // tray icon: a StatusNotifierItem on the panel (SPEC §4.6); registered at shelf start
    let tray = switch(sill_core::config::load().tray());
    tray.set_tooltip_text(Some("A sill icon on the panel's tray: a click shows or hides the shelf, its menu quits. Needs a panel that takes StatusNotifier items (Cinnamon, KDE, XFCE, MATE, LXQt; GNOME with the AppIndicator extension). Takes effect at the next shelf start."));
    tray.connect_state_set(|_, on| {
        let _ = sill_core::config::update(|c| c.tray = Some(on));
        glib::Propagation::Proceed
    });
    row(&card, "Tray icon", Some("Takes effect at the next shelf start"), &tray);
    root.append(&sec);

    // ---- Sessions: pins, desktop, auto-revert, host terminal
    let (sec, card) = section("Sessions");

    // remember pins: a new session starts with the latest pins (SPEC §3.2 Pins)
    let pins = switch(sill_core::config::load().remember_pins());
    pins.set_tooltip_text(Some("A new session starts with the pinned folders and files of the latest session that had any, so the workspace carries over. Off: a new session starts with no pins."));
    pins.connect_state_set(|_, on| {
        let _ = sill_core::config::update(|c| c.remember_pins = Some(on));
        glib::Propagation::Proceed
    });
    row(&card, "Remember pins", Some("A new session starts with the latest session's pins"), &pins);

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
    row(&card, "Desktop", Some("For new sessions; a session's own menu can override"), &dbox);

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
    row(&card, "Auto-revert", Some("Unkept changes outside git and pins go back after this long"), &ar_dd);

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
    wide(&card, "Host terminal", Some("Ctrl+Shift+H; empty = the first installed terminal sill knows"), &term);
    root.append(&sec);

    // ---- Instructions (SPEC §3.1): every agent gets them with its first prompt, whatever its type
    let (sec, card) = section("Instructions");
    let instr = gtk::TextView::new();
    instr.set_wrap_mode(gtk::WrapMode::WordChar);
    instr.set_top_margin(6);
    instr.set_bottom_margin(6);
    instr.set_left_margin(8);
    instr.set_right_margin(8);
    instr.buffer().set_text(&sill_core::instructions::load_global());
    let instr_scroll = gtk::ScrolledWindow::builder().child(&instr).min_content_height(96).hexpand(true).build();
    instr_scroll.add_css_class("frame");
    instr_scroll.set_tooltip_text(Some(
        "Standing instructions for every agent, whatever its type, in Markdown: sent with the first prompt after the agent starts and again with the next prompt after an edit — not with every prompt. Sill puts its own line about the jail (managed processes) before them. Stored in ~/.config/sill/instructions.md, which the agent cannot see.",
    ));
    let instr_msg = wide(&card, "", Some("For every agent, in Markdown: sent once after it starts and again after an edit"), &instr_scroll);
    {
        let msg = instr_msg.clone();
        instr.buffer().connect_changed(move |b| {
            let text = b.text(&b.start_iter(), &b.end_iter(), false);
            match sill_core::instructions::save_global(&text) {
                Ok(_) => msg.set_text("For every agent, in Markdown: sent once after it starts and again after an edit"),
                Err(e) => msg.set_text(&format!("save failed: {e:#}")),
            }
        });
    }
    root.append(&sec);

    // ---- Memory (docs/memory.md): summary, recall, compaction, the actions
    let (sec, card) = section("Memory");

    let memory = switch(sill_core::config::load().memory());
    memory.set_tooltip_text(Some("When a session's tab is closed, its agent writes a short summary as its last turn (what was asked, done, decided, left open), kept with the session and shown in the Reopen list; sill writes one from the facts when the agent can't be asked. Off: sessions end without a summary, as before."));
    memory.connect_state_set(|_, on| {
        let _ = sill_core::config::update(|c| c.memory = Some(on));
        super::window::refresh_memory(); // written, or removed with memory off
        glib::Propagation::Proceed
    });
    row(&card, "Memory", Some("A summary when a session's tab closes"), &memory);

    // how many earlier sessions a new agent is told about (docs/memory.md §2.3)
    let counts: [(u32, &str); 5] = [(0, "none"), (1, "1 session"), (3, "3 sessions"), (5, "5 sessions"), (10, "10 sessions")];
    let labels: Vec<&str> = counts.iter().map(|(_, l)| *l).collect();
    let current = sill_core::config::load().memory_recall() as u32;
    let nearest = counts.iter().enumerate().min_by_key(|(_, (n, _))| (*n as i64 - current as i64).abs()).map(|(i, _)| i);
    let recall_dd = choice(&labels, nearest, move |i| {
        if let Some((n, _)) = counts.get(i) {
            let _ = sill_core::config::update(|c| c.memory_recall = Some(*n));
        }
    });
    recall_dd.set_tooltip_text(Some("How many earlier sessions a new agent is told about when it starts: the ones in the same folders, nearest first, else the latest anywhere. Each session can narrow this in its session menu. None: the agent is only told how to search."));
    row(&card, "Recall", Some("Earlier sessions a new agent is told about"), &recall_dd);

    // compaction and retention (docs/memory.md §2.4): three durations, each off at 0
    let durations: [(&str, &str); 7] = [("0", "never"), ("7d", "a week"), ("30d", "a month"), ("90d", "3 months"), ("180d", "6 months"), ("1y", "a year"), ("2y", "two years")];
    let dur_labels: Vec<&str> = durations.iter().map(|(_, l)| *l).collect();
    let duration_row = |label: &'static str, sub: &'static str, tip: &'static str, current: i64, set: fn(&mut sill_core::config::Config, Option<String>)| {
        let nearest = durations
            .iter()
            .enumerate()
            .min_by_key(|(_, (d, _))| (sill_memory::parse_age(d).unwrap_or(0) - current).abs())
            .map(|(i, _)| i);
        let dd = choice(&dur_labels, nearest, move |i| {
            if let Some((d, _)) = durations.get(i) {
                let _ = sill_core::config::update(|c| set(c, Some(d.to_string())));
            }
        });
        dd.set_tooltip_text(Some(tip));
        row(&card, label, Some(sub), &dd);
    };
    let cfg = sill_core::config::load();
    duration_row(
        "Compact after",
        "After the end: thoughts, tool outputs and diffs go",
        "This long after a session ended, its transcript loses the agent's thoughts, tool outputs and diffs; the conversation still reads. Nightly.",
        cfg.compact_after(),
        |c, v| c.compact_after = v,
    );
    duration_row(
        "Full text for",
        "Then the agent's messages go too; summary, notes and prompts stay",
        "This long after the end, the agent's messages go too. The summary, your notes, your prompts and the tool titles stay — what recall and search use.",
        cfg.keep_full_transcripts(),
        |c, v| c.keep_full_transcripts = v,
    );
    duration_row(
        "Delete after",
        "Then the session is deleted outright",
        "This long after the end the session is deleted outright, as the trash can does: row, summary, notes, transcript, kept journal. Never, unless this machine needs an end date.",
        cfg.delete_after(),
        |c, v| c.delete_after = v,
    );

    // compact now / export / delete all: the actions, a row of their own
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let compact = gtk::Button::with_label("Compact now");
    compact.set_tooltip_text(Some("Run compaction and retention now, by the durations above."));
    compact.connect_clicked(|b| {
        let b = b.clone();
        b.set_sensitive(false);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // the daemon's (docs/daemon.md §7 M1c): compaction, retention, vacuum, the snapshot
            let _ = tx.send(super::client::compact());
        });
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || match rx.try_recv() {
            Ok(r) => {
                b.set_sensitive(true);
                b.set_label(&match r {
                    Ok(d) => format!("Compacted: {} / {} trimmed / {} deleted", d.compacted, d.trimmed, d.deleted),
                    Err(e) => format!("Compaction failed: {e}"),
                });
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => {
                b.set_sensitive(true);
                glib::ControlFlow::Break
            }
        });
    });
    actions.append(&compact);
    let export = gtk::Button::with_label("Export…");
    export.set_tooltip_text(Some("Write every session as a Markdown file — facts, summary, notes, the conversation — into a folder you pick: your data in a form that outlives sill."));
    export.connect_clicked(|b| {
        let dialog = gtk::FileDialog::builder().title("Export memory to a folder").modal(true).build();
        let parent = b.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        let b = b.clone();
        dialog.select_folder(parent.as_ref(), None::<&gtk::gio::Cancellable>, move |res| {
            let Ok(f) = res else { return };
            let Some(dir) = f.path() else { return };
            let r = sill_core::store::Store::open_default().and_then(|store| {
                let home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default();
                sill_core::memory::export_all(&store, &dir, &home)
            });
            b.set_label(&match r {
                Ok(n) => format!("Exported {n} sessions"),
                Err(e) => format!("Export failed: {e}"),
            });
        });
    });
    actions.append(&export);
    // delete every ended session: one click arms it, the next does it (was a row in the "+"
    // menu, too close to the Reopen list for its weight); open tabs are not ended, so they stay
    let delete = gtk::Button::with_label("Delete all…");
    delete.set_tooltip_text(Some("Delete every ended session with its summary, notes, transcript and recorded changes. Sessions open in a tab stay. Click twice."));
    delete.connect_clicked(|b| {
        if !b.has_css_class("destructive-action") {
            let n = sill_core::store::Store::open_default().and_then(|s| s.sessions()).map(|v| v.iter().filter(|s| s.ended_at.is_some()).count()).unwrap_or(0);
            if n == 0 {
                b.set_label("Nothing to delete");
                return;
            }
            b.add_css_class("destructive-action");
            b.set_label(&format!("Delete {n} sessions?"));
            return;
        }
        b.remove_css_class("destructive-action");
        b.set_sensitive(false);
        let b = b.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = super::client::forget_ended();
            let _ = tx.send(r);
        });
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || match rx.try_recv() {
            Ok(r) => {
                b.set_sensitive(true);
                b.set_label(&match r {
                    Ok(n) => format!("Deleted {n} sessions"),
                    Err(e) => format!("Delete failed: {e}"),
                });
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => {
                b.set_sensitive(true);
                glib::ControlFlow::Break
            }
        });
    });
    actions.append(&delete);
    plain(&card, &actions);
    root.append(&sec);

    // ---- Where things are: reference, folded — a title that opens a card
    let home = std::env::var("HOME").unwrap_or_default();
    let info: Vec<(String, String)> = vec![
        ("Config".into(), format!("{home}/.config/sill/sill.toml")),
        ("Instructions".into(), format!("{home}/.config/sill/instructions.md — the box above; per agent type and per session come later")),
        ("Policy".into(), "default — /etc/sill/policies/default.toml, then ~/.config/sill/policies/, then built-in".into()),
        ("Agents".into(), "the Agents page: install from the ACP registry, the gear edits command, environment, domains (~/.config/sill/agents/*.toml)".into()),
        ("Sessions".into(), format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/")),
        ("Daemon".into(), "sill daemon status — sessions are its children in scopes sill-<id>.scope under sill.slice (systemctl --user list-units 'sill-*.scope'); logs: journalctl --user -t sill-<id>, or $XDG_RUNTIME_DIR/sill/<id>/jail.log without journald".into()),
        ("Keys".into(), "hotkey show/hide · Escape sessions ⇄ backend · Enter send · Shift+Enter newline · Ctrl+T shell · Ctrl+Shift+H host shell · Ctrl+Shift+T / W new / close tab · Ctrl+Shift+F tree · F2 or double-click rename".into()),
    ];
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("sill-settings-card");
    card.set_margin_top(6);
    for (k, v) in info {
        let r = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        r.add_css_class("sill-settings-row");
        r.add_css_class("sill-settings-info");
        if card.first_child().is_some() {
            r.add_css_class("sill-settings-next");
        }
        let kl = gtk::Label::new(Some(&k));
        kl.add_css_class("dim-label");
        kl.set_xalign(0.0);
        kl.set_yalign(0.0);
        kl.set_width_chars(11);
        let vl = gtk::Label::new(Some(&v));
        vl.set_xalign(0.0);
        vl.set_wrap(true);
        vl.set_selectable(true);
        vl.set_hexpand(true);
        r.append(&kl);
        r.append(&vl);
        card.append(&r);
    }
    let folded = gtk::Expander::new(None);
    let t = gtk::Label::new(Some("Where things are"));
    t.add_css_class("sill-settings-title");
    folded.set_label_widget(Some(&t));
    folded.set_child(Some(&card));
    root.append(&folded);

    // held to a reading width, re-measured when the text scale changes
    let clamp = super::clamp::Clamp::new(&root, (MEASURE * theme::text_scale()) as i32);
    {
        let weak = clamp.downgrade();
        theme::on_change(move |_| {
            if let Some(c) = weak.upgrade() {
                c.set_maximum_size((MEASURE * theme::text_scale()) as i32);
            }
        });
    }
    clamp.upcast()
}

/// A section: its title over an empty card. Rows go into the card with [`row`], [`wide`] or
/// [`plain`]; the whole goes into the page.
fn section(title: &str) -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let t = gtk::Label::new(Some(title));
    t.add_css_class("sill-settings-title");
    t.set_xalign(0.0);
    outer.append(&t);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("sill-settings-card");
    outer.append(&card);
    (outer, card)
}

/// An empty row in a card: padding, a hairline above unless it is the first.
fn slot(card: &gtk::Box, orientation: gtk::Orientation, spacing: i32) -> gtk::Box {
    let r = gtk::Box::new(orientation, spacing);
    r.add_css_class("sill-settings-row");
    if card.first_child().is_some() {
        r.add_css_class("sill-settings-next");
    }
    card.append(&r);
    r
}

/// Label and a dim line under it (hidden while empty; the line is returned so a row can speak
/// through it — the hotkey's outcome, a save failure).
fn titles(label: &str, sub: Option<&str>) -> (gtk::Box, gtk::Label) {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.set_hexpand(true);
    col.set_valign(gtk::Align::Center);
    if !label.is_empty() {
        let l = gtk::Label::new(Some(label));
        l.set_xalign(0.0);
        col.append(&l);
    }
    let s = gtk::Label::new(sub);
    s.add_css_class("dim-label");
    s.add_css_class("sill-settings-sub");
    s.set_xalign(0.0);
    s.set_wrap(true);
    s.set_visible(sub.is_some_and(|t| !t.is_empty()));
    s.connect_label_notify(|l| l.set_visible(!l.label().is_empty()));
    col.append(&s);
    (col, s)
}

/// A row with a compact control on the right: label and dim line left, the control right.
fn row(card: &gtk::Box, label: &str, sub: Option<&str>, w: &impl IsA<gtk::Widget>) -> gtk::Label {
    let r = slot(card, gtk::Orientation::Horizontal, 12);
    let (col, s) = titles(label, sub);
    r.append(&col);
    w.set_halign(gtk::Align::End);
    w.set_valign(gtk::Align::Center);
    page_scrolls(w);
    r.append(w);
    s
}

/// A row whose control takes the width: label and dim line, the control under them.
fn wide(card: &gtk::Box, label: &str, sub: Option<&str>, w: &impl IsA<gtk::Widget>) -> gtk::Label {
    let r = slot(card, gtk::Orientation::Vertical, 6);
    let (col, s) = titles(label, sub);
    if label.is_empty() && sub.is_none() {
        col.set_visible(false);
    }
    r.append(&col);
    w.set_hexpand(true);
    if !w.is::<gtk::ScrolledWindow>() {
        page_scrolls(w); // a text box scrolls its own text
    }
    r.append(w);
    s
}

/// The wheel over a control scrolls the page, never the control: a scale would otherwise take the
/// wheel and move its value mid-scroll. The event is taken before the control sees it and the
/// page's scrolled window moved by hand (a wheel notch as GTK does it, by the page size).
fn page_scrolls(w: &impl IsA<gtk::Widget>) {
    let ctl = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = w.downgrade();
    ctl.connect_scroll(move |c, _dx, dy| {
        let Some(w) = weak.upgrade() else { return glib::Propagation::Proceed };
        let Some(page) = w.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>() else {
            return glib::Propagation::Proceed;
        };
        let adj = page.vadjustment();
        let step = if c.unit() == gdk::ScrollUnit::Wheel { adj.page_size().powf(2.0 / 3.0) } else { 1.0 };
        adj.set_value(adj.value() + dy * step);
        glib::Propagation::Stop
    });
    w.add_controller(ctl);
}

/// A row of just the widget (the Memory actions).
fn plain(card: &gtk::Box, w: &impl IsA<gtk::Widget>) {
    let r = slot(card, gtk::Orientation::Horizontal, 0);
    r.append(w);
}

/// A switch sized like the session menu's (`sill-control`), coloured by the theme (theme.rs).
fn switch(on: bool) -> gtk::Switch {
    let s = gtk::Switch::new();
    s.set_active(on);
    s.add_css_class("sill-control");
    s.set_valign(gtk::Align::Center);
    s
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
    let enabled = switch(agent.enabled);
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
