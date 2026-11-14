//! Backend page "Files" (SPEC §3.1): a tree of the repositories touched by the sessions (and the home
//! directory), with change badges from the ledger. Click = diff of a changed file.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use sill_core::changes;
use sill_core::ledger::Change;
use sill_core::store::Store;

use super::session;

pub struct FilesPage {
    pub root: gtk::Box,
    view: gtk::ListView,
    diff: gtk::TextView,
    store: Rc<Store>,
    roots_model: gio::ListStore,
    /// Badges: host path → letter; directories get "·" when something below changed.
    badges: Rc<RefCell<BTreeMap<PathBuf, String>>>,
    changes: Rc<RefCell<Vec<Change>>>,
    roots: Rc<RefCell<Vec<PathBuf>>>,
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
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    for (_, name) in entries {
        store.append(&gtk::StringObject::new(&dir.join(name).to_string_lossy()));
    }
    store
}

impl FilesPage {
    pub fn new(store: Rc<Store>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_top(6);
        root.set_margin_bottom(12);

        let badges: Rc<RefCell<BTreeMap<PathBuf, String>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let roots_model = gio::ListStore::new::<gtk::StringObject>();
        let tree = gtk::TreeListModel::new(roots_model.clone(), false, false, |item| {
            let path = PathBuf::from(item.downcast_ref::<gtk::StringObject>()?.string().as_str());
            if path.is_dir() { Some(children_of(&path).upcast()) } else { None }
        });
        let selection = gtk::SingleSelection::new(Some(tree));
        let factory = gtk::SignalListItemFactory::new();
        {
            let badges = badges.clone();
            factory.connect_setup(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let expander = gtk::TreeExpander::new();
                let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let name = gtk::Label::new(None);
                name.set_xalign(0.0);
                name.set_hexpand(true);
                name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                let badge = gtk::Label::new(None);
                badge.add_css_class("monospace");
                badge.add_css_class("accent");
                b.append(&name);
                b.append(&badge);
                expander.set_child(Some(&b));
                item.set_child(Some(&expander));
                let _ = &badges;
            });
        }
        {
            let badges = badges.clone();
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
                let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
                expander.set_list_row(Some(&row));
                let path = PathBuf::from(row.item().and_downcast::<gtk::StringObject>().unwrap().string().as_str());
                let b = expander.child().and_downcast::<gtk::Box>().unwrap();
                let name = b.first_child().and_downcast::<gtk::Label>().unwrap();
                let badge = b.last_child().and_downcast::<gtk::Label>().unwrap();
                let label = if row.depth() == 0 {
                    path.display().to_string()
                } else {
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                };
                name.set_text(&label);
                badge.set_text(badges.borrow().get(&path).map(String::as_str).unwrap_or(""));
            });
        }
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        let view_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&view).build();

        let diff = gtk::TextView::builder().editable(false).cursor_visible(false).monospace(true).left_margin(8).right_margin(8).build();
        let diff_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&diff).build();
        diff_scroll.add_css_class("frame");
        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.set_start_child(Some(&view_scroll));
        paned.set_end_child(Some(&diff_scroll));
        paned.set_position(600);
        paned.set_vexpand(true);
        root.append(&paned);

        let page = Rc::new(Self {
            root,
            view,
            diff,
            store,
            roots_model,
            badges,
            changes: Rc::new(RefCell::new(Vec::new())),
            roots: Rc::new(RefCell::new(Vec::new())),
        });
        {
            let p = page.clone();
            selection.connect_selected_item_notify(move |sel| {
                let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
                let path = PathBuf::from(row.item().and_downcast::<gtk::StringObject>().unwrap().string().as_str());
                let changes = p.changes.borrow();
                match changes.iter().find(|c| c.path == path) {
                    Some(c) => p.diff.buffer().set_text(&changes::unified_diff(c).unwrap_or_else(|e| format!("diff: {e:#}"))),
                    None => p.diff.buffer().set_text(""),
                }
            });
        }
        page
    }

    /// Rescan all sessions and rebuild badges and roots.
    pub fn refresh(self: &Rc<Self>) {
        let sessions = self.store.sessions().unwrap_or_default();
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
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
                p.apply(all);
            }
        });
    }

    fn apply(self: &Rc<Self>, all: Vec<Change>) {
        let home = session::home().unwrap_or_default();
        let mut badges: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut roots: Vec<PathBuf> = Vec::new();
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
        roots.sort();
        if roots.is_empty() {
            roots.push(home);
        }
        *self.badges.borrow_mut() = badges;
        *self.changes.borrow_mut() = all;
        *self.roots.borrow_mut() = roots.clone();
        self.roots_model.remove_all();
        for r in roots {
            self.roots_model.append(&gtk::StringObject::new(&r.to_string_lossy()));
        }
        // repaint visible rows
        self.view.queue_draw();
    }
}

