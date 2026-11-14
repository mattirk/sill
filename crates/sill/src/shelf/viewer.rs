//! The file viewer (SPEC §3.1): opens over the main pane, at the shelf's own width — from a file in
//! the tree, a pinned file's chip, or an image in an agent message. Text with syntax highlighting,
//! Markdown rendered by the conversation's renderer, images fitted to the view; an *Edit* toggle makes
//! a text file editable in place (Ctrl+S writes the host file). A path with a pending session change
//! shows its diff instead: keep or revert it in Changes first. Escape steps back through what was
//! opened, then closes.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use sill_core::ledger::Change;
use sourceview5::prelude::*;

use super::{markdown, theme};

/// Images the viewer (and inline Markdown) show as pictures.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];
/// Larger files are not read at all.
const MAX_BYTES: u64 = 20_000_000;
/// GtkSourceView can't cope with minified/one-line files (highlighting + wrapping of a megabyte-long
/// line): those are shown as a truncated, plain, read-only preview instead.
const PREVIEW: usize = 200_000;

pub fn is_image(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

pub fn is_markdown(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| matches!(e.as_str(), "md" | "markdown" | "mdown"))
}

/// Reads the last `n` lines of a log; blocking, run off the main thread (a daemon call or files).
pub type LogReader = std::sync::Arc<dyn Fn(usize) -> String + Send + Sync>;

pub struct Viewer {
    pub root: gtk::Box,
    title: gtk::Label,
    /// Make the shown text editable (a Markdown file: its source).
    edit: gtk::ToggleButton,
    stack: gtk::Stack,
    text: sourceview5::Buffer,
    text_view: sourceview5::View,
    md: gtk::Box,
    picture: gtk::Picture,
    info: gtk::Label,
    message: gtk::Label,
    /// Every session's pending changes (shared with the tree): a pending path shows its diff.
    changes: Rc<RefCell<Vec<(String, Change)>>>,
    /// The shown file can be edited (a writable text file, not a diff or a preview).
    editable: Cell<bool>,
    dirty: Cell<bool>,
    /// Escape with unsaved edits asks once; the next Escape discards.
    discard_armed: Cell<bool>,
    /// What was opened, the shown one last; Escape walks back through it.
    history: RefCell<Vec<PathBuf>>,
    weak: RefCell<Weak<Viewer>>,
    /// The close button (and Escape past the first entry) asks the owner to close the viewer.
    pub on_close: RefCell<Option<Box<dyn Fn()>>>,
    /// A file was written (Ctrl+S): the owner refreshes what shows its state (tree badges, chips).
    pub on_saved: RefCell<Option<Box<dyn Fn(&Path)>>>,
    /// A session's log being shown: its history key (`log: <title>`) and where it comes from.
    log: RefCell<Option<(PathBuf, LogReader)>>,
    /// The follow timer while a log is shown.
    log_tick: RefCell<Option<glib::SourceId>>,
    /// A page of the shelf's own (a session's summary and notes, docs/memory.md §2.5) under its
    /// history key (`page: <title>`), and the slot it is shown in.
    page: RefCell<Option<(PathBuf, gtk::Widget)>>,
    page_slot: gtk::Box,
}

impl Viewer {
    pub fn new(changes: Rc<RefCell<Vec<(String, Change)>>>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sill-viewer");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("sill-viewer-bar");
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        title.add_css_class("dim-label");
        let edit = gtk::ToggleButton::with_label("Edit");
        edit.add_css_class("flat");
        edit.add_css_class("sill-viewer-btn");
        edit.set_tooltip_text(Some("Edit the file in place (Ctrl+S saves to the host)"));
        let close = super::icons::glyph_button("sill-x");
        close.add_css_class("flat");
        close.add_css_class("sill-viewer-btn");
        close.set_tooltip_text(Some("Close (Escape steps back)"));
        bar.append(&title);
        bar.append(&edit);
        bar.append(&close);
        root.append(&bar);

        // text: a source view in the theme's scheme, read-only until Edit
        let text = sourceview5::Buffer::new(None);
        text.set_highlight_matching_brackets(false);
        let text_view = sourceview5::View::with_buffer(&text);
        text_view.set_editable(false);
        text_view.set_cursor_visible(false);
        text_view.set_monospace(true);
        text_view.set_show_line_numbers(true);
        text_view.set_tab_width(4);
        text_view.set_wrap_mode(gtk::WrapMode::None); // code never wraps: it scrolls sideways
        text_view.set_left_margin(6);
        text_view.set_top_margin(6);
        text_view.add_css_class("sill-editor");
        let text_scroll = gtk::ScrolledWindow::builder().child(&text_view).vexpand(true).hscrollbar_policy(gtk::PolicyType::Automatic).build();

        // markdown: the conversation's renderer, in a column of its own
        let md = gtk::Box::new(gtk::Orientation::Vertical, 0);
        md.add_css_class("sill-msg");
        md.set_margin_start(12);
        md.set_margin_end(12);
        md.set_margin_top(6);
        md.set_margin_bottom(12);
        let md_scroll = gtk::ScrolledWindow::builder().child(&md).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

        // image: fitted, never enlarged
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::ScaleDown);
        picture.set_vexpand(true);
        picture.set_hexpand(true);

        let info = gtk::Label::new(None);
        info.add_css_class("dim-label");
        info.set_wrap(true);
        info.set_wrap_mode(gtk::pango::WrapMode::WordChar);

        // a page of the shelf's own, in a column that scrolls
        let page_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let page_scroll = gtk::ScrolledWindow::builder().child(&page_slot).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

        let stack = gtk::Stack::new();
        stack.add_named(&page_scroll, Some("page"));
        stack.add_named(&text_scroll, Some("text"));
        stack.add_named(&md_scroll, Some("markdown"));
        stack.add_named(&picture, Some("image"));
        stack.add_named(&info, Some("info"));
        stack.set_vexpand(true);
        root.append(&stack);
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.add_css_class("dim-label");
        message.add_css_class("sill-menu-caption");
        message.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        message.set_visible(false);
        root.append(&message);

        // style schemes generated from the palettes, then follow the theme
        if let Some(dir) = theme::write_source_schemes() {
            sourceview5::StyleSchemeManager::default().append_search_path(&dir.to_string_lossy());
        }
        let v = Rc::new(Self {
            root,
            title,
            edit: edit.clone(),
            stack,
            text: text.clone(),
            text_view: text_view.clone(),
            md,
            picture,
            info,
            message,
            changes,
            editable: Cell::new(false),
            dirty: Cell::new(false),
            discard_armed: Cell::new(false),
            history: RefCell::new(Vec::new()),
            weak: RefCell::new(Weak::new()),
            on_close: RefCell::new(None),
            on_saved: RefCell::new(None),
            log: RefCell::new(None),
            log_tick: RefCell::new(None),
            page: RefCell::new(None),
            page_slot,
        });
        *v.weak.borrow_mut() = Rc::downgrade(&v);
        {
            let weak = Rc::downgrade(&v);
            close.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.close();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            edit.connect_toggled(move |t| {
                let Some(v) = weak.upgrade() else { return };
                let on = t.is_active();
                v.text_view.set_editable(on && v.editable.get());
                v.text_view.set_cursor_visible(on);
                v.text_view.set_highlight_current_line(on);
                let Some(p) = v.history.borrow().last().cloned() else { return };
                if is_markdown(&p) {
                    if on {
                        v.stack.set_visible_child_name("text");
                        v.text_view.grab_focus();
                    } else {
                        v.render_markdown(&p); // back from editing: show what the editor holds
                        v.stack.set_visible_child_name("markdown");
                    }
                } else if on {
                    v.text_view.grab_focus();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            text.connect_changed(move |_| {
                if let Some(v) = weak.upgrade()
                    && v.edit.is_active()
                    && v.editable.get()
                    && !v.dirty.get()
                {
                    v.dirty.set(true);
                    v.update_title();
                }
            });
        }
        {
            let weak = Rc::downgrade(&v);
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, state| {
                if key == gdk::Key::s
                    && state.contains(gdk::ModifierType::CONTROL_MASK)
                    && let Some(v) = weak.upgrade()
                {
                    v.save();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            text_view.add_controller(keys);
        }
        {
            let weak = Rc::downgrade(&v);
            theme::on_change(move |t| {
                if let Some(v) = weak.upgrade() {
                    v.apply_scheme(t);
                }
            });
        }
        v.apply_scheme(&theme::current());
        v
    }

    fn apply_scheme(&self, t: &theme::Theme) {
        if let Some(s) = sourceview5::StyleSchemeManager::default().scheme(&theme::source_scheme(t)) {
            self.text.set_style_scheme(Some(&s));
        }
    }

    /// Open a file on top of what is shown (Escape comes back to it).
    pub fn open(&self, path: &Path) {
        self.drop_edits();
        let mut h = self.history.borrow_mut();
        if h.last().map(|l| l.as_path()) != Some(path) {
            h.push(path.to_path_buf());
        }
        drop(h);
        self.show(path);
    }

    /// Escape: with unsaved edits, ask once and discard on the second; else one step back, and when
    /// nothing is left to show, close.
    pub fn escape(&self) {
        if self.dirty.get() && !self.discard_armed.get() {
            self.discard_armed.set(true);
            self.say("unsaved changes: Ctrl+S saves, Escape again discards them");
            return;
        }
        self.drop_edits();
        let mut h = self.history.borrow_mut();
        h.pop();
        let prev = h.last().cloned();
        drop(h);
        match prev {
            Some(p) => self.show(&p),
            None => self.close(),
        }
    }

    /// Keyboard focus into the viewer (its close button: always there, never editable).
    pub fn focus(&self) {
        if self.edit.is_active() {
            self.text_view.grab_focus();
            return;
        }
        if let Some(bar) = self.root.first_child()
            && let Some(close) = bar.last_child()
        {
            close.grab_focus();
        }
    }

    /// A session's log on top of what is shown, followed while it is shown (Escape comes back).
    pub fn open_log(&self, title: &str, src: LogReader) {
        self.drop_edits();
        let key = PathBuf::from(format!("log: {title}"));
        *self.log.borrow_mut() = Some((key.clone(), src));
        let mut h = self.history.borrow_mut();
        if h.last() != Some(&key) {
            h.push(key.clone());
        }
        drop(h);
        self.show(&key);
    }

    /// The log in the text view: read off the main thread now and every 2 s while shown; the view
    /// stays at the end unless the person scrolled up.
    /// A page of the shelf's own on top of what is shown (Escape comes back): `widget` fills the
    /// viewer under `title`. One page at a time; a new one replaces the last.
    pub fn open_page(&self, title: &str, widget: &impl IsA<gtk::Widget>) {
        self.drop_edits();
        let key = PathBuf::from(format!("page: {title}"));
        while let Some(c) = self.page_slot.first_child() {
            self.page_slot.remove(&c);
        }
        self.page_slot.append(widget);
        *self.page.borrow_mut() = Some((key.clone(), widget.clone().upcast()));
        let mut h = self.history.borrow_mut();
        h.retain(|k| !k.to_string_lossy().starts_with("page: "));
        h.push(key.clone());
        drop(h);
        self.show(&key);
    }

    fn show_log(&self, src: LogReader) {
        self.text.set_language(None);
        self.text.set_highlight_syntax(false);
        self.stack.set_visible_child_name("text");
        self.say("following · Escape goes back");
        let weak = self.weak.borrow().clone();
        let s = src.clone();
        let id = glib::timeout_add_seconds_local(2, move || match weak.upgrade() {
            Some(v) if v.log_tick.borrow().is_some() => {
                v.refresh_log(s.clone());
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        });
        *self.log_tick.borrow_mut() = Some(id);
        self.refresh_log(src);
    }

    fn refresh_log(&self, src: LogReader) {
        let (tx, rx) = async_channel::bounded::<String>(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(src(2000));
        });
        let weak = self.weak.borrow().clone();
        glib::spawn_future_local(async move {
            if let Ok(text) = rx.recv().await
                && let Some(v) = weak.upgrade()
                && v.log_tick.borrow().is_some()
            {
                v.set_log_text(&text);
            }
        });
    }

    fn set_log_text(&self, text: &str) {
        if self.text.text(&self.text.start_iter(), &self.text.end_iter(), true) == text {
            return;
        }
        let at_end = self
            .text_view
            .parent()
            .and_downcast::<gtk::ScrolledWindow>()
            .map(|sw| {
                let a = sw.vadjustment();
                a.value() + a.page_size() >= a.upper() - 2.0
            })
            .unwrap_or(true);
        self.text.set_text(text);
        if at_end {
            let mark = self.text.create_mark(None, &self.text.end_iter(), false);
            self.text_view.scroll_to_mark(&mark, 0.0, true, 0.0, 1.0);
        }
    }

    fn stop_follow(&self) {
        if let Some(id) = self.log_tick.borrow_mut().take() {
            id.remove();
        }
    }

    /// Forget everything and ask the owner to close.
    pub fn close(&self) {
        self.stop_follow();
        *self.log.borrow_mut() = None;
        *self.page.borrow_mut() = None;
        while let Some(c) = self.page_slot.first_child() {
            self.page_slot.remove(&c);
        }
        self.drop_edits();
        self.history.borrow_mut().clear();
        if let Some(cb) = self.on_close.borrow().as_ref() {
            cb();
        }
    }

    fn drop_edits(&self) {
        if self.dirty.get()
            && let Some(p) = self.history.borrow().last()
        {
            self.say(&format!("unsaved changes in {} discarded", p.display()));
        }
        self.dirty.set(false);
        self.discard_armed.set(false);
        self.edit.set_active(false);
    }

    fn save(&self) {
        let Some(path) = self.history.borrow().last().cloned() else { return };
        if !self.editable.get() || !self.edit.is_active() {
            return;
        }
        let text = self.text.text(&self.text.start_iter(), &self.text.end_iter(), true).to_string();
        // whole or not at all, and never through a link the agent may have put at the path
        match super::remote::write(&path, text.as_bytes()) {
            Ok(()) => {
                self.dirty.set(false);
                self.discard_armed.set(false);
                self.update_title();
                self.say(&format!("saved {}", path.display()));
                if let Some(cb) = self.on_saved.borrow().as_ref() {
                    cb(&path);
                }
            }
            Err(e) => self.say(&format!("save failed: {e}")),
        }
    }

    fn update_title(&self) {
        let Some(path) = self.history.borrow().last().cloned() else { return };
        let home = super::session::home().unwrap_or_default().display().to_string();
        let shown = path.display().to_string().replacen(&home, "~", 1);
        self.title.set_text(&if self.dirty.get() { format!("{shown}  ● unsaved (Ctrl+S)") } else { shown });
        self.title.set_tooltip_text(Some(&path.display().to_string()));
    }

    /// Render the editor's text as Markdown (images relative to the file, links open in the viewer).
    fn render_markdown(&self, path: &Path) {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let weak = self.weak.borrow().clone();
        markdown::set_context(
            &self.md,
            Rc::new(markdown::Ctx {
                resolve: Box::new(move |src| {
                    let p = Path::new(src);
                    let p = if p.is_absolute() { p.to_path_buf() } else { dir.join(p) };
                    super::remote::image(&p)
                }),
                open: Some(Box::new(move |p| {
                    if let Some(v) = weak.upgrade() {
                        v.open(p);
                    }
                })),
            }),
        );
        let text = self.text.text(&self.text.start_iter(), &self.text.end_iter(), true);
        // in a repository, the blocks that differ from HEAD are tinted (git's view, SPEC §3.1)
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        if let Some(repo) = super::remote::repo_root(path)
            && let Ok(rel) = path.strip_prefix(&repo)
        {
            match super::remote::changed_lines(&repo, rel) {
                Some(r) if !r.is_empty() => {
                    self.say(&format!("{} hunk(s) differ from HEAD: tinted", r.len()));
                    ranges = r;
                }
                Some(_) => {}
                None => self.say("untracked in git: everything here is new"),
            }
        }
        markdown::render_marked(&self.md, "sill-agent", &text, &ranges);
    }

    fn show(&self, path: &Path) {
        self.say("");
        self.stop_follow();
        self.editable.set(false);
        self.edit.set_visible(false);
        self.text_view.set_wrap_mode(gtk::WrapMode::None);
        self.update_title();
        // a session's log under its history key, not a file
        let log = self.log.borrow().as_ref().filter(|(k, _)| k == path).map(|(_, s)| s.clone());
        if let Some(src) = log {
            return self.show_log(src);
        }
        if self.page.borrow().as_ref().is_some_and(|(k, _)| k == path) {
            self.stack.set_visible_child_name("page");
            return;
        }
        // a link under a pin's name is the agent's pointer anywhere on the host: not shown as the
        // pin's file (security.md S9); the target itself can be opened by its own name
        if super::remote::metadata(path).is_ok_and(|m| m.symlink) {
            let target = super::remote::read_link(path).map(|t| t.display().to_string()).unwrap_or_else(|_| "?".into());
            return self.say_only(&format!("{} is a link → {target}: not shown under this name; open the target itself if you mean it", path.display()));
        }
        if !super::remote::is_file(path) {
            return self.say_only(&format!("{} is not a file (or is gone)", path.display()));
        }
        // pending session change: the diff, not the host file (keep or revert it in Changes first)
        let pending = self.changes.borrow().iter().find(|(_, c)| c.path == path).cloned();
        if let Some((session, c)) = pending {
            // the daemon reads the session's copy (docs/daemon.md §7 M2): asked off the GTK thread
            self.text.set_language(sourceview5::LanguageManager::default().language("diff").as_ref());
            self.text.set_highlight_syntax(true);
            self.text.set_text("");
            self.stack.set_visible_child_name("text");
            self.say(&format!("pending change ({}) of a session: keep or revert it in Changes before editing", c.kind.letter()));
            let (tx, rx) = async_channel::bounded::<String>(1);
            let of = sill_core::wire::DiffOf::Change { session, path: c.path.clone() };
            std::thread::spawn(move || {
                let text = match super::client::call::<sill_proto::Text>(sill_proto::method::CHANGES_DIFF, of) {
                    Ok(t) => t.text,
                    Err(e) => format!("diff: {e:#}"),
                };
                let _ = tx.send_blocking(text);
            });
            let buf = self.text.clone();
            glib::spawn_future_local(async move {
                if let Ok(text) = rx.recv().await {
                    buf.set_text(&text);
                }
            });
            return;
        }
        let meta = super::remote::metadata(path);
        let size = meta.as_ref().map(|m| m.length).unwrap_or(0);
        if size > MAX_BYTES {
            return self.say_only(&format!("{} MB: too large to show", size / 1_000_000));
        }
        if is_image(path) {
            self.picture.set_filename(super::remote::image(path).as_deref());
            self.stack.set_visible_child_name("image");
            return;
        }
        let bytes = super::remote::read(path).unwrap_or_default();
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return self.say_only(&format!("binary file, {} KB", size.div_ceil(1000)));
        }
        let longest = bytes.split(|b| *b == b'\n').map(|l| l.len()).max().unwrap_or(0);
        if size as usize > 1_500_000 || longest > 20_000 {
            let cut = bytes.len().min(PREVIEW);
            self.text.set_language(None);
            self.text.set_highlight_syntax(false);
            self.text_view.set_wrap_mode(gtk::WrapMode::Char);
            self.text.set_text(&String::from_utf8_lossy(&bytes[..cut]));
            self.stack.set_visible_child_name("text");
            self.say(&format!("{} KB, longest line {} chars: first {} KB shown read-only", size / 1000, longest, PREVIEW / 1000));
            return;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let lang = sourceview5::LanguageManager::default().guess_language(Some(path), None);
        self.text.set_language(lang.as_ref());
        self.text.set_highlight_syntax(true);
        self.text.set_text(&text);
        self.editable.set(meta.map(|m| !m.readonly).unwrap_or(false));
        self.edit.set_visible(self.editable.get());
        self.dirty.set(false);
        if is_markdown(path) && !self.edit.is_active() {
            self.render_markdown(path);
            self.stack.set_visible_child_name("markdown");
        } else {
            self.stack.set_visible_child_name("text");
        }
    }

    fn say(&self, what: &str) {
        self.message.set_text(what);
        self.message.set_visible(!what.is_empty());
    }

    fn say_only(&self, what: &str) {
        self.info.set_text(what);
        self.stack.set_visible_child_name("info");
    }
}
