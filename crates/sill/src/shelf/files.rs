//! Backend page "Files" (SPEC §3.1): a global file explorer over the host — tree rooted at the home
//! directory plus every repository a session touched (pinned folders first), change badges from the
//! combined ledger — and a simple editor beside it (GtkSourceView: syntax highlighting, line numbers,
//! Ctrl+S saves). A file with a pending session change shows the diff instead of the editor.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use sill_core::changes;
use sill_core::ledger::Change;
use sill_core::store::Store;
use sourceview5::prelude::*;

use super::{session, theme};

pub struct FilesPage {
    pub root: gtk::Box,
    view: gtk::ListView,
    editor: sourceview5::View,
    buffer: sourceview5::Buffer,
    header: gtk::Label,
    message: gtk::Label,
    store: Rc<Store>,
    roots_model: gio::ListStore,
    badges: Rc<RefCell<BTreeMap<PathBuf, String>>>,
    changes: Rc<RefCell<Vec<Change>>>,
    pinned: Rc<RefCell<Vec<PathBuf>>>,
    /// File open in the editor (None while showing a diff or nothing).
    open: RefCell<Option<PathBuf>>,
    dirty: Cell<bool>,
    /// Session that receives pins (the selected tab), set by the window.
    pub pin_target: RefCell<Option<String>>,
    pub on_pin: RefCell<Option<Box<dyn Fn(&str)>>>,
}

/// Icon for a tree row: folder (open/closed) or the file's MIME symbolic icon.
fn file_icon(path: &Path, is_dir: bool, expanded: bool) -> gio::Icon {
    if is_dir {
        return gio::ThemedIcon::new(if expanded { "folder-open-symbolic" } else { "folder-symbolic" }).upcast();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (ctype, _) = gio::content_type_guess(Some(name.as_str()), None);
    gio::content_type_get_symbolic_icon(&ctype)
}

fn children_of(dir: &Path) -> gio::ListStore {
    let store = gio::ListStore::new::<gtk::StringObject>();
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    // directories first, dot files last, case-insensitive
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.starts_with('.').cmp(&b.1.starts_with('.')))
            .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    for (_, name) in entries {
        store.append(&gtk::StringObject::new(&dir.join(name).to_string_lossy()));
    }
    store
}

impl FilesPage {
    pub fn new(store: Rc<Store>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        // --- tree
        let badges: Rc<RefCell<BTreeMap<PathBuf, String>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let roots_model = gio::ListStore::new::<gtk::StringObject>();
        let tree = gtk::TreeListModel::new(roots_model.clone(), false, false, |item| {
            let path = PathBuf::from(item.downcast_ref::<gtk::StringObject>()?.string().as_str());
            if path.is_dir() { Some(children_of(&path).upcast()) } else { None }
        });
        let selection = gtk::SingleSelection::new(Some(tree));
        selection.set_autoselect(false);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let expander = gtk::TreeExpander::new();
            expander.set_indent_for_icon(false);
            expander.set_indent_for_depth(true);
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::from_icon_name("folder-symbolic");
            icon.add_css_class("sill-file-icon");
            b.append(&icon);
            let name = gtk::Label::new(None);
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let badge = gtk::Label::new(None);
            badge.add_css_class("sill-badge");
            let pin = gtk::Button::from_icon_name("list-add-symbolic");
            pin.set_tooltip_text(Some("Pin to the current session"));
            pin.add_css_class("flat");
            pin.add_css_class("sill-pin");
            pin.set_visible(false);
            b.append(&name);
            b.append(&badge);
            b.append(&pin);
            expander.set_child(Some(&b));
            item.set_child(Some(&expander));
        });
        let page_for_bind: Rc<RefCell<Option<std::rc::Weak<FilesPage>>>> = Rc::new(RefCell::new(None));
        let pinned_paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let badges = badges.clone();
            let page_for_bind = page_for_bind.clone();
            let pinned_paths = pinned_paths.clone();
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
                let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
                expander.set_list_row(Some(&row));
                let path = PathBuf::from(row.item().and_downcast::<gtk::StringObject>().unwrap().string().as_str());
                let b = expander.child().and_downcast::<gtk::Box>().unwrap();
                let icon = b.first_child().and_downcast::<gtk::Image>().unwrap();
                let name = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
                let badge = name.next_sibling().and_downcast::<gtk::Label>().unwrap();
                let pin = b.last_child().and_downcast::<gtk::Button>().unwrap();
                let is_dir = path.is_dir();
                icon.set_from_gicon(&file_icon(&path, is_dir, row.is_expanded()));
                let fname = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                for c in ["sill-changed", "sill-dim", "sill-pinned"] {
                    name.remove_css_class(c);
                }
                if fname.starts_with('.') || matches!(fname.as_str(), "target" | "node_modules" | "__pycache__" | "build") {
                    name.add_css_class("sill-dim");
                }
                let label = if row.depth() == 0 {
                    path.display().to_string().replace(&std::env::var("HOME").unwrap_or_default(), "~")
                } else {
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                };
                name.set_text(&label);
                let badge_text = badges.borrow().get(&path).cloned().unwrap_or_default();
                if !badge_text.is_empty() && badge_text != "·" {
                    name.add_css_class("sill-changed");
                }
                let pinned = pinned_paths.borrow().contains(&path);
                if pinned {
                    name.add_css_class("sill-pinned");
                }
                badge.set_text(&if pinned { format!("{badge_text} ⌖").trim().to_string() } else { badge_text });
                badge.set_tooltip_text(if pinned { Some("pinned to the current session") } else { None });
                pin.set_visible(is_dir);
                pin.set_icon_name(if pinned { "list-remove-symbolic" } else { "list-add-symbolic" });
                pin.set_tooltip_text(Some(if pinned { "Unpin from the current session" } else { "Pin to the current session" }));
                let p = path.clone();
                let weak = page_for_bind.borrow().clone();
                // one handler per bind: disconnect the previous by replacing the button's action
                pin.set_action_name(None);
                let handler_id = pin.connect_clicked(move |_| {
                    if let Some(page) = weak.as_ref().and_then(|w| w.upgrade()) {
                        page.toggle_pin(&p);
                    }
                });
                unsafe {
                    // remember the handler so the next bind can drop it
                    if let Some(old) = pin.steal_data::<glib::SignalHandlerId>("sill-pin-handler") {
                        pin.disconnect(old);
                    }
                    pin.set_data("sill-pin-handler", handler_id);
                }
            });
        }
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        view.add_css_class("sill-files");
        let tree_scroll = gtk::ScrolledWindow::builder().vexpand(true).hexpand(false).child(&view).width_request(300).build();

        // --- editor
        let buffer = sourceview5::Buffer::new(None);
        let editor = sourceview5::View::with_buffer(&buffer);
        editor.add_css_class("sill-editor");
        editor.set_show_line_numbers(true);
        editor.set_monospace(true);
        editor.set_tab_width(4);
        editor.set_highlight_current_line(true);
        editor.set_left_margin(6);
        editor.set_top_margin(6);
        editor.set_vexpand(true);
        editor.set_hexpand(true);
        let editor_scroll = gtk::ScrolledWindow::builder().child(&editor).vexpand(true).hexpand(true).build();
        let header = gtk::Label::new(Some("select a file"));
        header.set_xalign(0.0);
        header.add_css_class("dim-label");
        header.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        header.set_margin_start(8);
        header.set_margin_top(6);
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.add_css_class("dim-label");
        message.set_margin_start(8);
        let right = gtk::Box::new(gtk::Orientation::Vertical, 2);
        right.append(&header);
        right.append(&editor_scroll);
        right.append(&message);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&tree_scroll));
        paned.set_end_child(Some(&right));
        paned.set_position(340);
        paned.set_hexpand(true);
        root.append(&paned);

        let page = Rc::new(Self {
            root,
            view,
            editor,
            buffer,
            header,
            message,
            store,
            roots_model,
            badges,
            changes: Rc::new(RefCell::new(Vec::new())),
            pinned: pinned_paths.clone(),
            open: RefCell::new(None),
            dirty: Cell::new(false),
            pin_target: RefCell::new(None),
            on_pin: RefCell::new(None),
        });
        *page_for_bind.borrow_mut() = Some(Rc::downgrade(&page));

        // style schemes generated from the palettes, then follow the theme
        if let Some(dir) = theme::write_source_schemes() {
            sourceview5::StyleSchemeManager::default().append_search_path(&dir.to_string_lossy());
        }
        page.apply_scheme(&theme::current());
        {
            let weak = Rc::downgrade(&page);
            theme::on_change(move |t| {
                if let Some(p) = weak.upgrade() {
                    p.apply_scheme(t);
                }
            });
        }
        {
            let p = page.clone();
            selection.connect_selected_item_notify(move |sel| {
                let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
                let path = PathBuf::from(row.item().and_downcast::<gtk::StringObject>().unwrap().string().as_str());
                if path.is_file() {
                    p.show_file(&path);
                }
            });
        }
        {
            let p = page.clone();
            page.buffer.connect_changed(move |_| {
                if p.open.borrow().is_some() && !p.dirty.get() {
                    p.dirty.set(true);
                    p.update_header();
                }
            });
        }
        {
            let p = page.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, state| {
                if key == gdk::Key::s && state.contains(gdk::ModifierType::CONTROL_MASK) {
                    p.save();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            page.editor.add_controller(keys);
        }
        page
    }

    fn apply_scheme(&self, t: &theme::Theme) {
        let id = theme::source_scheme(t);
        if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&id) {
            self.buffer.set_style_scheme(Some(&s));
        }
    }

    fn update_header(&self) {
        match self.open.borrow().as_ref() {
            Some(p) => self.header.set_text(&format!("{}{}", p.display(), if self.dirty.get() { "  ● unsaved (Ctrl+S)" } else { "" })),
            None => {}
        }
    }

    fn show_file(self: &Rc<Self>, path: &Path) {
        if self.dirty.get()
            && let Some(open) = self.open.borrow().as_ref()
        {
            self.message.set_text(&format!("unsaved changes in {} discarded", open.display()));
        }
        self.dirty.set(false);
        // pending session change: show the diff, not the host file
        let pending = self.changes.borrow().iter().find(|c| c.path == path).cloned();
        if let Some(c) = pending {
            *self.open.borrow_mut() = None;
            self.header.set_text(&format!("{} — pending change ({}); keep or revert it in Changes before editing", path.display(), c.kind.letter()));
            let text = changes::unified_diff(&c).unwrap_or_else(|e| format!("diff: {e:#}"));
            self.buffer.set_language(sourceview5::LanguageManager::default().language("diff").as_ref());
            self.buffer.set_text(&text);
            self.editor.set_editable(false);
            return;
        }
        let md = std::fs::metadata(path);
        if md.as_ref().map(|m| m.len() > 2_000_000).unwrap_or(false) {
            *self.open.borrow_mut() = None;
            self.header.set_text(&format!("{} — too large for the editor", path.display()));
            self.buffer.set_text("");
            return;
        }
        let bytes = std::fs::read(path).unwrap_or_default();
        if bytes.iter().take(8192).any(|b| *b == 0) {
            *self.open.borrow_mut() = None;
            self.header.set_text(&format!("{} — binary", path.display()));
            self.buffer.set_text("");
            return;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let lang = sourceview5::LanguageManager::default().guess_language(Some(path), None);
        self.buffer.set_language(lang.as_ref());
        self.buffer.set_text(&text);
        self.editor.set_editable(md.map(|m| !m.permissions().readonly()).unwrap_or(false));
        *self.open.borrow_mut() = Some(path.to_path_buf());
        self.dirty.set(false);
        self.update_header();
        self.message.set_text("");
    }

    fn save(&self) {
        let Some(path) = self.open.borrow().clone() else { return };
        let text = self.buffer.text(&self.buffer.start_iter(), &self.buffer.end_iter(), true).to_string();
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.dirty.set(false);
                self.update_header();
                self.message.set_text(&format!("saved {}", path.display()));
            }
            Err(e) => self.message.set_text(&format!("save failed: {e}")),
        }
    }

    fn toggle_pin(self: &Rc<Self>, path: &Path) {
        let Some(sid) = self.pin_target.borrow().clone() else {
            self.message.set_text("no session selected to pin to");
            return;
        };
        let was = self.pinned.borrow().contains(&path.to_path_buf());
        let r = if was { self.store.unpin(&sid, path) } else { self.store.pin(&sid, path) };
        match r {
            Ok(()) => {
                self.message.set_text(&format!("{} {}", if was { "unpinned" } else { "pinned" }, path.display()));
                if let Some(cb) = self.on_pin.borrow().as_ref() {
                    cb(&sid);
                }
                self.refresh();
            }
            Err(e) => self.message.set_text(&format!("pin failed: {e}")),
        }
    }

    /// Rescan all sessions and rebuild badges and roots.
    pub fn refresh(self: &Rc<Self>) {
        let sessions = self.store.sessions().unwrap_or_default();
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        let pins: Vec<PathBuf> = sessions.iter().flat_map(|s| self.store.pins(&s.id).unwrap_or_default()).collect();
        let target_pins: Vec<PathBuf> = self.pin_target.borrow().as_ref().and_then(|t| self.store.pins(t).ok()).unwrap_or_default();
        *self.pinned.borrow_mut() = target_pins;
        let (tx, rx) = async_channel::bounded::<Vec<Change>>(1);
        std::thread::spawn(move || {
            let mut all = Vec::new();
            for s in sessions {
                let Ok(policy) = sill_core::policy::Policy::load(&s.policy, &home) else { continue };
                let Ok(state_dir) = sill_core::session::state_dir(uid, &s.id) else { continue };
                if !state_dir.is_dir() {
                    continue;
                }
                let Ok(plan) = changes::session_plan(policy.clone(), state_dir, &home, uid, &user) else { continue };
                all.extend(changes::scan(&policy, &plan, None).unwrap_or_default());
            }
            let _ = tx.send_blocking(all);
        });
        let p = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(all) = rx.recv().await {
                p.apply(all, pins);
            }
        });
    }

    fn apply(self: &Rc<Self>, all: Vec<Change>, pins: Vec<PathBuf>) {
        let home = session::home().unwrap_or_default();
        let mut badges: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut roots: Vec<PathBuf> = pins;
        for c in &all {
            badges.insert(c.path.clone(), format!("{}{}", c.kind.letter(), if c.conflict { "!" } else { "" }));
            let mut p = c.path.parent();
            while let Some(d) = p {
                badges.entry(d.to_path_buf()).or_insert_with(|| "·".into());
                p = d.parent();
            }
            if let Some(r) = changes::repo_root(&c.path)
                && !roots.contains(&r)
            {
                roots.push(r);
            }
        }
        roots.dedup();
        if !roots.contains(&home) {
            roots.push(home);
        }
        *self.badges.borrow_mut() = badges;
        *self.changes.borrow_mut() = all;
        // keep the tree if the roots didn't change (expansion state survives)
        let current: Vec<String> = (0..self.roots_model.n_items())
            .filter_map(|i| self.roots_model.item(i).and_downcast::<gtk::StringObject>().map(|o| o.string().to_string()))
            .collect();
        let wanted: Vec<String> = roots.iter().map(|r| r.to_string_lossy().into_owned()).collect();
        if current != wanted {
            self.roots_model.remove_all();
            for r in &wanted {
                self.roots_model.append(&gtk::StringObject::new(r));
            }
        }
        self.view.queue_draw();
    }
}
