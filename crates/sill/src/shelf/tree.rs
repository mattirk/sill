//! The file tree (SPEC §3.1): a drawer beside the main pane, taking its share of the shelf's width (`x11::drawer_width`: 30 % until the handle between the panes is dragged, then the dragged width, remembered)
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
use super::remote_store::Store;

use super::{session, theme};

/// Folders below this depth are opened when a scoped tree is shown (0 = the pinned folder itself).
const OPEN_DEPTH: u32 = 1;
/// Opening stops once this many rows are shown (a huge folder must not stall the shelf).
const OPEN_LIMIT: u32 = 600;

/// Build output and tool state: listed, never opened by default, dimmed.
/// What a repository has checked out, for its root row: the branch name, or the short hash when
/// HEAD is detached (the flag says which). Read from `.git/HEAD` (through a worktree's `gitdir:` file when `.git` is
/// one) — no git process, so it is cheap enough for every bind; the tree watches `.git`, so a
/// checkout re-decorates the row.
fn head_label(repo: &Path) -> Option<(String, bool)> {
    super::remote::head(repo)
}

/// A search shows at most this many results and looks at this many entries (a home full of
/// photos must not stall the walk for seconds; the message says when it was cut).
const SEARCH_CAP: usize = 200;
const SEARCH_VISITS: usize = 50_000;

/// Files and folders below `bases` whose name — or path below the base, when `q` has a `/` —
/// contains `q` (already lower-cased). Breadth-first so the shallow hits come first, noise folders
/// and links skipped, results sorted by path; `(found, cut)` where `cut` says a limit stopped it.
fn find_files(bases: &[PathBuf], q: &str, cap: usize, visits: usize) -> (Vec<PathBuf>, bool) {
    super::remote::find(bases, q, cap, visits)
}

fn is_noise(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules" | "__pycache__" | "build" | "dist")
}

/// The global view's rows are grouped under captions (Pinned here · Repositories with changes ·
/// Home): a caption is a root item whose string starts with this (no path starts with it).
const SECTION: &str = "\u{1}";

/// A row's path — `None` for a section caption.
fn path_of(row: &gtk::TreeListRow) -> Option<PathBuf> {
    let s = row.item().and_downcast::<gtk::StringObject>()?;
    let s = s.string();
    (!s.starts_with(SECTION)).then(|| PathBuf::from(s.as_str()))
}

/// A section caption's text, when the row is one.
fn section_of(row: &gtk::TreeListRow) -> Option<String> {
    let s = row.item().and_downcast::<gtk::StringObject>()?;
    s.string().strip_prefix(SECTION).map(str::to_owned)
}

/// The glyph for a tree row: folder (open/closed; a pinned root looks like any other folder, its
/// icon's accent colour says it is pinned), else the file's kind by its extension — sill's own glyphs, so every
/// machine shows the same tree (no icon theme, no MIME database).
fn row_glyph(path: &Path, is_dir: bool, expanded: bool) -> &'static str {
    if is_dir {
        return if expanded { "sill-folder-open" } else { "sill-files" };
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif" | "tiff" => "sill-image",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "deb" | "rpm" | "jar" => "sill-file-archive",
        "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "lock" | "env" | "properties" => "sill-file-cog",
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" => "sill-file-terminal",
        "md" | "txt" | "rst" | "org" | "adoc" | "log" | "csv" => "sill-file-text",
        _ if changes::file_language(path).is_some() || matches!(ext.as_str(), "xml" | "sql" | "proto" | "cmake" | "mk" | "nix") => "sill-file-code",
        _ => "sill-file",
    }
}

/// A directory itself — not a link to one: the tree lists what is in a pin, never what a link in it
/// points at elsewhere on the host (security.md S9).
fn is_real_dir(path: &Path) -> bool {
    super::remote::is_dir(path)
}

fn is_link(path: &Path) -> bool {
    super::remote::metadata(path).is_ok_and(|m| m.symlink)
}

/// A directory's entries as full paths: directories first, dot files last, case-insensitive.
/// `ghosts`: paths (below `dir`) that a session's overlay has and the host hasn't — listed as files
/// too, so a change always has a row (a folder's dot never points at nothing).
fn list_dir(dir: &Path, ghosts: &[String]) -> Vec<String> {
    let mut entries: Vec<(bool, String)> = super::remote::list(dir).unwrap_or_default().into_iter()
        .map(|e| (e.metadata.directory, e.path.file_name().unwrap_or_default().to_string_lossy().into_owned())).collect();
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
            Badge::Below => "",
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
        .filter(|(p, b)| p.parent() == Some(dir) && b.is_change() && !super::remote::exists(p))
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect()
}

/// Bring a listed directory up to date without rebuilding it: entries that went are removed, new
/// ones inserted at their place; everything else keeps its row (and what is expanded below it).
fn sync_children(dir: &Path, store: &gio::ListStore, ghosts: &[String]) {
    let wanted = list_dir(dir, ghosts);
    sync_names(store, &wanted);
}

fn sync_names(store: &gio::ListStore, wanted: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sync_keeps_unchanged_row_objects() {
        if gtk::init().is_err() { return }
        let store = gio::ListStore::new::<gtk::StringObject>();
        sync_names(&store, &["a".into(), "c".into()]);
        let first = store.item(0).unwrap();
        let last = store.item(1).unwrap();
        sync_names(&store, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(store.item(0).unwrap(), first);
        assert_eq!(store.item(2).unwrap(), last);
        sync_names(&store, &["c".into()]);
        assert_eq!(store.item(0).unwrap(), last);
        assert_eq!(store.n_items(), 1);
        sync_names(&store, &[]);
        assert_eq!(store.n_items(), 0);
    }
}

pub struct FolderTree {
    pub root: gtk::Box,
    /// The bar's search box: a name (or a path fragment with `/`) finds files below the shown
    /// roots; empty = the tree.
    search: gtk::SearchEntry,
    /// What the box holds, lower-cased; non-empty = the rows are search results, not the tree.
    query: RefCell<String>,
    /// Each search bumps this; a walk that finishes for an older query is dropped.
    search_seq: std::cell::Cell<u32>,
    /// Back to the global tree while scoped to one folder.
    all: gtk::Button,
    /// Shows `git diff --stat` of the repositories in view instead of the tree.
    diff: gtk::ToggleButton,
    panes: gtk::Stack,
    stat: Rc<super::diffstat::DiffStat>,
    view: gtk::ListView,
    roots: gio::ListStore,
    tree: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    /// When set, the tree shows only this folder (opened from a pin chip).
    scope: RefCell<Option<PathBuf>>,
    store: Rc<Store>,
    badges: Rc<RefCell<BTreeMap<PathBuf, Badge>>>,
    /// Every session's pending changes, with the session's id: the badges here, and the viewer asks
    /// the daemon for a pending path's diff.
    pub changes: Rc<RefCell<Vec<(String, Change)>>>,
    pinned: Rc<RefCell<Vec<PathBuf>>>,
    /// Pinned by some other session than the current one: shown, but the button pins here.
    elsewhere: Rc<RefCell<Vec<PathBuf>>>,
    /// The global pins not pinned to the current session (docs/pins.md): their own group, the
    /// tooltip says they are bound in every jail.
    global: Rc<RefCell<Vec<PathBuf>>>,
    message: gtk::Label,
    /// Under the rows when the global view has nothing but home: how to get a folder into it.
    hint: gtk::Label,
    /// Session that receives pins (the selected tab), set by the window.
    pub pin_target: RefCell<Option<String>>,
    /// Set by the first `set_scope` (a chip, "all", or the homing below): until then the drawer is
    /// at its default, and the first refresh that finds the selected session's pinned folder scopes
    /// to it — the drawer opens on the work, not on the global view with that folder collapsed.
    homed: std::cell::Cell<bool>,
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
    poll: RefCell<Option<glib::SourceId>>,
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
        let search = gtk::SearchEntry::new();
        search.set_hexpand(true);
        search.add_css_class("sill-tree-search");
        super::icons::adopt_search_entry(&search);
        let all = gtk::Button::with_label("all");
        all.add_css_class("flat");
        all.add_css_class("sill-viewer-btn");
        all.set_tooltip_text(Some("Show everything: home, pinned folders, touched repositories"));
        all.set_visible(false);
        let diff = gtk::ToggleButton::with_label("diff");
        diff.add_css_class("flat");
        diff.add_css_class("sill-viewer-btn");
        diff.set_tooltip_text(Some("git diff --stat of the pinned repository (the shown folder's, else this session's pinned ones)"));
        let close = super::icons::glyph_button("sill-x");
        close.add_css_class("flat");
        close.add_css_class("sill-viewer-btn");
        close.set_tooltip_text(Some("Close the tree (Ctrl+Shift+F)"));
        bar.append(&search);
        bar.append(&all);
        bar.append(&diff);
        bar.append(&close);
        root.append(&bar);

        let badges: Rc<RefCell<BTreeMap<PathBuf, Badge>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let pinned: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        let elsewhere: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        let global: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
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
                if !is_real_dir(&path) {
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
            // no arrows: a click on the row opens a folder, the icon says open or closed, and the
            // indents draw a guide line per level (CSS `.sill-files treeexpander > indent`)
            expander.set_hide_expander(true);
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            b.add_css_class("sill-tree-row");
            let icon = gtk::Image::new();
            icon.add_css_class("sill-file-icon");
            let name = gtk::Label::new(None);
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let badge = gtk::Label::new(None);
            badge.add_css_class("sill-badge");
            badge.set_xalign(1.0);
            badge.set_halign(gtk::Align::End);
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
            // the pin sits between the name and the badge, so the badge (`git master`) ends the row
            // at the tree's right edge
            b.append(&icon);
            b.append(&name);
            b.append(&pin);
            b.append(&badge);
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
            let global = global.clone();
            let tree_rc = tree_rc.clone();
            let home = session::home().unwrap_or_default();
            Rc::new(move |item: &gtk::ListItem| {
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
                let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else { return };
                expander.set_list_row(Some(&row));
                let Some(b) = expander.child().and_downcast::<gtk::Box>() else { return };
                let (Some(icon), Some(name), Some(pin), Some(badge)) = (
                    b.first_child().and_downcast::<gtk::Image>(),
                    b.first_child().and_then(|w| w.next_sibling()).and_downcast::<gtk::Label>(),
                    b.first_child().and_then(|w| w.next_sibling()).and_then(|w| w.next_sibling()).and_downcast::<gtk::Button>(),
                    b.last_child().and_downcast::<gtk::Label>(),
                ) else {
                    return;
                };
                // a section caption: dim text alone, nothing to click (rows are recycled, so a
                // path row below puts everything back)
                let section = section_of(&row);
                item.set_selectable(section.is_none());
                item.set_activatable(section.is_none());
                icon.set_visible(section.is_none());
                pin.set_visible(section.is_none());
                if let Some(caption) = section {
                    for c in ["sill-changed", "sill-pending", "sill-below", "sill-dim", "sill-new"] {
                        name.remove_css_class(c);
                    }
                    name.add_css_class("sill-tree-section");
                    name.set_text(&caption);
                    name.set_tooltip_text(None);
                    badge.set_text("");
                    badge.set_tooltip_text(None);
                    return;
                }
                let Some(path) = path_of(&row) else { return };
                // the global view's home root is a row of its own: "home" at the rows' size, its
                // folder icon in its own colour, no pin — a folder among the pinned ones otherwise
                let home_caption = row.depth() == 0 && path == home && tree_rc.borrow().as_ref().and_then(|w| w.upgrade()).is_some_and(|t| t.scope.borrow().is_none());
                name.remove_css_class("sill-tree-section");
                icon.set_visible(true);
                pin.set_visible(!home_caption);
                let is_dir = is_real_dir(&path);
                let link = is_link(&path);
                // while a search is on, the top rows are its results, not roots: they read as the
                // path below the searched folder (the scope, else ~) and wear no root mark
                let searching = tree_rc.borrow().as_ref().and_then(|w| w.upgrade()).is_some_and(|t| !t.query.borrow().is_empty());
                let is_root = row.depth() == 0 && !home_caption && !searching;
                // every row's icon is a sill glyph in the foreground colour (rows are rebound on a
                // theme change, so they follow it); a link shows as one, whatever it points at. A
                // pinned row's icon is in the accent, at full strength — the name's colour is the
                // change's alone
                let is_pinned = pinned.borrow().contains(&path);
                let mut fg = super::icons::fg_for(&icon, &theme::current());
                if is_pinned {
                    if let Ok(accent) = gtk::gdk::RGBA::parse(theme::current().accent) {
                        fg = accent;
                    }
                    icon.add_css_class("sill-pinned");
                } else {
                    icon.remove_css_class("sill-pinned");
                }
                if home_caption {
                    if let Ok(c) = gtk::gdk::RGBA::parse(theme::current().success) {
                        fg = c;
                    }
                    icon.add_css_class("sill-pinned");
                }
                let glyph = if link { "sill-link" } else { row_glyph(&path, is_dir, row.is_expanded()) };
                icon.set_paintable(super::icons::stroke_texture(glyph, fg).as_ref());
                let fname = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                for c in ["sill-changed", "sill-pending", "sill-below", "sill-dim", "sill-new"] {
                    name.remove_css_class(c);
                }
                if is_noise(&fname) {
                    name.add_css_class("sill-dim");
                }
                // every row shows its name alone — a root too, since the branch took the room the
                // path had; its glyph marks it as a root and the tooltip has the whole path
                let label = if home_caption {
                    "home".to_string()
                } else if searching && row.depth() == 0 {
                    let base = tree_rc.borrow().as_ref().and_then(|w| w.upgrade()).and_then(|t| t.scope.borrow().clone());
                    match base.as_ref().and_then(|b| path.strip_prefix(b).ok()) {
                        Some(rel) => rel.display().to_string(),
                        None => path.display().to_string().replacen(&*home.to_string_lossy(), "~", 1),
                    }
                } else if fname.is_empty() {
                    path.display().to_string()
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
                if link {
                    let target = super::remote::read_link(&path).map(|t| t.display().to_string()).unwrap_or_else(|_| "?".into());
                    name.set_tooltip_text(Some(&format!("a link → {target}: not followed here; open the target itself if you mean it")));
                }
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
                if is_root {
                    let shown = path.display().to_string().replacen(&*home.to_string_lossy(), "~", 1);
                    let tip = match name.tooltip_text() {
                        Some(t) => format!("{shown}\n{t}"),
                        None => shown,
                    };
                    name.set_tooltip_text(Some(&tip));
                }
                let is_repo = is_dir && super::remote::exists(&path.join(".git"));
                // a repository root: `git <branch>` (the short hash when detached)
                let head = if is_repo { head_label(&path) } else { None };
                let mut parts: Vec<String> = Vec::new();
                if !badge_text.is_empty() {
                    parts.push(badge_text);
                }
                if is_repo {
                    parts.push("git".into());
                    if let Some((h, _)) = &head {
                        parts.push(h.clone());
                    }
                }
                // pinned: the icon's colour says so (the tooltip names another session's pin)
                let is_elsewhere = !is_pinned && elsewhere.borrow().contains(&path);
                badge.set_text(&parts.join(" "));
                let mut tip: Vec<String> = Vec::new();
                if is_repo {
                    tip.push(match &head {
                        Some((h, true)) => format!("git repository · detached at {h}"),
                        Some((h, false)) => format!("git repository · on {h}"),
                        None => "git repository".into(),
                    });
                }
                let is_global = !is_pinned && global.borrow().contains(&path);
                if is_pinned {
                    tip.push("pinned to the current session".into());
                } else if is_global {
                    tip.push("a global pin: in every session's jail; + makes it this session's focus too".into());
                } else if is_elsewhere {
                    tip.push("pinned by another session, not this one".into());
                }
                let tip = tip.join(" · ");
                badge.set_tooltip_text((!tip.is_empty()).then_some(tip.as_str()));
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
        // no sideways scrollbar: names ellipsize, the pin column stays put. External, not Never: with
        // Never the scrolled window asks for the rows' full natural width, the box gave the drawer
        // that instead of its share and the main pane took the difference, so Drawer width moved the
        // window but never the drawer (2026-11-01). External keeps the natural width at the minimum,
        // so the drawer is exactly the size request the window gives it.
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).hscrollbar_policy(gtk::PolicyType::External).build();
        let stat = super::diffstat::DiffStat::new();
        let panes = gtk::Stack::new();
        panes.set_vexpand(true);
        panes.add_named(&scroll, Some("tree"));
        panes.add_named(&stat.root, Some("diff"));
        root.append(&panes);
        let hint = gtk::Label::new(Some("Nothing pinned to this session — pin a folder with + on its row, or open one from Home."));
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        hint.add_css_class("dim-label");
        hint.add_css_class("sill-menu-caption");
        hint.set_visible(false);
        root.append(&hint);
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.add_css_class("dim-label");
        message.add_css_class("sill-menu-caption");
        message.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        message.set_visible(false);
        root.append(&message);

        let t = Rc::new(Self {
            root,
            search: search.clone(),
            query: RefCell::new(String::new()),
            search_seq: std::cell::Cell::new(0),
            all: all.clone(),
            diff: diff.clone(),
            panes,
            stat,
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
            global,
            message,
            hint,
            pin_target: RefCell::new(None),
            homed: std::cell::Cell::new(false),
            on_open: RefCell::new(None),
            on_close: RefCell::new(None),
            on_pin: RefCell::new(None),
            listed,
            bound,
            decorate,
            monitors: RefCell::new(BTreeMap::new()),
            poll: RefCell::new(None),
            refresh_pending: std::cell::Cell::new(false),
            scanning: std::cell::Cell::new(false),
            again: std::cell::Cell::new(false),
        });
        *tree_rc.borrow_mut() = Some(Rc::downgrade(&t));
        {
            // typing searches (the entry debounces); Escape clears and the tree is back
            let weak = Rc::downgrade(&t);
            search.connect_search_changed(move |e| {
                if let Some(t) = weak.upgrade() {
                    t.search(e.text().as_str());
                }
            });
            search.connect_stop_search(|e| e.set_text(""));
        }
        {
            let weak = Rc::downgrade(&t);
            t.view.connect_activate(move |_, pos| {
                let Some(t) = weak.upgrade() else { return };
                let Some(row) = t.tree.row(pos) else { return };
                let Some(path) = path_of(&row) else { return };
                if super::remote::is_dir(&path) {
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
            diff.connect_toggled(move |b| {
                if let Some(t) = weak.upgrade() {
                    t.set_diff_mode(b.is_active());
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
        if super::remote::is_dir(&p) { Some(p) } else { p.parent().map(Path::to_path_buf) }
    }

    pub fn scope(&self) -> Option<PathBuf> {
        self.scope.borrow().clone()
    }

    /// A diff row clicked: the viewer shows that file's git diff.
    pub fn on_open_diff(&self, f: impl Fn(&Path, &Path) + 'static) {
        *self.stat.on_open.borrow_mut() = Some(Box::new(f));
    }

    /// Tree or `git diff --stat`: the search box filters whichever is shown.
    fn set_diff_mode(self: &Rc<Self>, on: bool) {
        self.search.set_text("");
        self.query.borrow_mut().clear();
        self.stat.set_filter("");
        self.say("");
        self.panes.set_visible_child_name(if on { "diff" } else { "tree" });
        if on {
            self.hint.set_visible(false);
            self.refresh_stat(true);
        } else {
            self.refresh();
        }
    }

    /// The repositories the diff view shows: the scoped folder's, else the current session's pinned
    /// folders that are (in) repositories — a repository must be pinned to be shown.
    fn stat_repos(&self) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = match self.scope.borrow().clone() {
            Some(s) => vec![s],
            None => self.pinned.borrow().clone(),
        };
        let mut repos: Vec<PathBuf> = Vec::new();
        for d in dirs {
            if let Some(r) = super::remote::repo_root(&d.join("x"))
                && !repos.contains(&r)
            {
                repos.push(r);
            }
        }
        repos
    }

    fn refresh_stat(self: &Rc<Self>, force: bool) {
        if self.diff.is_active() {
            let repos = self.stat_repos();
            self.stat.refresh(repos, force);
        }
    }

    /// A session just opened in a tab (new, or resumed): the tree shows its first pinned folder,
    /// or everything when it has none — a fresh agent starts at Files, a resumed one at its work.
    /// Nothing happens while the drawer is closed; opening it later decides for itself.
    pub fn follow_session(self: &Rc<Self>, id: &str) {
        if !self.root.is_visible() {
            return;
        }
        let first = self.store.pins(id).unwrap_or_default().into_iter().find(|p| super::remote::is_dir(&p));
        if first != self.scope() {
            self.set_scope(first);
        }
    }

    /// Limit the tree to one folder (from a pin chip), or show everything again.
    pub fn set_scope(self: &Rc<Self>, path: Option<PathBuf>) {
        // a new scope ends a search: the box is cleared, which refreshes by itself when it held text
        self.search.set_text("");
        self.all.set_visible(path.is_some());
        *self.scope.borrow_mut() = path;
        self.homed.set(true);
        self.refresh();
    }

    /// Search below the shown roots (the scope, or every root of the global view) for `text`:
    /// a case-insensitive part of the name, or of the path below the root when the text has a `/`.
    /// The walk runs off the main thread and lands only if the box still says the same; an empty
    /// text puts the tree back.
    fn search(self: &Rc<Self>, text: &str) {
        let q = text.trim().to_lowercase();
        if self.diff.is_active() {
            self.stat.set_filter(&q);
            return;
        }
        let seq = self.search_seq.get().wrapping_add(1);
        self.search_seq.set(seq);
        let was = std::mem::replace(&mut *self.query.borrow_mut(), q.clone());
        if q.is_empty() {
            if !was.is_empty() {
                self.say("");
                // the roots differ from the results, so the refresh rebuilds them
                self.refresh();
            }
            return;
        }
        let bases: Vec<PathBuf> = match self.scope.borrow().clone() {
            Some(s) => vec![s],
            None => (0..self.roots.n_items())
                .filter_map(|i| self.roots.item(i).and_downcast::<gtk::StringObject>())
                .filter_map(|o| (!o.string().starts_with(SECTION)).then(|| PathBuf::from(o.string().as_str())))
                .collect(),
        };
        let (tx, rx) = async_channel::bounded::<(Vec<PathBuf>, bool)>(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(find_files(&bases, &q, SEARCH_CAP, SEARCH_VISITS));
        });
        let t = self.clone();
        glib::spawn_future_local(async move {
            let Ok((found, cut)) = rx.recv().await else { return };
            if t.search_seq.get() != seq {
                return;
            }
            t.roots.remove_all();
            for p in &found {
                t.roots.append(&gtk::StringObject::new(&p.to_string_lossy()));
            }
            t.selection.set_selected(gtk::INVALID_LIST_POSITION);
            t.say(&match (found.len(), cut) {
                (0, false) => "no file matches".to_string(),
                (0, true) => "no file matches in what was looked at (the walk was cut short)".to_string(),
                (n, false) => format!("{n} match{}", if n == 1 { "" } else { "es" }),
                (n, true) => format!("first {n} matches; type more to narrow"),
            });
        });
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
        let r = super::client::pin(&sid, path, !was);
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
        // the drawer's first look at a session with a pinned folder: that folder (see `homed`)
        if !self.homed.get() && self.scope.borrow().is_none() {
            let target = self.pin_target.borrow().clone();
            let first = target.as_ref().and_then(|t| self.store.pins(t).ok()).unwrap_or_default().into_iter().find(|p| super::remote::is_dir(&p));
            if let Some(dir) = first {
                self.set_scope(Some(dir));
                return;
            }
        }
        if self.scanning.replace(true) {
            self.again.set(true);
            return;
        }
        let sessions = self.store.sessions().unwrap_or_default();
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
        // the global pins (docs/pins.md): bound in every jail, shown as their own group, suggestions
        // for the pin button; one pinned here too is in "Pinned here" and not repeated
        let global: Vec<PathBuf> = self.store.global_pins().unwrap_or_default().into_iter().filter(|p| !target_pins.contains(p)).collect();
        for p in &global {
            if !pins.contains(p) {
                pins.push(p.clone());
            }
        }
        *self.global.borrow_mut() = global;
        *self.elsewhere.borrow_mut() = elsewhere;
        *self.pinned.borrow_mut() = target_pins;
        let scope = self.scope.borrow().clone();
        let pins_for_git = pins.clone();
        type Statuses = Vec<(PathBuf, Vec<(PathBuf, String)>)>;
        let (tx, rx) = async_channel::bounded::<(Vec<(String, Change)>, BTreeMap<PathBuf, String>, Statuses)>(1);
        drop(sessions);
        std::thread::spawn(move || {
            // every session's overlay changes, as the daemon scans them (docs/daemon.md §7 M2)
            let mut pairs: Vec<(String, Change)> = Vec::new();
            let mut owners: BTreeMap<PathBuf, String> = BTreeMap::new(); // path → the session that changed it
            match crate::changes_client::list(&|m, p| super::client::call(m, p)) {
                Ok(list) => {
                    for s in list.sessions {
                        for r in s.changes.into_iter().chain(s.agent_state) {
                            owners.entry(r.change.path.clone()).or_insert_with(|| s.title.clone());
                            pairs.push((s.id.clone(), r.change));
                        }
                    }
                }
                Err(e) => eprintln!("sill: the tree's badges: {e:#}"),
            }
            let all: Vec<&Change> = pairs.iter().map(|(_, c)| c).collect();
            // git working-tree status of every repo in view (pins, repos with session changes, the
            // scope's repo) — here, not on the GTK thread: `git status` on a big tree takes a while
            // (SPEC §4.11 #13)
            let mut repos: Vec<PathBuf> = pins_for_git.iter().filter(|p| super::remote::exists(&p.join(".git"))).cloned().collect();
            for c in &all {
                if let Some(r) = super::remote::repo_root(&c.path)
                    && !repos.contains(&r)
                {
                    repos.push(r);
                }
            }
            if let Some(s) = &scope
                && let Some(r) = super::remote::repo_root(&s.join("x"))
                && !repos.contains(&r)
            {
                repos.push(r);
            }
            let statuses: Statuses = repos.into_iter().map(|r| (super::remote::git_status(&r).unwrap_or_default(), r)).map(|(st, r)| (r, st)).collect();
            let _ = tx.send_blocking((pairs, owners, statuses));
        });
        let t = self.clone();
        glib::spawn_future_local(async move {
            if let Ok((all, owners, statuses)) = rx.recv().await {
                t.apply(all, owners, statuses);
            }
            t.scanning.set(false);
            if t.again.replace(false) {
                t.refresh();
            }
        });
    }

    fn apply(self: &Rc<Self>, pairs: Vec<(String, Change)>, owners: BTreeMap<PathBuf, String>, statuses: Vec<(PathBuf, Vec<(PathBuf, String)>)>) {
        let all: Vec<Change> = pairs.iter().map(|(_, c)| c.clone()).collect();
        let home = session::home().unwrap_or_default();
        let mut badges: BTreeMap<PathBuf, Badge> = BTreeMap::new();
        // git working-tree status of every repo in view, as the scan thread read it: M/A/D/R from
        // `git status`, `?` for untracked; the session ledger's letters win below
        let scope = self.scope.borrow().clone();
        let repos: Vec<PathBuf> = statuses.iter().map(|(r, _)| r.clone()).collect();
        let mut from_git = 0usize;
        for (repo, status) in &statuses {
            for (rel, st) in status {
                from_git += 1;
                let path = repo.join(rel);
                badges.insert(path.clone(), Badge::Git(st.clone()));
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
        // folders only as roots: a pinned file is a chip, not a tree. Pinned here is the selected
        // session's own pins (`pinned`, set by refresh); other sessions' pins are scanned for git
        // status but show only as repositories with changes
        let mut roots: Vec<PathBuf> = match &scope {
            Some(s) => vec![s.clone()],
            None => self.pinned.borrow().iter().filter(|p| super::remote::is_dir(&p)).cloned().collect(),
        };
        let global: Vec<PathBuf> = self.global.borrow().iter().filter(|p| super::remote::is_dir(&p)).cloned().collect();
        // the global view's third group: every repository a session changed that isn't pinned
        let mut touched: Vec<PathBuf> = Vec::new();
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
                && let Some(r) = super::remote::repo_root(&c.path)
                && !roots.contains(&r)
                && !global.contains(&r)
                && !touched.contains(&r)
            {
                touched.push(r);
            }
        }
        roots.dedup();
        // the global view with nothing but home: say how to get a folder into it
        self.hint.set_visible(!self.diff.is_active() && scope.is_none() && roots.is_empty() && global.is_empty() && touched.is_empty());
        // the rows to show: the scoped folder alone, or the global view's groups under their
        // captions (a group that is empty has no caption)
        let text = |p: &PathBuf| p.to_string_lossy().into_owned();
        let wanted: Vec<String> = match &scope {
            Some(_) => roots.iter().map(text).collect(),
            None => {
                let mut w: Vec<String> = Vec::new();
                let mut section = |caption: &str, paths: &[PathBuf]| {
                    if !paths.is_empty() {
                        w.push(format!("{SECTION}{caption}"));
                        w.extend(paths.iter().map(text));
                    }
                };
                section("Pinned here", &roots);
                section("Global pins", &global);
                section("Repositories with changes", &touched);
                // home is its own caption: the row reads "Home" and opens on click (decorate)
                if !roots.contains(&home) && !global.contains(&home) && !touched.contains(&home) {
                    w.push(text(&home));
                }
                w
            }
        };
        // the search box says, on hover, what it searches and when the badges were last read
        let when = glib::DateTime::now_local().ok().and_then(|d| d.format("%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
        let from_sessions = all.iter().filter(|c| !c.ignored).count();
        self.search.set_tooltip_text(Some(&format!(
            "Find a file below the shown folders by name (a / in the text matches the path). Badges read {when}: {from_git} from git status in {} repositor{}, {from_sessions} from sessions' overlays",
            repos.len(),
            if repos.len() == 1 { "y" } else { "ies" }
        )));
        *self.badges.borrow_mut() = badges;
        *self.changes.borrow_mut() = pairs;
        // directories already listed: files that appeared or went since, in place
        self.listed.borrow_mut().retain(|_, w| w.upgrade().is_some());
        let listed: Vec<(PathBuf, gio::ListStore)> = self.listed.borrow().iter().filter_map(|(d, w)| w.upgrade().map(|s| (d.clone(), s))).collect();
        for (dir, store) in listed {
            let g = ghosts(&self.badges.borrow(), &dir);
            sync_children(&dir, &store, &g);
        }
        // keep the tree if the roots didn't change (expansion state survives); search results stay
        // what they are — the badges on them are fresh, which is what the refresh was for
        let current: Vec<String> = (0..self.roots.n_items())
            .filter_map(|i| self.roots.item(i).and_downcast::<gtk::StringObject>().map(|o| o.string().to_string()))
            .collect();
        if current != wanted && self.query.borrow().is_empty() {
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
        self.refresh_stat(false);
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
    fn watch(self: &Rc<Self>, _dirs: Vec<PathBuf>) {
        if self.poll.borrow().is_some() { return }
        let weak = Rc::downgrade(self);
        *self.poll.borrow_mut() = Some(glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            let Some(tree) = weak.upgrade() else { return glib::ControlFlow::Break };
            if tree.root.is_visible() { tree.refresh_soon(); }
            glib::ControlFlow::Continue
        }));
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

    /// A scoped tree starts one level open: the pinned folder itself (its children stay closed).
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
