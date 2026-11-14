//! Session tab strip (SPEC §3.1). Our own, not adwaita's: each tab's label turns into an entry in
//! place on double-click (or F2), with the same font and padding, so renaming looks like the text
//! itself becoming editable.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

pub struct Tab {
    pub id: String,
    root: gtk::Box,
    /// Holds the agent glyph (set by the window, redrawn on theme change).
    icon_slot: gtk::Box,
    stack: gtk::Stack,
    label: gtk::Label,
    entry: gtk::Entry,
}

pub struct TabStrip {
    pub root: gtk::Box,
    tabs: RefCell<Vec<Rc<Tab>>>,
    selected: RefCell<Option<String>>,
    pub on_select: RefCell<Option<Box<dyn Fn(&str)>>>,
    pub on_close: RefCell<Option<Box<dyn Fn(&str)>>>,
    pub on_rename: RefCell<Option<Box<dyn Fn(&str, &str)>>>,
    /// The session menu (SPEC §3.1): opened from the tab's glyph or a right-click on the tab;
    /// called with the tab id and the tab widget to anchor a popover to.
    pub on_menu: RefCell<Option<Box<dyn Fn(&str, &gtk::Widget)>>>,
}

impl TabStrip {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("sill-tabs");
        root.set_hexpand(true);
        Rc::new(Self {
            root,
            tabs: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            on_select: RefCell::new(None),
            on_close: RefCell::new(None),
            on_rename: RefCell::new(None),
            on_menu: RefCell::new(None),
        })
    }

    pub fn has(&self, id: &str) -> bool {
        self.tabs.borrow().iter().any(|t| t.id == id)
    }

    pub fn selected(&self) -> Option<String> {
        self.selected.borrow().clone()
    }

    pub fn add(self: &Rc<Self>, id: &str, title: &str) {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        root.add_css_class("sill-tab");
        let icon_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_slot.add_css_class("sill-tab-icon");
        icon_slot.set_valign(gtk::Align::Center);
        icon_slot.set_cursor_from_name(Some("pointer"));
        root.append(&icon_slot);
        let stack = gtk::Stack::new();
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(true);
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(28);
        let entry = gtk::Entry::new();
        entry.add_css_class("sill-tab-entry");
        entry.set_has_frame(false);
        stack.add_named(&label, Some("label"));
        stack.add_named(&entry, Some("entry"));
        stack.set_visible_child_name("label");
        let close = super::icons::glyph_button("sill-x");
        close.add_css_class("flat");
        close.add_css_class("sill-tab-close");
        root.append(&stack);
        root.append(&close);
        let tab = Rc::new(Tab { id: id.to_string(), root: root.clone(), icon_slot, stack, label, entry });

        // click selects, double-click edits
        let click = gtk::GestureClick::new();
        click.set_button(1);
        {
            let strip = self.clone();
            let tab = tab.clone();
            click.connect_pressed(move |_, n, _, _| {
                if n == 1 {
                    strip.select(&tab.id);
                } else if n == 2 {
                    strip.begin_edit(&tab);
                }
            });
        }
        root.add_controller(click);
        // the glyph opens the session menu; so does a right-click anywhere on the tab
        {
            let strip = self.clone();
            let t = tab.clone();
            let on_glyph = gtk::GestureClick::new();
            on_glyph.set_button(1);
            on_glyph.connect_released(move |_, _, _, _| strip.open_menu(&t));
            tab.icon_slot.add_controller(on_glyph);
        }
        {
            let strip = self.clone();
            let t = tab.clone();
            let right = gtk::GestureClick::new();
            right.set_button(3);
            right.connect_pressed(move |_, _, _, _| strip.open_menu(&t));
            root.add_controller(right);
        }
        {
            let strip = self.clone();
            let id = tab.id.clone();
            close.connect_clicked(move |_| {
                if let Some(cb) = strip.on_close.borrow().as_ref() {
                    cb(&id);
                }
            });
        }
        // entry: Enter commits, Escape cancels, focus loss commits
        {
            let strip = self.clone();
            let t = tab.clone();
            tab.entry.connect_activate(move |_| strip.end_edit(&t, true));
        }
        {
            let strip = self.clone();
            let t = tab.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    strip.end_edit(&t, false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            tab.entry.add_controller(keys);
        }
        {
            let strip = self.clone();
            let t = tab.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_leave(move |_| {
                if t.stack.visible_child_name().as_deref() == Some("entry") {
                    strip.end_edit(&t, true);
                }
            });
            tab.entry.add_controller(focus);
        }
        self.root.append(&root);
        self.tabs.borrow_mut().push(tab);
    }

    pub fn remove(&self, id: &str) {
        let mut tabs = self.tabs.borrow_mut();
        if let Some(i) = tabs.iter().position(|t| t.id == id) {
            let t = tabs.remove(i);
            self.root.remove(&t.root);
        }
        let was_selected = self.selected.borrow().as_deref() == Some(id);
        drop(tabs);
        if was_selected {
            *self.selected.borrow_mut() = None;
            if let Some(next) = self.tabs.borrow().last().map(|t| t.id.clone()) {
                self.select(&next);
            }
        }
    }

    pub fn select(&self, id: &str) {
        for t in self.tabs.borrow().iter() {
            if t.id == id {
                t.root.add_css_class("selected");
            } else {
                t.root.remove_css_class("selected");
            }
        }
        let changed = self.selected.borrow().as_deref() != Some(id);
        *self.selected.borrow_mut() = Some(id.to_string());
        if changed && let Some(cb) = self.on_select.borrow().as_ref() {
            cb(id);
        }
    }

    /// Put (or replace) the small agent glyph in front of a tab's title.
    pub fn set_icon(&self, id: &str, icon: &impl IsA<gtk::Widget>) {
        if let Some(t) = self.tabs.borrow().iter().find(|t| t.id == id) {
            while let Some(c) = t.icon_slot.first_child() {
                t.icon_slot.remove(&c);
            }
            t.icon_slot.append(icon);
        }
    }

    /// Ids of all tabs, in order.
    pub fn ids(&self) -> Vec<String> {
        self.tabs.borrow().iter().map(|t| t.id.clone()).collect()
    }

    pub fn set_title(&self, id: &str, title: &str) {
        if let Some(t) = self.tabs.borrow().iter().find(|t| t.id == id) {
            t.label.set_text(title);
        }
    }

    pub fn set_tooltip(&self, id: &str, text: Option<&str>) {
        if let Some(t) = self.tabs.borrow().iter().find(|t| t.id == id) {
            t.root.set_tooltip_text(text);
        }
    }

    fn open_menu(&self, tab: &Rc<Tab>) {
        self.select(&tab.id);
        if let Some(cb) = self.on_menu.borrow().as_ref() {
            cb(&tab.id, tab.root.upcast_ref());
        }
    }

    /// Mark a tab as busy (its agent is in a turn): the agent glyph pulses, the title does not.
    pub fn set_busy(&self, id: &str, busy: bool) {
        if let Some(t) = self.tabs.borrow().iter().find(|t| t.id == id) {
            if busy {
                t.root.add_css_class("busy");
            } else {
                t.root.remove_css_class("busy");
            }
        }
    }


    /// Start editing the selected tab's name (F2).
    pub fn edit_selected(&self) {
        let Some(id) = self.selected() else { return };
        let tab = self.tabs.borrow().iter().find(|t| t.id == id).cloned();
        if let Some(t) = tab {
            self.begin_edit(&t);
        }
    }

    fn begin_edit(&self, tab: &Rc<Tab>) {
        tab.entry.set_text(&tab.label.text());
        // exactly the label's width so nothing moves (plus a little room to type)
        tab.entry.set_width_chars(1);
        tab.entry.set_size_request(tab.label.width().max(40) + 12, -1);
        tab.stack.set_visible_child_name("entry");
        tab.entry.grab_focus();
        tab.entry.select_region(0, -1);
    }

    fn end_edit(&self, tab: &Rc<Tab>, commit: bool) {
        if tab.stack.visible_child_name().as_deref() != Some("entry") {
            return;
        }
        if commit {
            let title = tab.entry.text().trim().to_string();
            if !title.is_empty() && title != tab.label.text().as_str() {
                tab.label.set_text(&title);
                if let Some(cb) = self.on_rename.borrow().as_ref() {
                    cb(&tab.id, &title);
                }
            }
        }
        tab.stack.set_visible_child_name("label");
    }
}

pub const CSS: &str = r#"
.sill-tabs { padding: 0; }
.sill-tab-icon { opacity: 0.55; margin-right: 3px; }
.sill-tab.selected .sill-tab-icon { opacity: 0.85; }
.sill-tab { padding: 4px 6px 4px 10px; margin-right: 1px; background: alpha(@view_fg_color, 0.04);
            border-bottom: 2px solid transparent; }
.sill-tab.selected { background: alpha(@view_fg_color, 0.09); border-bottom: 2px solid alpha(@accent_bg_color, 0.9); }
.sill-tab label { padding: 0; }
/* the agent of this tab is working: only its glyph pulses; the title stays as it is */
@keyframes sill-glyph-pulse { 50% { transform: scale(0.8); } }
.sill-tab.busy .sill-tab-icon { animation: sill-glyph-pulse 2.2s ease-in-out infinite; }
.sill-tab-entry, .sill-tab-entry:focus { border: none; box-shadow: none; outline: none; background: transparent;
                                         padding: 0; margin: 0; min-height: 0; border-radius: 0; }
.sill-tab-entry text { background: transparent; }
.sill-tab-entry selection { background: alpha(@accent_bg_color, 0.45); }
.sill-tab-close { padding: 0 2px; min-height: 0; min-width: 0; opacity: 0.6; }
.sill-tab-close:hover { opacity: 1; }
"#;
