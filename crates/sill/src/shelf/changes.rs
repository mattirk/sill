//! Backend page "Changes": the global ledger across sessions (SPEC §3.1), grouped by session and
//! repository, with keep / revert and a diff view.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use sill_core::changes;
use sill_core::ledger::{Change, Kind};
use sill_core::store::{Session, Store};

use super::session;

pub struct ChangesPage {
    pub root: gtk::Box,
    list: gtk::ListBox,
    diff: gtk::TextView,
    summary: gtk::Label,
    message: gtk::Label,
    store: Rc<Store>,
    rows: RefCell<Vec<(String, Change)>>, // (session id, change) per list row index
}

impl ChangesPage {
    pub fn new(store: Rc<Store>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_top(6);
        root.set_margin_bottom(12);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let summary = gtk::Label::new(Some("no changes"));
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.add_css_class("dim-label");
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
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
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();

        let diff = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(8)
            .right_margin(8)
            .build();
        let diff_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&diff).build();
        diff_scroll.add_css_class("frame");

        let paned = gtk::Paned::new(gtk::Orientation::Vertical);
        paned.set_start_child(Some(&list_scroll));
        paned.set_end_child(Some(&diff_scroll));
        paned.set_position(500);
        paned.set_vexpand(true);
        root.append(&paned);

        let page = Rc::new(Self { root, list, diff, summary, message, store, rows: RefCell::new(Vec::new()) });
        {
            let p = page.clone();
            refresh.connect_clicked(move |_| p.refresh());
        }
        {
            let p = page.clone();
            page.list.connect_row_selected(move |_, row| {
                let Some(row) = row else { return };
                let idx = row.index() as usize;
                if let Some((_, c)) = p.rows.borrow().get(idx) {
                    let text = changes::unified_diff(c).unwrap_or_else(|e| format!("diff: {e:#}"));
                    p.diff.buffer().set_text(&text);
                }
            });
        }
        page
    }

    /// Rescan every stored session's upper trees (fast: only touched files are read).
    pub fn refresh(self: &Rc<Self>) {
        let sessions = self.store.sessions().unwrap_or_default();
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        let (tx, rx) = async_channel::bounded::<Vec<(Session, Vec<Change>)>>(1);
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for s in sessions {
                let Ok(policy) = sill_core::policy::Policy::load(&s.policy, &home) else { continue };
                let Ok(state_dir) = sill_core::session::state_dir(uid, &s.id) else { continue };
                if !state_dir.is_dir() {
                    continue;
                }
                let Ok(plan) = changes::session_plan(policy.clone(), state_dir, &home, uid, &user) else { continue };
                let list = changes::scan(&policy, &plan, None).unwrap_or_default();
                if !list.is_empty() {
                    out.push((s, list));
                }
            }
            let _ = tx.send_blocking(out);
        });
        let p = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(groups) = rx.recv().await {
                p.populate(groups);
            }
        });
    }

    fn populate(self: &Rc<Self>, groups: Vec<(Session, Vec<Change>)>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        self.diff.buffer().set_text("");
        let mut total = 0;
        let mut conflicts = 0;
        for (s, list) in groups {
            for (repo, changes_in_repo) in changes::group_by_repo(&list) {
                let header = gtk::Label::new(Some(&format!(
                    "{} · {}",
                    s.title,
                    if repo.as_os_str().is_empty() { "outside any repo".to_string() } else { repo.display().to_string() }
                )));
                header.set_xalign(0.0);
                header.add_css_class("heading");
                header.set_margin_top(8);
                let hrow = gtk::ListBoxRow::new();
                hrow.set_selectable(false);
                hrow.set_activatable(false);
                let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                header.set_hexpand(true);
                hbox.append(&header);
                let keep_all = gtk::Button::with_label("Keep all");
                keep_all.add_css_class("flat");
                {
                    let p = self.clone();
                    let items: Vec<(String, Change)> = changes_in_repo.iter().map(|c| (s.id.clone(), c.clone())).collect();
                    keep_all.connect_clicked(move |_| {
                        let mut n = 0;
                        for (_, c) in &items {
                            if c.conflict || c.ignored {
                                continue;
                            }
                            if sill_core::keep::keep(c, &sill_core::keep::KeepOptions::default()).is_ok() {
                                n += 1;
                            }
                        }
                        p.message.set_text(&format!("kept {n} file(s)"));
                        p.refresh();
                    });
                }
                hbox.append(&keep_all);
                hrow.set_child(Some(&hbox));
                self.list.append(&hrow);
                self.rows.borrow_mut().push((s.id.clone(), changes_in_repo[0].clone())); // placeholder for the header index

                for c in changes_in_repo {
                    total += 1;
                    if c.conflict {
                        conflicts += 1;
                    }
                    let row = gtk::ListBoxRow::new();
                    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let letter = gtk::Label::new(Some(&format!("{}{}", c.kind.letter(), if c.conflict { "!" } else { " " })));
                    letter.add_css_class("monospace");
                    match c.kind {
                        Kind::Added => letter.add_css_class("success"),
                        Kind::Deleted => letter.add_css_class("error"),
                        _ => letter.add_css_class("accent"),
                    }
                    let path = gtk::Label::new(Some(&display_path(&c.path, &repo)));
                    path.set_xalign(0.0);
                    path.set_hexpand(true);
                    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                    if c.ignored {
                        path.add_css_class("dim-label");
                    }
                    let keep_b = gtk::Button::with_label("Keep");
                    keep_b.add_css_class("flat");
                    keep_b.set_sensitive(!c.conflict);
                    let revert_b = gtk::Button::with_label("Revert");
                    revert_b.add_css_class("flat");
                    {
                        let p = self.clone();
                        let c2 = c.clone();
                        keep_b.connect_clicked(move |_| {
                            match sill_core::keep::keep(&c2, &sill_core::keep::KeepOptions::default()) {
                                Ok(what) => {
                                    p.message.set_text(&format!("kept {}: {what}", c2.path.display()));
                                    p.refresh();
                                }
                                Err(e) => p.message.set_text(&format!("keep failed: {e:#}")),
                            }
                        });
                    }
                    {
                        let p = self.clone();
                        let sid = s.id.clone();
                        let path = c.path.clone();
                        let upper = c.upper.clone();
                        revert_b.connect_clicked(move |_| {
                            // running session: revert inside the jail; stopped session (overlay not
                            // mounted): dropping the upper entry is the revert
                            let result = if sill_core::session::is_running(&sid) {
                                session::revert(&sid, &path)
                            } else {
                                remove_upper_entry(&upper).map(|_| "dropped from the session's record".to_string())
                            };
                            match result {
                                Ok(what) => {
                                    p.message.set_text(&format!("reverted {}: {what}", path.display()));
                                    p.refresh();
                                }
                                Err(e) => p.message.set_text(&format!("revert failed: {e:#}")),
                            }
                        });
                    }
                    b.append(&letter);
                    b.append(&path);
                    b.append(&keep_b);
                    b.append(&revert_b);
                    row.set_child(Some(&b));
                    self.list.append(&row);
                    self.rows.borrow_mut().push((s.id.clone(), c.clone()));
                }
            }
        }
        self.summary.set_text(&if total == 0 {
            "no pending changes".to_string()
        } else {
            format!("{total} pending change(s){}", if conflicts > 0 { format!(", {conflicts} conflict(s)") } else { String::new() })
        });
    }
}

/// Remove an entry from an unmounted upper tree (whiteout, file, symlink or directory).
fn remove_upper_entry(upper: &std::path::Path) -> anyhow::Result<()> {
    match upper.symlink_metadata() {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(upper)?,
        Ok(_) => std::fs::remove_file(upper)?,
        Err(_) => {}
    }
    Ok(())
}

fn display_path(path: &PathBuf, repo: &PathBuf) -> String {
    if !repo.as_os_str().is_empty()
        && let Ok(rel) = path.strip_prefix(repo)
    {
        return rel.display().to_string();
    }
    path.display().to_string()
}
