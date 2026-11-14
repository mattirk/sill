//! The folder tree of a session (SPEC §3.1 pins): a click on a pinned folder's chip widens the shelf
//! to the right and shows that folder here, two levels open. A click on a file opens it in the
//! viewer on the left; a click on a folder opens or closes it. Escape (from the viewer, then here)
//! steps back and finally narrows the shelf again.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gio;
use gtk::prelude::*;

use super::files::{children_of, file_icon};

/// Folders below this depth are opened when the tree is shown (0 = the pinned folder itself).
const OPEN_DEPTH: u32 = 2;
/// Opening stops once this many rows are shown (a huge folder must not stall the shelf).
const OPEN_LIMIT: u32 = 600;

/// Build output and tool state: listed, never opened by default, dimmed.
fn is_noise(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules" | "__pycache__" | "build" | "dist")
}

fn path_of(row: &gtk::TreeListRow) -> Option<PathBuf> {
    row.item().and_downcast::<gtk::StringObject>().map(|s| PathBuf::from(s.string().as_str()))
}

pub struct FolderTree {
    pub root: gtk::Box,
    title: gtk::Label,
    roots: gio::ListStore,
    tree: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    folder: RefCell<Option<PathBuf>>,
    pub on_open: RefCell<Option<Box<dyn Fn(&Path)>>>,
    pub on_close: RefCell<Option<Box<dyn Fn()>>>,
    pub on_pin: RefCell<Option<Box<dyn Fn(&Path)>>>,
    /// "Open in Files": the full explorer and editor on the backend page.
    pub on_files: RefCell<Option<Box<dyn Fn(&Path)>>>,
}

impl FolderTree {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sill-tree-panel");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("sill-viewer-bar");
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        title.add_css_class("dim-label");
        let files = gtk::Button::from_icon_name("document-edit-symbolic");
        files.add_css_class("flat");
        files.add_css_class("sill-viewer-btn");
        files.set_tooltip_text(Some("Open in Files (edit)"));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("sill-viewer-btn");
        close.set_tooltip_text(Some("Close the tree (Escape)"));
        bar.append(&title);
        bar.append(&files);
        bar.append(&close);
        root.append(&bar);

        let roots = gio::ListStore::new::<gtk::StringObject>();
        let tree = gtk::TreeListModel::new(roots.clone(), false, false, |item| {
            let path = PathBuf::from(item.downcast_ref::<gtk::StringObject>()?.string().as_str());
            if path.is_dir() { Some(children_of(&path).upcast()) } else { None }
        });
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let expander = gtk::TreeExpander::new();
            expander.set_indent_for_icon(false);
            expander.set_indent_for_depth(true);
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let icon = gtk::Image::new();
            icon.add_css_class("sill-file-icon");
            let name = gtk::Label::new(None);
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let pin = gtk::Button::from_icon_name("view-pin-symbolic");
            pin.add_css_class("flat");
            pin.add_css_class("sill-pin");
            pin.set_tooltip_text(Some("Pin this file: a chip opens it full width"));
            b.append(&icon);
            b.append(&name);
            b.append(&pin);
            expander.set_child(Some(&b));
            item.set_child(Some(&expander));
        });
        let tree_rc: Rc<RefCell<Option<std::rc::Weak<FolderTree>>>> = Rc::new(RefCell::new(None));
        {
            let tree_rc = tree_rc.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
                let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else { return };
                expander.set_list_row(Some(&row));
                let Some(path) = path_of(&row) else { return };
                let Some(b) = expander.child().and_downcast::<gtk::Box>() else { return };
                let (Some(icon), Some(name), Some(pin)) = (
                    b.first_child().and_downcast::<gtk::Image>(),
                    b.first_child().and_then(|w| w.next_sibling()).and_downcast::<gtk::Label>(),
                    b.last_child().and_downcast::<gtk::Button>(),
                ) else {
                    return;
                };
                let is_dir = path.is_dir();
                icon.set_from_gicon(&file_icon(&path, is_dir, row.is_expanded()));
                let fname = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                name.set_text(&fname);
                if is_noise(&fname) {
                    name.add_css_class("sill-dim");
                } else {
                    name.remove_css_class("sill-dim");
                }
                // pinning a file: files only (folders are pinned from the Files page: that rebuilds the jail)
                pin.set_visible(!is_dir);
                let weak = tree_rc.borrow().clone();
                let p = path.clone();
                let id = pin.connect_clicked(move |_| {
                    if let Some(t) = weak.as_ref().and_then(|w| w.upgrade())
                        && let Some(cb) = t.on_pin.borrow().as_ref()
                    {
                        cb(&p);
                    }
                });
                // one handler per bind
                unsafe {
                    if let Some(old) = pin.steal_data::<gtk::glib::SignalHandlerId>("sill-pin-handler") {
                        pin.disconnect(old);
                    }
                    pin.set_data("sill-pin-handler", id);
                }
            });
        }
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        view.add_css_class("sill-files");
        view.set_single_click_activate(true);
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        root.append(&scroll);

        let t = Rc::new(Self {
            root,
            title,
            roots,
            tree,
            selection,
            folder: RefCell::new(None),
            on_open: RefCell::new(None),
            on_close: RefCell::new(None),
            on_pin: RefCell::new(None),
            on_files: RefCell::new(None),
        });
        *tree_rc.borrow_mut() = Some(Rc::downgrade(&t));
        {
            let weak = Rc::downgrade(&t);
            view.connect_activate(move |_, pos| {
                let Some(t) = weak.upgrade() else { return };
                let Some(row) = t.tree.row(pos) else { return };
                let Some(path) = path_of(&row) else { return };
                if path.is_dir() {
                    row.set_expanded(!row.is_expanded());
                } else if let Some(cb) = t.on_open.borrow().as_ref() {
                    t.selection.set_selected(pos);
                    cb(&path);
                }
            });
        }
        {
            let weak = Rc::downgrade(&t);
            close.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade()
                    && let Some(cb) = t.on_close.borrow().as_ref()
                {
                    cb();
                }
            });
        }
        {
            let weak = Rc::downgrade(&t);
            files.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade()
                    && let Some(p) = t.folder.borrow().clone()
                    && let Some(cb) = t.on_files.borrow().as_ref()
                {
                    cb(&p);
                }
            });
        }
        t
    }

    pub fn folder(&self) -> Option<PathBuf> {
        self.folder.borrow().clone()
    }

    /// Show `folder`, opened two levels deep (build output and dot folders stay closed).
    pub fn set_folder(&self, folder: &Path) {
        let home = std::env::var("HOME").unwrap_or_default();
        self.title.set_text(&folder.display().to_string().replacen(&home, "~", 1));
        self.title.set_tooltip_text(Some(&folder.display().to_string()));
        *self.folder.borrow_mut() = Some(folder.to_path_buf());
        self.roots.remove_all();
        self.roots.append(&gtk::StringObject::new(&folder.to_string_lossy()));
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        let mut i = 0;
        while i < self.tree.n_items() && self.tree.n_items() < OPEN_LIMIT {
            if let Some(row) = self.tree.row(i)
                && row.depth() < OPEN_DEPTH
                && row.is_expandable()
            {
                let name = path_of(&row).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
                if row.depth() == 0 || !is_noise(&name) {
                    row.set_expanded(true);
                }
            }
            i += 1;
        }
    }
}
