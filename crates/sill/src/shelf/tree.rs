//! The file tree (SPEC §3.1): a drawer beside the main pane — the shelf widens by `x11::TREE_FRACTION`
//! while it is open and narrows again when it closes. Global (the home directory, the selected
//! session's pinned folders first, plus every repository a session touched) or scoped to one pinned
//! folder from its chip, two levels open (dot folders and build output listed but closed, dimmed).
//! Change badges come from the combined ledger of all sessions and from git status; the pin button on
//! a folder row makes a live mount (the jail is rebuilt), on a file row a chip only. A click on a file
//! opens it in the viewer over the main pane.

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

use super::{session, theme};

/// Folders below this depth are opened when a scoped tree is shown (0 = the pinned folder itself).
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

/// Icon for a tree row: folder (open/closed) or the file's MIME symbolic icon.
fn file_icon(path: &Path, is_dir: bool, expanded: bool) -> gio::Icon {
    if is_dir {
        return gio::ThemedIcon::new(if expanded { "folder-open-symbolic" } else { "folder-symbolic" }).upcast();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (ctype, _) = gio::content_type_guess(Some(name.as_str()), None);
    gio::content_type_get_symbolic_icon(&ctype)
}

/// A directory's entries as full paths: directories first, dot files last, case-insensitive.
/// `ghosts`: paths (below `dir`) that a session's overlay has and the host hasn't — listed as files
/// too, so a change always has a row (a folder's dot never points at nothing).
fn list_dir(dir: &Path, ghosts: &[String]) -> Vec<String> {
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default();
    for g in ghosts {
        let name = Path::new(g).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !name.is_empty() && !entries.iter().any(|(_, n)| *n == name) {
            entries.push((false, name));
        }
    }
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.starts_with('.').cmp(&b.1.starts_with('.')))
            .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    entries.into_iter().map(|(_, name)| dir.join(name).to_string_lossy().into_owned()).collect()
}

/// The directories the tree has listed, by their store (weak: the model drops a store when its row
/// collapses). A refresh brings each live one up to date in place.
type Listed = Rc<RefCell<BTreeMap<PathBuf, glib::WeakRef<gio::ListStore>>>>;

fn children_of(dir: &Path, listed: &Listed, ghosts: &[String]) -> gio::ListStore {
    let store = gio::ListStore::new::<gtk::StringObject>();
    for p in list_dir(dir, ghosts) {
        store.append(&gtk::StringObject::new(&p));
    }
    listed.borrow_mut().insert(dir.to_path_buf(), store.downgrade());
    store
}

/// What a row's badge says and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Badge {
    /// git's working-tree status of the path: `M`, `A`, `D`, `R` …, `?` untracked.
    Git(String),
    /// A session's unkept overlay change — not on the host until kept in Changes: the ledger's
    /// letter (with `!` for a conflict) and the session's title.
    Session { mark: String, session: String },
    /// A folder with a change somewhere below.
    Below,
}

impl Badge {
    fn text(&self) -> &str {
        match self {
            Badge::Git(s) | Badge::Session { mark: s, .. } => s,
            Badge::Below => "•",
        }
    }

    /// A change of its own (not a folder's dot).
    fn is_change(&self) -> bool {
        !matches!(self, Badge::Below)
    }
}

/// The changed paths right below `dir` that the host doesn't have (a session added them in its
/// overlay, or they went on the host while an overlay copy remains): rows of their own.
fn ghosts(badges: &BTreeMap<PathBuf, Badge>, dir: &Path) -> Vec<String> {
    badges
        .iter()
        .filter(|(p, b)| p.parent() == Some(dir) && b.is_change() && !p.exists())
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect()
}

/// Bring a listed directory up to date without rebuilding it: entries that went are removed, new
/// ones inserted at their place; everything else keeps its row (and what is expanded below it).
fn sync_children(dir: &Path, store: &gio::ListStore, ghosts: &[String]) {
    let wanted = list_dir(dir, ghosts);
    let (mut i, mut w) = (0u32, 0usize);
    loop {
        let have = store.item(i).and_downcast::<gtk::StringObject>().map(|o| o.string().to_string());
        match (have, wanted.get(w)) {
            (Some(h), Some(want)) if &h == want => {
                i += 1;
                w += 1;
            }
            (Some(h), Some(want)) => {
                if wanted[w..].contains(&h) {
                    store.insert(i, &gtk::StringObject::new(want)); // new, sorts before `h`
                    i += 1;
                    w += 1;
                } else {
                    store.remove(i); // gone
                }
            }
            (Some(_), None) => store.remove(i),
            (None, Some(want)) => {
                store.append(&gtk::StringObject::new(want));
                i += 1;
                w += 1;
            }
            (None, None) => break,
        }
    }
}

pub struct FolderTree {
    pub root: gtk::Box,
    title: gtk::Label,
    /// Back to the global tree while scoped to one folder.
    all: gtk::Button,
    view: gtk::ListView,
    roots: gio::ListStore,
    tree: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    /// When set, the tree shows only this folder (opened from a pin chip).
    scope: RefCell<Option<PathBuf>>,
    store: Rc<Store>,
    badges: Rc<RefCell<BTreeMap<PathBuf, Badge>>>,
    /// Every session's pending changes: the badges here, and the viewer shows a pending path's diff.
    pub changes: Rc<RefCell<Vec<Change>>>,
    pinned: Rc<RefCell<Vec<PathBuf>>>,
    /// Pinned by some other session than the current one: shown, but the button pins here.
    elsewhere: Rc<RefCell<Vec<PathBuf>>>,
    message: gtk::Label,
    /// Session that receives pins (the selected tab), set by the window.
    pub pin_target: RefCell<Option<String>>,
    pub on_open: RefCell<Option<Box<dyn Fn(&Path)>>>,
    pub on_close: RefCell<Option<Box<dyn Fn()>>>,
    /// A pin of this session changed: the window refreshes its chips and, for a folder, its jail.
    pub on_pin: RefCell<Option<Box<dyn Fn(&str)>>>,
    listed: Listed,
    /// The rows on screen and what decorates one from the current badges: after a refresh every
    /// bound row is decorated again.
    bound: Rc<RefCell<Vec<gtk::ListItem>>>,
    decorate: Rc<dyn Fn(&gtk::ListItem)>,
    /// Watches on every listed directory and on the repositories' `.git`: the tree follows what
    /// happens on disk (the agent's writes, hand edits, commits) while it is shown.
    monitors: RefCell<BTreeMap<PathBuf, gio::FileMonitor>>,
    refresh_pending: std::cell::Cell<bool>,
    /// A scan is in flight; a refresh asked meanwhile runs once more after it (`again`).
    scanning: std::cell::Cell<bool>,
    again: std::cell::Cell<bool>,
}

impl FolderTree {
    pub fn new(store: Rc<Store>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sill-tree-panel");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        bar.add_css_class("sill-viewer-bar");
        let title = gtk::Label::new(Some("Files"));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        title.add_css_class("dim-label");
        let all = gtk::Button::with_label("all");
        all.add_css_class("flat");
        all.add_css_class("sill-viewer-btn");
        all.set_tooltip_text(Some("Show everything: home, pinned folders, touched repositories"));
        all.set_visible(false);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("sill-viewer-btn");
        close.set_tooltip_text(Some("Close the tree (Escape)"));
        bar.append(&title);
        bar.append(&all);
        bar.append(&close);
        root.append(&bar);

        let badges: Rc<RefCell<BTreeMap<PathBuf, Badge>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let pinned: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        let elsewhere: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        let roots = gio::ListStore::new::<gtk::StringObject>();
        let listed: Listed = Rc::new(RefCell::new(BTreeMap::new()));
        // the tree itself, for closures made before it exists (set below)
        let tree_rc: Rc<RefCell<Option<std::rc::Weak<FolderTree>>>> = Rc::new(RefCell::new(None));
        let tree = gtk::TreeListModel::new(roots.clone(), false, false, {
            let listed = listed.clone();
            let tree_rc = tree_rc.clone();
            let badges = badges.clone();
            move |item| {
                let path = PathBuf::from(item.downcast_ref::<gtk::StringObject>()?.string().as_str());
                if !path.is_dir() {
                    return None;
                }
                let children = children_of(&path, &listed, &ghosts(&badges.borrow(), &path));
                if let Some(t) = tree_rc.borrow().as_ref().and_then(|w| w.upgrade()) {
                    t.watch_one(&path); // an opened directory is followed from now on
                }
                Some(children.upcast())
            }
        });
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        let setup_tree_rc = tree_rc.clone();
        factory.connect_setup(move |_, item| {
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
            let badge = gtk::Label::new(None);
            badge.add_css_class("sill-badge");
            let pin = gtk::Button::new();
            pin.add_css_class("flat");
            pin.add_css_class("sill-pin");
            // wired once: a click pins the row's path, which decorate keeps as the widget name
            let weak = setup_tree_rc.clone();
            pin.connect_clicked(move |b| {
                if let Some(t) = weak.borrow().as_ref().and_then(|w| w.upgrade()) {
                    t.toggle_pin(Path::new(b.widget_name().as_str()));
                }
            });
            b.append(&icon);
            b.append(&name);
            b.append(&badge);
            b.append(&pin);
            expander.set_child(Some(&b));
            item.set_child(Some(&expander));
        });
        // what a row shows for its path (icon, name and its colour, badge, pin) from the current
        // badges: at bind, and again for every bound row after a refresh — a badge that changed must
        // show without the row being made anew (closing and opening the folder was the only way)
        let bound: Rc<RefCell<Vec<gtk::ListItem>>> = Rc::new(RefCell::new(Vec::new()));
        let decorate: Rc<dyn Fn(&gtk::ListItem)> = {
            let badges = badges.clone();
            let pinned = pinned.clone();
            let elsewhere = elsewhere.clone();
            Rc::new(move |item: &gtk::ListItem| {
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
                let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else { return };
                expander.set_list_row(Some(&row));
                let Some(path) = path_of(&row) else { return };
                let Some(b) = expander.child().and_downcast::<gtk::Box>() else { return };
                let (Some(icon), Some(name), Some(badge), Some(pin)) = (
                    b.first_child().and_downcast::<gtk::Image>(),
                    b.first_child().and_then(|w| w.next_sibling()).and_downcast::<gtk::Label>(),
                    b.first_child().and_then(|w| w.next_sibling()).and_then(|w| w.next_sibling()).and_downcast::<gtk::Label>(),
                    b.last_child().and_downcast::<gtk::Button>(),
                ) else {
                    return;
                };
                let is_dir = path.is_dir();
                icon.set_from_gicon(&file_icon(&path, is_dir, row.is_expanded()));
                let fname = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                for c in ["sill-changed", "sill-pending", "sill-below", "sill-dim", "sill-pinned", "sill-new"] {
                    name.remove_css_class(c);
                }
                if is_noise(&fname) {
                    name.add_css_class("sill-dim");
                }
                // a root shows where it is; everything below just its name
                let label = if row.depth() == 0 {
                    path.display().to_string().replacen(&std::env::var("HOME").unwrap_or_default(), "~", 1)
                } else {
                    fname.clone()
                };
                name.set_text(&label);
                let badge_entry = badges.borrow().get(&path).cloned();
                let badge_text = badge_entry.as_ref().map(|b| b.text().to_string()).unwrap_or_default();
                // the name's colour says the kind: git's change in the warning colour (untracked in
                // success), a session's unkept overlay change in the accent — not on the host, the
                // tooltip says so —, a folder with changes below in a softer warning with the list
                name.set_tooltip_text(None);
                match &badge_entry {
                    Some(Badge::Session { session, .. }) => {
                        name.add_css_class("sill-pending");
                        name.set_tooltip_text(Some(&format!("changed by session “{session}” in its overlay — not on the host until kept there (Changes)")));
                    }
                    Some(Badge::Git(s)) if s.starts_with('?') => name.add_css_class("sill-new"),
                    Some(Badge::Git(_)) => name.add_css_class("sill-changed"),
                    Some(Badge::Below) => {
                        name.add_css_class("sill-below");
                        let below: Vec<String> = badges
                            .borrow()
                            .iter()
                            .filter(|(p, b)| p.starts_with(&path) && **p != path && b.is_change())
                            .take(8)
                            .map(|(p, b)| format!("{}  {}", b.text(), p.strip_prefix(&path).unwrap_or(p).display()))
                            .collect();
                        if !below.is_empty() {
                            name.set_tooltip_text(Some(&format!("changed below:\n{}", below.join("\n"))));
                        }
                    }
                    None => {}
                }
                let is_pinned = pinned.borrow().contains(&path);
                if is_pinned {
                    name.add_css_class("sill-pinned");
                }
                let is_repo = is_dir && path.join(".git").exists();
                let mut parts: Vec<String> = Vec::new();
                if !badge_text.is_empty() {
                    parts.push(badge_text);
                }
                if is_repo {
                    parts.push("git".into());
                }
                let is_elsewhere = !is_pinned && elsewhere.borrow().contains(&path);
                if is_pinned {
                    parts.push("⌖".into());
                } else if is_elsewhere {
                    parts.push("⌖ other".into());
                }
                badge.set_text(&parts.join(" "));
                badge.set_tooltip_text(match (is_repo, is_pinned, is_elsewhere) {
                    (true, true, _) => Some("git repository · pinned to the current session"),
                    (true, false, true) => Some("git repository · pinned by another session, not this one"),
                    (true, false, false) => Some("git repository"),
                    (false, true, _) => Some("pinned to the current session"),
                    (false, false, true) => Some("pinned by another session, not this one"),
                    _ => None,
                });
                // folders: a live mount (the jail is rebuilt); files: a chip that opens the file
                pin.set_child(Some(&super::icons::scalable(
                    if is_pinned { "sill-minus" } else { "sill-plus" },
                    super::icons::fg_for(&pin, &theme::current()),
                )));
                pin.set_tooltip_text(Some(match (is_pinned, is_dir) {
                    (true, _) => "Unpin from the current session",
                    (false, true) => "Pin to the current session: bound live into its jail",
                    (false, false) => "Pin this file: a chip that opens it",
                }));
                pin.set_widget_name(&path.to_string_lossy()); // what the click (wired at setup) pins
            })
        };
        {
            let decorate = decorate.clone();
            let on_bind = bound.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
                on_bind.borrow_mut().push(item.clone());
                decorate(item);
            });
            let bound = bound.clone();
            factory.connect_unbind(move |_, item| {
                if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                    bound.borrow_mut().retain(|i| i != item);
                }
            });
        }
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory));
        view.add_css_class("sill-files");
        view.set_single_click_activate(true);
        // never scroll sideways: names ellipsize, the pin column stays put
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        root.append(&scroll);
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.add_css_class("dim-label");
        message.add_css_class("sill-menu-caption");
        message.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        message.set_visible(false);
        root.append(&message);

        let t = Rc::new(Self {
            root,
            title,
            all: all.clone(),
            view,
            roots,
            tree,
            selection,
            scope: RefCell::new(None),
            store,
            badges,
            changes: Rc::new(RefCell::new(Vec::new())),
            pinned,
            elsewhere,
            message,
            pin_target: RefCell::new(None),
            on_open: RefCell::new(None),
            on_close: RefCell::new(None),
            on_pin: RefCell::new(None),
            listed,
            bound,
            decorate,
            monitors: RefCell::new(BTreeMap::new()),
            refresh_pending: std::cell::Cell::new(false),
            scanning: std::cell::Cell::new(false),
            again: std::cell::Cell::new(false),
        });
        *tree_rc.borrow_mut() = Some(Rc::downgrade(&t));
        {
            let weak = Rc::downgrade(&t);
            t.view.connect_activate(move |_, pos| {
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
            all.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.set_scope(None);
                }
            });
        }
        t
    }

    /// The directory of the selected row (a file's parent): where "a terminal here" opens.
    pub fn selected_dir(&self) -> Option<PathBuf> {
        let row = self.selection.selected_item().and_downcast::<gtk::TreeListRow>()?;
        let p = path_of(&row)?;
        if p.is_dir() { Some(p) } else { p.parent().map(Path::to_path_buf) }
    }

    pub fn scope(&self) -> Option<PathBuf> {
        self.scope.borrow().clone()
    }

    /// Limit the tree to one folder (from a pin chip), or show everything again.
    pub fn set_scope(self: &Rc<Self>, path: Option<PathBuf>) {
        let home = std::env::var("HOME").unwrap_or_default();
        match &path {
            Some(p) => {
                self.title.set_text(&p.display().to_string().replacen(&home, "~", 1));
                self.title.set_tooltip_text(Some(&p.display().to_string()));
            }
            None => {
                self.title.set_text("Files");
                self.title.set_tooltip_text(None);
            }
        }
        self.all.set_visible(path.is_some());
        *self.scope.borrow_mut() = path;
        self.refresh();
    }

    fn say(&self, what: &str) {
        self.message.set_text(what);
        self.message.set_visible(!what.is_empty());
    }

    fn toggle_pin(self: &Rc<Self>, path: &Path) {
        let Some(sid) = self.pin_target.borrow().clone() else {
            self.say("no session selected to pin to");
            return;
        };
        let was = self.pinned.borrow().contains(&path.to_path_buf());
        let r = if was { self.store.unpin(&sid, path) } else { self.store.pin(&sid, path) };
        match r {
            Ok(()) => {
                self.say(&format!("{} {}", if was { "unpinned" } else { "pinned" }, path.display()));
                if let Some(cb) = self.on_pin.borrow().as_ref() {
                    cb(&sid);
                }
                self.refresh();
            }
            Err(e) => self.say(&format!("pin failed: {e}")),
        }
    }

    /// Rescan every session's ledger and git status off the main thread, then rebuild badges and roots.
    /// Refreshes come from several places (the monitors, a turn's end, an edit, a save, focus,
    /// the tab): at most one scan runs, and one asked meanwhile runs once more after it — a burst
    /// costs one or two scans, never a pile, and an older, slower scan can't land after a newer one.
    /// `refresh_soon` is the coalesced form for the bursty callers.
    pub fn refresh(self: &Rc<Self>) {
        if self.scanning.replace(true) {
            self.again.set(true);
            return;
        }
        let _ = self.store.prune_pins(); // pins of deleted sessions would otherwise haunt the tree
        let sessions = self.store.sessions().unwrap_or_default();
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        let target = self.pin_target.borrow().clone();
        let target_pins: Vec<PathBuf> = target.as_ref().and_then(|t| self.store.pins(t).ok()).unwrap_or_default();
        // the selected session's pins first, then everyone else's
        let mut pins: Vec<PathBuf> = target_pins.clone();
        let elsewhere: Vec<PathBuf> = sessions
            .iter()
            .filter(|s| Some(&s.id) != target.as_ref())
            .flat_map(|s| self.store.pins(&s.id).unwrap_or_default())
            .filter(|p| !target_pins.contains(p))
            .collect();
        for p in &elsewhere {
            if !pins.contains(p) {
                pins.push(p.clone());
            }
        }
        *self.elsewhere.borrow_mut() = elsewhere;
        *self.pinned.borrow_mut() = target_pins;
        let (tx, rx) = async_channel::bounded::<(Vec<Change>, BTreeMap<PathBuf, String>)>(1);
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let mut owners: BTreeMap<PathBuf, String> = BTreeMap::new(); // path → the session that changed it
            for s in sessions {
                let Ok(policy) = sill_core::policy::Policy::load(&s.policy, &home) else { continue };
                let Ok(state_dir) = sill_core::session::state_dir(uid, &s.id) else { continue };
                if !state_dir.is_dir() {
                    continue;
                }
                let binds = match Store::open_default() {
                    Ok(st) => session::binds_of(&s, &policy, &home, &st),
                    Err(_) => vec![],
                };
                let Ok(plan) = changes::session_plan(policy.clone(), state_dir, &home, uid, &user, binds) else { continue };
                let found = changes::scan(&policy, &plan, None).unwrap_or_default();
                for c in &found {
                    owners.entry(c.path.clone()).or_insert_with(|| s.title.clone());
                }
                all.extend(found);
            }
            let _ = tx.send_blocking((all, owners));
        });
        let t = self.clone();
        glib::spawn_future_local(async move {
            if let Ok((all, owners)) = rx.recv().await {
                t.apply(all, owners, pins);
            }
            t.scanning.set(false);
            if t.again.replace(false) {
                t.refresh();
            }
        });
    }

    fn apply(self: &Rc<Self>, all: Vec<Change>, owners: BTreeMap<PathBuf, String>, pins: Vec<PathBuf>) {
        let home = session::home().unwrap_or_default();
        let mut badges: BTreeMap<PathBuf, Badge> = BTreeMap::new();
        // git working-tree status of every repo in view (pins, repos with session changes, the scope's
        // repo): M/A/D/R from `git status`, `?` for untracked; the session ledger's letters win below
        let mut repos: Vec<PathBuf> = pins.iter().filter(|p| p.join(".git").exists()).cloned().collect();
        for c in &all {
            if let Some(r) = changes::repo_root(&c.path)
                && !repos.contains(&r)
            {
                repos.push(r);
            }
        }
        let scope = self.scope.borrow().clone();
        if let Some(s) = &scope
            && let Some(r) = changes::repo_root(&s.join("x"))
            && !repos.contains(&r)
        {
            repos.push(r);
        }
        let mut from_git = 0usize;
        for repo in &repos {
            for (rel, st) in changes::git_status(repo) {
                from_git += 1;
                let path = repo.join(rel);
                badges.insert(path.clone(), Badge::Git(st));
                let mut d = path.parent();
                while let Some(dir) = d {
                    if dir == repo.as_path() {
                        break;
                    }
                    badges.entry(dir.to_path_buf()).or_insert(Badge::Below);
                    d = dir.parent();
                }
            }
        }
        // folders only as roots: a pinned file is a chip, not a tree
        let mut roots: Vec<PathBuf> = match &scope {
            Some(s) => vec![s.clone()],
            None => pins.into_iter().filter(|p| p.is_dir()).collect(),
        };
        for c in all.iter().filter(|c| !c.ignored) {
            let mark = format!("{}{}", c.kind.letter(), if c.conflict { "!" } else { "" });
            let session = owners.get(&c.path).cloned().unwrap_or_default();
            badges.insert(c.path.clone(), Badge::Session { mark, session });
            let mut p = c.path.parent();
            while let Some(d) = p {
                badges.entry(d.to_path_buf()).or_insert(Badge::Below);
                p = d.parent();
            }
            if scope.is_none()
                && let Some(r) = changes::repo_root(&c.path)
                && !roots.contains(&r)
            {
                roots.push(r);
            }
        }
        roots.dedup();
        if scope.is_none() && !roots.contains(&home) {
            roots.push(home);
        }
        // the title says, on hover, when the badges were last read and where they come from
        let when = glib::DateTime::now_local().ok().and_then(|d| d.format("%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
        let from_sessions = all.iter().filter(|c| !c.ignored).count();
        self.title.set_tooltip_text(Some(&format!(
            "badges read {when}: {from_git} from git status in {} repositor{}, {from_sessions} from sessions' overlays",
            repos.len(),
            if repos.len() == 1 { "y" } else { "ies" }
        )));
        *self.badges.borrow_mut() = badges;
        *self.changes.borrow_mut() = all;
        // directories already listed: files that appeared or went since, in place
        self.listed.borrow_mut().retain(|_, w| w.upgrade().is_some());
        let listed: Vec<(PathBuf, gio::ListStore)> = self.listed.borrow().iter().filter_map(|(d, w)| w.upgrade().map(|s| (d.clone(), s))).collect();
        for (dir, store) in listed {
            let g = ghosts(&self.badges.borrow(), &dir);
            sync_children(&dir, &store, &g);
        }
        // keep the tree if the roots didn't change (expansion state survives)
        let current: Vec<String> = (0..self.roots.n_items())
            .filter_map(|i| self.roots.item(i).and_downcast::<gtk::StringObject>().map(|o| o.string().to_string()))
            .collect();
        let wanted: Vec<String> = roots.iter().map(|r| r.to_string_lossy().into_owned()).collect();
        if current != wanted {
            self.roots.remove_all();
            for r in &wanted {
                self.roots.append(&gtk::StringObject::new(r));
            }
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
            if scope.is_some() {
                self.open_levels();
            }
        }
        // every row on screen shows its badge as it is now
        for item in self.bound.borrow().clone() {
            (self.decorate)(&item);
        }
        self.view.queue_draw();
        // follow the disk: the listed directories, and each repository's .git (commits, checkouts)
        let mut watch: Vec<PathBuf> = self.listed.borrow().keys().cloned().collect();
        for r in &repos {
            watch.push(r.join(".git"));
            watch.push(r.join(".git/refs/heads"));
        }
        self.watch(watch);
    }

    /// Watch exactly these directories (non-recursive; the listed ones are what is on screen).
    fn watch(self: &Rc<Self>, dirs: Vec<PathBuf>) {
        let mut monitors = self.monitors.borrow_mut();
        monitors.retain(|d, _| dirs.contains(d));
        for d in dirs {
            if monitors.contains_key(&d) || !d.is_dir() {
                continue;
            }
            let Ok(m) = gio::File::for_path(&d).monitor_directory(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) else { continue };
            let weak = Rc::downgrade(self);
            m.connect_changed(move |_, _, _, _| {
                if let Some(t) = weak.upgrade() {
                    t.refresh_soon();
                }
            });
            monitors.insert(d, m);
        }
    }

    /// A directory the person just opened: watched from now on (the next refresh would do it too).
    fn watch_one(self: &Rc<Self>, dir: &Path) {
        let mut dirs: Vec<PathBuf> = self.monitors.borrow().keys().cloned().collect();
        dirs.push(dir.to_path_buf());
        self.watch(dirs);
    }

    /// Coalesced refresh (monitors fire in bursts; an agent's edit lands here at once, SPEC §3.1);
    /// nothing while the tree is hidden — showing it refreshes anyway.
    pub fn refresh_soon(self: &Rc<Self>) {
        if !self.root.is_visible() || self.refresh_pending.replace(true) {
            return;
        }
        let t = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            t.refresh_pending.set(false);
            if t.root.is_visible() {
                t.refresh();
            }
        });
    }

    /// A scoped tree starts two levels open (build output and dot folders stay closed).
    fn open_levels(&self) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(store: &gio::ListStore) -> Vec<String> {
        (0..store.n_items())
            .filter_map(|i| store.item(i).and_downcast::<gtk::StringObject>())
            .filter_map(|o| Path::new(o.string().as_str()).file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    }

    /// A refresh lists what appeared and drops what went, keeping the other rows in place.
    #[test]
    fn sync_children_edits_the_store_in_place() {
        if gtk::init().is_err() {
            eprintln!("no display: skipped"); // gtk::StringObject needs GTK up
            return;
        }
        let dir = std::env::temp_dir().join(format!("sill-tree-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        let listed: Listed = Rc::new(RefCell::new(BTreeMap::new()));
        let store = children_of(&dir, &listed, &[]);
        assert_eq!(names(&store), ["src", "b.txt", "README.md"]);
        let first = store.item(0).unwrap(); // the `src` row object must survive the sync
        std::fs::write(dir.join("a.txt"), "").unwrap(); // new, sorts first among the files
        std::fs::write(dir.join(".hidden"), "").unwrap(); // new, dot file: last
        std::fs::remove_file(dir.join("b.txt")).unwrap(); // gone
        sync_children(&dir, &store, &[]);
        assert_eq!(names(&store), ["src", "a.txt", "README.md", ".hidden"]);
        assert_eq!(store.item(0).unwrap(), first);
        // nothing changed: nothing moves
        sync_children(&dir, &store, &[]);
        assert_eq!(names(&store), ["src", "a.txt", "README.md", ".hidden"]);
        // a ghost: a path a session has that the host hasn't gets a row among the files
        let ghost = dir.join("gone.rs").to_string_lossy().into_owned();
        sync_children(&dir, &store, &[ghost]);
        assert_eq!(names(&store), ["src", "a.txt", "gone.rs", "README.md", ".hidden"]);
        // the directory went: the store empties
        std::fs::remove_dir_all(&dir).unwrap();
        sync_children(&dir, &store, &[]);
        assert_eq!(store.n_items(), 0);
    }
}
