//! One session tab: conversation + bottom area (prompt now; prompt/shell switcher in M3). Owns the
//! driver, persists the transcript, counts pending changes at turn end.

use std::collections::HashMap;
use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use sill_core::acp::{Command, ConfigKind, ConfigValue, Controls, Event};
use sill_core::store::{Session, Store};

use super::conversation::Conversation;
use super::{session, theme};
use vte4::prelude::*;

pub struct SessionView {
    pub root: gtk::Box,
    prompt: gtk::TextView,
    /// Bottom slot: "prompt" or "shell" (SPEC §3.1 switcher).
    bottom: gtk::Stack,
    terminal: vte4::Terminal,
    term_scroll: gtk::ScrolledWindow,
    /// The shell's pty is opened on first use and kept while hidden.
    shell_open: std::cell::Cell<bool>,
    shell_btn: RefCell<Option<gtk::ToggleButton>>,
    pins_row: gtk::Box,
    /// Agent controls (mode, model, effort, toggles) + send button, under the prompt.
    controls_row: gtk::Box,
    /// Set while the controls are being rebuilt so their change signals don't echo back.
    rebuilding: std::cell::Cell<bool>,
    /// sill's own switch: answer permission requests with "allow once" without asking.
    auto_allow: std::cell::Cell<bool>,
    /// Send button; becomes a stop button while a turn is running.
    send_btn: RefCell<Option<gtk::Button>>,
    busy: std::cell::Cell<bool>,
    pub session: RefCell<Session>,
    pub conv: Rc<Conversation>,
    pub status: gtk::Label,
    driver: RefCell<Option<session::Driver>>,
    store: Rc<Store>,
    /// Markdown of the agent message being streamed (persisted at turn end).
    agent_md: RefCell<String>,
    /// Stored entry `seq` per live tool-call id, so title/status updates reach the transcript too.
    tool_seqs: RefCell<HashMap<String, i64>>,
    /// After a resume with replay: ignore replayed events until the first turn starts.
    replaying: RefCell<bool>,
    /// Called when a turn ends (the window refreshes the ledger pages).
    pub on_busy: RefCell<Option<Box<dyn Fn(&str, bool)>>>,
    pub on_turn_end: RefCell<Option<Box<dyn Fn(&str)>>>,
    pub on_title: RefCell<Option<Box<dyn Fn(&str, &str)>>>,
    /// Click on a pin chip: open the Files page scoped to that folder.
    pub on_open_folder: RefCell<Option<Box<dyn Fn(&std::path::Path)>>>,
    /// Shell toggle in the controls row (M3 wires the terminal pane to it).
    pub on_shell: RefCell<Option<Box<dyn Fn(bool)>>>,
    /// Pins that were live when the jail started; changed pins need a session restart.
    live_pins: RefCell<Vec<std::path::PathBuf>>,
    /// Watches on pinned repos' `.git` (commits, index) so the chip numbers follow host activity too.
    git_monitors: RefCell<Vec<gtk::gio::FileMonitor>>,
    refresh_pending: std::cell::Cell<bool>,
    pub on_restart: RefCell<Option<Box<dyn Fn(&str)>>>,
}

impl SessionView {
    pub fn new(session: Session, store: Rc<Store>) -> Rc<Self> {
        let status = gtk::Label::new(Some("starting…"));
        status.add_css_class("dim-label");
        let conv = Conversation::new(status.clone());

        let prompt = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(10)
            .right_margin(10)
            .top_margin(8)
            .bottom_margin(8)
            .accepts_tab(false)
            .build();
        prompt.add_css_class("sill-prompt");
        let prompt_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .max_content_height(220)
            .propagate_natural_height(true)
            .child(&prompt)
            .margin_start(0)
            .margin_end(0)
            .margin_bottom(0)
            .build();
        prompt_scroll.add_css_class("sill-prompt-box");

        let controls_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls_row.add_css_class("sill-controls");
        controls_row.set_margin_start(8);
        controls_row.set_margin_end(8);
        controls_row.set_margin_top(2);
        controls_row.set_margin_bottom(6);
        // the person's shell inside the jail (SPEC §3.1): same mounts as the agent, own pty
        let terminal = vte4::Terminal::new();
        terminal.set_size_request(-1, 200);
        terminal.set_scrollback_lines(10_000);
        terminal.set_font(Some(&gtk::pango::FontDescription::from_string("monospace 10")));
        terminal.set_scroll_on_output(false);
        terminal.set_scroll_on_keystroke(true);
        terminal.add_css_class("sill-terminal");
        let term_scroll = gtk::ScrolledWindow::builder()
            .child(&terminal)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .min_content_height(300)
            .build();
        let bottom = gtk::Stack::new();
        bottom.set_transition_type(gtk::StackTransitionType::Crossfade);
        // not homogeneous: the prompt keeps its own (small) height, the terminal its third of the shelf
        bottom.set_vhomogeneous(false);
        bottom.set_interpolate_size(true);
        bottom.add_named(&prompt_scroll, Some("prompt"));
        bottom.add_named(&term_scroll, Some("shell"));
        bottom.set_visible_child_name("prompt");

        let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        prompt_box.add_css_class("sill-prompt-box");
        prompt_scroll.remove_css_class("sill-prompt-box");
        prompt_box.append(&bottom);
        prompt_box.append(&controls_row);

        let pins_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pins_row.add_css_class("sill-pins");
        pins_row.set_margin_start(12);
        pins_row.set_margin_end(12);
        pins_row.set_margin_bottom(4);
        pins_row.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&pins_row);
        root.append(&conv.root);
        root.append(&prompt_box);

        let auto_allow_initial = session.auto_allow;
        let view = Rc::new(Self {
            root,
            prompt: prompt.clone(),
            bottom,
            terminal: terminal.clone(),
            term_scroll: term_scroll.clone(),
            shell_open: std::cell::Cell::new(false),
            shell_btn: RefCell::new(None),
            pins_row,
            controls_row,
            rebuilding: std::cell::Cell::new(false),
            auto_allow: std::cell::Cell::new(auto_allow_initial),
            send_btn: RefCell::new(None),
            busy: std::cell::Cell::new(false),
            session: RefCell::new(session),
            conv,
            status,
            driver: RefCell::new(None),
            store,
            agent_md: RefCell::new(String::new()),
            tool_seqs: RefCell::new(HashMap::new()),
            replaying: RefCell::new(false),
            on_busy: RefCell::new(None),
            on_turn_end: RefCell::new(None),
            on_title: RefCell::new(None),
            on_open_folder: RefCell::new(None),
            on_shell: RefCell::new(None),
            live_pins: RefCell::new(Vec::new()),
            git_monitors: RefCell::new(Vec::new()),
            refresh_pending: std::cell::Cell::new(false),
            on_restart: RefCell::new(None),
        });

        // stored transcript
        if let Ok(entries) = view.store.entries(&view.session.borrow().id) {
            for e in entries {
                match e.kind.as_str() {
                    "user" => view.conv.add_user(&e.text),
                    "agent" => view.conv.add_agent_block(&e.text),
                    "thought" => view.conv.add_thought_block(&e.text),
                    "tool" => view.conv.add_tool_call(format!("stored-{}", e.seq), &e.text, &e.extra),
                    _ => view.conv.add_note(&e.text),
                }
            }
        }

        // Enter sends, Shift+Enter inserts a newline
        let keys = gtk::EventControllerKey::new();
        {
            let v = view.clone();
            let prompt = prompt.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                if (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                    let buf = prompt.buffer();
                    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
                    if !text.is_empty() && v.send(&text) {
                        buf.set_text("");
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        prompt.add_controller(keys);

        view.refresh_pins();
        view.build_controls(&Controls::default());
        view.apply_terminal_theme(&theme::current());
        {
            let weak = Rc::downgrade(&view);
            theme::on_change(move |t| {
                if let Some(v) = weak.upgrade() {
                    v.apply_terminal_theme(t);
                }
            });
        }
        {
            // the shell toggle in the controls row drives the switcher
            let v = view.clone();
            *view.on_shell.borrow_mut() = Some(Box::new(move |on| v.show_shell(on)));
        }
        {
            // the pty is foreign (the shell's pid is in the jail's namespace, VTE can't watch it), so
            // the shell going away shows up as EOF on the master, not as child-exited. Detach the dead
            // pty and drop back to the prompt; the next toggle opens a fresh shell.
            let v = view.clone();
            terminal.connect_eof(move |_| v.shell_closed());
        }
        view.connect();
        view
    }

    /// Terminal colours from the theme palette: fg/bg plus the 16 ANSI slots mapped onto the theme's
    /// semantic colours (red=error, green=success, yellow=warning, blue=accent, cyan=link …).
    fn apply_terminal_theme(&self, t: &theme::Theme) {
        if t.bg.is_empty() {
            self.terminal.set_default_colors();
            return; // Adwaita: VTE's defaults follow the GTK theme
        }
        let parse = |c: &str| gtk::gdk::RGBA::parse(c).ok();
        let (Some(fg), Some(bg)) = (parse(t.fg), parse(t.bg)) else { return };
        let slots = [
            t.surface, t.error, t.success, t.warning, t.accent, t.accent, t.link, t.fg, // normal 0–7
            t.fg_dim, t.error, t.success, t.warning, t.accent, t.accent, t.link, t.fg, // bright 8–15
        ];
        let palette: Vec<gtk::gdk::RGBA> = slots.iter().filter_map(|c| parse(c)).collect();
        if palette.len() == 16 {
            let refs: Vec<&gtk::gdk::RGBA> = palette.iter().collect();
            self.terminal.set_colors(Some(&fg), Some(&bg), &refs);
        } else {
            self.terminal.set_colors(Some(&fg), Some(&bg), &[]);
        }
        if let Some(cursor) = parse(t.accent) {
            self.terminal.set_color_cursor(Some(&cursor));
        }
    }

    /// Switch the bottom slot between prompt and shell; open the shell's pty on first use.
    pub fn show_shell(self: &Rc<Self>, on: bool) {
        if let Some(b) = self.shell_btn.borrow().as_ref()
            && b.is_active() != on
        {
            b.set_active(on);
        }
        if !on {
            self.bottom.set_visible_child_name("prompt");
            self.prompt.grab_focus();
            return;
        }
        if !self.shell_open.get() {
            match self.open_shell() {
                Ok(()) => self.shell_open.set(true),
                Err(e) => {
                    self.conv.add_note(&format!("shell: {e:#}"));
                    self.show_shell(false);
                    return;
                }
            }
        }
        // a third of the shelf's height for the terminal (the scrolled window owns the size)
        let h = self.root.height();
        if h > 0 {
            self.term_scroll.set_min_content_height(h / 3);
        }
        self.bottom.set_visible_child_name("shell");
        self.terminal.grab_focus();
    }

    /// The shell's pty hit EOF (exit, Ctrl+D): forget it and show the prompt again.
    fn shell_closed(self: &Rc<Self>) {
        self.shell_open.set(false);
        self.terminal.set_pty(None);
        self.terminal.reset(true, true);
        self.show_shell(false);
    }

    pub fn toggle_shell(self: &Rc<Self>) {
        let on = self.bottom.visible_child_name().as_deref() != Some("shell");
        self.show_shell(on);
    }

    fn open_shell(&self) -> anyhow::Result<()> {
        let sock = sill_core::session::socket_path(&self.id())?;
        let mut c = sill_core::shim::Control::connect(&sock).map_err(|_| anyhow::anyhow!("session is not running"))?;
        let home = session::home()?;
        let cwd = self.store.pins(&self.id()).unwrap_or_default().into_iter().find(|p| p.is_dir()).unwrap_or(home);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let cols = self.terminal.column_count().max(20) as u16;
        let rows = self.terminal.row_count().max(5) as u16;
        let (fd, _pid) = c.open_pty(&[shell, "-l".into()], &std::collections::BTreeMap::new(), Some(&cwd), cols, rows)?;
        let pty = vte4::Pty::foreign_sync(fd, None::<&gtk::gio::Cancellable>)?;
        self.terminal.set_pty(Some(&pty));
        self.terminal.feed(b"\x1b[2J\x1b[H");
        Ok(())
    }

    /// Update and persist the remembered agent settings.
    fn remember(&self, f: impl FnOnce(&mut sill_core::store::AgentSettings)) {
        let mut s = self.session.borrow_mut();
        f(&mut s.agent_settings);
        let _ = self.store.set_agent_settings(&s.id, &s.agent_settings);
    }

    fn command(&self, c: Command) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(c);
        }
    }

    /// Rebuild the controls row from what the agent advertises: a dropdown per mode set / select
    /// option, a switch per boolean, and the send button on the right.
    fn build_controls(self: &Rc<Self>, c: &Controls) {
        self.rebuilding.set(true);
        while let Some(w) = self.controls_row.first_child() {
            self.controls_row.remove(&w);
        }
        // left: attach (later: file upload) and the shell toggle (M3); everything else sits right
        let attach = gtk::Button::from_icon_name("list-add-symbolic");
        attach.add_css_class("flat");
        attach.add_css_class("sill-ctl-icon");
        attach.set_tooltip_text(Some("Attach a file (coming)"));
        self.controls_row.append(&attach);
        let term = gtk::ToggleButton::new();
        term.set_icon_name("utilities-terminal-symbolic");
        term.add_css_class("flat");
        term.add_css_class("sill-ctl-icon");
        term.set_tooltip_text(Some("Shell inside this session's jail (Ctrl+`)"));
        term.set_active(self.bottom.visible_child_name().as_deref() == Some("shell"));
        *self.shell_btn.borrow_mut() = Some(term.clone());
        {
            let v = self.clone();
            term.connect_toggled(move |t| {
                if let Some(cb) = v.on_shell.borrow().as_ref() {
                    cb(t.is_active());
                }
            });
        }
        self.controls_row.append(&term);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        self.controls_row.append(&spacer);
        let dropdown = |names: &[String], current: usize| {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let d = gtk::DropDown::from_strings(&refs);
            d.add_css_class("sill-control");
            d.set_selected(current as u32);
            d
        };
        if let Some((current, modes)) = &c.modes {
            let names: Vec<String> = modes.iter().map(|(_, n)| n.clone()).collect();
            let cur = modes.iter().position(|(id, _)| id == current).unwrap_or(0);
            let d = dropdown(&names, cur);
            d.set_tooltip_text(Some("Mode"));
            let v = self.clone();
            let ids: Vec<String> = modes.iter().map(|(id, _)| id.clone()).collect();
            d.connect_selected_notify(move |d| {
                if v.rebuilding.get() {
                    return;
                }
                if let Some(id) = ids.get(d.selected() as usize) {
                    v.command(Command::SetMode(id.clone()));
                    v.remember(|s| s.mode = Some(id.clone()));
                }
            });
            self.controls_row.append(&d);
        }
        for opt in &c.options {
            // the mode set already covers a "mode"-category option (Claude Code sends both)
            if opt.category == "mode" && c.modes.is_some() {
                continue;
            }
            match &opt.kind {
                ConfigKind::Select(current, values) => {
                    let names: Vec<String> = values.iter().map(|(_, n)| n.clone()).collect();
                    let cur = values.iter().position(|(id, _)| id == current).unwrap_or(0);
                    let d = dropdown(&names, cur);
                    d.set_tooltip_text(Some(&opt.name));
                    let v = self.clone();
                    let ids: Vec<String> = values.iter().map(|(id, _)| id.clone()).collect();
                    let oid = opt.id.clone();
                    d.connect_selected_notify(move |d| {
                        if v.rebuilding.get() {
                            return;
                        }
                        if let Some(id) = ids.get(d.selected() as usize) {
                            v.command(Command::SetConfig(oid.clone(), ConfigValue::Id(id.clone())));
                            let (o, i) = (oid.clone(), id.clone());
                            v.remember(move |s| {
                                s.config.insert(o, i);
                            });
                        }
                    });
                    self.controls_row.append(&d);
                }
                ConfigKind::Boolean(on) => {
                    let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                    let l = gtk::Label::new(Some(&opt.name));
                    l.add_css_class("dim-label");
                    let sw = gtk::Switch::new();
                    sw.set_active(*on);
                    sw.add_css_class("sill-control");
                    sw.set_valign(gtk::Align::Center);
                    let v = self.clone();
                    let oid = opt.id.clone();
                    sw.connect_state_set(move |_, state| {
                        if !v.rebuilding.get() {
                            v.command(Command::SetConfig(oid.clone(), ConfigValue::Bool(state)));
                            let o = oid.clone();
                            v.remember(move |s| {
                                s.config.insert(o, state.to_string());
                            });
                        }
                        glib::Propagation::Proceed
                    });
                    b.append(&l);
                    b.append(&sw);
                    self.controls_row.append(&b);
                }
            }
        }
        // sill's own: auto-allow permission requests (the agent's "auto" modes still ask over ACP).
        // An open lock = requests are answered "allow once" by sill; closed = they wait for you.
        {
            let t = gtk::ToggleButton::new();
            t.add_css_class("flat");
            t.add_css_class("sill-ctl-icon");
            t.set_active(self.auto_allow.get());
            let set_look = |t: &gtk::ToggleButton| {
                if t.is_active() {
                    t.set_icon_name("changes-allow-symbolic");
                    t.set_tooltip_text(Some("Auto-allow is on: permission requests are answered \"allow once\" (each is still listed)"));
                    t.add_css_class("sill-unlocked");
                } else {
                    t.set_icon_name("changes-prevent-symbolic");
                    t.set_tooltip_text(Some("Auto-allow is off: permission requests wait for your answer"));
                    t.remove_css_class("sill-unlocked");
                }
            };
            set_look(&t);
            let v = self.clone();
            t.connect_toggled(move |t| {
                set_look(t);
                let state = t.is_active();
                v.auto_allow.set(state);
                let _ = v.store.set_auto_allow(&v.id(), state);
                v.session.borrow_mut().auto_allow = state;
            });
            self.controls_row.append(&t);
        }
        let send = gtk::Button::new();
        send.add_css_class("sill-send");
        {
            let v = self.clone();
            send.connect_clicked(move |_| {
                if v.busy.get() {
                    v.cancel();
                    return;
                }
                let buf = v.prompt.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
                if !text.is_empty() && v.send(&text) {
                    buf.set_text("");
                }
            });
        }
        self.controls_row.append(&send);
        *self.send_btn.borrow_mut() = Some(send);
        self.set_busy(self.busy.get());
        self.rebuilding.set(false);
    }

    /// Flip the send button between "send" and "stop" (SPEC: one button, red while running).
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        if let Some(b) = self.send_btn.borrow().as_ref() {
            if busy {
                b.set_label("■");
                b.set_tooltip_text(Some("Stop the agent"));
                b.add_css_class("sill-stop");
            } else {
                b.set_label("➤");
                b.set_tooltip_text(Some("Send (Enter)"));
                b.remove_css_class("sill-stop");
            }
        }
    }

    /// Ask the agent to stop the running turn.
    pub fn cancel(&self) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Cancel);
        }
    }

    /// Coalesced refresh of the chips (file monitors fire in bursts).
    fn refresh_pins_soon(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let v = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            v.refresh_pending.set(false);
            v.refresh_pins();
        });
    }

    /// Chips under the session bar, one per pinned folder, with an unpin ×.
    pub fn refresh_pins(self: &Rc<Self>) {
        while let Some(c) = self.pins_row.first_child() {
            self.pins_row.remove(&c);
        }
        let pins = self.store.pins(&self.id()).unwrap_or_default();
        // watch each pinned repo's .git (index/HEAD/refs change on commit, checkout, host or jail)
        self.git_monitors.borrow_mut().clear();
        for p in &pins {
            let git = p.join(".git");
            if !git.is_dir() {
                continue;
            }
            for dir in [git.clone(), git.join("refs/heads")] {
                if let Ok(m) = gtk::gio::File::for_path(&dir).monitor_directory(gtk::gio::FileMonitorFlags::NONE, None::<&gtk::gio::Cancellable>) {
                    let v = self.clone();
                    m.connect_changed(move |_, _, _, _| v.refresh_pins_soon());
                    self.git_monitors.borrow_mut().push(m);
                }
            }
        }
        self.pins_row.set_visible(!pins.is_empty());
        if pins.is_empty() {
            return;
        }
        let head = if gtk::IconTheme::for_display(&gtk::gdk::Display::default().expect("display")).has_icon("view-pin-symbolic") {
            gtk::Image::from_icon_name("view-pin-symbolic").upcast::<gtk::Widget>()
        } else {
            gtk::Label::new(Some("⌖")).upcast::<gtk::Widget>()
        };
        head.add_css_class("dim-label");
        head.set_tooltip_text(Some("pinned folders"));
        self.pins_row.append(&head);
        let home = std::env::var("HOME").unwrap_or_default();
        let mut summaries: Vec<(std::path::PathBuf, gtk::Box)> = Vec::new();
        for p in pins {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            chip.add_css_class("sill-chip");
            let l = gtk::Button::new();
            let lbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            lbox.append(&gtk::Label::new(Some(&p.to_string_lossy().replace(&home, "~"))));
            summaries.push((p.clone(), lbox.clone()));
            l.set_child(Some(&lbox));
            l.add_css_class("flat");
            l.add_css_class("sill-chip-open");
            l.set_tooltip_text(Some("Open in Files · +lines −lines vs HEAD, ?untracked files"));
            {
                let v = self.clone();
                let p = p.clone();
                l.connect_clicked(move |_| {
                    if let Some(cb) = v.on_open_folder.borrow().as_ref() {
                        cb(&p);
                    }
                });
            }
            let x = gtk::Button::from_icon_name("window-close-symbolic");
            x.add_css_class("flat");
            x.add_css_class("sill-tab-close");
            let v = self.clone();
            x.connect_clicked(move |_| {
                let _ = v.store.unpin(&v.id(), &p);
                v.refresh_pins();
                if let Some(cb) = v.on_restart.borrow().as_ref() {
                    cb(&v.id()); // pins are mounts: the jail is rebuilt (after the current turn if busy)
                }
            });
            chip.append(&l);
            chip.append(&x);
            self.pins_row.append(&chip);
        }
        // git working-tree summary next to each name: +added −deleted (vs HEAD), untracked count.
        // git runs off the main thread (it walks the repo; on the main thread it stalled painting and
        // scrolling for seconds); the chips' boxes are the handle, so a refresh in between — which
        // removes them from the row — makes a late result harmless.
        let (tx, rx) = async_channel::bounded::<Vec<Option<(u64, u64, u64)>>>(1);
        let paths: Vec<std::path::PathBuf> = summaries.iter().map(|(p, _)| p.clone()).collect();
        let boxes: Vec<gtk::Box> = summaries.into_iter().map(|(_, b)| b).collect();
        std::thread::spawn(move || {
            let found: Vec<Option<(u64, u64, u64)>> = paths.iter().map(|p| git_summary(p)).collect();
            let _ = tx.send_blocking(found);
        });
        glib::spawn_future_local(async move {
            let Ok(found) = rx.recv().await else { return };
            for (lbox, s) in boxes.iter().zip(found) {
                let Some((add, del, untracked)) = s else { continue };
                if lbox.parent().is_none() {
                    continue; // replaced by a newer refresh
                }
                if add > 0 {
                    let a = gtk::Label::new(Some(&format!("+{add}")));
                    a.add_css_class("sill-git-add");
                    lbox.append(&a);
                }
                if del > 0 {
                    let d = gtk::Label::new(Some(&format!("−{del}")));
                    d.add_css_class("sill-git-del");
                    lbox.append(&d);
                }
                if untracked > 0 {
                    let u = gtk::Label::new(Some(&format!("?{untracked}")));
                    u.add_css_class("dim-label");
                    lbox.append(&u);
                }
            }
        });
    }

    /// Pins the jail was started with, for deciding whether a rebuild is needed.
    pub fn pins_changed(&self) -> bool {
        self.store.pins(&self.id()).unwrap_or_default() != *self.live_pins.borrow()
    }

    /// Context block prepended to prompts while pins exist (SPEC §3.1 pins).
    fn pin_prefix(&self) -> String {
        let pins = self.store.pins(&self.id()).unwrap_or_default();
        if pins.is_empty() {
            return String::new();
        }
        let list: Vec<String> = pins.iter().map(|p| p.display().to_string()).collect();
        format!("[Pinned folders — focus your work there unless told otherwise: {}]\n\n", list.join(", "))
    }

    pub fn focus_prompt(&self) {
        self.prompt.grab_focus();
    }

    pub fn id(&self) -> String {
        self.session.borrow().id.clone()
    }

    fn connect(self: &Rc<Self>) {
        let s = self.session.borrow().clone();
        *self.live_pins.borrow_mut() = self.store.pins(&s.id).unwrap_or_default();
        match session::start(&s, &self.store) {
            Ok(d) => {
                let events = d.events.clone();
                *self.driver.borrow_mut() = Some(d);
                let v = self.clone();
                glib::spawn_future_local(async move {
                    while let Ok(ev) = events.recv().await {
                        v.handle(ev);
                    }
                });
            }
            Err(e) => {
                self.status.set_text("failed to start");
                self.conv.add_note(&format!("could not start the session: {e:#}"));
            }
        }
    }

    /// Send a prompt; false if the agent is busy or the session is down.
    pub fn send(&self, text: &str) -> bool {
        let trace = std::env::var_os("SILL_TRACE").is_some();
        let t0 = std::time::Instant::now();
        let lap = |what: &str| {
            if trace {
                eprintln!("{} send: {what} at {} ms", super::trace_ts(), t0.elapsed().as_millis());
            }
        };
        let Some(d) = self.driver.borrow().as_ref().map(|d| d.commands.clone()) else {
            self.conv.add_note("session is not running");
            return false;
        };
        let outgoing = format!("{}{}", self.pin_prefix(), text);
        lap("prefix");
        match d.try_send(Command::Prompt(outgoing)) {
            Ok(()) => {
                self.conv.add_user(text);
                lap("add_user");
                let id = self.id();
                let _ = self.store.append(&id, "user", text, "");
                let _ = self.store.touch(&id);
                lap("store append+touch");
                // first prompt names the session (keywords), unless it has nothing to say
                let first = self.store.count_kind(&id, "user").map(|n| n <= 1).unwrap_or(false);
                lap("store entries");
                if first && let Some(title) = sill_core::title::from_prompt(text) {
                    let _ = self.store.set_title(&id, &title);
                    self.session.borrow_mut().title = title.clone();
                    if let Some(cb) = self.on_title.borrow().as_ref() {
                        cb(&id, &title);
                    }
                }
                lap("done");
                true
            }
            Err(_) => {
                self.conv.add_note("the agent is busy; wait for the turn to end");
                false
            }
        }
    }

    fn flush_agent(&self) {
        let md = std::mem::take(&mut *self.agent_md.borrow_mut());
        if !md.trim().is_empty() {
            let _ = self.store.append(&self.id(), "agent", &md, "");
        }
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        let replaying = *self.replaying.borrow();
        match &ev {
            Event::Ready { session_id, resumed, controls } => {
                let _ = self.store.set_acp_session_id(&self.id(), session_id);
                self.session.borrow_mut().acp_session_id = Some(session_id.clone());
                *self.replaying.borrow_mut() = *resumed;
                self.status.set_text(if *resumed { "resumed" } else { "ready" });
                self.build_controls(controls);
                // bring the agent back to the settings the person last chose
                let remembered = self.session.borrow().agent_settings.clone();
                if let (Some(mode), Some((current, modes))) = (&remembered.mode, &controls.modes)
                    && mode != current
                    && modes.iter().any(|(id, _)| id == mode)
                {
                    self.command(Command::SetMode(mode.clone()));
                }
                for opt in &controls.options {
                    let Some(want) = remembered.config.get(&opt.id) else { continue };
                    match &opt.kind {
                        ConfigKind::Select(current, values) => {
                            if want != current && values.iter().any(|(id, _)| id == want) {
                                self.command(Command::SetConfig(opt.id.clone(), ConfigValue::Id(want.clone())));
                            }
                        }
                        ConfigKind::Boolean(current) => {
                            let w = want == "true";
                            if w != *current {
                                self.command(Command::SetConfig(opt.id.clone(), ConfigValue::Bool(w)));
                            }
                        }
                    }
                }
                return;
            }
            Event::Controls(c) => {
                self.build_controls(c);
                return;
            }
            Event::PermissionRequest { .. } if self.auto_allow.get() => {
                let Event::PermissionRequest { title, options, reply } = ev else { unreachable!() };
                use sill_core::acp::PermissionOptionKind as K;
                let pick = options
                    .iter()
                    .find(|o| o.kind == K::AllowOnce)
                    .or_else(|| options.iter().find(|o| o.kind == K::AllowAlways))
                    .map(|o| o.id.clone());
                let _ = reply.send(pick.clone());
                self.conv.add_note(&format!("auto-allowed: {title}{}", if pick.is_none() { " (no allow option; cancelled)" } else { "" }));
                return;
            }
            Event::TurnStarted => {
                *self.replaying.borrow_mut() = false;
                self.set_busy(true);
                if let Some(cb) = self.on_busy.borrow().as_ref() {
                    cb(&self.id(), true);
                }
            }
            Event::UserText(_) | Event::Text(_) | Event::Thought(_) | Event::ToolCall { .. } | Event::ToolCallUpdate { .. }
                if replaying =>
            {
                return; // the stored transcript is already on screen
            }
            Event::Text(t) => self.agent_md.borrow_mut().push_str(t),
            Event::ToolCall { id, title, status, .. } => {
                self.flush_agent();
                if let Ok(seq) = self.store.append(&self.id(), "tool", title, status) {
                    self.tool_seqs.borrow_mut().insert(id.clone(), seq);
                }
            }
            Event::ToolCallUpdate { id, status, title } => {
                if let Some(seq) = self.tool_seqs.borrow().get(id) {
                    let _ = self.store.update_tool(&self.id(), *seq, title.as_deref(), status.as_deref());
                }
            }
            Event::Stopped(_) => {
                self.set_busy(false);
                if let Some(cb) = self.on_busy.borrow().as_ref() {
                    cb(&self.id(), false);
                }
                self.flush_agent();
                let id = self.id();
                let v = self.clone();
                glib::idle_add_local_once(move || v.after_turn(&id));
            }
            _ => {}
        }
        self.conv.handle(ev);
    }

    /// Count the session's pending changes (SPEC §4.4) and tell the window to refresh the ledger pages.
    fn after_turn(self: &Rc<Self>, id: &str) {
        let s = self.session.borrow().clone();
        let store2 = Store::open_default().ok();
        let (tx, rx) = async_channel::bounded::<String>(1);
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        std::thread::spawn(move || {
            let msg = (|| -> anyhow::Result<String> {
                let policy = sill_core::policy::Policy::load(&s.policy, &home)?;
                let state_dir = sill_core::session::state_dir(uid, &s.id)?;
                let binds = match store2.as_ref() {
                    Some(st) => session::binds_of(&s, &policy, &home, st),
                    None => vec![],
                };
                let plan = sill_core::changes::session_plan(policy.clone(), state_dir, &home, uid, &user, binds)?;
                let changes = sill_core::changes::scan(&policy, &plan, None)?;
                let pending = changes.iter().filter(|c| !c.ignored).count();
                Ok(if pending > 0 { format!("{pending} change(s) pending in Changes") } else { String::new() })
            })();
            let _ = tx.send_blocking(msg.unwrap_or_else(|e| format!("ledger: {e:#}")));
        });
        let v = self.clone();
        let id = id.to_string();
        glib::spawn_future_local(async move {
            if let Ok(msg) = rx.recv().await {
                if !msg.is_empty() {
                    v.conv.add_note(&msg);
                }
                v.refresh_pins(); // git numbers on the chips
                if let Some(cb) = v.on_turn_end.borrow().as_ref() {
                    cb(&id);
                }
            }
        });
    }

    pub fn quit(&self) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Quit);
        }
    }
}

/// (+insertions, −deletions, untracked files) of a git working tree vs HEAD; None if not a repo.
fn git_summary(dir: &std::path::Path) -> Option<(u64, u64, u64)> {
    if !dir.join(".git").exists() {
        return None;
    }
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["diff", "--numstat", "HEAD"]).output().ok()?;
    let (mut add, mut del) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        add += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        del += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    }
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["ls-files", "--others", "--exclude-standard"]).output().ok()?;
    let untracked = String::from_utf8_lossy(&out.stdout).lines().count() as u64;
    Some((add, del, untracked))
}
