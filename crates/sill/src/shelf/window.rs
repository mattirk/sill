//! The shelf window (SPEC §3.1): top bar with the action button (dropdown: Sessions / Changes / Files
//! / Settings), session tabs and the state; a stack of pages below. Placement and visibility (§4.6).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use sill_core::changes;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use super::remote::agents;
use sill_core::store::Session;
use super::remote_store::Store;

use super::agents_page::AgentsPage;
use super::changes::ChangesPage;
use super::tree::FolderTree;
use super::viewer::Viewer;
use super::session_view::SessionView;
use super::tabs::TabStrip;
use super::{session, settings, theme, x11};

thread_local! {
    pub static HOTKEY: RefCell<Option<Rc<super::hotkey::Hotkey>>> = const { RefCell::new(None) };
    /// The one shelf, for the tray's Quit (`quit_with_summaries`).
    static SHELF: RefCell<Option<std::rc::Weak<Shelf>>> = const { RefCell::new(None) };
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
    /// Top-bar status: idle square or activity spinner, with elapsed time sliding out to its left.
    status: gtk::Stack,
    session_timer: gtk::Label,
    timer_reveal: gtk::Revealer,
    timer_auto: Rc<std::cell::Cell<bool>>,
    timer_hovered: std::cell::Cell<bool>,
    status_text: RefCell<String>,
    spinner: Rc<super::spinner::Spinner>,
    herring: Rc<super::herring::Herring>,
    /// Main pane | tree, sharing the window's width: the handle between them drags the split.
    paned: gtk::Paned,
    /// Full shelf width, with the drawer temporarily hidden.
    expanded: std::cell::Cell<bool>,
    restore_tree: std::cell::Cell<bool>,
    open_drawer: gtk::Button,
    busy: RefCell<std::collections::HashSet<String>>,
    pending_restart: RefCell<std::collections::HashSet<String>>,
    /// The prompt being written in a session whose view is closed to be made anew (a pin restarts
    /// the jail): the new view takes it back when it opens.
    drafts: RefCell<HashMap<String, super::session_view::Draft>>,
    views: RefCell<HashMap<String, Rc<SessionView>>>,
    /// Sessions whose tab and page are gone while their agent still writes the summary
    /// (`background_view`): the view lives until the turn ends, then `close_view` finishes.
    backgrounded: RefCell<std::collections::HashSet<String>>,
    /// Sessions the daemon was asked to end and has not yet (`close_session`): their tabs are
    /// closing, a second close hides them, the idle sweep leaves them alone.
    ending: RefCell<std::collections::HashSet<String>>,
    /// Ghost tabs whose close the person confirmed (or that had nothing to keep): the next
    /// `close_session` goes through without asking.
    discard_ok: RefCell<std::collections::HashSet<String>>,
    /// Quit was asked: the shelf exits as soon as the last view has gone (`close_view`).
    quitting: std::cell::Cell<bool>,
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
        .env(changes::HOST_SHELL_GIT_ENV.0, changes::HOST_SHELL_GIT_ENV.1) // S8: the folder's git config is the agent's
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
    let config = super::remote::config::load();
    super::theme::set_text_scale(config.text_scale()); // applies the theme too
    super::theme::apply(&super::theme::by_id(&config.theme));
    if let Some(w) = config.width {
        x11::set_width_fraction(w);
    }
    if let Some(w) = config.drawer_width {
        x11::set_drawer_width(w);
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
    refresh_memory(); // the jail's snapshot follows the store from the start

    let box_name = std::env::var("SILL_BOX").ok().filter(|name| !name.is_empty() && name != "local");
    let title = box_name.as_ref().map(|name| format!("sill · {name}")).unwrap_or_else(|| "sill".into());
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(&title)
        .decorated(false)
        .default_width(700)
        .default_height(1000)
        .build();

    // --- top bar: action button, tabs, "+", status
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_start(8);
    bar.set_margin_end(4);
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
    if let Some(name) = box_name {
        let label = gtk::Label::new(Some(&name));
        label.add_css_class("dim-label");
        label.set_tooltip_text(Some("Connected box: sessions, files and host shells run here"));
        bar.append(&label);
    }

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

    let top_actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    top_actions.add_css_class("sill-top-actions");
    bar.append(&top_actions);
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
    let plus_overlay = gtk::Overlay::new();
    plus_overlay.set_child(Some(&plus));
    plus_overlay.set_overflow(gtk::Overflow::Visible);
    top_actions.append(&plus_overlay);
    let open_drawer = super::icons::glyph_button("sill-chevron-right");
    open_drawer.add_css_class("flat");
    open_drawer.add_css_class("sill-drawer-open");
    open_drawer.set_halign(gtk::Align::End);
    open_drawer.set_valign(gtk::Align::Center);
    open_drawer.set_cursor_from_name(Some("pointer"));
    plus_overlay.add_overlay(&open_drawer);
    plus_overlay.set_measure_overlay(&open_drawer, false);
    plus_overlay.set_clip_overlay(&open_drawer, false);

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
    let session_timer = gtk::Label::new(Some("00:00"));
    session_timer.add_css_class("dim-label");
    session_timer.add_css_class("monospace");
    session_timer.set_width_chars(5);
    session_timer.set_margin_end(6);
    session_timer.set_tooltip_text(Some("Time the agent has worked (excludes idle time and waiting for input)"));
    let timer_reveal = gtk::Revealer::new();
    timer_reveal.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    timer_reveal.set_transition_duration(180);
    timer_reveal.set_child(Some(&session_timer));
    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    status_bar.set_halign(gtk::Align::End);
    status_bar.set_margin_end(6);
    status_bar.append(&timer_reveal);
    status_bar.append(&status);

    // --- pages
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_vexpand(true);
    // the window's minimum height is the shown page's, not the tallest page's: with a terminal pane
    // under the pages, the Settings page's rows would otherwise push the window past the screen
    stack.set_vhomogeneous(false);
    stack.set_hhomogeneous(false);
    stack.add_named(&pages, Some("sessions"));
    let changes = ChangesPage::new(store.clone());
    let tree = FolderTree::new(store.clone());
    open_drawer.set_visible(!tree.root.is_visible());
    let viewer = Viewer::new(tree.changes.clone());
    let agents_page = AgentsPage::new();
    let timer_auto = Rc::new(std::cell::Cell::new(super::remote::config::load().timer_auto.unwrap_or(false)));
    let settings_page = gtk::ScrolledWindow::builder()
        .child(&settings::build(timer_auto.clone()))
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

    // the main pane (pages), the viewer over it, the tree beside it, sharing the configured width:
    // Settings → Width is the shelf with the tree open; closing the tree takes the drawer's width
    // off the window, so the main pane is the same width either way (Shelf::placement)
    let main = gtk::Overlay::new();
    main.set_child(Some(&stack));
    viewer.root.set_visible(false);
    main.add_overlay(&viewer.root);
    main.set_hexpand(true);
    main.set_vexpand(true);
    tree.root.set_visible(true); // open by default (Shelf::split sets the width at realize)
    tree.root.set_size_request(x11::DRAWER_MIN, -1);
    // a paned, not a box: the separator (invisible, see the stylesheet) is the drawer's drag handle.
    // The main pane's width is the invariant (resize end, not start): the split is set to the main
    // pane's width and the drawer gets whatever the window's new width leaves once it arrives.
    let body = gtk::Paned::new(gtk::Orientation::Horizontal);
    body.set_start_child(Some(&main));
    body.set_end_child(Some(&tree.root));
    body.set_resize_start_child(false);
    body.set_shrink_start_child(false);
    body.set_resize_end_child(true);
    body.set_shrink_end_child(false);
    body.set_vexpand(true);
    // a terminal pane spans the window: under the tree and the main pane alike
    let pane_host = gtk::Stack::new();
    pane_host.set_vhomogeneous(false);
    pane_host.set_visible(false);
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = gtk::Paned::new(gtk::Orientation::Horizontal);
    header.set_start_child(Some(&bar));
    header.set_end_child(Some(&status_bar));
    header.set_resize_start_child(false);
    header.set_shrink_start_child(false);
    header.set_resize_end_child(true);
    body.bind_property("position", &header, "position").sync_create().build();
    {
        let header = header.clone();
        let body = body.clone();
        tree.root.connect_visible_notify(move |tree| {
            header.set_position(if tree.is_visible() { body.position() } else { i32::MAX });
        });
    }
    column.append(&header);
    column.append(&body);
    column.append(&pane_host);
    let expand = super::icons::glyph_button("sill-chevron-right");
    expand.add_css_class("flat");
    expand.add_css_class("sill-drawer-close");
    expand.set_halign(gtk::Align::End);
    expand.set_valign(gtk::Align::End);
    expand.set_margin_end(4);
    expand.set_margin_bottom(10);
    if let Some(image) = expand.child() {
        image.set_opacity(0.55);
    }
    let surface = gtk::Overlay::new();
    surface.set_child(Some(&column));
    surface.add_overlay(&expand);
    // A corner control must not contribute another row or change the configured shelf width.
    surface.set_measure_overlay(&expand, false);
    tree.root.bind_property("visible", &expand, "visible").sync_create().build();
    window.set_child(Some(&surface));

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
        session_timer,
        timer_reveal,
        timer_auto,
        timer_hovered: std::cell::Cell::new(false),
        status_text: RefCell::new(String::new()),
        spinner: spinner.clone(),
        herring: herring.clone(),
        paned: body.clone(),
        expanded: std::cell::Cell::new(false),
        restore_tree: std::cell::Cell::new(false),
        open_drawer: open_drawer.clone(),
        busy: RefCell::new(std::collections::HashSet::new()),
        pending_restart: RefCell::new(std::collections::HashSet::new()),
        drafts: RefCell::new(HashMap::new()),
        views: RefCell::new(HashMap::new()),
        backgrounded: RefCell::new(std::collections::HashSet::new()),
        ending: RefCell::new(std::collections::HashSet::new()),
        discard_ok: RefCell::new(std::collections::HashSet::new()),
        quitting: std::cell::Cell::new(false),
        changes: changes.clone(),
        agents: agents_page.clone(),
        tree: tree.clone(),
        viewer: viewer.clone(),
        pane_host: pane_host.clone(),
    });
    SHELF.with(|s| *s.borrow_mut() = Some(Rc::downgrade(&shelf)));
    {
        // the daemon's lifecycle events (a session's end, a checkpoint's answer, a note) come on
        // one channel for the window to route
        let (tx, rx) = async_channel::unbounded::<(String, super::client::Lifecycle)>();
        super::client::set_lifecycle(tx);
        let weak = Rc::downgrade(&shelf);
        glib::spawn_future_local(async move {
            while let Ok((id, ev)) = rx.recv().await {
                let Some(s) = weak.upgrade() else { break };
                s.lifecycle(&id, ev);
            }
        });
    }
    {
        let hover = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&shelf);
        hover.connect_enter(move |_, _, _| {
            if let Some(s) = weak.upgrade() {
                s.timer_hovered.set(true);
                s.sync_session_timer();
            }
        });
        let weak = Rc::downgrade(&shelf);
        hover.connect_leave(move |_| {
            if let Some(s) = weak.upgrade() {
                s.timer_hovered.set(false);
                s.sync_session_timer();
            }
        });
        status.add_controller(hover);
    }
    {
        let weak = Rc::downgrade(&shelf);
        glib::timeout_add_seconds_local(1, move || {
            let Some(s) = weak.upgrade() else { return glib::ControlFlow::Break };
            s.sync_session_timer();
            glib::ControlFlow::Continue
        });
    }
    {
        // auto-revert (SPEC §4.4), the idle-timeout end and memory's nightly run are the daemon's
        // minute sweep (docs/daemon.md §3.6, §7 M2); the Changes page follows it while shown
        let c = changes.clone();
        gtk::glib::timeout_add_seconds_local(60, move || {
            c.refresh_if_shown();
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
        let weak = Rc::downgrade(&shelf);
        tree.root.connect_visible_notify(move |tree| {
            if let Some(s) = weak.upgrade() {
                s.open_drawer.set_visible(!s.expanded.get() && !tree.is_visible());
            }
        });
        let weak = Rc::downgrade(&shelf);
        open_drawer.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.toggle_drawer();
            }
        });
    }
    {
        let weak = Rc::downgrade(&shelf);
        expand.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                s.set_expanded(!s.expanded.get());
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
        tree.on_open_diff(move |repo, rel| {
            s.viewer.open_git_diff(repo, rel);
            s.viewer.root.set_visible(true);
            s.viewer.focus();
        });
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
                let page = super::remote::config::load().default_page().to_string();
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
            s.tree.follow_session(id);
            s.sync_status();
            s.focus_prompt();
        }));
    }
    {
        let s = shelf.clone();
        *tabs.on_rename.borrow_mut() = Some(Box::new(move |id, title| {
            super::client::set(id, sill_core::wire::SessionSet::Title { title: title.to_string(), kind: "person".into() });
            if let Some(v) = s.views.borrow().get(id) {
                let mut sess = v.session.borrow_mut();
                sess.title = title.to_string();
                sess.title_kind = "person".into();
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
        *tabs.on_close.borrow_mut() = Some(Box::new(move |id| s.close_session(id, "close")));
    }
    {
        let s = shelf.clone();
        let f2 = gtk::EventControllerKey::new();
        f2.connect_key_pressed(move |_, key, _, state| {
            // Restore the split before a focused child or the page-level Escape handler
            // handles the key. A subsequent Escape resumes normal viewer/backend navigation.
            if key == gdk::Key::Escape && s.expanded.get() {
                s.set_expanded(false);
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::F2 {
                s.tabs.edit_selected();
                return glib::Propagation::Stop;
            }
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            // Ctrl+Shift+Left/Right walk the tabs, Ctrl+Shift+F toggles the file tree, Ctrl+Shift+T
            // and W open and close a session tab, Ctrl+Shift+H opens a terminal on the host,
            // Ctrl+Shift+E expands the main pane across the shelf, temporarily hiding the drawer
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
                        s.toggle_drawer();
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::E | gdk::Key::e => {
                        s.set_expanded(!s.expanded.get());
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::T | gdk::Key::t => {
                        s.new_session_like_selected();
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::W | gdk::Key::w => {
                        if let Some(id) = s.tabs.selected() {
                            s.close_session(&id, "close");
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

    // open the most recent session that is still going, or a new one with claude. An ended session
    // is not reopened by itself: opening a tab starts its jail and agent, and the person closed it
    // (it comes back from the "+" menu)
    {
        let sessions = store.sessions().unwrap_or_default();
        match sessions.into_iter().find(|s| s.ended_at.is_none()) {
            Some(s) => shelf.open_session(s),
            None => {
                if let Ok(s) = create_session(&store, "claude", "default", false) {
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

    // After expanded mode has been handled in capture, Escape navigates viewer/backend pages.
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
                    let page = super::remote::config::load().default_page().to_string();
                    s.show_page(&page);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(esc);

    // the separator dragged: the drawer's new width is remembered once the drag pauses
    {
        let s = shelf.clone();
        let pending: Rc<std::cell::Cell<Option<glib::SourceId>>> = Rc::new(std::cell::Cell::new(None));
        body.connect_position_notify(move |p| {
            if !s.tree.root.is_visible() {
                return;
            }
            let total = p.width();
            let w: gtk::Window = s.window.clone().upcast();
            let expected = s.placement(&w).map(|area| area.width as i32 / x11::scale_of(&w));
            // Ignore layout negotiation at an obsolete or minimum-forced window size.
            // Only a divider adjustment within the configured shelf width is remembered.
            if expected != Some(total) {
                return;
            }
            let drawer = total - p.position();
            if drawer < x11::DRAWER_MIN || drawer == x11::drawer_width(total) {
                return;
            }
            if let Some(id) = pending.take() {
                id.remove();
            }
            let pending2 = pending.clone();
            pending.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                pending2.set(None);
                x11::set_drawer_width(drawer);
                let _ = super::remote::config::update(|c| c.drawer_width = Some(drawer));
            })));
        });
    }

    // geometry before the first frame: on realize the X window exists but isn't mapped yet, so
    // the window manager shows it straight at the left edge, final size, no visible jump
    {
        let s = shelf.clone();
        window.connect_realize(move |w| {
            let w: gtk::Window = w.clone().upcast();
            if let Some(area) = s.placement(&w) {
                // GTK sizes in logical pixels, the work area is in device pixels
                let k = x11::scale_of(&w);
                w.set_default_size(area.width as i32 / k, area.height as i32 / k);
                if s.tree.root.is_visible() {
                    s.split(area.width as i32 / k);
                }
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
            if let Some(area) = s.placement(&w) {
                x11::pre_place(&w, &area);
            }
            let s = s.clone();
            glib::idle_add_local_once(move || {
                if let Some(area) = s.placement(&w) {
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
    /// Where the shelf goes: the configured fraction of the primary monitor's work area with the
    /// tree open; the drawer's width less while it is closed, so the main pane never changes
    /// width. Expanded mode keeps the full configured width with the tree hidden.
    fn placement(&self, w: &gtk::Window) -> Option<x11::Placement> {
        let mut area = x11::work_area_with(w, x11::width_fraction())?;
        if !self.expanded.get() && !self.tree.root.is_visible() {
            // the drawer is in logical pixels, the work area in device pixels
            let k = x11::scale_of(w);
            let total = area.width as i32 / k;
            area.width = ((total - x11::drawer_width(total)) * k) as u32;
        }
        Some(area)
    }

    /// Borrow the drawer's space without shrinking the shelf window; the viewer shares
    /// the same main pane, so files and code expand along with the conversation.
    fn set_expanded(&self, expanded: bool) {
        if self.expanded.replace(expanded) == expanded {
            return;
        }
        if expanded {
            self.restore_tree.set(self.tree.root.is_visible());
            self.tree.root.set_visible(false);
        } else {
            self.tree.root.set_visible(self.restore_tree.get());
        }
        self.open_drawer.set_visible(!expanded && !self.tree.root.is_visible());
        for view in self.views.borrow().values() {
            view.conv.set_expanded(expanded);
        }
        if self.tree.root.is_visible() {
            let w: gtk::Window = self.window.clone().upcast();
            if let Some(area) = self.placement(&w) {
                self.split(area.width as i32 / x11::scale_of(&w));
            }
        }
        self.place();
    }

    /// The split for a shelf `total` logical pixels wide with the tree open: the main pane's width.
    fn split(&self, total: i32) {
        self.paned.set_position((total - x11::drawer_width(total)).max(x11::MAIN_MIN));
    }

    /// Show the tree — scoped to one folder (a chip) or global (`None`) — or close
    /// it when the chip of the folder it already shows is clicked again.
    fn toggle_tree(&self, scope: Option<PathBuf>) {
        self.set_expanded(false);
        if self.tree.root.is_visible() && scope.is_some() && self.tree.scope() == scope {
            self.close_tree();
            return;
        }
        self.tree.set_scope(scope);
        self.tree.root.set_visible(true);
        // the window grows by the drawer's width; the main pane keeps its width, the drawer gets the growth
        let w: gtk::Window = self.window.clone().upcast();
        if let Some(area) = self.placement(&w) {
            self.split(area.width as i32 / x11::scale_of(&w));
        }
        self.place();
    }

    /// A session's log in the viewer, followed (SPEC §4.7): read through the daemon, which knows
    /// whether it is a journald stream or files.
    fn open_log(&self, id: &str, title: &str) {
        let unit = sill_core::session::unit_name(id);
        let sid = id.to_string();
        self.viewer.open_log(&format!("{title} · {unit}"), std::sync::Arc::new(move |lines| super::client::session_log(&sid, lines)));
        self.viewer.root.set_visible(true);
        self.viewer.focus();
    }

    /// A shell on the host (Ctrl+Shift+H, the shell button's right-click). In the selected session's
    /// bottom slot — the same pane as the jail shell, marked as host, with a directory of its own —
    /// or, with `window`, the configured terminal in a window of its own, in the folder the person is
    /// looking at: the tree's selection, else the session's first pinned folder, else home. A host
    /// process by design — outside the jail, nothing it does is recorded.
    fn open_host_terminal(&self, window: bool) {
        if !window {
            // the pane keeps its own directory (SessionView::show_host_shell), whatever the tree shows
            if let Some(id) = self.tabs.selected()
                && let Some(v) = self.views.borrow().get(&id)
            {
                v.toggle_host_shell();
            }
            return;
        }
        if sill_proto::transport::selected().is_ok_and(|target| target.is_some()) {
            if let Some(id) = self.tabs.selected()
                && let Some(view) = self.views.borrow().get(&id)
            {
                view.show_host_shell(true);
                self.set_status("the box's host shell opens in the pane; a desktop terminal window belongs to this machine");
            }
            return;
        }
        let tree = if self.tree.root.is_visible() { self.tree.selected_dir() } else { None };
        let pinned = || self.tabs.selected().and_then(|id| self.store.pins(&id).ok()).and_then(|pins| pins.into_iter().find(|p| super::remote::is_dir(p)));
        let cwd = tree
            .or_else(pinned)
            .or_else(|| session::home().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let Some(cmd) = super::remote::config::load().host_terminal() else {
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

    fn toggle_drawer(&self) {
        if self.tree.root.is_visible() {
            self.close_tree();
        } else {
            self.set_expanded(false);
            self.tree.root.set_visible(true);
            let w: gtk::Window = self.window.clone().upcast();
            if let Some(area) = self.placement(&w) {
                self.split(area.width as i32 / x11::scale_of(&w));
            }
            self.place();
        }
    }

    fn close_tree(&self) {
        if !self.tree.root.is_visible() {
            return;
        }
        self.tree.root.set_visible(false);
        self.place(); // the drawer's width off the window
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
        let (agent, ghost) = self
            .tabs
            .selected()
            .and_then(|id| self.views.borrow().get(&id).map(|v| (v.session.borrow().agent.clone(), v.session.borrow().ghost)))
            .unwrap_or_else(|| ("claude".into(), false));
        match create_session(&self.store, &agent, "default", ghost) {
            Ok(s) => self.open_session(s),
            Err(e) => self.set_status(&format!("new session: {e:#}")),
        }
    }

    /// A session's page in the viewer (docs/memory.md §2.5): summary, notes, the facts; from the
    /// session menu or a Reopen row. Edits refresh memory's snapshot so the jail reads them too.
    fn open_session_page(self: &Rc<Self>, id: &str) {
        let Some(s) = self.store.sessions().unwrap_or_default().into_iter().find(|s| s.id == id) else { return };
        let view = self.views.borrow().get(id).cloned();
        let page = super::session_page::build(self.store.clone(), &s, view, Rc::new(refresh_memory));
        self.viewer.open_page(&s.title, &page);
        self.viewer.root.set_visible(true);
        self.viewer.focus();
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
        if let Some(area) = self.placement(&w) {
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
        self.sync_session_timer();
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
        let expected = self.placement(w).map(|area| area.width as i32 / x11::scale_of(w));
        let paned = self.paned.clone();
        let drawer = self.tree.root.clone();
        w.add_tick_callback(move |w, _| {
            frames.set(frames.get() + 1);
            let settled = conv.as_ref().map(|c| c.settled()).unwrap_or(true);
            let late = start.elapsed() > std::time::Duration::from_millis(300);
            if settled || late {
                x11::set_opacity(&w.clone().upcast(), 1.0);
                if let Some(expected) = expected {
                    let horizontal = gtk::Orientation::Horizontal;
                    eprintln!("sill width: requested={expected} actual={} minimum={} drawer-visible={} split={} scale={}",
                        w.width(), w.measure(horizontal, -1).0, drawer.is_visible(), paned.position(), w.scale_factor());
                    // Follow the largest minimum-width child to locate the constraint without
                    // logging conversation text, filenames or widget contents.
                    let mut node: gtk::Widget = w.clone().upcast();
                    for _ in 0..16 {
                        eprintln!("sill width: {} css={:?} allocated={} minimum={}",
                            node.type_().name(), node.css_classes(), node.width(), node.measure(horizontal, -1).0);
                        let mut child = node.first_child();
                        let mut widest = None;
                        let mut minimum = -1;
                        while let Some(current) = child {
                            child = current.next_sibling();
                            if !current.is_visible() || !current.is_child_visible() { continue; }
                            let size = current.measure(horizontal, -1).0;
                            if size > minimum { minimum = size; widest = Some(current); }
                        }
                        let Some(next) = widest else { break };
                        node = next;
                    }
                    for (name, child) in [("main", paned.start_child()), ("drawer", paned.end_child())] {
                        if let Some(child) = child {
                            eprintln!("sill width: {name} allocated={} minimum={}", child.width(), child.measure(horizontal, -1).0);
                        }
                    }
                }
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
        for (id, title) in super::remote::config::PAGES {
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

    /// The small agent glyph for a tab, in the strip's current text colour — in the accent colour while the
    /// agent is in a turn (the glyph pulses then), SPEC §4.11 #4.
    fn tab_icon(&self, agent: &str, busy: bool) -> gtk::Image {
        let home = session::home().unwrap_or_default();
        let registry = agents::load(&home);
        let t = theme::current();
        let fg = super::icons::fg_for(&self.tabs.root, &t);
        let color = if busy {
            gtk::gdk::RGBA::parse(t.accent).unwrap_or(fg)
        } else {
            fg
        };
        super::icons::image(registry.get(agent), color, 12)
    }

    /// Refresh a tab's activity glyph and session-menu tooltip.
    fn refresh_tab(&self, s: &Session) {
        self.tabs.set_icon(&s.id, &self.tab_icon(&s.agent, self.busy.borrow().contains(&s.id)));
        self.tabs.set_class(&s.id, "sill-ghost", s.ghost);
        if s.ghost {
            self.tabs.set_tooltip(&s.id, Some(GHOST_TIP));
            return;
        }
        let tip = "Session menu: click the glyph or right-click";
        self.tabs.set_tooltip(&s.id, Some(tip));
    }

    /// The session menu (SPEC §3.1): what belongs to the session rather than to the next turn —
    /// name, policy — and where its jail runs. Built fresh on each open and dropped
    /// when it closes; a policy change closes it first, since the restart replaces the tab
    /// it is anchored to.
    fn show_policy_details(self: &Rc<Self>, id: &str) {
        let dialog = gtk::Window::builder().title("Session policy").transient_for(&self.window)
            .default_width(660).default_height(560).build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_top(12); content.set_margin_bottom(12);
        content.set_margin_start(12); content.set_margin_end(12);
        let status = gtk::Label::new(Some("Reading policy…"));
        status.set_xalign(0.0); status.set_wrap(true);
        content.append(&status);
        let text = gtk::TextView::builder().editable(false).cursor_visible(false).monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar).build();
        let scroll = gtk::ScrolledWindow::builder().vexpand(true).hexpand(true).child(&text).build();
        content.append(&scroll);
        let rebuild = gtk::Button::with_label("Rebuild with current policy");
        rebuild.set_halign(gtk::Align::Start); rebuild.set_sensitive(false);
        content.append(&rebuild);
        let shelf = self.clone(); let sid = id.to_string(); let weak = dialog.downgrade();
        rebuild.connect_clicked(move |_| {
            if let Some(view) = shelf.views.borrow().get(&sid).cloned() {
                view.restart_for_jail();
                if let Some(dialog) = weak.upgrade() { dialog.close(); }
            }
        });
        dialog.set_child(Some(&content)); dialog.present();
        let (tx, rx) = async_channel::bounded(1); let sid = id.to_string();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let key = sill_proto::sessions::SessionKey { session_id: sid };
                let snapshot: sill_proto::admin::PolicySnapshot = super::client::call("_sill/policy.snapshot", &key)?;
                let preview: sill_proto::admin::PolicyView = super::client::call("_sill/policy.preview", &key)?;
                Ok((snapshot, preview))
            })();
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok((snapshot, preview))) => {
                    let describe = |view: &sill_proto::admin::PolicyView| {
                        let mut text = format!("Base: {}\n", view.name);
                        for source in &view.sources {
                            text.push_str(&format!("{}{}\n", if source.ignored { "Ignored for this build: " } else { "Fragment: " }, source.path.display));
                            if let Some(error) = &source.error { text.push_str(&format!("{error}\n")); }
                        }
                        for suggestion in &view.suggestions {
                            text.push_str(&format!("Suggested host: {} ({})\n", suggestion.host, suggestion.source.display));
                        }
                        text.push_str(&format!("Read-only paths: {:?}\nHidden paths: {:?}\n\n", view.fragment_ro.iter().map(|p| &p.display).collect::<Vec<_>>(), view.fragment_masks.iter().map(|p| &p.display).collect::<Vec<_>>()));
                        for diagnostic in &view.diagnostics { text.push_str(&format!("{diagnostic}\n")); }
                        text.push_str(view.text.as_deref().unwrap_or("No valid resolved policy.")); text
                    };
                    let current = snapshot.snapshot.as_ref().map(describe).unwrap_or_else(|| "No recorded launch snapshot (not launched or an older jail).".into());
                    text.buffer().set_text(&format!("RECORDED LAUNCH RULES\n{current}\n\nNEXT BUILD PREVIEW\n{}", describe(&preview)));
                    let valid = preview.diagnostics.is_empty() && preview.text.is_some();
                    rebuild.set_sensitive(valid);
                    let changed = snapshot.snapshot.as_ref().is_some_and(|launch| launch.fingerprint != preview.fingerprint);
                    status.set_text(if valid && changed { "Policy sources changed. The running jail retains its recorded launch rules. Rebuild waits for the active turn to finish." } else if valid { "Preview changes take effect only when you rebuild. Rebuild waits for the active turn to finish." } else { "Current fragments contain errors. Correct them before rebuilding; the running jail keeps its launch rules." });
                }
                Ok(Err(error)) => status.set_text(&format!("Could not inspect policy: {error:#}")),
                Err(_) => status.set_text("Policy inspection ended without a result."),
            }
        });
    }

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
                    super::client::set(&sid, sill_core::wire::SessionSet::Title { title: title.clone(), kind: "person".into() });
                    shelf.tabs.set_title(&sid, &title);
                    if let Some(v) = shelf.views.borrow().get(&sid) {
                        let mut sess = v.session.borrow_mut();
                        sess.title = title;
                        sess.title_kind = "person".into();
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
        let mut policies: Vec<String> = super::remote::request(sill_core::frontend::Request::Policies).unwrap_or_default();
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
        let policy_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        policy_row.append(&pol);
        let details = gtk::Button::with_label("Details");
        details.set_tooltip_text(Some("Recorded launch rules, repository fragments and next-build preview"));
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let pop = pop.clone();
            details.connect_clicked(move |_| { pop.popdown(); shelf.show_policy_details(&sid); });
        }
        policy_row.append(&details);
        grid.attach(&policy_row, 1, 2, 1, 1);
        // recall scope (docs/memory.md §2.3): which earlier sessions the agent is told about at
        // start. Three choices; the one the pins would pick is marked (default), and choosing it
        // stores "" so the session keeps following its pins
        use sill_core::memory::Scope;
        let all = [Scope::Here, Scope::Everywhere, Scope::Off];
        let pinned = !self.store.pins(id).unwrap_or_default().is_empty();
        let default = Scope::resolve("", pinned);
        let labels: Vec<String> = all.iter().map(|sc| if *sc == default { format!("{} (default)", sc.label()) } else { sc.label().to_string() }).collect();
        let mem = gtk::DropDown::from_strings(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        let current = Scope::resolve(&s.memory_scope, pinned);
        mem.set_selected(all.iter().position(|sc| *sc == current).unwrap_or(0) as u32);
        if s.ghost && !super::remote::config::load().ghost_memory() {
            mem.set_sensitive(false);
            mem.set_tooltip_text(Some("A ghost session starts blank: Settings → Ghosts see memory turns recall on for ghosts"));
        } else if super::remote::config::load().memory() {
            mem.set_tooltip_text(Some("Earlier sessions quoted to the agent when it starts. It can always search all sessions with sill-memory."));
        } else {
            mem.set_sensitive(false);
            mem.set_tooltip_text(Some("Memory is off (Settings)"));
        }
        {
            let shelf = self.clone();
            let sid = id.to_string();
            mem.connect_selected_notify(move |d| {
                let Some(sc) = all.get(d.selected() as usize) else { return };
                let scope = if *sc == default { "" } else { sc.id() };
                let view = shelf.views.borrow().get(&sid).cloned();
                match view {
                    Some(v) => v.set_memory_scope(scope),
                    None => super::client::set(&sid, sill_core::wire::SessionSet::MemoryScope(scope.to_string())),
                }
            });
        }
        grid.attach(&key("Recall"), 0, 4, 1, 1);
        grid.attach(&mem, 1, 4, 1, 1);
        // where the jail runs (docs/daemon.md §3.1: a child of the daemon in scope sill-<id>), copyable
        let unit = sill_core::session::unit_name(id);
        let whereabouts = format!("{unit}.scope · journalctl --user -t {unit} · $XDG_RUNTIME_DIR/sill/{id}/");
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
        // the session's page (docs/memory.md §2.5): summary, notes, Summarize now
        let page_btn = gtk::Button::with_label("Summary and notes");
        page_btn.set_halign(gtk::Align::Start);
        page_btn.set_tooltip_text(Some("The session's page in the viewer: its summary (editable), your notes, Summarize now (Escape comes back)"));
        {
            let shelf = self.clone();
            let sid = id.to_string();
            let p = pop.clone();
            page_btn.connect_clicked(move |_| {
                p.popdown();
                shelf.open_session_page(&sid);
            });
        }
        if s.ghost {
            page_btn.set_sensitive(false);
            page_btn.set_tooltip_text(Some("A ghost session is not remembered: no summary, no notes"));
        }
        grid.attach(&key("Page"), 0, 5, 1, 1);
        grid.attach(&page_btn, 1, 5, 1, 1);
        grid.attach(&key("Log"), 0, 6, 1, 1);
        grid.attach(&log_btn, 1, 6, 1, 1);
        let foot = gtk::Label::new(Some(&whereabouts));
        foot.add_css_class("sill-menu-caption");
        foot.set_xalign(0.0);
        foot.set_selectable(true);
        foot.set_wrap(true);
        grid.attach(&foot, 0, 7, 2, 1);
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

    fn sync_session_timer(&self) {
        let views = self.views.borrow();
        let selected = self.tabs.selected();
        let view = selected.as_ref().filter(|_| self.stack.visible_child_name().as_deref() == Some("sessions"))
            .and_then(|id| views.get(id));
        self.session_timer.set_visible(view.is_some());
        self.timer_reveal.set_reveal_child(view.is_some_and(|v|
            self.timer_hovered.get() || (self.timer_auto.get() && v.timer_working())));
        if let Some(view) = view {
            let elapsed = view.active_seconds();
            let text = if elapsed < 3600 {
                format!("{}:{:02}", elapsed / 60, elapsed % 60)
            } else {
                format!("{}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60)
            };
            self.session_timer.set_text(&text);
        }
    }

    fn sync_status(&self) {
        self.sync_session_timer();
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
    /// file was last edited is stopped and rebuilt first, so policy edits (permissions, masks…)
    /// take effect without closing anything by hand.
    fn open_session_with(self: &Rc<Self>, s: Session, note: Option<String>) {
        let id = s.id.clone();
        if self.tabs.has(&id) {
            self.tabs.select(&id);
            return;
        }
        if note.is_none() && super::client::call::<sill_proto::sessions::Session>("_sill/sessions.get", sill_proto::sessions::SessionKey { session_id: id.clone() }).is_ok_and(|s| s.running && s.policy_changed) {
            let dialog = gtk::AlertDialog::builder().modal(true)
                .message("Policy sources changed")
                .detail("This jail still uses its recorded launch rules. Keep using it, or rebuild with the current policy and repository fragments?")
                .buttons(["Keep current jail", "Rebuild jail"]).default_button(0).cancel_button(0).build();
            let shelf = self.clone();
            dialog.choose(Some(&self.window), None::<&gtk::gio::Cancellable>, move |answer| {
                if answer == Ok(1) {
                    let shelf = shelf.clone();
                    let s = s.clone();
                    stop_then(&s.id.clone(), move || shelf.open_session_with(s,
                        Some("rebuilt with the current policy and repository fragments".into())));
                } else {
                    shelf.open_session_with(s.clone(), Some("policy sources changed; keeping this jail's recorded launch rules until you choose to rebuild".into()));
                }
            });
            return;
        }
        // an ended session comes back: open again, its last summary shown first
        let note = match (&note, s.ended_at) {
            (None, Some(t)) if !s.summary.is_empty() => {
                super::client::set(&id, sill_core::wire::SessionSet::Reopen);
                Some(format!("resumed; the session ended {} with this summary:\n{}", sill_core::memory::date(t), s.summary))
            }
            _ => {
                if s.ended_at.is_some() {
                    super::client::set(&id, sill_core::wire::SessionSet::Reopen);
                }
                note
            }
        };
        let view = SessionView::new(s.clone(), self.store.clone());
        let draft = self.drafts.borrow_mut().remove(&id);
        if let Some(d) = draft {
            view.restore_draft(d);
        }
        view.conv.set_expanded(self.expanded.get());
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
                // the glyph takes the accent colour for the turn: redrawn from the session as it
                // is now, so a desktop grant changed since the tab opened still wins
                if let Some(sess) = shelf.store.sessions().unwrap_or_default().into_iter().find(|x| x.id == id) {
                    shelf.refresh_tab(&sess);
                }
                shelf.sync_spinner();
                super::tray::set_busy(!shelf.busy.borrow().is_empty());
                if !busy && shelf.pending_restart.borrow_mut().remove(id) {
                    let shelf2 = shelf.clone();
                    let id = id.to_string();
                    glib::idle_add_local_once(move || shelf2.restart_session(&id));
                }
            }));
        }
        {
            let shelf = self.clone();
            *view.on_restart.borrow_mut() = Some(Rc::new(move |id| shelf.restart_session(id)));
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
            *view.on_open_log.borrow_mut() = Some(Box::new(move |title, session, name| {
                shelf.viewer.open_log(title, std::sync::Arc::new(move |lines| super::remote::request::<String>(sill_core::frontend::Request::ProcTail { session: session.clone(), name: name.clone(), lines: lines.clamp(1, 10_000) }).unwrap_or_else(|e| format!("{e:#}"))));
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
        self.tree.follow_session(&id); // a new agent starts at Files, a resumed one at its pinned folder
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
        // what is typed survives the restart: the new view takes it back (open_session_with)
        let draft = self.views.borrow().get(id).map(|v| v.take_draft());
        if let Some(d) = draft.filter(|d| !d.is_empty()) {
            self.drafts.borrow_mut().insert(id.to_string(), d);
        }
        self.close_view(id);
        let shelf = self.clone();
        stop_then(id, move || {
            if let Some(s) = stored {
                shelf.open_session(s);
            }
        });
    }

    /// The tab closes: the session ends (docs/memory.md §2.2) — in the daemon (`sessions.end`),
    /// which asks the agent for the summary when it is up, idle and saw the conversation, else
    /// writes sill's own, deletes a session nobody prompted, and stops the jail. While the agent
    /// writes, the tab stays (`ending`) and `SessionEnded` closes it; closed again meanwhile, the
    /// tab goes and the summary finishes out of sight (`background_view`). `reason`: `close`,
    /// `idle`, `quit`.
    fn close_session(self: &Rc<Self>, id: &str, reason: &str) {
        // a ghost's tab asks once when it has unkept changes (docs/memory.md §3.3): closing deletes
        // them, and Keep is the one way anything of a ghost survives
        let ghost = self.views.borrow().get(id).is_some_and(|v| v.session.borrow().ghost);
        if ghost && reason == "close" && !self.ending.borrow().contains(id) && !self.discard_ok.borrow_mut().remove(id) {
            self.ask_ghost_close(id);
            return;
        }
        if !self.ending.borrow_mut().insert(id.to_string()) {
            self.background_view(id);
            return;
        }
        let (tx, rx) = async_channel::bounded::<anyhow::Result<sill_proto::Ending>>(1);
        let (sid, why) = (id.to_string(), reason.to_string());
        std::thread::Builder::new()
            .name(format!("sill-end-{sid}"))
            .spawn(move || {
                let _ = tx.send_blocking(super::client::end(&sid, &why));
            })
            .ok();
        let shelf = self.clone();
        let sid = id.to_string();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(e)) if e.pending => shelf.set_status("the session ends: its summary is being written"),
                Ok(Ok(e)) => shelf.session_ended(&sid, e.deleted),
                Ok(Err(e)) => {
                    eprintln!("sill: ending session {sid}: {e:#}");
                    shelf.set_status(&format!("ending the session failed: {e:#}"));
                    shelf.session_ended(&sid, false);
                }
                Err(_) => {}
            }
        });
    }

    /// Before a ghost's tab closes: the daemon counts its unkept writes; none, and it closes; some,
    /// and the person chooses — *Keep some* opens Changes and leaves the tab, *Discard and close*
    /// closes it. Escape does neither.
    fn ask_ghost_close(self: &Rc<Self>, id: &str) {
        let (tx, rx) = async_channel::bounded::<anyhow::Result<usize>>(1);
        let sid = id.to_string();
        std::thread::Builder::new()
            .name(format!("sill-ghost-ask-{sid}"))
            .spawn(move || {
                // the public list leaves out ignored build output, as the question should
                let count = crate::changes_client::list(&|m, p| super::client::call(m, p)).map(|list| {
                    list.sessions.iter().find(|s| s.id == sid).map_or(0, |s| s.changes.len() + s.agent_state.len())
                });
                let _ = tx.send_blocking(count);
            })
            .ok();
        let shelf = self.clone();
        let sid = id.to_string();
        glib::spawn_future_local(async move {
            let n = match rx.recv().await {
                Ok(Ok(n)) => n,
                // the count could not be had: ask anyway rather than discard unseen
                Ok(Err(e)) => {
                    eprintln!("sill: counting ghost {sid}'s changes: {e:#}");
                    usize::MAX
                }
                Err(_) => return,
            };
            if n == 0 {
                shelf.discard_ok.borrow_mut().insert(sid.clone());
                shelf.close_session(&sid, "close");
                return;
            }
            let title = shelf.views.borrow().get(&sid).map(|v| v.session.borrow().title.clone()).unwrap_or_default();
            let what = if n == usize::MAX { "unkept changes".to_string() } else if n == 1 { "1 unkept change".to_string() } else { format!("{n} unkept changes") };
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message(format!("“{title}” is a ghost with {what}"))
                .detail("Closing it deletes them with the session. Keep copies a change to the host.")
                .buttons(["Keep some", "Discard and close"])
                .default_button(0)
                .build();
            let s2 = shelf.clone();
            dialog.choose(Some(&shelf.window), None::<&gtk::gio::Cancellable>, move |r| match r {
                Ok(1) => {
                    s2.discard_ok.borrow_mut().insert(sid.clone());
                    s2.close_session(&sid, "close");
                }
                Ok(0) => s2.show_page("changes"),
                _ => {}
            });
        });
    }

    /// The daemon ended the session (the answer to `sessions.end`, or its `SessionEnded` event —
    /// whichever comes first; the other finds nothing left to do): the view and tab go.
    fn session_ended(self: &Rc<Self>, id: &str, _deleted: bool) {
        self.ending.borrow_mut().remove(id);
        if self.views.borrow().contains_key(id) || self.tabs.ids().iter().any(|t| t == id) {
            self.close_view(id);
        }
        refresh_memory();
    }

    /// A session's lifecycle event from the daemon (`client::set_lifecycle`): the end, a
    /// checkpoint's answer, a note for the conversation.
    fn lifecycle(self: &Rc<Self>, id: &str, ev: super::client::Lifecycle) {
        use sill_core::wire::Event as W;
        let ev = match ev {
            super::client::Lifecycle::Event(e) => e,
            super::client::Lifecycle::ChangesChanged => {
                self.changes.refresh_if_shown();
                if self.tree.root.is_visible() {
                    self.tree.refresh();
                }
                return;
            }
            super::client::Lifecycle::Resolved { tool_call_id } => {
                if let Some(v) = self.views.borrow().get(id).cloned() {
                    v.conv.resolve_permission(&tool_call_id);
                }
                return;
            }
        };
        match ev {
            W::SessionEnded { deleted, reason } => {
                if reason == "idle" && let Some(v) = self.views.borrow().get(id) {
                    self.set_status(&format!("“{}” ended: idle", v.session.borrow().title));
                }
                self.session_ended(id, deleted)
            }
            W::Summarized { text, title } => {
                if let Some(v) = self.views.borrow().get(id).cloned() {
                    v.summarized(text, title);
                }
            }
            W::Note(n) => {
                if let Some(v) = self.views.borrow().get(id).cloned() {
                    v.note(&n);
                }
            }
            _ => {}
        }
    }

    /// The tab, page and pane go while the agent still writes the summary: the view stays in
    /// `views` so the turn runs on (the 90 s cap of `ask_summary` still holds) and `close_view`
    /// takes it when the answer arrives. Until then the session is in neither list.
    fn background_view(self: &Rc<Self>, id: &str) {
        if !self.backgrounded.borrow_mut().insert(id.to_string()) {
            return;
        }
        if let Some(v) = self.views.borrow().get(id) {
            self.pages.remove(&v.root);
            self.pane_host.remove(&v.pane);
        }
        self.drop_tab(id);
        self.set_status("the session ends: its summary is written in the background");
    }

    /// Quit: the window hides, every open session ends as its tab would be closed — the agents
    /// that can write their summaries do, out of sight — and the shelf exits when the last view
    /// has gone. Nothing open: exits at once.
    fn quit_all(self: &Rc<Self>) {
        self.window.set_visible(false);
        self.quitting.set(true);
        for id in self.tabs.ids() {
            self.close_session(&id, "quit");
            if self.views.borrow().contains_key(&id) {
                self.background_view(&id);
            }
        }
        self.quit_if_done();
    }

    fn quit_if_done(&self) {
        if self.quitting.get() && self.views.borrow().is_empty() && let Some(app) = self.window.application() {
            app.quit();
        }
    }

    /// Drop the tab and the view; the jail is stopped separately (it can take seconds).
    fn close_view(self: &Rc<Self>, id: &str) {
        let hidden = self.backgrounded.borrow_mut().remove(id);
        if let Some(v) = self.views.borrow_mut().remove(id) {
            v.quit();
            if !hidden {
                self.pages.remove(&v.root);
                self.pane_host.remove(&v.pane);
            }
            // this `v` should be the last reference: anything else is a leak (SPEC §4.11 #15)
            if std::env::var_os("SILL_TRACE").is_some() {
                eprintln!("{} view {id} closed, {} other reference(s) left", super::trace_ts(), Rc::strong_count(&v) - 1);
            }
        }
        self.drop_tab(id);
        self.sync_pane();
        // a view that went while in a turn sends no idle event: the busy set and the tray follow
        self.busy.borrow_mut().remove(id);
        super::tray::set_busy(!self.busy.borrow().is_empty());
        // the status is the selected tab's again: a session that ended in the background (its tab
        // long gone, `background_view`) left "its summary is written in the background" on
        // whichever tab took the selection, and nothing else would replace it
        if self.tabs.selected().is_none() {
            self.set_status("");
        } else {
            self.sync_status();
        }
        self.quit_if_done();
    }

    /// The tab goes; the drawer stops showing the gone session's work: it follows the tab that
    /// takes the selection (its pinned folder, or Files), or goes back to Files when none is left.
    fn drop_tab(&self, id: &str) {
        let was_selected = self.tabs.selected().as_deref() == Some(id);
        self.tabs.remove(id);
        if !was_selected {
            return;
        }
        match self.tabs.selected() {
            Some(next) => self.tree.follow_session(&next),
            None => {
                *self.tree.pin_target.borrow_mut() = None;
                self.tree.set_scope(None);
            }
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
        // `sub`: a dim second line under the name (an ended session's summary, docs/memory.md §2.5)
        let row = |agent: Option<&agents::Agent>, text: &str, sub: Option<&str>| -> (gtk::Button, gtk::SizeGroup) {
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("sill-menu-item");
            // Keep the icon in the title's grid row so a date below cannot shift its centre.
            let grid = gtk::Grid::new();
            grid.set_column_spacing(10);
            let i = super::icons::image(agent, color, 15);
            i.add_css_class("sill-menu-icon");
            i.set_valign(gtk::Align::Center);
            let l = gtk::Label::new(Some(text));
            l.set_xalign(0.0);
            l.set_valign(gtk::Align::Center);
            l.set_hexpand(true);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            // caps the menu's width: a long session title truncates instead of widening it
            l.set_max_width_chars(22);
            let title_height = gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical);
            title_height.add_widget(&i);
            title_height.add_widget(&l);
            grid.attach(&i, 0, 0, 1, 1);
            grid.attach(&l, 1, 0, 1, 1);
            if let Some(sub) = sub {
                let s = gtk::Label::new(Some(sub));
                s.set_xalign(0.0);
                s.set_ellipsize(gtk::pango::EllipsizeMode::End);
                s.set_max_width_chars(22);
                s.add_css_class("dim-label");
                s.add_css_class("sill-menu-sub");
                grid.attach(&s, 1, 1, 1, 1);
            }
            b.set_child(Some(&grid));
            (b, title_height)
        };
        // only agents that can actually start here: enabled, and their command exists (a built-in
        // whose package was removed on the Agents page drops out until it is installed again).
        // The row starts a normal session; the ghost glyph at its right a ghost (docs/memory.md
        // §3.1), fixed from then on.
        for (id, agent) in registry.iter().filter(|(_, a)| a.enabled && super::remote::registry::available(a, &home).is_some()) {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let (b, _) = row(Some(agent), &agent.name, None);
            b.set_hexpand(true);
            let g = super::icons::glyph_button("sill-ghost");
            g.add_css_class("flat");
            g.add_css_class("sill-menu-ghost");
            g.set_tooltip_text(Some(&format!("{} as a ghost — leaves no session behind: {}", agent.name, GHOST_TIP.trim_start_matches("Ghost — leaves no session behind: "))));
            for (button, ghost) in [(&b, false), (&g, true)] {
                let s = self.clone();
                let pop = pop.clone();
                let id = id.clone();
                button.connect_clicked(move |_| {
                    pop.popdown();
                    match create_session(&s.store, &id, "default", ghost) {
                        Ok(sess) => s.open_session(sess),
                        Err(e) => s.set_status(&format!("new session: {e:#}")),
                    }
                });
            }
            line.append(&b);
            line.append(&g);
            menu.append(&line);
        }
        let stored: Vec<Session> = self
            .store
            .sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !self.tabs.has(&s.id) && !(s.ghost && s.ended_at.is_some()))
            .collect();
        if !stored.is_empty() {
            let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
            sep.add_css_class("sill-menu-sep");
            menu.append(&sep);
            let head = gtk::Label::new(Some("Reopen"));
            head.add_css_class("sill-menu-caption");
            head.set_xalign(0.0);
            head.set_hexpand(true);
            menu.append(&head);
            // search over memory (docs/memory.md §2.5): titles, summaries, notes, the person's
            // prompts, with folder: agent: since: — the list keeps the hits, best first
            let search = gtk::SearchEntry::new();
            search.add_css_class("sill-menu-search");
            super::icons::adopt_search_entry(&search);
            search.set_tooltip_text(Some("Search the sessions' titles, summaries, notes and your prompts. folder:~/path, agent:claude and since:30d narrow it."));
            menu.append(&search);
            let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let none = gtk::Label::new(Some("nothing found"));
            none.add_css_class("sill-menu-caption");
            none.set_xalign(0.0);
            none.set_visible(false);
            menu.append(&list);
            menu.append(&none);
            // the newest few by default; the rest behind one row, so a long history does not
            // push the menu off the screen. A search shows every hit regardless.
            const SHOWN: usize = 5;
            let more = gtk::Button::new();
            more.add_css_class("flat");
            more.add_css_class("sill-menu-more");
            more.set_visible(stored.len() > SHOWN);
            more.set_label(&format!("{} more…", stored.len().saturating_sub(SHOWN)));
            if let Some(l) = more.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0); // left, under the rows' names like the captions
            }
            menu.append(&more);
            let expanded = Rc::new(std::cell::Cell::new(false));
            let mut lines: Vec<(String, gtk::Box)> = Vec::new();
            for (n, sess) in stored.into_iter().enumerate() {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                line.set_visible(n < SHOWN);
                let sub = if sess.ghost {
                    Some("ghost · still running".to_string())
                } else {
                    sess.ended_at.filter(|_| !sess.summary.is_empty()).map(|t| sill_core::memory::date(t))
                };
                let (b, title_height) = row(registry.get(&sess.agent), &sess.title, sub.as_deref());
                if !sess.summary.is_empty() {
                    // the opening paragraphs only: a whole summary as a tooltip covers the menu
                    b.set_tooltip_text(Some(&sill_core::memory::brief(&sess.summary, 400)));
                }
                b.set_hexpand(true);
                let del = super::icons::glyph_button("sill-trash");
                del.add_css_class("flat");
                del.add_css_class("sill-menu-trash");
                del.set_tooltip_text(Some("Delete session and its recorded changes (click twice)"));
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
                    let pop = pop.clone();
                    let id = sess.id.clone();
                    // one click arms it, the next deletes: a slip on an ended row would lose its summary
                    del.connect_clicked(move |b| {
                        if !b.has_css_class("sill-armed") {
                            b.add_css_class("sill-armed");
                            b.set_tooltip_text(Some("Delete? Click again"));
                            return;
                        }
                        pop.popdown();
                        delete_then(&id, refresh_memory);
                    });
                }
                line.append(&b);
                let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                actions.set_halign(gtk::Align::End);
                title_height.add_widget(&actions);
                let actions_wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
                actions_wrap.set_valign(gtk::Align::Start);
                actions_wrap.add_css_class("sill-menu-actions");
                actions_wrap.append(&actions);
                line.append(&actions_wrap);
                // its page: summary and notes, without opening it (docs/memory.md §2.5)
                {
                    let page = super::icons::glyph_button("sill-file-text");
                    page.add_css_class("flat");
                    page.add_css_class("sill-menu-trash");
                    page.set_tooltip_text(Some("Summary and notes: the session's page, without opening the session"));
                    let s = self.clone();
                    let pop = pop.clone();
                    let id = sess.id.clone();
                    page.connect_clicked(move |_| {
                        pop.popdown();
                        s.open_session_page(&id);
                    });
                    actions.append(&page);
                }
                // an ended session: a new one in the same folders, told this one's summary first
                if sess.ended_at.is_some() && !sess.summary.is_empty() {
                    let here = super::icons::glyph_button("sill-plus");
                    here.add_css_class("flat");
                    here.add_css_class("sill-menu-trash");
                    here.set_tooltip_text(Some("New here: a new session with the same agent, policy and pins, told this one's summary first"));
                    let s = self.clone();
                    let pop = pop.clone();
                    let sess = sess.clone();
                    here.connect_clicked(move |_| {
                        pop.popdown();
                        s.new_here(&sess);
                    });
                    actions.append(&here);
                }
                actions.append(&del);
                list.append(&line);
                lines.push((sess.id.clone(), line));
            }
            {
                let lines = lines.clone();
                let expanded = expanded.clone();
                more.connect_clicked(move |b| {
                    expanded.set(true);
                    b.set_visible(false);
                    for (_, line) in &lines {
                        line.set_visible(true);
                    }
                });
            }
            {
                let shelf = self.clone();
                let list = list.clone();
                let none = none.clone();
                let more = more.clone();
                search.connect_search_changed(move |e| {
                    let text = e.text().to_string();
                    let q = sill_memory::Query::parse(&text, &home);
                    if q.is_empty() {
                        // back to the stored order and the first few, unless *more* was clicked
                        let mut prev: Option<gtk::Box> = None;
                        for (n, (_, line)) in lines.iter().enumerate() {
                            list.reorder_child_after(line, prev.as_ref());
                            prev = Some(line.clone());
                            line.set_visible(expanded.get() || n < SHOWN);
                        }
                        more.set_visible(!expanded.get() && lines.len() > SHOWN);
                        none.set_visible(false);
                        return;
                    }
                    more.set_visible(false);
                    let hits = shelf.store.search(&text, 500).unwrap_or_default();
                    let mut prev: Option<gtk::Box> = None;
                    for h in &hits {
                        if let Some((_, line)) = lines.iter().find(|(id, _)| id == h) {
                            list.reorder_child_after(line, prev.as_ref());
                            prev = Some(line.clone());
                        }
                    }
                    let mut shown = 0;
                    for (id, line) in &lines {
                        let hit = hits.iter().any(|h| h == id);
                        line.set_visible(hit);
                        shown += hit as usize;
                    }
                    none.set_visible(shown == 0);
                });
            }
        }
    }

    /// *New here* on an ended session (docs/memory.md §2.5): a new session with its agent, policy,
    /// memory scope and pins, whose recall leads with that session's summary.
    fn new_here(self: &Rc<Self>, from: &Session) {
        let new = sill_proto::NewSession {
            agent: from.agent.clone(),
            policy: from.policy.clone(),
            pins: self.store.pins(&from.id).unwrap_or_default(),
            memory_scope: from.memory_scope.clone(),
            follows: from.id.clone(),
            remember_pins: false,
            ghost: false,
        };
        let id = match super::client::new_session(new) {
            Ok(id) => id,
            Err(e) => {
                self.set_status(&format!("new session: {e:#}"));
                return;
            }
        };
        let Some(s) = self.store.sessions().unwrap_or_default().into_iter().find(|x| x.id == id) else { return };
        let note = format!("new here, after “{}” ({}): its summary goes to the agent first", from.title, from.ended_at.map(sill_core::memory::date).unwrap_or_default());
        self.open_session_with(s, Some(note));
    }
}

/// A new session, with the pins of the latest session that had any when Settings → Remember pins
/// is on (the sessions come newest first) — the same workspace without pinning it again. *New
/// here* copies its own source's pins and does not come this way.
fn create_session(store: &Store, agent: &str, policy: &str, ghost: bool) -> anyhow::Result<Session> {
    let new = sill_proto::NewSession {
        agent: agent.to_string(),
        policy: policy.to_string(),
        remember_pins: !ghost && super::remote::config::load().remember_pins(),
        ghost,
        ..Default::default()
    };
    let id = super::client::new_session(new)?;
    store.sessions()?.into_iter().find(|x| x.id == id).ok_or_else(|| anyhow::anyhow!("the new session {id} is not in the store"))
}

/// The file tree reads again (the global pins changed in Settings).
pub fn refresh_tree() {
    if let Some(shelf) = SHELF.with(|s| s.borrow().as_ref().and_then(|w| w.upgrade())) {
        shelf.tree.refresh_soon();
    }
}

/// Write the jail's memory snapshot anew off the GTK thread (docs/memory.md §2.3): when a session
/// ends or is deleted, at start, when memory is switched. Removes it with memory off.
pub fn refresh_memory() {
    std::thread::spawn(|| {
        if let Err(e) = super::remote::request::<serde_json::Value>(sill_core::frontend::Request::RefreshSnapshot) {
            eprintln!("memory snapshot: {e:#}");
        }
    });
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
            if let Err(e) = super::client::call::<serde_json::Value>("_sill/sessions.stop", sill_proto::sessions::SessionKey { session_id: id.clone() }) {
                eprintln!("sill: stopping session {id}: {e:#}");
            }
            let _ = tx.send_blocking(());
        })
        .ok();
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
        then();
    });
}

/// Delete a session through the daemon on a worker thread (jail stop, state, row) and run `then`
/// on the main loop when it is gone.
fn delete_then(id: &str, then: impl FnOnce() + 'static) {
    let id = id.to_string();
    let (tx, rx) = async_channel::bounded::<()>(1);
    std::thread::Builder::new()
        .name(format!("sill-delete-{id}"))
        .spawn(move || {
            if let Err(e) = super::client::delete(&id) {
                eprintln!("sill: deleting session {id}: {e:#}");
            }
            let _ = tx.send_blocking(());
        })
        .ok();
    glib::spawn_future_local(async move {
        let _ = rx.recv().await;
        then();
    });
}

/// What a ghost session's tab says (decision 9: "leaves no session behind", not "private").
const GHOST_TIP: &str = "Ghost — leaves no session behind: no pins, no memory written; writes outside sill revert; the session, its transcript and its overlay are deleted when it closes. Keep is the one way anything survives.";

/// The tray's Quit: end every session with its summary, then exit (`Shelf::quit_all`); with no
/// shelf built yet, exit at once.
pub fn quit_with_summaries(app: &gtk::Application) {
    match SHELF.with(|s| s.borrow().as_ref().and_then(|w| w.upgrade())) {
        Some(shelf) => shelf.quit_all(),
        None => app.quit(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The command line is split like a shell would, `{cwd}` is filled in and is the working directory.
    #[test]
    fn launch_terminal_runs_in_the_folder() {
        let dir = std::env::temp_dir().join(format!("sill-term-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        launch_terminal("sh -c 'pwd > \"$0/out.txt\"; echo \"$0\" >> \"$0/out.txt\"; echo \"$GIT_CONFIG_PARAMETERS\" >> \"$0/out.txt\"' {cwd}", &dir).unwrap();
        let out = dir.join("out.txt");
        let mut tries = 0;
        while !out.exists() && tries < 100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            tries += 1;
        }
        let text = std::fs::read_to_string(&out).unwrap();
        let canon = dir.canonicalize().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(std::path::Path::new(lines[0]).canonicalize().unwrap(), canon);
        assert_eq!(lines[1], dir.to_string_lossy());
        assert!(launch_terminal("", &dir).is_err());
        assert_eq!(lines[2], changes::HOST_SHELL_GIT_ENV.1, "git's fsmonitor is off in a host shell (S8)");
        assert!(launch_terminal("/nonexistent/terminal", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
