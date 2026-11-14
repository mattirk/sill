//! The drawer's diff view: `git diff --stat` of the repositories in view — the tree's scoped folder's
//! repository, else the current session's pinned ones. One row per changed file (working tree and
//! index against HEAD, untracked files as new): its status, its path below the repository, `+added
//! −removed`. A click opens the file's diff in the viewer.
//!
//! Counts come from `_sill/repos.summary` per file, off the GTK thread; a rescan that finds the same
//! status and totals as the last one stops there, so the 2 s poll of a shown drawer costs two calls
//! per repository while nothing changes.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;

/// Files past this many in a repository are listed without counts (one daemon call each).
const COUNT_CAP: usize = 150;
/// An untracked file's lines are counted only below this size.
const NEW_FILE_MAX: u64 = 1_000_000;

type Statuses = Vec<(PathBuf, Vec<(PathBuf, String)>)>;
/// Per list row: the file's (repository, path below it); None for captions and notes.
type Rows = Rc<RefCell<Vec<Option<(PathBuf, PathBuf)>>>>;
type OpenDiff = Rc<RefCell<Option<Box<dyn Fn(&Path, &Path)>>>>;

#[derive(Debug, Clone)]
struct Entry {
    rel: PathBuf,
    status: String,
    /// (added, removed); None past the cap or for a binary file
    counts: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct Repo {
    root: PathBuf,
    entries: Vec<Entry>,
}

/// One scan: the repositories' status and totals (what decides whether counting is needed again).
fn signature(repos: &[PathBuf]) -> (String, Statuses) {
    let mut sig = String::new();
    let mut out = Vec::new();
    for r in repos {
        let status = super::remote::git_status(r).unwrap_or_default();
        let totals = super::remote::git_summary(r);
        sig.push_str(&format!("{r:?}{status:?}{totals:?};"));
        out.push((r.clone(), status));
    }
    (sig, out)
}

fn count(repo: &Path, rel: &Path, status: &str) -> Option<(u64, u64)> {
    let path = repo.join(rel);
    if status == "?" {
        let meta = super::remote::metadata(&path).ok()?;
        if !meta.file || meta.length > NEW_FILE_MAX {
            return None;
        }
        let bytes = super::remote::read(&path).ok()?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return None;
        }
        let lines = bytes.split(|b| *b == b'\n').count() as u64 - u64::from(bytes.ends_with(b"\n") || bytes.is_empty());
        return Some((lines, 0));
    }
    let (a, d, _) = super::remote::git_summary(&path)?;
    // numstat says `-` for a binary file: no counts, no bar
    (a > 0 || d > 0).then_some((a, d))
}

pub struct DiffStat {
    pub root: gtk::ScrolledWindow,
    list: gtk::ListBox,
    /// The drawer's search box, lower-cased: rows whose path holds it.
    filter: Rc<RefCell<String>>,
    /// The signature of what is shown; None = nothing shown yet (or forced).
    shown: RefCell<Option<String>>,
    scanning: Cell<bool>,
    again: RefCell<Option<Vec<PathBuf>>>,
    rows: Rows,
    pub on_open: OpenDiff,
}

impl DiffStat {
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.add_css_class("sill-files");
        list.add_css_class("sill-stat");
        let filter: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        {
            let filter = filter.clone();
            list.set_filter_func(move |row| {
                let q = filter.borrow();
                q.is_empty() || !row.is_activatable() || row.widget_name().to_lowercase().contains(q.as_str())
            });
        }
        let root = gtk::ScrolledWindow::builder()
            .child(&super::markdown::still_viewport(&list))
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::External)
            .build();
        let rows: Rows = Rc::new(RefCell::new(Vec::new()));
        let on_open: OpenDiff = Rc::new(RefCell::new(None));
        {
            let rows = rows.clone();
            let on_open = on_open.clone();
            list.connect_row_activated(move |_, row| {
                let Some(Some((repo, rel))) = rows.borrow().get(row.index() as usize).cloned() else { return };
                if let Some(cb) = on_open.borrow().as_ref() {
                    cb(&repo, &rel);
                }
            });
        }
        Rc::new(Self { root, list, filter, shown: RefCell::new(None), scanning: Cell::new(false), again: RefCell::new(None), rows, on_open })
    }

    pub fn set_filter(&self, q: &str) {
        *self.filter.borrow_mut() = q.to_string();
        self.list.invalidate_filter();
    }

    /// Rescan `repos`; `force` redraws even when nothing changed (the repositories in view did).
    pub fn refresh(self: &Rc<Self>, repos: Vec<PathBuf>, force: bool) {
        if force {
            *self.shown.borrow_mut() = None;
        }
        if self.scanning.replace(true) {
            *self.again.borrow_mut() = Some(repos);
            return;
        }
        let last = self.shown.borrow().clone();
        let (tx, rx) = async_channel::bounded::<Option<(String, Vec<Repo>)>>(1);
        std::thread::spawn(move || {
            let (sig, statuses) = signature(&repos);
            if last.as_deref() == Some(sig.as_str()) {
                let _ = tx.send_blocking(None);
                return;
            }
            let shown = statuses
                .into_iter()
                .map(|(root, status)| {
                    let entries = status
                        .into_iter()
                        .enumerate()
                        .map(|(i, (rel, st))| {
                            let counts = if i < COUNT_CAP { count(&root, &rel, &st) } else { None };
                            Entry { rel, status: st, counts }
                        })
                        .collect();
                    Repo { root, entries }
                })
                .collect();
            let _ = tx.send_blocking(Some((sig, shown)));
        });
        let s = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(Some((sig, repos))) = rx.recv().await {
                s.show(&repos);
                *s.shown.borrow_mut() = Some(sig);
            }
            s.scanning.set(false);
            if let Some(next) = s.again.borrow_mut().take() {
                s.refresh(next, false);
            }
        });
    }

    fn show(&self, repos: &[Repo]) {
        let selected = self.list.selected_row().map(|r| r.widget_name().to_string());
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let mut rows = Vec::new();
        if repos.is_empty() {
            self.list.append(&note("No pinned git repository here: pin a repository's folder (+ on its row) to see its diff."));
            rows.push(None);
        }
        for repo in repos {
            let (files, add, del) = repo.entries.iter().fold((0, 0, 0), |(f, a, d), e| {
                let (ea, ed) = e.counts.unwrap_or((0, 0));
                (f + 1, a + ea, d + ed)
            });
            let head = super::remote::head(&repo.root).map(|(h, _)| format!("  git {h}")).unwrap_or_default();
            // the repository's folder name; the whole path is the tooltip
            let name = repo.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| repo.root.display().to_string());
            let caption = gtk::Label::new(Some(&format!("{name}{head}")));
            caption.set_xalign(0.0);
            caption.set_hexpand(true);
            caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
            caption.add_css_class("sill-tree-section");
            caption.set_tooltip_text(Some(&repo.root.display().to_string()));
            let total = gtk::Label::new(Some(&if files == 0 {
                "clean".to_string()
            } else {
                format!("{files} file{} · +{add} −{del}", if files == 1 { "" } else { "s" })
            }));
            total.add_css_class("sill-tree-section");
            total.set_tooltip_text(Some(&format!(
                "{files} file{} changed, {add} insertion{}(+), {del} deletion{}(−) against HEAD (untracked files count as insertions)",
                if files == 1 { "" } else { "s" },
                if add == 1 { "" } else { "s" },
                if del == 1 { "" } else { "s" },
            )));
            let hb = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            hb.append(&caption);
            hb.append(&total);
            let header = gtk::ListBoxRow::builder().child(&hb).activatable(false).selectable(false).build();
            self.list.append(&header);
            rows.push(None);
            if repo.entries.is_empty() {
                self.list.append(&note("Nothing differs from HEAD."));
                rows.push(None);
            }
            for e in &repo.entries {
                let row = file_row(e);
                let key = e.rel.display().to_string();
                row.set_widget_name(&key);
                self.list.append(&row);
                if selected.as_deref() == Some(key.as_str()) {
                    self.list.select_row(Some(&row));
                }
                rows.push(Some((repo.root.clone(), e.rel.clone())));
            }
            if repo.entries.len() > COUNT_CAP {
                self.list.append(&note(&format!("past {COUNT_CAP} files the counts are not read")));
                rows.push(None);
            }
        }
        *self.rows.borrow_mut() = rows;
    }
}

fn note(text: &str) -> gtk::ListBoxRow {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.add_css_class("dim-label");
    l.add_css_class("sill-menu-caption");
    gtk::ListBoxRow::builder().child(&l).activatable(false).selectable(false).build()
}

fn file_row(e: &Entry) -> gtk::ListBoxRow {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.add_css_class("sill-tree-row");
    let st = gtk::Label::new(Some(if e.status == "?" { "N" } else { e.status.as_str() }));
    st.add_css_class("monospace");
    st.add_css_class("sill-badge");
    st.add_css_class(match e.status.as_str() {
        "?" | "A" => "sill-new",
        "D" => "sill-stat-del",
        _ => "sill-changed",
    });
    st.set_tooltip_text(Some(match e.status.as_str() {
        "?" => "new: untracked in git",
        "A" => "added to the index",
        "D" => "deleted",
        "R" => "renamed",
        "M" => "modified",
        _ => "changed",
    }));
    // the file name alone; its path below the repository is the tooltip (and what the search matches)
    let name = e.rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path = gtk::Label::new(Some(&name));
    path.set_xalign(0.0);
    path.set_hexpand(true);
    path.set_ellipsize(gtk::pango::EllipsizeMode::End);
    path.set_tooltip_text(Some(&e.rel.display().to_string()));
    b.append(&st);
    b.append(&path);
    match e.counts {
        Some((a, d)) => {
            // `+12 −3` in git's colours, a side with nothing left out
            for (k, sign, class) in [(a, "+", "sill-stat-add"), (d, "−", "sill-stat-del")] {
                if k > 0 {
                    let l = gtk::Label::new(Some(&format!("{sign}{k}")));
                    l.add_css_class("monospace");
                    l.add_css_class("sill-stat-n");
                    l.add_css_class(class);
                    b.append(&l);
                }
            }
        }
        None => {
            let l = gtk::Label::new(Some("—"));
            l.add_css_class("dim-label");
            l.set_tooltip_text(Some("no line counts: binary, too large, or past the first files"));
            b.append(&l);
        }
    }
    gtk::ListBoxRow::builder().child(&b).build()
}
