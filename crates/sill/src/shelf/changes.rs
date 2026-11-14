//! Backend page "Changes": the global ledger across sessions (SPEC §3.1), grouped by session and
//! repository, with keep / revert and a highlighted diff. Minimal rows: a status letter, the path,
//! quiet buttons that light up on hover. Where a change lives is said in badges: `git` (a
//! repository: review with git), `live` (a pinned folder: host = jail), `no git` (an overlay change
//! outside any repository — these auto-revert, SPEC §4.4, and the row says when). What was kept
//! stays listed under the session with an Undo until the session is deleted (keep with undo).

//!
//! The scan, the diffs, keep, revert and undo are the daemon's (docs/daemon.md §7 M2): this page
//! asks for `changes.list` and renders it; a click names a change by session and path.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use gtk::glib;
use sill_core::ledger::{FileType, Kind};
use super::remote_store::Store;
use sill_core::history::{Event, Filter, Kind as HKind};
use sill_core::wire::{ChangeRow, ChangesList, DiffOf, Done, LiveRepo, SessionChanges};
use sourceview5::prelude::*;

use super::{session, theme};

pub struct ChangesPage {
    pub root: gtk::Box,
    list: gtk::ListBox,
    diff: sourceview5::Buffer,
    summary: gtk::Label,
    message: gtk::Label,
    rows: RefCell<Vec<Row>>, // what each list row index shows
    /// A refresh is out (the daemon answers it), and whether another was asked for meanwhile:
    /// `changes.changed` and the page's own actions arrive together, one rescan follows.
    loading: Cell<bool>,
    again: Cell<bool>,
    /// History (SPEC §3.6): what sessions did to the host over time, shown instead of the list.
    history: gtk::ListBox,
    hrows: RefCell<Vec<Option<Event>>>,
    filter: gtk::SearchEntry,
    lists: gtk::Stack,
}

/// What a list row stands for: decides the diff the daemon is asked for when it is selected.
enum Row {
    /// A group header: nothing to show.
    Header,
    Diff(DiffOf),
}

/// Keep all keeps what a glance can vouch for: a regular file added or modified. Deletions, type
/// changes, replaced directories and symlinks go one by one, each a click of its own.
fn keeps_in_bulk(r: &ChangeRow) -> bool {
    let c = &r.change;
    matches!(c.kind, Kind::Added | Kind::Modified) && matches!(c.file_type, FileType::File | FileType::Dir)
}

/// One daemon call off the GTK thread, `done` with its result on the main loop.
fn ask<T: serde::de::DeserializeOwned + Send + 'static>(method: &'static str, params: impl serde::Serialize + Clone + Send + 'static, done: impl FnOnce(anyhow::Result<T>) + 'static) {
    offload(move || super::client::call::<T>(method, params), done);
}

/// What a keep, revert or undo did, as the line under the bar.
fn said(verb: &str, d: &Done) -> String {
    match (d.done.len(), d.failed.first()) {
        (1, None) => format!("{verb} {}: {}", short_path(&d.done[0].0), d.done[0].1),
        (n, None) => format!("{verb} {n} path(s)"),
        (0, Some((p, e))) if d.failed.len() == 1 => format!("{} failed: {}: {e}", verb, short_path(p)),
        (n, Some((p, e))) => format!("{verb} {n} path(s), {} failed — {}: {e}", d.failed.len(), short_path(p)),
    }
}

impl ChangesPage {
    pub fn new(_store: Rc<Store>) -> Rc<Self> {
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
        // Pending (the overlays now) | History (SPEC §3.6: what was kept, undone, reverted)
        let pending_b = gtk::ToggleButton::with_label("Pending");
        let history_b = gtk::ToggleButton::with_label("History");
        history_b.set_group(Some(&pending_b));
        pending_b.set_active(true);
        for b in [&pending_b, &history_b] {
            b.add_css_class("flat");
            b.add_css_class("sill-change-btn");
        }
        history_b.set_tooltip_text(Some("What sessions did to this machine over time: kept, undone, reverted, auto-reverted, and the commits made in pinned folders"));
        let filter = gtk::SearchEntry::new();
        filter.set_placeholder_text(Some("path, e.g. ~/.config"));
        filter.set_visible(false);
        bar.append(&summary);
        bar.append(&filter);
        bar.append(&pending_b);
        bar.append(&history_b);
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

        let history = gtk::ListBox::new();
        history.add_css_class("sill-changes");
        history.set_selection_mode(gtk::SelectionMode::Single);
        let history_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&history).build();
        let lists = gtk::Stack::new();
        lists.add_named(&list_scroll, Some("pending"));
        lists.add_named(&history_scroll, Some("history"));

        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.set_start_child(Some(&lists));
        paned.set_end_child(Some(&diff_scroll));
        paned.set_position(420);
        paned.set_vexpand(true);
        root.append(&paned);

        let page = Rc::new(Self { root, list, diff, summary, message, rows: RefCell::new(Vec::new()), loading: Cell::new(false), again: Cell::new(false), history, hrows: RefCell::new(Vec::new()), filter, lists });
        {
            let p = page.clone();
            history_b.connect_toggled(move |b| {
                let on = b.is_active();
                p.lists.set_visible_child_name(if on { "history" } else { "pending" });
                p.filter.set_visible(on);
                p.diff.set_text("");
                p.refresh();
            });
        }
        {
            let p = page.clone();
            page.filter.connect_search_changed(move |_| p.refresh());
        }
        {
            let p = page.clone();
            page.history.connect_row_selected(move |_, row| {
                let Some(row) = row else { return };
                let Some(Some(e)) = p.hrows.borrow().get(row.index() as usize).cloned() else {
                    p.diff.set_text("");
                    return;
                };
                p.show_event(&e);
            });
        }
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
                let of = match p.rows.borrow().get(idx) {
                    Some(Row::Diff(of)) => of.clone(),
                    _ => {
                        p.diff.set_text("");
                        return;
                    }
                };
                let p2 = p.clone();
                ask::<sill_proto::Text>(sill_proto::method::CHANGES_DIFF, of, move |r| {
                    p2.diff.set_text(&match r {
                        Ok(t) => t.text,
                        Err(e) => format!("diff: {e:#}"),
                    })
                });
            });
        }
        page
    }

    fn apply_scheme(&self, t: &theme::Theme) {
        if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&theme::source_scheme(t)) {
            self.diff.set_style_scheme(Some(&s));
        }
    }

    pub fn refresh(self: &Rc<Self>) {
        if self.lists.visible_child_name().as_deref() == Some("history") {
            return self.refresh_history();
        }
        if self.loading.replace(true) {
            self.again.set(true);
            return;
        }
        let p = self.clone();
        ask::<ChangesList>(sill_proto::method::CHANGES_LIST, sill_proto::Empty::default(), move |r| {
            match r {
                Ok(list) => p.populate(list.live, list.sessions),
                Err(e) => p.message.set_text(&format!("changes: {e:#}")),
            }
            p.loading.set(false);
            if p.again.replace(false) {
                p.refresh();
            }
        });
    }

    /// The minute timer: the daemon reverts what is due (SPEC §4.4); the page, when it is on
    /// screen, shows what is left.
    pub fn refresh_if_shown(self: &Rc<Self>) {
        if self.root.is_mapped() {
            self.refresh();
        }
    }

    /// The history (SPEC §3.6), newest first, a header per day; the filter box narrows by path.
    fn refresh_history(self: &Rc<Self>) {
        let text = self.filter.text().trim().to_string();
        let home = session::home().unwrap_or_default();
        let path = (!text.is_empty()).then(|| match text.strip_prefix('~') {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(&text),
        });
        let f = Filter { path, limit: 1000, ..Default::default() };
        let p = self.clone();
        ask::<Vec<Event>>(sill_proto::method::CHANGES_HISTORY, f, move |r| match r {
            Ok(rows) => p.populate_history(rows),
            Err(e) => p.message.set_text(&format!("history: {e:#}")),
        });
    }

    fn populate_history(&self, rows: Vec<Event>) {
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        self.hrows.borrow_mut().clear();
        self.diff.set_text("");
        let home = session::home().unwrap_or_default();
        self.summary.set_text(&match rows.len() {
            0 => "nothing in the history yet".to_string(),
            n => format!("{n} event(s)"),
        });
        let mut day = String::new();
        for e in rows {
            let d = sill_memory::date(e.ts);
            if d != day {
                let h = self.header_row(&d, None, &[], vec![]);
                self.history.append(&h);
                self.hrows.borrow_mut().push(None);
                day = d;
            }
            let (letter, class) = match e.kind {
                HKind::Kept => ("K", "success"),
                HKind::Undone => ("U", "accent"),
                HKind::Reverted | HKind::AutoReverted => ("R", "error"),
                HKind::Pinned => ("P", "accent"),
                HKind::Deleted => ("×", "error"),
                HKind::Folded => ("…", "dim-label"),
            };
            let what = match e.kind {
                HKind::AutoReverted => format!("{} path(s) under {}", e.count, short_path(&e.path)),
                HKind::Pinned => format!("{} · {} commit(s), {} file(s)", short_path(&e.path), e.commits.unwrap_or(0), e.files.unwrap_or(0)),
                HKind::Deleted => "the session was deleted".to_string(),
                HKind::Folded => format!("{} under {}", e.what, short_path(&e.path)),
                _ => short_path(&e.path),
            };
            let who = if e.ghost { format!("{} · a ghost", e.agent) } else if e.session_title.is_empty() { e.agent.clone() } else { format!("{} · {}", e.agent, e.session_title) };
            let time = sill_memory::datetime(e.ts);
            let note = format!("{} · {who} · {}", e.kind.id(), time.get(11..).unwrap_or(&time));
            let row = self.change_row(letter, class, &what, Some(note), vec![]);
            row.set_tooltip_text(Some(sill_memory::history::line(&e, &home).trim_end()));
            self.history.append(&row);
            self.hrows.borrow_mut().push(Some(e));
        }
    }

    /// A history row selected: the keep's diff while its session (and so its undo payload) lives,
    /// otherwise what the row knows — the paths of a sweep, a range's heads, the hashes.
    fn show_event(self: &Rc<Self>, e: &Event) {
        let home = session::home().unwrap_or_default();
        let mut text = sill_memory::history::line(e, &home);
        for p in &e.paths {
            text.push_str(&format!("  {}\n", short_path(p)));
        }
        if let (Some(a), Some(b)) = (&e.head_start, &e.head_end) {
            text.push_str(&format!("\n  git log {a}..{b}\n"));
        }
        for (label, h) in [("written", &e.hash_new), ("replaced", &e.hash_old)] {
            if let Some(h) = h {
                text.push_str(&format!("  {label}: sha256 {h}\n"));
            }
        }
        self.diff.set_text(&text);
        if let (HKind::Kept, Some(session), Some(id)) = (e.kind, &e.session_id, &e.kept_id) {
            let p = self.clone();
            let of = DiffOf::Kept { session: session.clone(), id: id.clone() };
            ask::<sill_proto::Text>(sill_proto::method::CHANGES_DIFF, of, move |r| {
                // the payload went with its session: the description stays
                if let Ok(t) = r {
                    p.diff.set_text(&format!("{text}\n{}", t.text));
                }
            });
        }
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

    fn populate(self: &Rc<Self>, live: Vec<LiveRepo>, groups: Vec<SessionChanges>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        self.diff.set_text("");
        let minutes = super::remote::config::load().auto_revert_minutes();
        let mut total = 0;
        let mut conflicts = 0;
        // pinned folders first: live (host = jail); git repositories among them show git's view
        for LiveRepo { repo, status } in live {
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
                    let req = sill_proto::GitRevert { repo: repo.clone(), rel: rel.clone() };
                    let shown = rel.display().to_string();
                    revert_b.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let (p, shown) = (p.clone(), shown.clone());
                        ask::<sill_proto::Text>(sill_proto::method::CHANGES_GIT_REVERT, req.clone(), move |r| match r {
                            Ok(t) => {
                                p.message.set_text(&format!("{shown}: {}", t.text));
                                p.refresh();
                            }
                            Err(e) => p.message.set_text(&format!("revert failed: {e:#}")),
                        });
                    });
                }
                let row = self.change_row(&st, class, &rel.display().to_string(), None, vec![revert_b]);
                self.list.append(&row);
                self.rows.borrow_mut().push(Row::Diff(DiffOf::Live { repo: repo.clone(), rel }));
            }
        }
        for g in groups {
            let sid = g.id.clone();
            // by enclosing repository, "" (outside any) first, as the host's paths sort
            let mut by_repo: std::collections::BTreeMap<PathBuf, Vec<ChangeRow>> = std::collections::BTreeMap::new();
            for r in g.changes {
                by_repo.entry(r.repo.clone()).or_default().push(r);
            }
            let mut repo_groups: Vec<(PathBuf, Vec<ChangeRow>, bool)> = by_repo.into_iter().map(|(r, v)| (r, v, false)).collect();
            if !g.agent_state.is_empty() {
                repo_groups.push((PathBuf::from("agent state"), g.agent_state, true));
            }
            for (repo, changes_in_repo, collapsed) in repo_groups {
                let in_git = !collapsed && !repo.as_os_str().is_empty();
                let title = if collapsed {
                    format!("{} · the agent's own files ({})", g.title, changes_in_repo.len())
                } else if in_git {
                    format!("{} · {}", g.title, short_path(&repo))
                } else {
                    format!("{} · outside any repository", g.title)
                };
                let tooltip = if in_git { Some(repo.display().to_string()) } else { None };
                let mut badges: Vec<(&str, &str)> = Vec::new();
                if g.ghost && !collapsed {
                    badges.push(("ghost", "sill-badge-nogit"));
                } else if in_git {
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
                    let sid = sid.clone();
                    let picked: Vec<(PathBuf, Option<String>)> = changes_in_repo
                        .iter()
                        .filter(|r| !r.change.conflict && !r.change.ignored && r.protected.is_none() && keeps_in_bulk(r))
                        .map(|r| (r.change.path.clone(), r.hash.clone()))
                        .collect();
                    let skipped = changes_in_repo.len() - picked.len();
                    keep_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let p = p.clone();
                        let req = sill_proto::ChangesKeep { session: sid.clone(), items: picked.clone() };
                        ask::<Done>(sill_proto::method::CHANGES_KEEP, req, move |r| {
                            p.message.set_text(&match r {
                                Ok(d) if skipped == 0 => format!("kept {} file(s){}", d.done.len(), if d.failed.is_empty() { String::new() } else { format!(", {} failed", d.failed.len()) }),
                                Ok(d) => format!("kept {} file(s); {skipped} left for a click of their own (deletions, type changes, links, conflicts)", d.done.len()),
                                Err(e) => format!("keep failed: {e:#}"),
                            });
                            p.refresh();
                        });
                    });
                }
                let revert_all = gtk::Button::with_label("Revert all");
                revert_all.add_css_class("flat");
                revert_all.add_css_class("sill-change-btn");
                {
                    let p = self.clone();
                    let req = sill_proto::ChangesPaths { session: sid.clone(), paths: changes_in_repo.iter().map(|r| r.change.path.clone()).collect() };
                    revert_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let p = p.clone();
                        ask::<Done>(sill_proto::method::CHANGES_REVERT, req.clone(), move |r| {
                            p.message.set_text(&match r {
                                Ok(d) => said("reverted", &d),
                                Err(e) => format!("revert failed: {e:#}"),
                            });
                            p.refresh();
                        });
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

                for r in changes_in_repo {
                    let c = &r.change;
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
                    let countdown = |m: u32| revert_due(r.written, m).map(|d| if d.is_zero() { "reverting".to_string() } else { format!("reverts in {}", human_minutes(d)) });
                    let note = if c.conflict {
                        Some("changed on the host too".to_string())
                    } else if g.ghost && !collapsed {
                        // a ghost's hour (docs/memory.md §3.2): everything, the daemon reverts it
                        countdown(sill_core::policy::GHOST_IDLE_MINUTES)
                    } else if !in_git && !collapsed && minutes > 0 && !c.ignored {
                        countdown(minutes)
                    } else {
                        None
                    };
                    let keep_b = gtk::Button::with_label("Keep");
                    keep_b.set_sensitive(!c.conflict);
                    keep_b.set_tooltip_text(Some("copy to the host"));
                    if let Some(why) = &r.protected {
                        // sill's own files and a repository's executable config: never from here
                        keep_b.set_sensitive(false);
                        keep_b.set_tooltip_text(Some(&format!("not kept by sill: {why} — do it by hand if you mean it")));
                    }
                    let revert_b = gtk::Button::with_label("Revert");
                    {
                        let p = self.clone();
                        let req = sill_proto::ChangesKeep { session: sid.clone(), items: vec![(c.path.clone(), r.hash.clone())] };
                        keep_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let p = p.clone();
                            ask::<Done>(sill_proto::method::CHANGES_KEEP, req.clone(), move |r| match r {
                                Ok(d) if d.failed.is_empty() => {
                                    p.message.set_text(&said("kept", &d));
                                    p.refresh();
                                }
                                Ok(d) => p.message.set_text(&said("keep", &d)),
                                Err(e) => p.message.set_text(&format!("keep failed: {e:#}")),
                            });
                        });
                    }
                    {
                        let p = self.clone();
                        let req = sill_proto::ChangesPaths { session: sid.clone(), paths: vec![c.path.clone()] };
                        revert_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let p = p.clone();
                            ask::<Done>(sill_proto::method::CHANGES_REVERT, req.clone(), move |r| match r {
                                Ok(d) if d.failed.is_empty() => {
                                    p.message.set_text(&said("reverted", &d));
                                    p.refresh();
                                }
                                Ok(d) => p.message.set_text(&said("revert", &d)),
                                Err(e) => p.message.set_text(&format!("revert failed: {e:#}")),
                            });
                        });
                    }
                    let row = self.change_row(&letter, class, &display_path(&c.path, &repo), note, vec![keep_b, revert_b]);
                    if collapsed {
                        row.set_visible(false);
                    }
                    self.list.append(&row);
                    self.rows.borrow_mut().push(Row::Diff(DiffOf::Change { session: sid.clone(), path: c.path.clone() }));
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
                    let req = sill_proto::ChangesUndo { session: sid.clone(), ids: vec![], force: false };
                    undo_all.connect_clicked(move |b| {
                        b.set_sensitive(false);
                        let p = p.clone();
                        ask::<Done>(sill_proto::method::CHANGES_UNDO, req.clone(), move |r| {
                            p.message.set_text(&match r {
                                Ok(d) if d.failed.is_empty() => format!("undid {} keep(s)", d.done.len()),
                                Ok(d) => format!("undid {} keep(s), {} changed on the host since", d.done.len(), d.failed.len()),
                                Err(e) => format!("undo failed: {e:#}"),
                            });
                            p.refresh();
                        });
                    });
                }
                let t = gtk::ToggleButton::new();
                t.set_child(Some(&super::icons::glyph_image("sill-chevron-right")));
                t.add_css_class("flat");
                t.set_tooltip_text(Some("Show what was kept"));
                let title = format!("{} · kept ({})", g.title, g.kept.len());
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
                        let (sid, id) = (sid.clone(), k.id.clone());
                        // a refusal (edited on the host since) turns the button into "Undo anyway"
                        let forced = Rc::new(Cell::new(false));
                        undo_b.connect_clicked(move |b| {
                            b.set_sensitive(false);
                            let (p, forced, b) = (p.clone(), forced.clone(), b.clone());
                            let req = sill_proto::ChangesUndo { session: sid.clone(), ids: vec![id.clone()], force: forced.get() };
                            ask::<Done>(sill_proto::method::CHANGES_UNDO, req, move |r| match r {
                                Ok(d) if d.failed.is_empty() => {
                                    p.message.set_text(&said("undid", &d));
                                    p.refresh();
                                }
                                Ok(d) => {
                                    forced.set(true);
                                    b.set_label("Undo anyway");
                                    b.set_sensitive(true);
                                    p.message.set_text(&said("undo", &d));
                                }
                                Err(e) => {
                                    b.set_sensitive(true);
                                    p.message.set_text(&format!("undo failed: {e:#}"));
                                }
                            });
                        });
                    }
                    let row = self.change_row(letter, class, &short_path(&k.path), Some(format!("kept {}", human_ago(k.ts))), vec![undo_b]);
                    row.set_visible(false);
                    self.list.append(&row);
                    self.rows.borrow_mut().push(Row::Diff(DiffOf::Kept { session: sid.clone(), id: k.id.clone() }));
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

/// Time left before auto-revert, from the upper entry's last write as listed; `None` when unknown.
fn revert_due(written: Option<i64>, minutes: u32) -> Option<Duration> {
    let age = (sill_core::store::now() - written?).max(0) as u64;
    Some(Duration::from_secs(minutes as u64 * 60).saturating_sub(Duration::from_secs(age)))
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

fn display_path(path: &Path, repo: &Path) -> String {
    if !repo.as_os_str().is_empty()
        && let Ok(rel) = path.strip_prefix(repo)
    {
        return rel.display().to_string();
    }
    short_path(path)
}
