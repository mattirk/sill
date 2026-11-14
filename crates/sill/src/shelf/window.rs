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

use super::changes::ChangesPage;
use super::conversation::CSS;
use super::files::FilesPage;
use super::session_view::SessionView;
use super::{session, settings, x11};

pub struct Shelf {
    pub window: adw::ApplicationWindow,
    store: Rc<Store>,
    stack: gtk::Stack,
    tab_view: adw::TabView,
    status: gtk::Label,
    views: RefCell<HashMap<String, Rc<SessionView>>>,
    changes: Rc<ChangesPage>,
    files: Rc<FilesPage>,
}

pub fn build(app: &adw::Application, _cwd: PathBuf) -> gtk::Window {
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
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

    let action = gtk::MenuButton::new();
    action.set_label("sill");
    action.add_css_class("heading");
    action.add_css_class("flat");
    let popover = gtk::Popover::new();
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    popover.set_child(Some(&menu));
    action.set_popover(Some(&popover));
    bar.append(&action);

    let tab_view = adw::TabView::new();
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_autohide(false);
    tab_bar.set_expand_tabs(false);
    tab_bar.set_hexpand(true);
    bar.append(&tab_bar);

    let plus = gtk::MenuButton::new();
    plus.set_icon_name("list-add-symbolic");
    plus.add_css_class("flat");
    plus.set_tooltip_text(Some("New session"));
    let plus_pop = gtk::Popover::new();
    let plus_menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    plus_pop.set_child(Some(&plus_menu));
    plus.set_popover(Some(&plus_pop));
    bar.append(&plus);

    let status = gtk::Label::new(Some(""));
    status.add_css_class("dim-label");
    bar.append(&status);

    // --- pages
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_vexpand(true);
    stack.add_named(&tab_view, Some("sessions"));
    let changes = ChangesPage::new(store.clone());
    stack.add_named(&changes.root, Some("changes"));
    let files = FilesPage::new(store.clone());
    stack.add_named(&files.root, Some("files"));
    stack.add_named(&settings::build(), Some("settings"));

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&bar);
    column.append(&stack);
    window.set_content(Some(&column));

    let shelf = Rc::new(Shelf {
        window: window.clone(),
        store: store.clone(),
        stack: stack.clone(),
        tab_view: tab_view.clone(),
        status: status.clone(),
        views: RefCell::new(HashMap::new()),
        changes: changes.clone(),
        files: files.clone(),
    });

    // dropdown entries
    for (label, page) in [("Sessions", "sessions"), ("Changes", "changes"), ("Files", "files"), ("Settings", "settings")] {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        let s = shelf.clone();
        let pop = popover.clone();
        b.connect_clicked(move |_| {
            pop.popdown();
            s.show_page(page);
        });
        menu.append(&b);
    }

    // "+" entries: one per agent, plus stored sessions that aren't open
    {
        let s = shelf.clone();
        let plus_menu = plus_menu.clone();
        plus_pop.connect_show(move |pop| s.fill_plus_menu(&plus_menu, pop));
    }

    // selected tab → status label follows it
    {
        let s = shelf.clone();
        tab_view.connect_selected_page_notify(move |_| s.sync_status());
    }
    // closing a tab: quit its driver and stop its unit; the stored session stays (reopen from "+")
    {
        let s = shelf.clone();
        tab_view.connect_close_page(move |tv, page| {
            if let Some(id) = page_session_id(page) {
                if let Some(v) = s.views.borrow_mut().remove(&id) {
                    v.quit();
                }
                let _ = sill_core::session::stop(&id);
            }
            tv.close_page_finish(page, true);
            glib::Propagation::Stop
        });
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

    // Escape: back to sessions, or hide
    let esc = gtk::EventControllerKey::new();
    {
        let s = shelf.clone();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if s.stack.visible_child_name().as_deref() != Some("sessions") {
                    s.show_page("sessions");
                } else {
                    s.window.set_visible(false);
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(esc);

    // place on the left edge each time it is mapped
    {
        let s = shelf.clone();
        window.connect_map(move |w| {
            let w: gtk::Window = w.clone().upcast();
            let s = s.clone();
            glib::idle_add_local_once(move || {
                if let Some(area) = x11::work_area(&w) {
                    x11::place(&w, &area);
                }
                x11::activate(&w);
                s.focus_prompt();
            });
        });
    }
    // keep the Rc alive as long as the window
    unsafe {
        window.set_data("sill-shelf", shelf.clone());
    }
    window.upcast()
}

fn page_session_id(page: &adw::TabPage) -> Option<String> {
    page.keyword().map(|k| k.to_string())
}

impl Shelf {
    fn show_page(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        match name {
            "changes" => self.changes.refresh(),
            "files" => self.files.refresh(),
            "sessions" => self.focus_prompt(),
            _ => {}
        }
    }

    fn focus_prompt(&self) {
        if let Some(page) = self.tab_view.selected_page()
            && let Some(id) = page_session_id(&page)
            && let Some(v) = self.views.borrow().get(&id)
        {
            v.focus_prompt();
        }
    }

    fn sync_status(&self) {
        if let Some(page) = self.tab_view.selected_page()
            && let Some(id) = page_session_id(&page)
            && let Some(v) = self.views.borrow().get(&id)
        {
            self.status.set_text(&v.status.text());
        }
    }

    fn open_session(self: &Rc<Self>, s: Session) {
        let id = s.id.clone();
        if let Some(page) = self.find_page(&id) {
            self.tab_view.set_selected_page(&page);
            return;
        }
        let view = SessionView::new(s.clone(), self.store.clone());
        let page = self.tab_view.append(&view.root);
        page.set_title(&s.title);
        page.set_keyword(&id);
        page.set_tooltip(&format!("{} · {}", s.agent, s.id));
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
            *view.on_title.borrow_mut() = Some(Box::new(move |id, title| {
                if let Some(p) = shelf.find_page(id) {
                    p.set_title(title);
                }
            }));
        }
        {
            // the top-bar status mirrors the selected view's status
            let shelf = self.clone();
            let vid = id.clone();
            view.status.connect_notify_local(Some("label"), move |l, _| {
                if let Some(page) = shelf.tab_view.selected_page()
                    && page_session_id(&page).as_deref() == Some(vid.as_str())
                {
                    shelf.status.set_text(&l.text());
                }
            });
        }
        self.views.borrow_mut().insert(id, view);
        self.tab_view.set_selected_page(&page);
        self.sync_status();
    }

    fn find_page(&self, id: &str) -> Option<adw::TabPage> {
        (0..self.tab_view.n_pages()).map(|i| self.tab_view.nth_page(i)).find(|p| page_session_id(p).as_deref() == Some(id))
    }

    fn fill_plus_menu(self: &Rc<Self>, menu: &gtk::Box, pop: &gtk::Popover) {
        while let Some(c) = menu.first_child() {
            menu.remove(&c);
        }
        let home = session::home().unwrap_or_default();
        let head = gtk::Label::new(Some("New session with"));
        head.add_css_class("dim-label");
        head.set_xalign(0.0);
        menu.append(&head);
        for (id, agent) in agents::load(&home) {
            let b = gtk::Button::with_label(&agent.name);
            b.add_css_class("flat");
            let s = self.clone();
            let pop = pop.clone();
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
            .filter(|s| self.find_page(&s.id).is_none())
            .collect();
        if !stored.is_empty() {
            let head = gtk::Label::new(Some("Reopen"));
            head.add_css_class("dim-label");
            head.set_xalign(0.0);
            head.set_margin_top(6);
            menu.append(&head);
            for sess in stored {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                let b = gtk::Button::with_label(&format!("{} ({})", sess.title, sess.agent));
                b.add_css_class("flat");
                b.set_hexpand(true);
                let del = gtk::Button::from_icon_name("user-trash-symbolic");
                del.add_css_class("flat");
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
                        let _ = sill_core::session::stop(&id);
                        let _ = sill_core::session::remove_state(session::uid(), &id);
                        let _ = s.store.delete(&id);
                    });
                }
                row.append(&b);
                row.append(&del);
                menu.append(&row);
            }
        }
    }
}

pub fn show(w: &gtk::Window) {
    w.present();
    x11::activate(w);
}

pub fn toggle(w: &gtk::Window) {
    if w.is_visible() && (w.is_active() || x11::is_active(w)) {
        w.set_visible(false);
    } else {
        show(w);
    }
}
