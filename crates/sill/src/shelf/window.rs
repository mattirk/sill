//! The shelf window (SPEC §3.1): top bar with the action button (dropdown: Sessions / Changes / Files
//! / Settings), session tabs and the state; a stack of pages below. Placement and visibility (§4.6).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use sill_core::agents;
use sill_core::store::{Session, Store};

use super::agents_page::AgentsPage;
use super::changes::ChangesPage;
use super::tree::FolderTree;
use super::viewer::Viewer;
use super::session_view::SessionView;
use super::tabs::TabStrip;
use super::{session, settings, theme, x11};

thread_local! {
    pub static HOTKEY: RefCell<Option<Rc<super::hotkey::Hotkey>>> = const { RefCell::new(None) };
}

pub struct Shelf {
    pub window: gtk::ApplicationWindow,
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
    spinner: Rc<super::spinner::Spinner>,
    herring: Rc<super::herring::Herring>,
    busy: RefCell<std::collections::HashSet<String>>,
    pending_restart: RefCell<std::collections::HashSet<String>>,
    views: RefCell<HashMap<String, Rc<SessionView>>>,
    changes: Rc<ChangesPage>,
    agents: Rc<AgentsPage>,
    /// The file tree (a drawer beside the main pane) and the viewer (over it), SPEC §3.1.
    tree: Rc<FolderTree>,
    viewer: Rc<Viewer>,
    /// The selected session's terminal pane, across the whole width under the tree and the main
    /// pane (one child per open session, named by its id); hidden while the prompt shows.
    pane_host: gtk::Stack,
}

/// Start the host terminal detached from the shelf: `{cwd}` in the command line is the directory,
/// which is the working directory too; the terminal outlives the shelf (own session, no fds) and
/// is reaped when it exits.
fn launch_terminal(cmdline: &str, cwd: &Path) -> anyhow::Result<()> {
    let words = shell_words::split(cmdline)?;
    let Some((prog, args)) = words.split_first() else { anyhow::bail!("empty command") };
    let dir = cwd.to_string_lossy();
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args.iter().map(|a| a.replace("{cwd}", &dir)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid between fork and exec only.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{prog}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn build(app: &gtk::Application, _cwd: PathBuf) -> gtk::Window {
    let config = sill_core::config::load();
    super::theme::set_text_scale(config.text_scale()); // applies the theme too
    super::theme::apply(&super::theme::by_id(&config.theme));
    if let Some(w) = config.width {
        x11::set_width_fraction(w);
    }
    // a store that will not open (held by another process past the busy timeout, a damaged file)
    // is a window saying so, not a panic the service restarts into forever (SPEC §4.11 #17)
    let store = match Store::open_default() {
        Ok(s) => Rc::new(s),
        Err(e) => {
            eprintln!("sill: cannot open the session store: {e:#}");
            return error_window(app, &format!("sill cannot open its session store:\n{e:#}"));
        }
    };

    let window = gtk::ApplicationWindow::builder()
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
    let herring = super::herring::Herring::new(); // the mark, flicks its tail on a press
    let action = gtk::ToggleButton::new();
    action.set_child(Some(&herring.area));
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
    plus.set_child(Some(&super::icons::glyph_image("sill-plus")));
    plus.add_css_class("flat");
    plus.set_tooltip_text(Some("New session"));
    let plus_pop = gtk::Popover::new();
    plus_pop.add_css_class("sill-menu");
    plus_pop.set_has_arrow(false);
    let plus_menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    plus_pop.set_child(Some(&plus_menu));
    plus.set_popover(Some(&plus_pop));
    bar.append(&plus);

    let files_btn = super::icons::glyph_button("sill-files");
    files_btn.add_css_class("flat");
    files_btn.set_tooltip_text(Some("Files"));
    bar.append(&files_btn);

    let spinner = super::spinner::Spinner::new(); // not GtkSpinner: see spinner.rs
    let idle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    idle.add_css_class("sill-status");
    idle.set_valign(gtk::Align::Center);
    idle.set_halign(gtk::Align::Center);
    let status = gtk::Stack::new();
    status.set_hhomogeneous(true);
    status.set_vhomogeneous(true);
    status.add_named(&idle, Some("idle"));
    status.add_named(&spinner.area, Some("busy"));
    status.set_valign(gtk::Align::Center);
    status.add_css_class("sill-status-slot");
    bar.append(&status);

    // --- pages
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_vexpand(true);
    // the window's minimum height is the shown page's, not the tallest page's: with a terminal pane
    // under the pages, the Settings page's rows would otherwise push the window past the screen
    stack.set_vhomogeneous(false);
    stack.add_named(&pages, Some("sessions"));
    let changes = ChangesPage::new(store.clone());
    let tree = FolderTree::new(store.clone());
    let viewer = Viewer::new(tree.changes.clone());
    let agents_page = AgentsPage::new();
    let settings_page = gtk::ScrolledWindow::builder()
        .child(&settings::build())
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    // backend pages: the page tabs in the bar say where you are, "sill" or Escape goes back
    for (name, body) in [
        ("changes", changes.root.clone().upcast::<gtk::Widget>()),
        ("agents", agents_page.root.clone().upcast::<gtk::Widget>()),
        ("settings", settings_page.clone().upcast::<gtk::Widget>()),
    ] {
        body.set_margin_top(6);
        stack.add_named(&body, Some(name));
    }

    // the main pane (pages) at the configured width, the viewer over it, the tree beside it: only
    // the tree changes the window's width (Shelf::fraction)
    let main = gtk::Overlay::new();
    main.set_child(Some(&stack));
    viewer.root.set_visible(false);
    main.add_overlay(&viewer.root);
    main.set_hexpand(true);
    main.set_vexpand(true);
    tree.root.set_visible(false);
    tree.root.set_hexpand(false);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&main);
    body.append(&tree.root);
    body.set_vexpand(true);
    // a terminal pane spans the window: under the tree and the main pane alike
    let pane_host = gtk::Stack::new();
    pane_host.set_vhomogeneous(false);
    pane_host.set_visible(false);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&bar);
    column.append(&body);
    column.append(&pane_host);
    window.set_child(Some(&column));

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
        herring: herring.clone(),
        busy: RefCell::new(std::collections::HashSet::new()),
        pending_restart: RefCell::new(std::collections::HashSet::new()),
        views: RefCell::new(HashMap::new()),
        changes: changes.clone(),
        agents: agents_page.clone(),
        tree: tree.clone(),
        viewer: viewer.clone(),
        pane_host: pane_host.clone(),
    });
    {
        // auto-revert (SPEC §4.4): the Changes page sweeps once a minute; a session that is mid-turn
        // is left alone
        let weak = Rc::downgrade(&shelf);
        changes.set_busy_probe(move |id| weak.upgrade().map(|s| s.busy.borrow().contains(id)).unwrap_or(true));
        let c = changes.clone();
        gtk::glib::timeout_add_seconds_local(60, move || {
            c.sweep();
            gtk::glib::ControlFlow::Continue
        });
    }
    {
        // drawn glyphs don't follow CSS: redraw the tab icons in the new theme's foreground
        let weak = Rc::downgrade(&shelf);
        theme::on_change(move |_| {
            if let Some(s) = weak.upgrade() {
                s.page_tab_icons();
                let sessions = s.store.sessions().unwrap_or_default();
                for id in s.tabs.ids() {
                    if let Some(sess) = sessions.iter().find(|x| x.id == id) {
                        s.refresh_tab(sess);
                    }
                }
            }
        });
    }

    {
        // Files: the global tree beside the main pane; again (or Ctrl+Shift+F) closes it
        let s = shelf.clone();
        files_btn.connect_clicked(move |_| {
            if s.tree.root.is_visible() {
                s.close_tree();
            } else {
                s.toggle_tree(None);
            }
        });
    }
    {
        let s = shelf.clone();
        *tree.on_pin.borrow_mut() = Some(Box::new(move |sid| {
            let view = s.views.borrow().get(sid).cloned();
            if let Some(v) = view {
                v.refresh_pins();
            }
            s.restart_session(sid); // a folder pin is a mount; a file pin leaves the jail alone
        }));
        let s = shelf.clone();
        *tree.on_open.borrow_mut() = Some(Box::new(move |p| s.open_file(p)));
        let s = shelf.clone();
        *tree.on_close.borrow_mut() = Some(Box::new(move || s.close_tree()));
        let s = shelf.clone();
        *viewer.on_close.borrow_mut() = Some(Box::new(move || {
            s.viewer.root.set_visible(false);
            s.focus_prompt();
        }));
        // a save in the viewer: the tree's badges and the pin chips' git numbers follow at once
        let s = shelf.clone();
        *viewer.on_saved.borrow_mut() = Some(Box::new(move |_| {
            if s.tree.root.is_visible() {
                s.tree.refresh_soon(); // with the monitor's own event: one scan
            }
            if let Some(id) = s.tabs.selected()
                && let Some(v) = s.views.borrow().get(&id)
            {
                v.refresh_pins();
            }
        }));
    }
    {
        // a monitor plugged in or out: the primary's work area may be another one now
        let s = shelf.clone();
        let w = window.clone();
        WidgetExt::display(&window).monitors().connect_items_changed(move |_, _, _, _| {
            if w.is_mapped() {
                s.place();
            }
        });
    }
    // the page tabs (built once, glyphs redrawn with the theme) and the "sill" toggle
    shelf.build_page_tabs(&page_tabs_box);
    {
        let s = shelf.clone();
        action.connect_toggled(move |b| {
            if s.syncing.get() {
                return;
            }
            s.herring.flick();
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
            s.sync_pane();
            *s.tree.pin_target.borrow_mut() = Some(id.to_string());
            if s.tree.root.is_visible() {
                s.tree.refresh(); // pinned-here badges follow the selected session
            }
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
        let s = shelf.clone();
        *tabs.on_menu.borrow_mut() = Some(Box::new(move |id, anchor| s.session_menu(id, anchor)));
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
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            // Ctrl+Shift+Left/Right walk the tabs, Ctrl+Shift+F toggles the file tree, Ctrl+Shift+T
            // and W open and close a session tab, Ctrl+Shift+H opens a terminal on the host
            if ctrl && shift {
                match key {
                    gdk::Key::Left => {
                        s.step_tab(-1);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::Right => {
                        s.step_tab(1);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::F | gdk::Key::f => {
                        // the session's first pinned folder, scoped (as its chip does); else everything
                        if s.tree.root.is_visible() {
                            s.close_tree()
                        } else {
                            let pinned = s.tabs.selected().and_then(|id| s.store.pins(&id).ok()).and_then(|pins| pins.into_iter().find(|p| p.is_dir()));
                            s.toggle_tree(pinned)
                        }
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::T | gdk::Key::t => {
                        s.new_session_like_selected();
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::W | gdk::Key::w => {
                        if let Some(id) = s.tabs.selected() {
                            s.close_session(&id);
                        }
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::H | gdk::Key::h => {
                        s.open_host_terminal(false);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            // Ctrl+T: the shell inside the jail
            if ctrl && !shift && key.to_lower() == gdk::Key::t {
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

    // back from the host (editor, terminal): refresh the pin chips' git numbers and the tree
    {
        let s = shelf.clone();
        window.connect_is_active_notify(move |w| {
            if !w.is_active() {
                return;
            }
            if let Some(id) = s.tabs.selected()
                && let Some(v) = s.views.borrow().get(&id)
            {
                v.refresh_pins();
            }
            if s.tree.root.is_visible() {
                s.tree.refresh_soon(); // a file edited by hand in a collapsed folder: no monitor saw it
            }
        });
    }

    // Escape does what the "sill" button does: sessions ⇄ backend (hiding is the hotkey's job)
    let esc = gtk::EventControllerKey::new();
    {
        let s = shelf.clone();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                s.herring.flick();
                // the viewer steps back (then closes), then the pages; the tree is Ctrl+Shift+F's
                if s.viewer.root.is_visible() {
                    s.viewer.escape();
                    return glib::Propagation::Stop;
                }
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
            let fraction = s.fraction();
            if let Some(area) = x11::work_area_with(&w, fraction) {
                // GTK sizes in logical pixels, the work area is in device pixels
                let k = x11::scale_of(&w);
                w.set_default_size(area.width as i32 / k, area.height as i32 / k);
                x11::pre_place(&w, &area);
            }
            // invisible until the first settled frame: no black frame before GTK's first paint, no
            // conversation visibly re-measuring and scrolling (the map handler reveals it)
            x11::set_opacity(&w, 0.0);
        });
    }
    // GTK rewrites the size hints when it presents the surface (dropping ours), and its MapWindow
    // sits in Xlib's buffer until the frame ends; a synchronous handler on `map` gets geometry and
    // hints to the server ahead of it. Then once more after the map as a correction.
    {
        let s = shelf.clone();
        window.connect_map(move |w| {
            let w: gtk::Window = w.clone().upcast();
            let fraction = s.fraction();
            if let Some(area) = x11::work_area_with(&w, fraction) {
                x11::pre_place(&w, &area);
            }
            let s = s.clone();
            glib::idle_add_local_once(move || {
                let fraction = s.fraction();
                if let Some(area) = x11::work_area_with(&w, fraction) {
                    x11::place(&w, &area);
                }
                x11::activate(&w, 0);
                s.focus_prompt();
                s.reveal_when_settled(&w);
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
    /// The shelf's width as a fraction of the primary monitor's work area: the configured width,
    /// plus the tree's share while the tree is open — nothing else moves the window (SPEC §3.1).
    fn fraction(&self) -> f64 {
        if self.tree.root.is_visible() { (x11::width_fraction() + x11::TREE_FRACTION).min(0.95) } else { x11::width_fraction() }
    }

    /// Show the tree — scoped to one folder (a chip) or global (`None`: the Files button) — or close
    /// it when the chip of the folder it already shows is clicked again.
    fn toggle_tree(&self, scope: Option<PathBuf>) {
        if self.tree.root.is_visible() && scope.is_some() && self.tree.scope() == scope {
            self.close_tree();
            return;
        }
        self.tree.set_scope(scope);
        // the tree's share of the work area, in this window's logical pixels
        let w: gtk::Window = self.window.clone().upcast();
        if let Some(area) = x11::work_area_with(&w, x11::TREE_FRACTION) {
            self.tree.root.set_size_request(area.width as i32 / x11::scale_of(&w), -1);
        }
        self.tree.root.set_visible(true);
        self.place();
    }

    /// A session's log in the viewer, followed (SPEC §4.7): the unit's journal, or the detached
    /// session's log files.
    fn open_log(&self, id: &str, title: &str) {
        let unit = sill_core::session::unit_name(id);
        self.viewer.open_log(&format!("{title} · {unit}"), sill_core::session::log_source(id));
        self.viewer.root.set_visible(true);
        self.viewer.focus();
    }

    /// A shell on the host (Ctrl+Shift+H, the shell button's right-click), in the folder the person
    /// is looking at: the tree's selection, else the session's first pinned folder, else home. In the
    /// selected session's bottom slot — the same pane as the jail shell, marked as host — or, with
    /// `window`, the configured terminal in a window of its own. A host process by design — outside
    /// the jail, nothing it does is recorded.
    fn open_host_terminal(&self, window: bool) {
        let tree = if self.tree.root.is_visible() { self.tree.selected_dir() } else { None };
        let pinned = || self.tabs.selected().and_then(|id| self.store.pins(&id).ok()).and_then(|pins| pins.into_iter().find(|p| p.is_dir()));
        let cwd = tree
            .or_else(pinned)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        if !window {
            if let Some(id) = self.tabs.selected()
                && let Some(v) = self.views.borrow().get(&id)
            {
                v.toggle_host_shell(Some(cwd));
            }
            return;
        }
        let Some(cmd) = sill_core::config::load().host_terminal() else {
            self.set_status("no terminal found on the host: set one under Settings → Host terminal");
            return;
        };
        match launch_terminal(&cmd, &cwd) {
            Ok(()) => self.set_status(&format!("terminal on the host in {}", cwd.display())),
            Err(e) => self.set_status(&format!("terminal failed: {e:#}")),
        }
    }

    /// Select the tab `delta` places over, wrapping around (Ctrl+Shift+Left/Right).
    fn step_tab(&self, delta: i32) {
        let ids = self.tabs.ids();
        if ids.is_empty() {
            return;
        }
        let cur = self.tabs.selected().and_then(|id| ids.iter().position(|i| *i == id)).unwrap_or(0) as i32;
        let n = ids.len() as i32;
        self.tabs.select(&ids[(cur + delta).rem_euclid(n) as usize]);
    }

    fn close_tree(&self) {
        if !self.tree.root.is_visible() {
            return;
        }
        self.tree.root.set_visible(false);
        self.place();
        self.focus_prompt();
    }

    /// Show the selected session's terminal pane across the window, or none while its prompt shows.
    fn sync_pane(&self) {
        let shown = self.tabs.selected().and_then(|id| {
            let views = self.views.borrow();
            let v = views.get(&id)?;
            v.pane_shown().map(|name| (id, name, v.pane.clone()))
        });
        let was = self.pane_host.is_visible();
        match shown {
            Some((id, name, pane)) => {
                pane.set_visible_child_name(&name);
                self.pane_host.set_visible_child_name(&id);
                self.pane_host.set_visible(true);
            }
            None => self.pane_host.set_visible(false),
        }
        // the slot changes the window's natural height; keep the window at the work area (as the
        // tree does with its width) instead of letting GTK grow it past the bottom of the screen
        if was != self.pane_host.is_visible() {
            self.place();
        }
    }

    /// A new session with the selected session's agent (Ctrl+Shift+T); claude when none is selected.
    fn new_session_like_selected(self: &Rc<Self>) {
        let agent = self
            .tabs
            .selected()
            .and_then(|id| self.views.borrow().get(&id).map(|v| v.session.borrow().agent.clone()))
            .unwrap_or_else(|| "claude".into());
        match self.store.create(&agent, "default") {
            Ok(s) => self.open_session(s),
            Err(e) => self.set_status(&format!("new session: {e:#}")),
        }
    }

    /// A file in the viewer, over the main pane; the tree stays as it is.
    fn open_file(&self, path: &Path) {
        self.viewer.open(path);
        self.viewer.root.set_visible(true);
        self.viewer.focus(); // keys go to the viewer, not the prompt underneath
    }

    /// Move/resize the window to the current fraction.
    fn place(&self) {
        let w: gtk::Window = self.window.clone().upcast();
        if let Some(area) = x11::work_area_with(&w, self.fraction()) {
            // GTK's own size first: a mapped toplevel re-sends its remembered size (grown to its
            // minimum) over whatever the X window was just configured to — with the tree open that
            // left the main pane squeezed instead of the window wider
            let k = x11::scale_of(&w);
            w.set_default_size(area.width as i32 / k, area.height as i32 / k);
            x11::place(&w, &area);
        }
    }

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
        self.place();
        match name {
            "changes" => self.changes.refresh(),
            "agents" => self.agents.refresh(),
            "sessions" => self.focus_prompt(),
            _ => {}
        }
    }

    /// Show the (opacity 0) window once the selected conversation's height has stopped changing —
    /// its messages re-measure after a show — or after 300 ms at the latest. No animation: it is
    /// simply there, at the bottom, at its final size.
    fn reveal_when_settled(&self, w: &gtk::Window) {
        let start = std::time::Instant::now();
        let conv = if self.stack.visible_child_name().as_deref() == Some("sessions") {
            self.tabs.selected().and_then(|id| self.views.borrow().get(&id).map(|v| v.conv.clone()))
        } else {
            None
        };
        if let Some(c) = &conv {
            c.reset_settle();
        }
        let frames = std::cell::Cell::new(0u32);
        w.add_tick_callback(move |w, _| {
            frames.set(frames.get() + 1);
            let settled = conv.as_ref().map(|c| c.settled()).unwrap_or(true);
            let late = start.elapsed() > std::time::Duration::from_millis(300);
            if settled || late {
                x11::set_opacity(&w.clone().upcast(), 1.0);
                if std::env::var_os("SILL_TRACE").is_some() {
                    eprintln!(
                        "{} reveal: after {} ms, {} frames ({})",
                        super::trace_ts(),
                        start.elapsed().as_millis(),
                        frames.get(),
                        if settled { "settled" } else { "timed out" }
                    );
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
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

    /// The small agent glyph for a tab, in the strip's current text colour — in the warning colour
    /// while the session's jail has the desktop (X11 / D-Bus), SPEC §4.11 #4.
    fn tab_icon(&self, agent: &str, desktop: bool) -> gtk::Image {
        let home = session::home().unwrap_or_default();
        let registry = agents::load(&home);
        let t = theme::current();
        let fg = super::icons::fg_for(&self.tabs.root, &t);
        let color = if desktop { gtk::gdk::RGBA::parse(t.warning).unwrap_or(fg) } else { fg };
        super::icons::image(registry.get(agent), color, 12)
    }

    /// A tab's glyph colour and tooltip follow the session's effective desktop access.
    fn refresh_tab(&self, s: &Session) {
        let (x11, dbus) = effective_desktop(s);
        self.tabs.set_icon(&s.id, &self.tab_icon(&s.agent, x11 || dbus));
        let tip = match (x11, dbus) {
            (true, true) => "X11 + D-Bus: the agent can see and drive the desktop and reach the session bus",
            (true, false) => "X11: the agent can run GUI programs — and, as any X11 client, read keys and drive every window",
            (false, true) => "D-Bus session bus: a way out of the jail",
            (false, false) => "Session menu: click the glyph or right-click",
        };
        self.tabs.set_tooltip(&s.id, Some(tip));
    }

    /// The session menu (SPEC §3.1): what belongs to the session rather than to the next turn —
    /// name, policy, desktop access — and where its jail runs. Built fresh on each open and dropped
    /// when it closes; a policy or desktop change closes it first, since the restart replaces the tab
    /// it is anchored to.
    fn session_menu(self: &Rc<Self>, id: &str, anchor: &gtk::Widget) {
        let Some(s) = self.store.sessions().unwrap_or_default().into_iter().find(|s| s.id == id) else { return };
        let home = session::home().unwrap_or_default();
        let pop = gtk::Popover::new();
        pop.add_css_class("sill-menu");
        pop.add_css_class("sill-session-menu");
        pop.set_has_arrow(false);
        pop.set_parent(anchor);
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        let key = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("sill-session-key");
            l.set_xalign(1.0);
            l
        };
        // name
        let name = gtk::Entry::new();
        name.set_text(&s.title);
        name.set_hexpand(true);
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let p = pop.clone();
            name.connect_activate(move |e| {
                let title = e.text().trim().to_string();
                if !title.is_empty() {
                    let _ = shelf.store.set_title(&sid, &title);
                    shelf.tabs.set_title(&sid, &title);
                    if let Some(v) = shelf.views.borrow().get(&sid) {
                        v.session.borrow_mut().title = title;
                    }
                }
                p.popdown();
            });
        }
        grid.attach(&key("Name"), 0, 0, 1, 1);
        grid.attach(&name, 1, 0, 1, 1);
        // agent
        let agent_name = agents::load(&home).get(&s.agent).map(|a| a.name.clone()).unwrap_or_else(|| s.agent.clone());
        let agent = gtk::Label::new(Some(&agent_name));
        agent.set_xalign(0.0);
        agent.add_css_class("dim-label");
        grid.attach(&key("Agent"), 0, 1, 1, 1);
        grid.attach(&agent, 1, 1, 1, 1);
        // policy: the files there are to choose from (plus the session's own if it is none of them)
        let mut policies = sill_core::policy::available(&home);
        if !policies.contains(&s.policy) {
            policies.push(s.policy.clone());
        }
        let refs: Vec<&str> = policies.iter().map(String::as_str).collect();
        let pol = gtk::DropDown::from_strings(&refs);
        pol.set_selected(policies.iter().position(|p| *p == s.policy).unwrap_or(0) as u32);
        pol.set_tooltip_text(Some("The jail's policy: mounts, network allow-list, limits. Changing it rebuilds the jail."));
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let p = pop.clone();
            let policies = policies.clone();
            pol.connect_selected_notify(move |d| {
                let Some(name) = policies.get(d.selected() as usize).cloned() else { return };
                let shelf = shelf.clone();
                let sid = sid.clone();
                p.popdown();
                glib::idle_add_local_once(move || {
                    // the restart this triggers takes `views` mutably: don't hold the borrow
                    let view = shelf.views.borrow().get(&sid).cloned();
                    if let Some(v) = view {
                        v.set_policy(&name);
                    }
                });
            });
        }
        grid.attach(&key("Policy"), 0, 2, 1, 1);
        grid.attach(&pol, 1, 2, 1, 1);
        // desktop access; "default" says what it resolves to for this session's policy
        let choices = sill_core::config::DESKTOP_CHOICES;
        let (dx, dd) = {
            let mut d = s.clone();
            d.desktop = String::new();
            effective_desktop(&d)
        };
        let onoff = |b: bool| if b { "on" } else { "off" };
        let default_label = format!("default (X11 {} · D-Bus {})", onoff(dx), onoff(dd));
        let labels: Vec<&str> = choices.iter().map(|(id, l)| if id.is_empty() { default_label.as_str() } else { *l }).collect();
        let desk = gtk::DropDown::from_strings(&labels);
        desk.set_selected(choices.iter().position(|(cid, _)| *cid == s.desktop).unwrap_or(0) as u32);
        desk.set_tooltip_text(Some("Desktop access of this session's jail. X11 lets the agent run and see GUI programs — and, as any X11 client, read keys and drive every window; D-Bus hands it the session bus. Changing it rebuilds the jail."));
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let p = pop.clone();
            desk.connect_selected_notify(move |d| {
                let Some((cid, _)) = choices.get(d.selected() as usize) else { return };
                let shelf = shelf.clone();
                let sid = sid.clone();
                p.popdown();
                glib::idle_add_local_once(move || {
                    let view = shelf.views.borrow().get(&sid).cloned();
                    if let Some(v) = view {
                        v.set_desktop(cid);
                    }
                });
            });
        }
        grid.attach(&key("Desktop"), 0, 3, 1, 1);
        grid.attach(&desk, 1, 3, 1, 1);
        // where the jail runs (SPEC §4.7 launcher modes), copyable
        let unit = sill_core::session::unit_name(id);
        let whereabouts = match sill_core::launcher::current().mode {
            sill_core::launcher::Mode::Unit => format!("unit {unit} · journalctl --user -u {unit}"),
            sill_core::launcher::Mode::Direct => format!("detached · $XDG_RUNTIME_DIR/sill/{id}/"),
        };
        // the log, in the viewer and followed: the unit's journal, or the detached session's files
        let log_btn = gtk::Button::with_label("Show, follow");
        log_btn.set_halign(gtk::Align::Start);
        log_btn.set_tooltip_text(Some("The session's log in the viewer, updated while shown (Escape comes back)"));
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let title = s.title.clone();
            let p = pop.clone();
            log_btn.connect_clicked(move |_| {
                p.popdown();
                shelf.open_log(&sid, &title);
            });
        }
        grid.attach(&key("Log"), 0, 4, 1, 1);
        grid.attach(&log_btn, 1, 4, 1, 1);
        let foot = gtk::Label::new(Some(&whereabouts));
        foot.add_css_class("sill-menu-caption");
        foot.set_xalign(0.0);
        foot.set_selectable(true);
        foot.set_wrap(true);
        grid.attach(&foot, 0, 5, 2, 1);
        pop.set_child(Some(&grid));
        pop.popup();
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
        self.pane_host.add_named(&view.pane, Some(&id));
        self.tabs.add(&id, &s.title);
        self.refresh_tab(&s);
        {
            let shelf = self.clone();
            *view.on_turn_end.borrow_mut() = Some(Box::new(move |_id| {
                if shelf.stack.visible_child_name().as_deref() == Some("changes") {
                    shelf.changes.refresh();
                }
                if shelf.tree.root.is_visible() {
                    shelf.tree.refresh_soon(); // badges: what the turn wrote where no monitor was
                }
            }));
        }
        {
            // an edit by the agent: the tree follows at once, not at the end of the turn
            let shelf = self.clone();
            *view.on_disk_change.borrow_mut() = Some(Box::new(move || shelf.tree.refresh_soon()));
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
            *view.on_open_folder.borrow_mut() = Some(Box::new(move |p| shelf.toggle_tree(Some(p.to_path_buf()))));
            let shelf = self.clone();
            *view.on_open_file.borrow_mut() = Some(Box::new(move |p| shelf.open_file(p)));
            let shelf = self.clone();
            view.connect_pane(move || shelf.sync_pane());
            let shelf = self.clone();
            *view.on_host_terminal.borrow_mut() = Some(Box::new(move |window| shelf.open_host_terminal(window)));
            let shelf = self.clone();
            *view.on_open_log.borrow_mut() = Some(Box::new(move |title, src| {
                shelf.viewer.open_log(title, src);
                shelf.viewer.root.set_visible(true);
                shelf.viewer.focus();
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
            self.pane_host.remove(&v.pane);
            // this `v` should be the last reference: anything else is a leak (SPEC §4.11 #15)
            if std::env::var_os("SILL_TRACE").is_some() {
                eprintln!("{} view {id} closed, {} other reference(s) left", super::trace_ts(), Rc::strong_count(&v) - 1);
            }
        }
        self.tabs.remove(id);
        self.sync_pane();
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
            head.set_hexpand(true);
            // delete every stored session: one click arms it, the next does it
            let all = gtk::Button::with_label("delete all");
            all.add_css_class("flat");
            all.add_css_class("sill-menu-delete-all");
            all.set_tooltip_text(Some("Delete every session listed here, with their recorded changes (click twice)"));
            {
                let s = self.clone();
                let pop = pop.clone();
                let ids: Vec<String> = stored.iter().map(|x| x.id.clone()).collect();
                all.connect_clicked(move |b| {
                    if !b.has_css_class("sill-armed") {
                        b.add_css_class("sill-armed");
                        b.set_label(&format!("delete {}?", ids.len()));
                        return;
                    }
                    pop.popdown();
                    for id in ids.clone() {
                        let s = s.clone();
                        stop_then(&id.clone(), move || {
                            let _ = sill_core::session::remove_state(session::uid(), &id);
                            let _ = s.store.delete(&id);
                        });
                    }
                });
            }
            let head_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            head_row.append(&head);
            head_row.append(&all);
            menu.append(&head_row);
            for sess in stored {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                let b = row(registry.get(&sess.agent), &sess.title);
                b.set_hexpand(true);
                let del = super::icons::glyph_button("sill-trash");
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

/// What the shelf shows when it cannot run at all: the reason, in a window, instead of a crash.
fn error_window(app: &gtk::Application, text: &str) -> gtk::Window {
    let w = gtk::ApplicationWindow::builder().application(app).title("sill").default_width(560).default_height(160).build();
    let l = gtk::Label::new(Some(text));
    l.set_wrap(true);
    l.set_selectable(true);
    l.set_margin_top(24);
    l.set_margin_bottom(24);
    l.set_margin_start(24);
    l.set_margin_end(24);
    w.set_child(Some(&l));
    w.upcast()
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
        let k = x11::scale_of(w);
        w.set_default_size(area.width as i32 / k, area.height as i32 / k);
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

/// What a session's jail gets, (x11, dbus): the session's own choice, else `sill.toml`, else its
/// policy's `[jail]` (SPEC §4.2).
fn effective_desktop(s: &Session) -> (bool, bool) {
    let home = session::home().unwrap_or_default();
    let (x11, dbus) = sill_core::config::desktop_grants(&s.desktop, &sill_core::config::load());
    let (px, pd) = sill_core::policy::Policy::load(&s.policy, &home).map(|p| (p.jail.x11, p.jail.dbus)).unwrap_or((false, false));
    (x11.unwrap_or(px), dbus.unwrap_or(pd))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command line is split like a shell would, `{cwd}` is filled in and is the working directory.
    #[test]
    fn launch_terminal_runs_in_the_folder() {
        let dir = std::env::temp_dir().join(format!("sill-term-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        launch_terminal("sh -c 'pwd > \"$0/out.txt\"; echo \"$0\" >> \"$0/out.txt\"' {cwd}", &dir).unwrap();
        let out = dir.join("out.txt");
        let mut tries = 0;
        while !out.exists() && tries < 100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            tries += 1;
        }
        let text = std::fs::read_to_string(&out).unwrap();
        let canon = dir.canonicalize().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(std::path::Path::new(lines[0]).canonicalize().unwrap(), canon);
        assert_eq!(lines[1], dir.to_string_lossy());
        assert!(launch_terminal("", &dir).is_err());
        assert!(launch_terminal("/nonexistent/terminal", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
