//! Backend page "Changes": the global ledger across sessions (SPEC §3.1), grouped by session and
//! repository, with keep / revert and a highlighted diff. Minimal rows: a status letter, the path,
//! quiet buttons that light up on hover. Where a change lives is said in badges: `git` (a
//! repository: review with git), `live` (a pinned folder: host = jail), `no git` (an overlay change
//! outside any repository — these auto-revert, SPEC §4.4, and the row says when). What was kept
//! stays listed under the session with an Undo until the session is deleted (keep with undo).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gtk4 as gtk;
use gtk::glib;
use sill_core::changes;
use sill_core::keep::{self, KeepOptions, Kept};
use sill_core::ledger::{Change, FileType, Kind};
use sill_core::store::{Session, Store};
use sourceview5::prelude::*;

use super::{session, theme};

pub struct ChangesPage {
    pub root: gtk::Box,
    list: gtk::ListBox,
    diff: sourceview5::Buffer,
    summary: gtk::Label,
    message: gtk::Label,
    store: Rc<Store>,
    rows: RefCell<Vec<Row>>, // what each list row index shows
    /// Is this session mid-turn? Auto-revert leaves it alone then (set by the window).
    busy: RefCell<Option<Box<dyn Fn(&str) -> bool>>>,
}

/// What a list row stands for: decides the diff shown when it is selected.
enum Row {
    /// A group header: nothing to show.
    Header,
    /// A pinned repository's own change: git's diff of `rel` in `repo`.
    Live { repo: PathBuf, rel: PathBuf },
    /// A session's overlay change: upper vs host.
    Change(Change),
    /// A keep that can be undone: host vs the saved previous version.
    Kept { host: PathBuf, prior: PathBuf },
}

/// A pinned repository with git's view of its working tree: (repo, `git_status` rows).
type LiveRepo = (PathBuf, Vec<(PathBuf, String)>);

/// A scanned session: its changes, split into the agent's own state and the rest, and its keeps.
struct Scanned {
    session: Session,
    changes: Vec<Change>,
    agent_state: Vec<Change>,
    /// `<state dir>/kept`: where keep saves the host's previous versions.
    kept_dir: PathBuf,
    kept: Vec<Kept>,
    /// SHA-256 of each changed regular file's upper content as listed: what the person reviews is
    /// what keep must copy (`KeepOptions::expected_hash`; a swap in between is refused).
    hashes: std::collections::HashMap<PathBuf, String>,
}

/// Keep all keeps what a glance can vouch for: a regular file added or modified. Deletions, type
/// changes, replaced directories and symlinks go one by one, each a click of its own.
fn keeps_in_bulk(c: &Change) -> bool {
    matches!(c.kind, Kind::Added | Kind::Modified) && matches!(c.file_type, FileType::File | FileType::Dir)
}

fn keep_opts(c: &Change, hashes: &std::collections::HashMap<PathBuf, String>, kept_dir: &Path) -> KeepOptions {
    KeepOptions { undo: Some(kept_dir.to_path_buf()), expected_hash: hashes.get(&c.path).cloned(), ..Default::default() }
}

impl ChangesPage {
    pub fn new(store: Rc<Store>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_top(6);
        root.set_margin_bottom(8);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let summary = gtk::Label::new(Some("no changes"));
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.add_css_class("dim-label");
        let refresh = super::icons::glyph_button("sill-refresh");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("Rescan"));
        bar.append(&summary);
        bar.append(&refresh);
        root.append(&bar);
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.set_wrap(true);
        message.set_selectable(true);
        message.add_css_class("dim-label");
        root.append(&message);

        let list = gtk::ListBox::new();
        list.add_css_class("sill-changes");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();

        // the diff: GtkSourceView's diff language in the theme's scheme, like the Files editor
        let diff = sourceview5::Buffer::new(None);
        diff.set_language(sourceview5::LanguageManager::default().language("diff").as_ref());
        diff.set_highlight_syntax(true);
        diff.set_highlight_matching_brackets(false);
        let diff_view = sourceview5::View::with_buffer(&diff);
        diff_view.set_editable(false);
        diff_view.set_cursor_visible(false);
        diff_view.set_monospace(true);
        diff_view.set_wrap_mode(gtk::WrapMode::WordChar);
        diff_view.set_left_margin(8);
        diff_view.set_right_margin(8);
        diff_view.set_top_margin(6);
        diff_view.add_css_class("sill-diff");
        let diff_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&diff_view).build();
        diff_scroll.add_css_class("sill-diff-frame");

        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.set_start_child(Some(&list_scroll));
        paned.set_end_child(Some(&diff_scroll));
        paned.set_position(420);
        paned.set_vexpand(true);
        root.append(&paned);

        let page = Rc::new(Self { root, list, diff, summary, message, store, rows: RefCell::new(Vec::new()), busy: RefCell::new(None) });
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
            refresh.connect_clicked(move |_| p.refresh());
        }
        {
            let p = page.clone();
            page.list.connect_row_selected(move |_, row| {
                let Some(row) = row else { return };
                let idx = row.index() as usize;
                if let Some(r) = p.rows.borrow().get(idx) {
                    let text = match r {
                        Row::Header => String::new(),
                        Row::Live { repo, rel } => changes::git_diff(repo, rel),
                        Row::Change(c) => changes::unified_diff(c).unwrap_or_else(|e| format!("diff: {e:#}")),
                        Row::Kept { host, prior } => changes::undo_diff(host, prior),
                    };
                    p.diff.set_text(&text);
                }
            });
        }
        page
    }

    fn apply_scheme(&self, t: &theme::Theme) {
        if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&theme::source_scheme(t)) {
            self.diff.set_style_scheme(Some(&s));
        }
    }

    pub fn set_busy_probe(&self, f: impl Fn(&str) -> bool + 'static) {
        *self.busy.borrow_mut() = Some(Box::new(f));
    }

    fn is_busy(&self, sid: &str) -> bool {
        self.busy.borrow().as_ref().map(|f| f(sid)).unwrap_or(false)
    }

    /// Scan every stored session's upper trees and the pinned repositories' git status off the
    /// GTK thread (the scan is fast — only touched files are read — `git status` on a big tree is
    /// not, SPEC §4.11 #13), then hand the result to `done`.
    fn scan(&self, done: impl FnOnce(Vec<LiveRepo>, Vec<Scanned>) + 'static) {
        let sessions = self.store.sessions().unwrap_or_default();
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        // pinned folders: live (host = jail); git repositories among them show git's view
        let mut pinned: Vec<PathBuf> = sessions.iter().flat_map(|s| self.store.pins(&s.id).unwrap_or_default()).filter(|p| p.join(".git").exists()).collect();
        pinned.sort();
        pinned.dedup();
        let (tx, rx) = async_channel::bounded::<(Vec<LiveRepo>, Vec<Scanned>)>(1);
        std::thread::spawn(move || {
            let live: Vec<LiveRepo> = pinned.into_iter().map(|r| (changes::git_status(&r), r)).filter(|(st, _)| !st.is_empty()).map(|(st, r)| (r, st)).collect();
            let mut out = Vec::new();
            for s in sessions {
                let Ok(mut policy) = sill_core::policy::Policy::load(&s.policy, &home) else { continue };
                if s.ghost {
                    policy.ghostly(&home); // the jail it runs: `direct` as `ro`
                }
                let Ok(state_dir) = sill_core::session::state_dir(uid, &s.id) else { continue };
                if !state_dir.is_dir() {
                    continue;
                }
                let binds = match Store::open_default() {
                    Ok(st) => session::binds_of(&s, &policy, &home, &st),
                    Err(_) => vec![],
                };
                let kept_dir = state_dir.join("kept");
                let Ok(plan) = changes::session_plan(policy.clone(), state_dir, &home, uid, &user, binds) else { continue };
                // build output (git-ignored / ledger_ignore) is the overlay's business, not the person's
                let list: Vec<Change> = changes::scan(&policy, &plan, None).unwrap_or_default().into_iter().filter(|c| !c.ignored).collect();
                let kept = keep::undoable(&kept_dir);
                if list.is_empty() && kept.is_empty() {
                    continue;
                }
                let state_dirs = sill_core::agents::load(&home)
                    .get(&s.agent)
                    .map(|a| sill_core::session::state_paths(a, &home))
                    .unwrap_or_default();
                let (agent_state, changes): (Vec<Change>, Vec<Change>) = list.into_iter().partition(|c| changes::is_agent_state(&c.path, &state_dirs, &home));
                // the content as listed, for keep to check against (regular files; the rest has none)
                let hashes = changes
                    .iter()
                    .chain(agent_state.iter())
                    .filter(|c| c.file_type == FileType::File && matches!(c.kind, Kind::Added | Kind::Modified))
                    .filter_map(|c| keep::sha256_file(&c.upper).ok().map(|h| (c.path.clone(), h)))
                    .collect();
                out.push(Scanned { session: s, changes, agent_state, kept_dir, kept, hashes });
            }
            let _ = tx.send_blocking((live, out));
        });
        glib::spawn_future_local(async move {
            if let Ok((live, groups)) = rx.recv().await {
                done(live, groups);
            }
        });
    }

    pub fn refresh(self: &Rc<Self>) {
        let p = self.clone();
        self.scan(move |live, groups| p.populate(live, groups));
    }

    /// Auto-revert (SPEC §4.4): once a minute, every overlay change outside any git repository
    /// whose last write is older than the configured delay is reverted — unless its session is
    /// mid-turn. Pinned folders never appear here (live), repositories are git's business, the
    /// agent's own state is left alone.
    pub fn sweep(self: &Rc<Self>) {
        let minutes = sill_core::config::load().auto_revert_minutes();
        if minutes == 0 {
            return;
        }
        let p = self.clone();
        self.scan(move |_, groups| {
            // what is due is decided here; the reverts themselves run off the GTK thread
            let mut jobs: Vec<(String, Vec<Change>)> = Vec::new();
            for g in &groups {
                if p.is_busy(&g.session.id) {
                    continue;
                }
                // deepest paths first, so directories go after their contents
                let mut due: Vec<Change> = g.changes.iter().filter(|c| auto_reverts(c) && revert_due(c, minutes).is_some_and(|d| d.is_zero())).cloned().collect();
                due.sort_by_key(|c| std::cmp::Reverse(c.path.components().count()));
                if !due.is_empty() {
                    jobs.push((g.session.id.clone(), due));
                }
            }
            if jobs.is_empty() {
                return;
            }
            offload(
                move || {
                    let mut n = 0;
                    for (sid, due) in &jobs {
                        let running = sill_core::session::is_running(sid);
                        for c in due {
                            let ok = if running { session::revert(sid, &c.path).is_ok() } else { remove_upper_entry(&c.upper).is_ok() };
                            if ok {
                                n += 1;
                            }
                        }
                    }
                    n
                },
                move |n| {
                    if n > 0 {
                        p.message.set_text(&format!("auto-reverted {n} path(s) outside git"));
                        p.refresh();
                    }
                },
            );
        });
    }

    /// A group header row: title, badges, actions.
    fn header_row(&self, title: &str, tooltip: Option<&str>, badges: &[(&str, &str)], actions: Vec<gtk::Widget>) -> gtk::ListBoxRow {
        let hrow = gtk::ListBoxRow::new();
        hrow.set_selectable(false);
        hrow.set_activatable(false);
        hrow.add_css_class("sill-changes-header");
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.add_css_class("heading");
        label.set_tooltip_text(tooltip);
        hbox.append(&label);
        for (text, class) in badges {
            let b = gtk::Label::new(Some(text));
            b.add_css_class("sill-badge");
            b.add_css_class(class);
            hbox.append(&b);
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        hbox.append(&spacer);
        for a in actions {
            hbox.append(&a);
        }
        hrow.set_child(Some(&hbox));
        hrow
    }

    /// A change row: letter, path, an optional note (dim, right), buttons.
    fn change_row(&self, letter: &str, class: &str, path: &str, note: Option<String>, buttons: Vec<gtk::Button>) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("sill-change");
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let l = gtk::Label::new(Some(letter));
        l.add_css_class("monospace");
        l.add_css_class(class);
        l.set_width_chars(2);
        let p = gtk::Label::new(Some(path));
        p.set_xalign(0.0);
        p.set_hexpand(true);
        p.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        p.set_tooltip_text(Some(path));
        b.append(&l);
        b.append(&p);
        if let Some(n) = note {
            let nl = gtk::Label::new(Some(&n));
            nl.add_css_class("dim-label");
            nl.add_css_class("sill-change-note");
            b.append(&nl);
        }
        for btn in buttons {
            btn.add_css_class("flat");
            btn.add_css_class("sill-change-btn");
            b.append(&btn);
        }
        row.set_child(Some(&b));
        row
    }

    fn populate(self: &Rc<Self>, live: Vec<LiveRepo>, groups: Vec<Scanned>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        self.diff.set_text("");
        let minutes = sill_core::config::load().auto_revert_minutes();
        let mut total = 0;
        let mut conflicts = 0;
        // pinned folders first: live (host = jail); git repositories among them show git's view
        for (repo, status) in live {
            let hrow = self.header_row(&short_path(&repo), Some(&repo.display().to_string()), &[("git", "sill-badge-git"), ("live", "sill-badge-live")], vec![]);
            self.list.append(&hrow);
            self.rows.borrow_mut().push(Row::Header);
            for (rel, st) in status {
                total += 1;
                let class = match st.as_str() {
                    "?" | "A" => "success",
                    "D" => "error",
                    _ => "accent",
                };
                let revert_b = gtk::Button::with_label("Revert");
                revert_b.set_tooltip_text(Some("git checkout HEAD (an untracked file is removed)"));
                {
                    let p = self.clone();
                    let repo = repo.clone();
                    let rel = rel.clone();
                    revert_b.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (p, repo, rel, shown) = (p.clone(), repo.clone(), rel.clone(), rel.display().to_string());
                        offload(
                            move || changes::git_revert(&repo, &rel).map_err(|e| format!("{e:#}")),
                            move |r| match r {
                                Ok(what) => {
                                    p.message.set_text(&format!("{shown}: {what}"));
                                    p.refresh();
                                }
                                Err(e) => p.message.set_text(&format!("revert failed: {e}")),
                            },
                        );
                    });
                }
                let row = self.change_row(&st, class, &rel.display().to_string(), None, vec![revert_b]);
                self.list.append(&row);
                self.rows.borrow_mut().push(Row::Live { repo: repo.clone(), rel });
            }
        }
        for g in groups {
            let s = g.session;
            let kept_dir = g.kept_dir;
            let hashes = Rc::new(g.hashes);
            let mut repo_groups: Vec<(PathBuf, Vec<Change>, bool)> =
                changes::group_by_repo(&g.changes).into_iter().map(|(r, v)| (r, v, false)).collect();
            if !g.agent_state.is_empty() {
                repo_groups.push((PathBuf::from("agent state"), g.agent_state, true));
            }
            for (repo, changes_in_repo, collapsed) in repo_groups {
                let in_git = !collapsed && !repo.as_os_str().is_empty();
                let title = if collapsed {
                    format!("{} · the agent's own files ({})", s.title, changes_in_repo.len())
                } else if in_git {
                    format!("{} · {}", s.title, short_path(&repo))
                } else {
                    format!("{} · outside any repository", s.title)
                };
                let tooltip = if in_git { Some(repo.display().to_string()) } else { None };
                let mut badges: Vec<(&str, &str)> = Vec::new();
                if in_git {
                    badges.push(("git", "sill-badge-git"));
                } else if !collapsed {
                    badges.push(("no git", "sill-badge-nogit"));
                    if minutes > 0 {
                        badges.push(("auto-revert", "sill-badge-nogit"));
                    }
                }
                let keep_all = gtk::Button::with_label("Keep all");
                keep_all.add_css_class("flat");
                keep_all.add_css_class("sill-change-btn");
                keep_all.set_tooltip_text(Some("copy every added or modified file to the host — deletions, type changes, replaced folders and links are kept one by one"));
                {
                    let p = self.clone();
                    let items: Vec<Change> = changes_in_repo.clone();
                    let hashes = hashes.clone();
                    let kept_dir = kept_dir.clone();
                    keep_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let p = p.clone();
                        let jobs: Vec<(Change, KeepOptions)> = items
                            .iter()
                            .filter(|c| !c.conflict && !c.ignored && keep::protected(&c.path).is_none() && keeps_in_bulk(c))
                            .map(|c| (c.clone(), keep_opts(c, &hashes, &kept_dir)))
                            .collect();
                        let skipped = items.len() - jobs.len();
                        offload(
                            move || jobs.iter().filter(|(c, opts)| keep::keep(c, opts).is_ok()).count(),
                            move |n| {
                                p.message.set_text(&if skipped == 0 {
                                    format!("kept {n} file(s)")
                                } else {
                                    format!("kept {n} file(s); {skipped} left for a click of their own (deletions, type changes, links, conflicts)")
                                });
                                p.refresh();
                            },
                        );
                    });
                }
                let revert_all = gtk::Button::with_label("Revert all");
                revert_all.add_css_class("flat");
                revert_all.add_css_class("sill-change-btn");
                {
                    let p = self.clone();
                    let sid = s.id.clone();
                    let items: Vec<Change> = changes_in_repo.clone();
                    revert_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (p, sid, items) = (p.clone(), sid.clone(), items.clone());
                        offload(
                            move || {
                                let running = sill_core::session::is_running(&sid);
                                let (mut n, mut failed) = (0, 0);
                                let mut sorted = items;
                                sorted.sort_by_key(|c| std::cmp::Reverse(c.path.components().count()));
                                for c in &sorted {
                                    let ok = if running { session::revert(&sid, &c.path).is_ok() } else { remove_upper_entry(&c.upper).is_ok() };
                                    if ok { n += 1 } else { failed += 1 }
                                }
                                (n, failed)
                            },
                            move |(n, failed)| {
                                p.message.set_text(&if failed == 0 { format!("reverted {n} path(s)") } else { format!("reverted {n} path(s), {failed} failed") });
                                p.refresh();
                            },
                        );
                    });
                }
                let mut actions: Vec<gtk::Widget> = vec![keep_all.upcast(), revert_all.upcast()];
                let mut toggle: Option<gtk::ToggleButton> = None;
                if collapsed {
                    let t = gtk::ToggleButton::new();
                    t.set_child(Some(&super::icons::glyph_image("sill-chevron-right")));
                    t.add_css_class("flat");
                    t.set_tooltip_text(Some("Show the agent's own files"));
                    actions.insert(0, t.clone().upcast());
                    toggle = Some(t);
                }
                let hrow = self.header_row(&title, tooltip.as_deref(), &badges, actions);
                self.list.append(&hrow);
                self.rows.borrow_mut().push(Row::Header);
                let mut group_rows: Vec<gtk::ListBoxRow> = Vec::new();

                for c in changes_in_repo {
                    total += 1;
                    if c.conflict {
                        conflicts += 1;
                    }
                    let class = match c.kind {
                        Kind::Added => "success",
                        Kind::Deleted => "error",
                        _ => "accent",
                    };
                    let letter = format!("{}{}", c.kind.letter(), if c.conflict { "!" } else { "" });
                    let note = if c.conflict {
                        Some("changed on the host too".to_string())
                    } else if s.ghost && !collapsed {
                        // a ghost's hour (docs/memory.md §3.2): everything, the daemon reverts it
                        revert_due(&c, sill_core::policy::GHOST_IDLE_MINUTES).map(|d| if d.is_zero() { "reverting".to_string() } else { format!("reverts in {}", human_minutes(d)) })
                    } else if !in_git && !collapsed && minutes > 0 && auto_reverts(&c) {
                        revert_due(&c, minutes).map(|d| if d.is_zero() { "reverting".to_string() } else { format!("reverts in {}", human_minutes(d)) })
                    } else {
                        None
                    };
                    let keep_b = gtk::Button::with_label("Keep");
                    keep_b.set_sensitive(!c.conflict);
                    keep_b.set_tooltip_text(Some("copy to the host"));
                    if let Some(why) = keep::protected(&c.path) {
                        // sill's own files and a repository's executable config: never from here
                        keep_b.set_sensitive(false);
                        keep_b.set_tooltip_text(Some(&format!("not kept by sill: {why} — do it by hand if you mean it")));
                    }
                    let revert_b = gtk::Button::with_label("Revert");
                    {
                        let p = self.clone();
                        let c2 = c.clone();
                        let opts = keep_opts(&c, &hashes, &kept_dir);
                        keep_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let (p, c, opts, shown) = (p.clone(), c2.clone(), opts.clone(), c2.path.display().to_string());
                            offload(
                                move || keep::keep(&c, &opts).map_err(|e| format!("{e:#}")),
                                move |r| match r {
                                    Ok(what) => {
                                        p.message.set_text(&format!("kept {shown}: {what}"));
                                        p.refresh();
                                    }
                                    Err(e) => p.message.set_text(&format!("keep failed: {e}")),
                                },
                            );
                        });
                    }
                    {
                        let p = self.clone();
                        let sid = s.id.clone();
                        let path = c.path.clone();
                        let upper = c.upper.clone();
                        revert_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let (p, sid, path, upper, shown) = (p.clone(), sid.clone(), path.clone(), upper.clone(), path.display().to_string());
                            offload(
                                move || {
                                    // running session: revert inside the jail; stopped session (overlay
                                    // not mounted): dropping the upper entry is the revert
                                    if sill_core::session::is_running(&sid) {
                                        session::revert(&sid, &path)
                                    } else {
                                        remove_upper_entry(&upper).map(|_| "dropped from the session's record".to_string())
                                    }
                                    .map_err(|e| format!("{e:#}"))
                                },
                                move |r| match r {
                                    Ok(what) => {
                                        p.message.set_text(&format!("reverted {shown}: {what}"));
                                        p.refresh();
                                    }
                                    Err(e) => p.message.set_text(&format!("revert failed: {e}")),
                                },
                            );
                        });
                    }
                    let row = self.change_row(&letter, class, &display_path(&c.path, &repo), note, vec![keep_b, revert_b]);
                    if collapsed {
                        row.set_visible(false);
                    }
                    self.list.append(&row);
                    self.rows.borrow_mut().push(Row::Change(c));
                    group_rows.push(row);
                }
                if let Some(t) = toggle {
                    t.connect_toggled(move |t| {
                        t.set_child(Some(&super::icons::glyph_image(if t.is_active() { "sill-chevron-down" } else { "sill-chevron-right" })));
                        for r in &group_rows {
                            r.set_visible(t.is_active());
                        }
                    });
                }
            }
            // what left the overlay: every keep stays undoable until the session is deleted (SPEC §4.4)
            if !g.kept.is_empty() {
                let undo_all = gtk::Button::with_label("Undo all");
                undo_all.add_css_class("flat");
                undo_all.add_css_class("sill-change-btn");
                undo_all.set_tooltip_text(Some("put every previous version back, newest first"));
                {
                    let p = self.clone();
                    let dir = kept_dir.clone();
                    let items = g.kept.clone();
                    undo_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (p, dir, items) = (p.clone(), dir.clone(), items.clone());
                        offload(
                            move || {
                                let (mut n, mut failed) = (0, 0);
                                for k in &items {
                                    if keep::undo(&dir, k, false).is_ok() { n += 1 } else { failed += 1 }
                                }
                                (n, failed)
                            },
                            move |(n, failed)| {
                                p.message.set_text(&if failed == 0 { format!("undid {n} keep(s)") } else { format!("undid {n} keep(s), {failed} changed on the host since") });
                                p.refresh();
                            },
                        );
                    });
                }
                let t = gtk::ToggleButton::new();
                t.set_child(Some(&super::icons::glyph_image("sill-chevron-right")));
                t.add_css_class("flat");
                t.set_tooltip_text(Some("Show what was kept"));
                let title = format!("{} · kept ({})", s.title, g.kept.len());
                let hrow = self.header_row(&title, None, &[("kept", "sill-badge-kept")], vec![t.clone().upcast(), undo_all.upcast()]);
                self.list.append(&hrow);
                self.rows.borrow_mut().push(Row::Header);
                let mut group_rows: Vec<gtk::ListBoxRow> = Vec::new();
                for k in g.kept {
                    let (letter, class) = match k.what.as_str() {
                        "added" => ("A", "success"),
                        "removed" => ("D", "error"),
                        _ => ("M", "accent"),
                    };
                    let undo_b = gtk::Button::with_label("Undo");
                    undo_b.set_tooltip_text(Some(if k.before == "absent" { "remove it from the host again" } else { "put the host's previous version back" }));
                    {
                        let p = self.clone();
                        let dir = kept_dir.clone();
                        let k2 = k.clone();
                        // a refusal (edited on the host since) turns the button into "Undo anyway"
                        let forced = Rc::new(Cell::new(false));
                        undo_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let (p, dir, k, force, forced, b, shown) = (p.clone(), dir.clone(), k2.clone(), forced.get(), forced.clone(), b.clone(), k2.path.display().to_string());
                            offload(
                                move || keep::undo(&dir, &k, force).map_err(|e| format!("{e:#}")),
                                move |r| match r {
                                    Ok(what) => {
                                        p.message.set_text(&format!("undid {shown}: {what}"));
                                        p.refresh();
                                    }
                                    Err(e) => {
                                        forced.set(true);
                                        b.set_label("Undo anyway");
                                        b.set_sensitive(true);
                                        p.message.set_text(&format!("undo failed: {e}"));
                                    }
                                },
                            );
                        });
                    }
                    let row = self.change_row(letter, class, &short_path(&k.path), Some(format!("kept {}", human_ago(k.ts))), vec![undo_b]);
                    row.set_visible(false);
                    self.list.append(&row);
                    self.rows.borrow_mut().push(Row::Kept { host: k.path.clone(), prior: keep::prior_path(&kept_dir, &k) });
                    group_rows.push(row);
                }
                t.connect_toggled(move |t| {
                    t.set_child(Some(&super::icons::glyph_image(if t.is_active() { "sill-chevron-down" } else { "sill-chevron-right" })));
                    for r in &group_rows {
                        r.set_visible(t.is_active());
                    }
                });
            }
        }
        self.summary.set_text(&if total == 0 {
            "no pending changes".to_string()
        } else {
            format!("{total} pending change(s){}", if conflicts > 0 { format!(", {conflicts} conflict(s)") } else { String::new() })
        });
    }
}

/// Run `work` on a thread — a keep, a revert, an undo: file copies and shim calls that must not
/// hold the GTK loop (SPEC §4.11 #13) — then `done` with its result on the main loop. The button
/// that started it is left insensitive; the refresh that follows rebuilds the rows.
fn offload<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    let (tx, rx) = async_channel::bounded::<T>(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(work());
    });
    glib::spawn_future_local(async move {
        if let Ok(r) = rx.recv().await {
            done(r);
        }
    });
}

/// Does auto-revert apply to this change? Only overlay changes outside any git repository; a
/// conflict (changed on the host too) is left for the person.
fn auto_reverts(c: &Change) -> bool {
    !c.conflict && !c.ignored && changes::repo_root(&c.path).is_none()
}

/// Time left before auto-revert, from the upper entry's last write; `None` when it can't be told.
fn revert_due(c: &Change, minutes: u32) -> Option<Duration> {
    let written = std::fs::symlink_metadata(&c.upper).ok()?.modified().ok()?;
    let age = SystemTime::now().duration_since(written).unwrap_or_default();
    Some(Duration::from_secs(minutes as u64 * 60).saturating_sub(age))
}

fn human_minutes(d: Duration) -> String {
    let m = d.as_secs().div_ceil(60);
    if m >= 120 { format!("{} h", m / 60) } else { format!("{m} min") }
}

/// `just now`, `3 min ago`, `2 h ago`, `4 d ago`.
fn human_ago(ts: i64) -> String {
    let m = (sill_core::store::now() - ts).max(0) as u64 / 60;
    if m < 1 {
        "just now".into()
    } else if m < 120 {
        format!("{m} min ago")
    } else if m < 48 * 60 {
        format!("{} h ago", m / 60)
    } else {
        format!("{} d ago", m / (24 * 60))
    }
}

/// `~/GIT/x/repo` → `GIT/x/repo`: the path without the home prefix.
fn short_path(p: &Path) -> String {
    let home = session::home().unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("~/{}", rel.display()),
        _ => p.display().to_string(),
    }
}

/// Remove an entry from an unmounted upper tree (whiteout, file, symlink or directory).
fn remove_upper_entry(upper: &Path) -> anyhow::Result<()> {
    match upper.symlink_metadata() {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(upper)?,
        Ok(_) => std::fs::remove_file(upper)?,
        Err(_) => {}
    }
    Ok(())
}

fn display_path(path: &Path, repo: &Path) -> String {
    if !repo.as_os_str().is_empty()
        && let Ok(rel) = path.strip_prefix(repo)
    {
        return rel.display().to_string();
    }
    short_path(path)
}
