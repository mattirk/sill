//! Backend page "Agents": the ACP registry (SPEC §4.1) as a quiet list — search, a three-way
//! filter, one row per agent with its icon, version, description, links and a single action.
//! Installs run on a worker thread into sill's tools dir; their output streams under the row.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use sill_core::registry::{self, Entry, Trust};

use super::{icons, session, settings, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Installed,
    NotInstalled,
}

/// What a row shows about an entry's state here.
enum State {
    /// Installed by sill (tools dir), with the version it has and where that came from.
    Managed(String, Trust),
    /// A built-in whose command is already on this machine (not by sill).
    System(PathBuf),
    Absent,
}

struct Row {
    state_box: gtk::Box,
    log: gtk::Label,
    icon_box: gtk::Box,
    /// Inline settings (gear), built on first open.
    editor: gtk::Box,
}

enum Progress {
    Line(String, String),
    Done(String, Result<String, String>),
    Icon(String, PathBuf),
}

pub struct AgentsPage {
    pub root: gtk::Box,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    note: gtk::Label,
    refresh_btn: gtk::Button,
    filter: Cell<Filter>,
    show_untested: Cell<bool>,
    entries: RefCell<Vec<Entry>>,
    rows: RefCell<HashMap<String, Row>>,
    installing: RefCell<std::collections::HashSet<String>>,
    logs: RefCell<HashMap<String, Vec<String>>>,
    tx: async_channel::Sender<Progress>,
    fetching: Cell<bool>,
    icons_queued: RefCell<std::collections::HashSet<String>>,
    /// The live-index entry whose Install was clicked once: the next click installs it.
    confirm: RefCell<Option<String>>,
}

impl AgentsPage {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_top(4);
        root.set_margin_bottom(12);

        // search + filter + refresh on one line
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let search = gtk::SearchEntry::new();
        icons::adopt_search_entry(&search);
        search.set_placeholder_text(Some("Search agents…"));
        search.set_hexpand(true);
        bar.append(&search);
        let filters = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        filters.add_css_class("linked");
        let all = gtk::ToggleButton::with_label("All");
        let inst = gtk::ToggleButton::with_label("Installed");
        inst.set_group(Some(&all));
        let not = gtk::ToggleButton::with_label("Not installed");
        not.set_group(Some(&all));
        all.set_active(true);
        for b in [&all, &inst, &not] {
            b.add_css_class("sill-filter");
            filters.append(b);
        }
        bar.append(&filters);
        // untested entries are hidden until asked for (installed ones always show)
        let more = gtk::ToggleButton::with_label("Untested");
        more.add_css_class("sill-filter");
        more.add_css_class("flat");
        more.set_tooltip_text(Some("Also list agents nobody has tried inside sill yet"));
        bar.append(&more);
        let refresh = super::icons::glyph_button("sill-refresh");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("Fetch the registry again"));
        bar.append(&refresh);
        root.append(&bar);

        let note = gtk::Label::new(None);
        note.add_css_class("dim-label");
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.set_visible(false);
        root.append(&note);

        let list = gtk::ListBox::new();
        list.add_css_class("sill-agents");
        list.set_selection_mode(gtk::SelectionMode::None);
        let scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();
        root.append(&scroll);

        let (tx, rx) = async_channel::unbounded::<Progress>();
        let page = Rc::new(Self {
            root,
            list,
            search: search.clone(),
            note,
            refresh_btn: refresh.clone(),
            filter: Cell::new(Filter::All),
            show_untested: Cell::new(false),
            entries: RefCell::new(Vec::new()),
            rows: RefCell::new(HashMap::new()),
            installing: RefCell::new(Default::default()),
            logs: RefCell::new(HashMap::new()),
            tx,
            fetching: Cell::new(false),
            confirm: RefCell::new(None),
            icons_queued: RefCell::new(Default::default()),
        });

        {
            let p = page.clone();
            search.connect_search_changed(move |_| p.rebuild());
        }
        for (b, f) in [(&all, Filter::All), (&inst, Filter::Installed), (&not, Filter::NotInstalled)] {
            let p = page.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    p.filter.set(f);
                    p.rebuild();
                }
            });
        }
        {
            let p = page.clone();
            refresh.connect_clicked(move |_| p.fetch());
        }
        {
            let p = page.clone();
            more.connect_toggled(move |b| {
                p.show_untested.set(b.is_active());
                p.rebuild();
            });
        }
        {
            // worker → UI
            let p = page.clone();
            glib::spawn_future_local(async move {
                while let Ok(msg) = rx.recv().await {
                    p.progress(msg);
                }
            });
        }
        page
    }

    /// Show the snapshot plus what is cached of the live index; fetch the live index when there is
    /// nothing or it is older than six hours.
    pub fn refresh(self: &Rc<Self>) {
        let home = session::home().unwrap_or_default();
        let (entries, when) = registry::merged(&home);
        *self.entries.borrow_mut() = entries;
        self.rebuild();
        self.set_note("");
        let snap = registry::snapshot();
        let n = self.entries.borrow().len();
        match when {
            Some(when) => {
                let age = SystemTime::now().duration_since(when).unwrap_or_default();
                self.refresh_btn.set_tooltip_text(Some(&format!(
                    "{n} agents: {} in sill's snapshot ({} @ {}), the live index fetched {} · fetch again",
                    snap.agents.len(),
                    snap.source,
                    &snap.commit[..snap.commit.len().min(12)],
                    ago(age)
                )));
                if age > Duration::from_secs(6 * 3600) {
                    self.fetch();
                }
            }
            None => self.fetch(),
        }
    }

    /// The note is for problems only; it takes no room otherwise.
    fn set_note(&self, text: &str) {
        self.note.set_text(text);
        self.note.set_visible(!text.is_empty());
    }

    fn fetch(self: &Rc<Self>) {
        if self.fetching.replace(true) {
            return;
        }
        self.refresh_btn.set_sensitive(false);
        let tx = self.tx.clone();
        let home = session::home().unwrap_or_default();
        std::thread::spawn(move || {
            let r = registry::fetch(&home).map(|_| String::new()).map_err(|e| format!("{e:#}"));
            let _ = tx.send_blocking(Progress::Done(String::new(), r));
        });
    }

    fn state_of(&self, entry: &Entry, home: &std::path::Path) -> State {
        if let Some(i) = registry::installed(home, &entry.id) {
            return State::Managed(i.version, i.trust);
        }
        if let Some(id) = registry::builtin_for(entry)
            && let Some(agent) = sill_core::agents::load(home).get(id)
            && let Some(path) = registry::available(agent, home)
        {
            return State::System(path);
        }
        State::Absent
    }

    /// Rebuild the list from the entries, the search text and the filter.
    fn rebuild(self: &Rc<Self>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.rows.borrow_mut().clear();
        let home = session::home().unwrap_or_default();
        let needle = self.search.text().to_lowercase();
        let mut entries: Vec<Entry> = self.entries.borrow().clone();
        entries.sort_by_key(|e| e.name.to_lowercase());
        let fg = icons::fg_for(&self.list, &theme::current());
        for e in entries {
            if !needle.is_empty()
                && !(e.name.to_lowercase().contains(&needle)
                    || e.id.to_lowercase().contains(&needle)
                    || e.description.to_lowercase().contains(&needle))
            {
                continue;
            }
            let state = self.state_of(&e, &home);
            let installed = !matches!(state, State::Absent);
            if !installed && !registry::is_tested(&e.id) && !self.show_untested.get() {
                continue;
            }
            match self.filter.get() {
                Filter::Installed if !installed => continue,
                Filter::NotInstalled if installed => continue,
                _ => {}
            }
            self.list.append(&self.row(&e, state, fg, &home));
        }
    }

    fn row(self: &Rc<Self>, e: &Entry, state: State, fg: gtk::gdk::RGBA, home: &std::path::Path) -> gtk::Box {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
        col.add_css_class("sill-agent");

        // line 1: icon, name, version · action
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_box.set_valign(gtk::Align::Center);
        icon_box.set_size_request(16, 16);
        let icon_path = registry::icon_cache_dir(home).join(format!("{}.svg", e.id));
        if icon_path.is_file() {
            icon_box.append(&icons::from_file(&icon_path, fg, 16));
        } else {
            icon_box.append(&icons::named(icons::PLACEHOLDER, fg, 16));
            self.queue_icon(e);
        }
        top.append(&icon_box);
        let name = gtk::Label::new(Some(&e.name));
        name.add_css_class("sill-agent-name");
        name.set_xalign(0.0);
        top.append(&name);
        let ver = gtk::Label::new(Some(&format!("v{}", e.version)));
        ver.add_css_class("dim-label");
        ver.add_css_class("sill-agent-version");
        top.append(&ver);
        if !registry::is_tested(&e.id) {
            let u = gtk::Label::new(Some("untested"));
            u.add_css_class("dim-label");
            u.add_css_class("sill-agent-version");
            u.set_tooltip_text(Some("Not yet tried inside sill's jail: install, login and a prompt may need work"));
            top.append(&u);
        }
        if e.trust == Trust::Live {
            let u = gtk::Label::new(Some("unverified"));
            u.add_css_class("dim-label");
            u.add_css_class("sill-agent-version");
            u.set_tooltip_text(Some(&format!(
                "From the live index only, not in sill's reviewed snapshot: an install takes {} as the index names it (exact version, no lockfile) and asks you to confirm",
                e.install_spec()
            )));
            top.append(&u);
        } else if let Some(n) = &e.newer {
            let u = gtk::Label::new(Some(&format!("v{} in the live index", n.version)));
            u.add_css_class("dim-label");
            u.add_css_class("sill-agent-version");
            u.set_tooltip_text(Some(&format!("Newer than sill's snapshot (v{}), unverified: installing it takes {} as the index names it and asks you to confirm", e.version, n.install_spec())));
            top.append(&u);
        }
        if !e.description.is_empty() {
            col.set_tooltip_text(Some(&e.description));
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        top.append(&spacer);
        // id and links sit before the action, quiet
        let id = gtk::Label::new(Some(&e.id));
        id.add_css_class("sill-agent-id");
        id.set_valign(gtk::Align::Center);
        top.append(&id);
        let bottom = top.clone();
        for (url, tip) in [(e.repository.as_deref(), "Source"), (e.website.as_deref(), "Website")] {
            let Some(url) = url else { continue };
            let b = gtk::Button::new();
            b.set_child(Some(&icons::named(if tip == "Source" { "sill-code" } else { "sill-link" }, fg, 13)));
            b.add_css_class("flat");
            b.add_css_class("sill-agent-link");
            b.set_tooltip_text(Some(&format!("{tip}: {url}")));
            b.set_valign(gtk::Align::Center);
            let url = url.to_string();
            b.connect_clicked(move |b| {
                let win = b.root().and_then(|r| r.downcast::<gtk::Window>().ok());
                gtk::UriLauncher::new(&url).launch(win.as_ref(), None::<&gtk::gio::Cancellable>, |_| {});
            });
            bottom.append(&b);
        }
        let state_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        state_box.set_valign(gtk::Align::Center);
        state_box.set_margin_start(6);
        top.append(&state_box);
        col.append(&top);

        // installer output (hidden until there is some)
        let log = gtk::Label::new(None);
        log.add_css_class("sill-agent-log");
        log.set_xalign(0.0);
        log.set_wrap(true);
        log.set_selectable(true);
        log.set_visible(false);
        col.append(&log);
        // inline settings, opened by the gear
        let editor = gtk::Box::new(gtk::Orientation::Vertical, 0);
        editor.set_visible(false);
        col.append(&editor);

        self.rows.borrow_mut().insert(e.id.clone(), Row { state_box: state_box.clone(), log: log.clone(), icon_box, editor });
        self.fill_state(e, state, &state_box);
        if let Some(lines) = self.logs.borrow().get(&e.id)
            && let Some(last) = lines.last()
        {
            log.set_text(last);
            log.set_visible(true);
        }
        col
    }

    /// The right-hand side of a row: one clear action, the rest quiet.
    fn fill_state(self: &Rc<Self>, e: &Entry, state: State, state_box: &gtk::Box) {
        while let Some(c) = state_box.first_child() {
            state_box.remove(&c);
        }
        if self.installing.borrow().contains(&e.id) {
            let sp = super::spinner::Spinner::new(); // not GtkSpinner: see spinner.rs
            sp.set_spinning(true);
            state_box.append(&sp.area);
            let l = gtk::Label::new(Some("installing…"));
            l.add_css_class("dim-label");
            state_box.append(&l);
            return;
        }
        if !matches!(state, State::Absent) {
            state_box.append(&self.gear(e));
            if let Some(b) = self.forget_button(e) {
                state_box.append(&b);
            }
        }
        match state {
            State::Managed(version, trust) => {
                // the snapshot's version is the update, unless the live version is what is installed
                let update = version != e.version && e.newer.as_ref().is_none_or(|n| n.version != version);
                let l = gtk::Label::new(Some(if update { "update available" } else { "installed" }));
                l.add_css_class("dim-label");
                l.set_tooltip_text(Some(&match trust {
                    Trust::Snapshot => format!("installed by sill from its snapshot: v{version}"),
                    Trust::Live => format!("installed by sill from the live index (unverified): v{version}"),
                }));
                state_box.append(&l);
                if update {
                    state_box.append(&self.action_button(e, &format!("Update to v{}", e.version), "sill-install"));
                } else if let Some(n) = &e.newer
                    && n.version != version
                {
                    state_box.append(&self.action_button(n, &format!("Update to v{} (unverified)", n.version), "sill-install"));
                }
                let rm = super::icons::glyph_button("sill-trash");
                rm.add_css_class("flat");
                rm.add_css_class("sill-menu-trash");
                rm.set_tooltip_text(Some("Remove this install"));
                let p = self.clone();
                let e2 = e.clone();
                rm.connect_clicked(move |_| p.uninstall(&e2));
                state_box.append(&rm);
            }
            State::System(path) => {
                let l = gtk::Label::new(Some("installed"));
                l.add_css_class("dim-label");
                l.set_tooltip_text(Some(&format!("found on this machine: {}", path.display())));
                state_box.append(&l);
            }
            State::Absent => match e.how() {
                Some(_) => state_box.append(&self.action_button(e, "Install", "sill-install")),
                None => {
                    let l = gtk::Label::new(Some("no Linux build"));
                    l.add_css_class("dim-label");
                    state_box.append(&l);
                }
            },
        }
    }

    /// The gear: shows or hides this agent's settings under the row (command, environment with API
    /// keys, proxy domains, offered or not).
    fn gear(self: &Rc<Self>, e: &Entry) -> gtk::Button {
        let fg = icons::fg_for(&self.list, &theme::current());
        let b = gtk::Button::new();
        b.set_child(Some(&icons::named("sill-settings", fg, 13)));
        b.add_css_class("flat");
        b.add_css_class("sill-agent-link");
        b.set_tooltip_text(Some("Settings: command, environment (API keys), domains"));
        let p = self.clone();
        let e2 = e.clone();
        b.connect_clicked(move |_| {
            let rows = p.rows.borrow();
            let Some(r) = rows.get(&e2.id) else { return };
            if r.editor.is_visible() {
                r.editor.set_visible(false);
                return;
            }
            let home = session::home().unwrap_or_default();
            let id = registry::builtin_for(&e2).map(str::to_string).unwrap_or_else(|| e2.id.clone());
            while let Some(c) = r.editor.first_child() {
                r.editor.remove(&c);
            }
            if let Some(agent) = sill_core::agents::load(&home).remove(&id) {
                r.editor.append(&settings::agent_editor(home, agent));
                r.editor.set_visible(true);
            }
        });
        b
    }

    /// One click forgets the agent's own memory (what it noted about the person and their projects
    /// in its sill-owned home); only when there is some. Sill's memory of the sessions stays.
    fn forget_button(self: &Rc<Self>, e: &Entry) -> Option<gtk::Button> {
        let home = session::home().ok()?;
        let id = registry::builtin_for(e).map(str::to_string).unwrap_or_else(|| e.id.clone());
        let agent = sill_core::agents::load(&home).remove(&id)?;
        let dirs = sill_core::agents::memory_dirs(&agent, &home);
        if dirs.is_empty() {
            return None;
        }
        let files: usize = dirs.iter().map(|d| sill_core::agents::count_files(d)).sum();
        let fg = icons::fg_for(&self.list, &theme::current());
        let b = gtk::Button::new();
        b.set_child(Some(&icons::named("sill-brain", fg, 13)));
        b.add_css_class("flat");
        b.add_css_class("sill-agent-link");
        b.set_tooltip_text(Some(&format!(
            "Forget {}'s own memory: {files} file{} under {} ({}). Sill's memory of the sessions stays.",
            agent.name,
            if files == 1 { "" } else { "s" },
            sill_core::session::agent_home(&home, &agent.id).display(),
            dirs.iter().filter_map(|d| d.strip_prefix(sill_core::session::agent_home(&home, &agent.id)).ok()).map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
        )));
        let p = self.clone();
        let e2 = e.clone();
        b.connect_clicked(move |_| {
            let msg = match sill_core::agents::forget_memory(&agent, &home) {
                Ok(n) => format!("forgot {n} memory file{}", if n == 1 { "" } else { "s" }),
                Err(err) => format!("forget failed: {err:#}"),
            };
            p.logs.borrow_mut().insert(e2.id.clone(), vec![msg]);
            p.refresh_row(&e2, &home);
        });
        Some(b)
    }

    fn action_button(self: &Rc<Self>, e: &Entry, label: &str, class: &str) -> gtk::Button {
        let b = gtk::Button::with_label(label);
        b.add_css_class(class);
        b.set_tooltip_text(Some(&match e.how() {
            Some("npx") => format!("npm package {} into ~/.local/state/sill/tools (no install scripts run)", e.package_name().unwrap_or_default()),
            Some("uvx") => format!("Python package {} with uv into ~/.local/state/sill/tools", e.package_name().unwrap_or_default()),
            _ => "download the Linux binary into ~/.local/state/sill/tools".to_string(),
        }));
        let p = self.clone();
        let e2 = e.clone();
        b.connect_clicked(move |b| {
            // a live-index entry: nothing reviewed vouches for it, so the first click only says
            // what would be installed and the second does it (security.md S13)
            if e2.trust == Trust::Live && p.confirm.borrow().as_deref() != Some(e2.id.as_str()) {
                *p.confirm.borrow_mut() = Some(e2.id.clone());
                b.set_label("Confirm");
                let msg = format!(
                    "{} v{} is not in sill's reviewed snapshot. Install takes {} exactly as the live index names it (no lockfile, dependencies resolved now). Click again to confirm.",
                    e2.name,
                    e2.version,
                    e2.install_spec()
                );
                p.logs.borrow_mut().insert(e2.id.clone(), vec![msg.clone()]);
                if let Some(r) = p.rows.borrow().get(&e2.id) {
                    r.log.set_text(&msg);
                    r.log.set_visible(true);
                }
                return;
            }
            p.confirm.borrow_mut().take();
            p.install(&e2);
        });
        b
    }

    fn install(self: &Rc<Self>, e: &Entry) {
        if !self.installing.borrow_mut().insert(e.id.clone()) {
            return;
        }
        self.logs.borrow_mut().insert(e.id.clone(), Vec::new());
        let home = session::home().unwrap_or_default();
        self.refresh_row(e, &home);
        let tx = self.tx.clone();
        let entry = e.clone();
        std::thread::Builder::new()
            .name(format!("sill-install-{}", e.id))
            .spawn(move || {
                let id = entry.id.clone();
                let progress = tx.clone();
                let mut log = |line: String| {
                    let _ = progress.send_blocking(Progress::Line(id.clone(), line));
                };
                let r = registry::install(&home, &entry, &mut log)
                    .map(|a| format!("installed {} — it is in the + menu now", a.name))
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send_blocking(Progress::Done(entry.id.clone(), r));
            })
            .ok();
    }

    fn uninstall(self: &Rc<Self>, e: &Entry) {
        let home = session::home().unwrap_or_default();
        let msg = match registry::uninstall(&home, e) {
            Ok(()) => "removed".to_string(),
            Err(err) => format!("remove failed: {err:#}"),
        };
        self.logs.borrow_mut().insert(e.id.clone(), vec![msg]);
        self.refresh_row(e, &home);
    }

    fn refresh_row(self: &Rc<Self>, e: &Entry, home: &std::path::Path) {
        let state = self.state_of(e, home);
        let rows = self.rows.borrow();
        if let Some(r) = rows.get(&e.id) {
            self.fill_state(e, state, &r.state_box);
            if let Some(lines) = self.logs.borrow().get(&e.id)
                && let Some(last) = lines.last()
            {
                r.log.set_text(last);
                r.log.set_visible(true);
            }
        }
    }

    fn queue_icon(self: &Rc<Self>, e: &Entry) {
        if e.icon.is_none() || !self.icons_queued.borrow_mut().insert(e.id.clone()) {
            return;
        }
        let tx = self.tx.clone();
        let entry = e.clone();
        let home = session::home().unwrap_or_default();
        std::thread::spawn(move || {
            if let Some(p) = registry::fetch_icon(&home, &entry) {
                let _ = tx.send_blocking(Progress::Icon(entry.id.clone(), p));
            }
        });
    }

    fn progress(self: &Rc<Self>, msg: Progress) {
        match msg {
            Progress::Line(id, line) => {
                let mut logs = self.logs.borrow_mut();
                logs.entry(id.clone()).or_default().push(line.clone());
                if let Some(r) = self.rows.borrow().get(&id) {
                    r.log.set_text(&line);
                    r.log.set_visible(true);
                }
            }
            Progress::Done(id, result) if id.is_empty() => {
                // the registry fetch
                self.fetching.set(false);
                self.refresh_btn.set_sensitive(true);
                match result {
                    Ok(_) => self.refresh(),
                    Err(e) => self.set_note(&format!("registry: {e}")),
                }
            }
            Progress::Done(id, result) => {
                self.installing.borrow_mut().remove(&id);
                let (text, failed) = match result {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                self.logs.borrow_mut().entry(id.clone()).or_default().push(text.clone());
                let home = session::home().unwrap_or_default();
                let entry = self.entries.borrow().iter().find(|e| e.id == id).cloned();
                if let Some(e) = entry {
                    self.refresh_row(&e, &home);
                }
                if let Some(r) = self.rows.borrow().get(&id) {
                    r.log.set_text(&text);
                    r.log.set_visible(true);
                    if failed {
                        r.log.add_css_class("sill-changed");
                    } else {
                        r.log.remove_css_class("sill-changed");
                    }
                }
            }
            Progress::Icon(id, path) => {
                if let Some(r) = self.rows.borrow().get(&id) {
                    while let Some(c) = r.icon_box.first_child() {
                        r.icon_box.remove(&c);
                    }
                    let fg = icons::fg_for(&self.list, &theme::current());
                    r.icon_box.append(&icons::from_file(&path, fg, 16));
                }
            }
        }
    }
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} days ago", s / 86_400)
    }
}
