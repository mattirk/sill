//! The shelf window (SPEC §3.1): top bar with the action button (dropdown: Sessions / Changes / Files
//! / Settings), session tabs and the state; a stack of pages below. Placement and visibility (§4.6).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use adw::prelude::*;
use sill_core::agents;
use sill_core::store::{Session, Store};

use super::agents_page::AgentsPage;
use super::changes::ChangesPage;
use super::files::FilesPage;
use super::session_view::SessionView;
use super::tabs::TabStrip;
use super::{session, settings, theme, x11};

thread_local! {
    pub static HOTKEY: RefCell<Option<Rc<super::hotkey::Hotkey>>> = const { RefCell::new(None) };
}

pub struct Shelf {
    pub window: adw::ApplicationWindow,
    store: Rc<Store>,
    stack: gtk::Stack,
    action: gtk::ToggleButton,
    tab_area: gtk::Stack,
    /// Backend page tabs: (page id, tab box, icon slot).
    page_tabs: RefCell<Vec<(String, gtk::Box, gtk::Box)>>,
    /// Set while show_page syncs the action button, so its toggled signal doesn't loop back.
    syncing: std::cell::Cell<bool>,
    tabs: Rc<TabStrip>,
    pages: gtk::Stack,
    /// Top-bar indicator: a quiet square while idle, a spinner while the selected agent works; the
    /// status text is its tooltip.
    status: gtk::Stack,
    status_text: RefCell<String>,
    spinner: gtk::Spinner,
    busy: RefCell<std::collections::HashSet<String>>,
    pending_restart: RefCell<std::collections::HashSet<String>>,
    views: RefCell<HashMap<String, Rc<SessionView>>>,
    changes: Rc<ChangesPage>,
    agents: Rc<AgentsPage>,
    files: Rc<FilesPage>,
}

pub fn build(app: &adw::Application, _cwd: PathBuf) -> gtk::Window {
    let config = sill_core::config::load();
    super::theme::set_text_scale(config.text_scale()); // applies the theme too
    super::theme::apply(&super::theme::by_id(&config.theme));
    if let Some(w) = config.width {
        x11::set_width_fraction(w);
    }
    let store = Rc::new(Store::open_default().expect("open session store"));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("sill")
        .decorated(false)
        .default_width(700)
        .default_height(1000)
        .build();

    // --- top bar: action button, tabs, "+", status
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_start(8);
    bar.set_margin_end(12);
    bar.set_margin_top(6);
    bar.set_margin_bottom(2);

    // "sill" flips between the sessions and the backend: pressed = backend, with the page tabs in
    // place of the session tabs
    let action = gtk::ToggleButton::with_label("sill");
    action.add_css_class("heading");
    action.add_css_class("flat");
    action.add_css_class("sill-action");
    action.set_tooltip_text(Some("Sessions ⇄ backend (Escape)"));
    bar.append(&action);

    let tabs = TabStrip::new();
    let page_tabs_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    page_tabs_box.add_css_class("sill-tabs");
    let tab_area = gtk::Stack::new();
    tab_area.set_hhomogeneous(false);
    tab_area.set_hexpand(true);
    tab_area.add_named(&tabs.root, Some("sessions"));
    tab_area.add_named(&page_tabs_box, Some("backend"));
    bar.append(&tab_area);
    let pages = gtk::Stack::new();
    pages.set_vexpand(true);

    let plus = gtk::MenuButton::new();
    plus.set_icon_name("list-add-symbolic");
    plus.add_css_class("flat");
    plus.set_tooltip_text(Some("New session"));
    let plus_pop = gtk::Popover::new();
    plus_pop.add_css_class("sill-menu");
    plus_pop.set_has_arrow(false);
    let plus_menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    plus_pop.set_child(Some(&plus_menu));
    plus.set_popover(Some(&plus_pop));
    bar.append(&plus);

    let files_btn = gtk::Button::from_icon_name("folder-symbolic");
    files_btn.add_css_class("flat");
    files_btn.set_tooltip_text(Some("Files"));
    bar.append(&files_btn);

    let spinner = gtk::Spinner::new();
    spinner.add_css_class("sill-spinner");
    spinner.set_valign(gtk::Align::Center);
    let idle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    idle.add_css_class("sill-status");
    idle.set_valign(gtk::Align::Center);
    idle.set_halign(gtk::Align::Center);
    let status = gtk::Stack::new();
    status.set_hhomogeneous(true);
    status.set_vhomogeneous(true);
    status.add_named(&idle, Some("idle"));
    status.add_named(&spinner, Some("busy"));
    status.set_valign(gtk::Align::Center);
    status.add_css_class("sill-status-slot");
    bar.append(&status);

    // --- pages
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_vexpand(true);
    stack.add_named(&pages, Some("sessions"));
    let changes = ChangesPage::new(store.clone());
    let files = FilesPage::new(store.clone());
    let agents_page = AgentsPage::new();
    let settings_page = settings::build();
    // backend pages: the page tabs in the bar say where you are, "sill" or Escape goes back
    for (name, body) in [
        ("changes", changes.root.clone().upcast::<gtk::Widget>()),
        ("files", files.root.clone().upcast::<gtk::Widget>()),
        ("agents", agents_page.root.clone().upcast::<gtk::Widget>()),
        ("settings", settings_page.clone().upcast::<gtk::Widget>()),
    ] {
        body.set_margin_top(6);
        stack.add_named(&body, Some(name));
    }

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&bar);
    column.append(&stack);
    window.set_content(Some(&column));

    let shelf = Rc::new(Shelf {
        window: window.clone(),
        store: store.clone(),
        stack: stack.clone(),
        action: action.clone(),
        tab_area: tab_area.clone(),
        page_tabs: RefCell::new(Vec::new()),
        syncing: std::cell::Cell::new(false),
        tabs: tabs.clone(),
        pages: pages.clone(),
        status: status.clone(),
        status_text: RefCell::new(String::new()),
        spinner: spinner.clone(),
        busy: RefCell::new(std::collections::HashSet::new()),
        pending_restart: RefCell::new(std::collections::HashSet::new()),
        views: RefCell::new(HashMap::new()),
        changes: changes.clone(),
        agents: agents_page.clone(),
        files: files.clone(),
    });
    {
        // drawn glyphs don't follow CSS: redraw the tab icons in the new theme's foreground
        let weak = Rc::downgrade(&shelf);
        theme::on_change(move |_| {
            if let Some(s) = weak.upgrade() {
                s.page_tab_icons();
                let sessions = s.store.sessions().unwrap_or_default();
                for id in s.tabs.ids() {
                    if let Some(sess) = sessions.iter().find(|x| x.id == id) {
                        s.tabs.set_icon(&id, &s.tab_icon(&sess.agent));
                    }
                }
            }
        });
    }

    {
        // Files: open it, or step back to the session when it is already open
        let s = shelf.clone();
        files_btn.connect_clicked(move |_| {
            if s.stack.visible_child_name().as_deref() == Some("files") {
                s.show_page("sessions");
            } else {
                s.show_page("files");
            }
        });
    }
    {
        let s = shelf.clone();
        *files.on_pin.borrow_mut() = Some(Box::new(move |sid| {
            if let Some(v) = s.views.borrow().get(sid) {
                v.refresh_pins();
            }
            s.restart_session(sid);
        }));
    }
    // the page tabs (built once, glyphs redrawn with the theme) and the "sill" toggle
    shelf.build_page_tabs(&page_tabs_box);
    {
        let s = shelf.clone();
        action.connect_toggled(move |b| {
            if s.syncing.get() {
                return;
            }
            if b.is_active() {
                let page = sill_core::config::load().default_page().to_string();
                s.show_page(&page);
            } else {
                s.show_page("sessions");
            }
        });
    }

    // "+" entries: one per agent, plus stored sessions that aren't open
    {
        let s = shelf.clone();
        let plus_menu = plus_menu.clone();
        plus_pop.connect_show(move |pop| s.fill_plus_menu(&plus_menu, pop));
    }

    // tab strip callbacks
    {
        let s = shelf.clone();
        *tabs.on_select.borrow_mut() = Some(Box::new(move |id| {
            s.pages.set_visible_child_name(id);
            *s.files.pin_target.borrow_mut() = Some(id.to_string());
            s.sync_status();
            s.focus_prompt();
        }));
    }
    {
        let s = shelf.clone();
        *tabs.on_rename.borrow_mut() = Some(Box::new(move |id, title| {
            let _ = s.store.set_title(id, title);
            if let Some(v) = s.views.borrow().get(id) {
                v.session.borrow_mut().title = title.to_string();
            }
        }));
    }
    {
        // closing a tab: quit its driver and stop its unit; the stored session stays (reopen from "+")
        let s = shelf.clone();
        *tabs.on_close.borrow_mut() = Some(Box::new(move |id| s.close_session(id)));
    }
    {
        let s = shelf.clone();
        let f2 = gtk::EventControllerKey::new();
        f2.connect_key_pressed(move |_, key, _, state| {
            if key == gdk::Key::F2 {
                s.tabs.edit_selected();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::t && state.contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(id) = s.tabs.selected()
                    && let Some(v) = s.views.borrow().get(&id)
                {
                    v.toggle_shell();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        f2.set_propagation_phase(gtk::PropagationPhase::Capture); // works from inside the terminal too
        window.add_controller(f2);
    }

    // open the most recent session, or create one with claude
    {
        let sessions = store.sessions().unwrap_or_default();
        match sessions.into_iter().next() {
            Some(s) => shelf.open_session(s),
            None => {
                if let Ok(s) = store.create("claude", "default") {
                    shelf.open_session(s);
                }
            }
        }
    }

    // back from the host (editor, terminal): refresh the pin chips' git numbers
    {
        let s = shelf.clone();
        window.connect_is_active_notify(move |w| {
            if w.is_active()
                && let Some(id) = s.tabs.selected()
                && let Some(v) = s.views.borrow().get(&id)
            {
                v.refresh_pins();
            }
        });
    }

    // Escape does what the "sill" button does: sessions ⇄ backend (hiding is the hotkey's job)
    let esc = gtk::EventControllerKey::new();
    {
        let s = shelf.clone();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if s.stack.visible_child_name().as_deref() != Some("sessions") {
                    s.show_page("sessions");
                } else {
                    let page = sill_core::config::load().default_page().to_string();
                    s.show_page(&page);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(esc);

    // geometry before the first frame: on realize the X window exists but isn't mapped yet, so
    // the window manager shows it straight at the left edge, final size, no visible jump
    {
        let s = shelf.clone();
        window.connect_realize(move |w| {
            let w: gtk::Window = w.clone().upcast();
            let fraction = if s.stack.visible_child_name().as_deref() == Some("files") {
                x11::width_fraction().max(0.5)
            } else {
                x11::width_fraction()
            };
            if let Some(area) = x11::work_area_with(&w, fraction) {
                w.set_default_size(area.width as i32, area.height as i32);
                x11::pre_place(&w, &area);
            }
        });
    }
    // GTK rewrites the size hints when it presents the surface (dropping ours), and its MapWindow
    // sits in Xlib's buffer until the frame ends; a synchronous handler on `map` gets geometry and
    // hints to the server ahead of it. Then once more after the map as a correction.
    {
        let s = shelf.clone();
        window.connect_map(move |w| {
            let w: gtk::Window = w.clone().upcast();
            let fraction = if s.stack.visible_child_name().as_deref() == Some("files") {
                x11::width_fraction().max(0.5)
            } else {
                x11::width_fraction()
            };
            if let Some(area) = x11::work_area_with(&w, fraction) {
                x11::pre_place(&w, &area);
            }
            let s = s.clone();
            glib::idle_add_local_once(move || {
                let fraction = if s.stack.visible_child_name().as_deref() == Some("files") {
                    x11::width_fraction().max(0.5)
                } else {
                    x11::width_fraction()
                };
                if let Some(area) = x11::work_area_with(&w, fraction) {
                    x11::place(&w, &area);
                }
                x11::activate(&w, 0);
                s.focus_prompt();
            });
        });
    }
    // global show/hide key, grabbed on the X server (configurable in Settings)
    {
        let mut spec = config.hotkey.clone().unwrap_or_else(|| super::hotkey::DEFAULT.to_string());
        if let Err(e) = super::hotkey::validate(&spec) {
            eprintln!("sill: saved hotkey {spec:?} rejected ({e}); using {}", super::hotkey::DEFAULT);
            spec = super::hotkey::DEFAULT.to_string();
        }
        let w = window.clone().upcast::<gtk::Window>();
        match super::hotkey::Hotkey::start(&spec, move |time| toggle(&w, time)) {
            Ok((hk, err)) => {
                if let Some(e) = err {
                    eprintln!("sill: hotkey {spec}: {e}");
                }
                HOTKEY.with(|h| *h.borrow_mut() = Some(Rc::new(hk)));
            }
            Err(e) => eprintln!("sill: hotkey: {e:#}"),
        }
    }
    // keep the Rc alive as long as the window
    unsafe {
        window.set_data("sill-shelf", shelf.clone());
    }
    window.upcast()
}

impl Shelf {
    fn show_page(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        let backend = name != "sessions";
        self.tab_area.set_visible_child_name(if backend { "backend" } else { "sessions" });
        for (id, tab, _) in self.page_tabs.borrow().iter() {
            if id == name {
                tab.add_css_class("selected");
            } else {
                tab.remove_css_class("selected");
            }
        }
        self.syncing.set(true);
        self.action.set_active(backend);
        self.syncing.set(false);
        // the Files page (tree + editor) needs room: half the work area while it is open
        let w: gtk::Window = self.window.clone().upcast();
        let fraction = if name == "files" { x11::width_fraction().max(0.5) } else { x11::width_fraction() };
        if let Some(area) = x11::work_area_with(&w, fraction) {
            x11::place(&w, &area);
        }
        match name {
            "changes" => self.changes.refresh(),
            "files" => self.files.refresh(),
            "agents" => self.agents.refresh(),
            "sessions" => self.focus_prompt(),
            _ => {}
        }
    }

    fn focus_prompt(&self) {
        if let Some(id) = self.tabs.selected()
            && let Some(v) = self.views.borrow().get(&id)
        {
            v.focus_prompt();
        }
    }

    /// One tab per backend page in the bar: glyph + title, styled like the session tabs.
    fn build_page_tabs(self: &Rc<Self>, strip: &gtk::Box) {
        for (id, title) in sill_core::config::PAGES {
            let tab = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            tab.add_css_class("sill-tab");
            let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            slot.add_css_class("sill-tab-icon");
            slot.set_valign(gtk::Align::Center);
            tab.append(&slot);
            let l = gtk::Label::new(Some(title));
            tab.append(&l);
            let click = gtk::GestureClick::new();
            let s = self.clone();
            click.connect_released(move |_, _, _, _| s.show_page(id));
            tab.add_controller(click);
            strip.append(&tab);
            self.page_tabs.borrow_mut().push((id.to_string(), tab, slot));
        }
        self.page_tab_icons();
    }

    /// (Re)draw the page tabs' glyphs in the strip's current foreground.
    fn page_tab_icons(&self) {
        let color = super::icons::fg_for(&self.tabs.root, &theme::current());
        for (id, _, slot) in self.page_tabs.borrow().iter() {
            while let Some(c) = slot.first_child() {
                slot.remove(&c);
            }
            slot.append(&super::icons::named(&format!("sill-{id}"), color, 12));
        }
    }

    /// The small agent glyph for a tab, in the strip's current text colour.
    fn tab_icon(&self, agent: &str) -> gtk::Image {
        let home = session::home().unwrap_or_default();
        let registry = agents::load(&home);
        let color = super::icons::fg_for(&self.tabs.root, &theme::current());
        super::icons::image(registry.get(agent), color, 12)
    }

    /// Spinner in the bar while the selected session's agent is working, a square otherwise.
    fn sync_spinner(&self) {
        let selected_busy = self.tabs.selected().map(|id| self.busy.borrow().contains(&id)).unwrap_or(false);
        self.status.set_visible_child_name(if selected_busy { "busy" } else { "idle" });
        self.spinner.set_spinning(selected_busy);
    }

    /// The selected session's status: shown on hover; states that need the person (login, a dead
    /// session, a failed start) tint the square.
    fn set_status(&self, text: &str) {
        *self.status_text.borrow_mut() = text.to_string();
        self.status.set_tooltip_text(if text.is_empty() { None } else { Some(text) });
        let attention = ["needed", "failed", "ended", "not running", "closed"].iter().any(|w| text.contains(w));
        if let Some(idle) = self.status.child_by_name("idle") {
            if attention {
                idle.add_css_class("sill-attention");
            } else {
                idle.remove_css_class("sill-attention");
            }
        }
    }

    fn sync_status(&self) {
        self.sync_spinner();
        if let Some(id) = self.tabs.selected()
            && let Some(v) = self.views.borrow().get(&id)
        {
            self.set_status(&v.status.text());
        }
    }

    fn open_session(self: &Rc<Self>, s: Session) {
        self.open_session_with(s, None);
    }

    /// Open a session; `note` is shown first in its conversation. A jail built before the policy
    /// file was last edited is stopped and rebuilt first, so policy edits (x11, dbus, masks…)
    /// take effect without closing anything by hand.
    fn open_session_with(self: &Rc<Self>, s: Session, note: Option<String>) {
        let id = s.id.clone();
        if self.tabs.has(&id) {
            self.tabs.select(&id);
            return;
        }
        let home = session::home().unwrap_or_default();
        if note.is_none() && sill_core::session::jail_outdated(&id, &s.policy, &home) {
            let shelf = self.clone();
            let s2 = s.clone();
            stop_then(&id, move || {
                shelf.open_session_with(
                    s2,
                    Some("policy changed since this jail was built: rebuilt it with the current policy".into()),
                )
            });
            return;
        }
        let view = SessionView::new(s.clone(), self.store.clone());
        if let Some(n) = &note {
            view.conv.add_note(n);
        }
        self.pages.add_named(&view.root, Some(&id));
        self.tabs.add(&id, &s.title);
        self.tabs.set_icon(&id, &self.tab_icon(&s.agent));
        {
            let shelf = self.clone();
            *view.on_turn_end.borrow_mut() = Some(Box::new(move |_id| {
                if shelf.stack.visible_child_name().as_deref() == Some("changes") {
                    shelf.changes.refresh();
                }
            }));
        }
        {
            let shelf = self.clone();
            *view.on_title.borrow_mut() = Some(Box::new(move |id, title| shelf.tabs.set_title(id, title)));
        }
        {
            let shelf = self.clone();
            *view.on_busy.borrow_mut() = Some(Box::new(move |id, busy| {
                shelf.tabs.set_busy(id, busy);
                if busy {
                    shelf.busy.borrow_mut().insert(id.to_string());
                } else {
                    shelf.busy.borrow_mut().remove(id);
                }
                shelf.sync_spinner();
                if !busy && shelf.pending_restart.borrow_mut().remove(id) {
                    let shelf2 = shelf.clone();
                    let id = id.to_string();
                    glib::idle_add_local_once(move || shelf2.restart_session(&id));
                }
            }));
        }
        {
            let shelf = self.clone();
            *view.on_restart.borrow_mut() = Some(Box::new(move |id| shelf.restart_session(id)));
        }
        {
            let shelf = self.clone();
            *view.on_open_folder.borrow_mut() = Some(Box::new(move |p| {
                shelf.files.set_scope(Some(p.to_path_buf()));
                shelf.show_page("files");
            }));
        }
        {
            // the top-bar status mirrors the selected view's status
            let shelf = self.clone();
            let vid = id.clone();
            view.status.connect_notify_local(Some("label"), move |l, _| {
                if shelf.tabs.selected().as_deref() == Some(vid.as_str()) {
                    shelf.set_status(&l.text());
                }
            });
        }
        self.views.borrow_mut().insert(id.clone(), view);
        self.tabs.select(&id);
        self.sync_status();
    }

    /// Rebuild a session's jail because its pins changed: close the tab, stop the unit, reopen →
    /// the conversation resumes. Deferred to the end of the turn while the agent is working.
    fn restart_session(self: &Rc<Self>, id: &str) {
        let needs = self.views.borrow().get(id).map(|v| v.pins_changed()).unwrap_or(false);
        if !needs {
            return;
        }
        if self.busy.borrow().contains(id) {
            self.pending_restart.borrow_mut().insert(id.to_string());
            if let Some(v) = self.views.borrow().get(id) {
                v.conv.add_note("pins changed: the session restarts after this turn");
            }
            return;
        }
        let stored = self.store.sessions().unwrap_or_default().into_iter().find(|s| s.id == id);
        self.close_view(id);
        let shelf = self.clone();
        stop_then(id, move || {
            if let Some(s) = stored {
                shelf.open_session(s);
            }
        });
    }

    fn close_session(self: &Rc<Self>, id: &str) {
        self.close_view(id);
        stop_then(id, || {});
    }

    /// Drop the tab and the view; the jail is stopped separately (it can take seconds).
    fn close_view(self: &Rc<Self>, id: &str) {
        if let Some(v) = self.views.borrow_mut().remove(id) {
            v.quit();
            self.pages.remove(&v.root);
        }
        self.tabs.remove(id);
        if self.tabs.selected().is_none() {
            self.set_status("");
        }
    }

    fn fill_plus_menu(self: &Rc<Self>, menu: &gtk::Box, pop: &gtk::Popover) {
        while let Some(c) = menu.first_child() {
            menu.remove(&c);
        }
        let home = session::home().unwrap_or_default();
        let registry = agents::load(&home);
        // a quiet row: icon, name, nothing else; the glyph takes the menu's own text colour
        let color = menu.color();
        let row = |agent: Option<&agents::Agent>, text: &str| -> gtk::Button {
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("sill-menu-item");
            let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let i = super::icons::image(agent, color, 15);
            i.add_css_class("sill-menu-icon");
            let l = gtk::Label::new(Some(text));
            l.set_xalign(0.0);
            l.set_hexpand(true);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            h.append(&i);
            h.append(&l);
            b.set_child(Some(&h));
            b
        };
        // only agents that can actually start here: enabled, and their command exists (a built-in
        // whose package was removed on the Agents page drops out until it is installed again)
        for (id, agent) in registry.iter().filter(|(_, a)| a.enabled && sill_core::registry::available(a, &home).is_some()) {
            let b = row(Some(agent), &agent.name);
            let s = self.clone();
            let pop = pop.clone();
            let id = id.clone();
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Ok(sess) = s.store.create(&id, "default") {
                    s.open_session(sess);
                }
            });
            menu.append(&b);
        }
        let stored: Vec<Session> = self
            .store
            .sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !self.tabs.has(&s.id))
            .collect();
        if !stored.is_empty() {
            let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
            sep.add_css_class("sill-menu-sep");
            menu.append(&sep);
            let head = gtk::Label::new(Some("Reopen"));
            head.add_css_class("sill-menu-caption");
            head.set_xalign(0.0);
            menu.append(&head);
            for sess in stored {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                let b = row(registry.get(&sess.agent), &sess.title);
                b.set_hexpand(true);
                let del = gtk::Button::from_icon_name("user-trash-symbolic");
                del.add_css_class("flat");
                del.add_css_class("sill-menu-trash");
                del.set_tooltip_text(Some("Delete session and its recorded changes"));
                {
                    let s = self.clone();
                    let pop = pop.clone();
                    let sess = sess.clone();
                    b.connect_clicked(move |_| {
                        pop.popdown();
                        s.open_session(sess.clone());
                    });
                }
                {
                    let s = self.clone();
                    let pop = pop.clone();
                    let id = sess.id.clone();
                    del.connect_clicked(move |_| {
                        pop.popdown();
                        let s = s.clone();
                        let id2 = id.clone();
                        stop_then(&id, move || {
                            let _ = sill_core::session::remove_state(session::uid(), &id2);
                            let _ = s.store.delete(&id2);
                        });
                    });
                }
                line.append(&b);
                line.append(&del);
                menu.append(&line);
            }
        }
    }
}

/// Stop a session's jail on a worker thread and run `then` on the main loop when it is gone. A
/// stop can take a while (the unit's stop timeout when a shell inside ignores SIGTERM), and the
/// GTK loop must not wait for it.
fn stop_then(id: &str, then: impl FnOnce() + 'static) {
    let id = id.to_string();
    let (tx, rx) = async_channel::bounded::<()>(1);
    std::thread::Builder::new()
        .name(format!("sill-stop-{id}"))
        .spawn(move || {
            let _ = sill_core::session::stop(&id);
            let _ = tx.send_blocking(());
        })
        .ok();
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
        then();
    });
}

/// Show and focus; `time` is the X time of the triggering key press (0 when unknown).
pub fn show(w: &gtk::Window, time: u32) {
    // the size GTK will realize the surface with (the realize handler refines position and hints)
    if let Some(area) = x11::work_area_with(w, x11::width_fraction()) {
        w.set_default_size(area.width as i32, area.height as i32);
    }
    w.present();
    x11::activate(w, time);
}

pub fn toggle(w: &gtk::Window, time: u32) {
    if w.is_visible() && (w.is_active() || x11::is_active(w)) {
        w.set_visible(false);
    } else {
        show(w, time);
    }
}
