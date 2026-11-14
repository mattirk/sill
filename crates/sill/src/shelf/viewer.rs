//! The file viewer of a session (SPEC §3.1 viewer): opens on the left, in place of the conversation —
//! from a file in the folder tree, a pinned file, or an image in an agent message. Text with syntax
//! highlighting, Markdown rendered (with a source toggle), images fitted to the view. Read-only:
//! editing is the Files page's job. Escape steps back through what was opened, then closes.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use gtk4 as gtk;
use sourceview5::prelude::*;

use super::{markdown, theme};

/// Images the viewer (and inline Markdown) show as pictures.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];
/// Larger files are not read at all.
const MAX_BYTES: u64 = 20_000_000;

pub fn is_image(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()))
}

pub fn is_markdown(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| matches!(e.as_str(), "md" | "markdown" | "mdown"))
}

pub struct Viewer {
    pub root: gtk::Box,
    title: gtk::Label,
    /// Markdown: show the source instead of the rendering.
    source: gtk::ToggleButton,
    stack: gtk::Stack,
    text: sourceview5::Buffer,
    md: gtk::Box,
    picture: gtk::Picture,
    info: gtk::Label,
    /// What was opened, the shown one last; Escape walks back through it.
    history: RefCell<Vec<PathBuf>>,
    weak: RefCell<Weak<Viewer>>,
    /// The close button (and Escape past the first entry) asks the owner to close the viewer.
    pub on_close: RefCell<Option<Box<dyn Fn()>>>,
}

impl Viewer {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("sill-viewer");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("sill-viewer-bar");
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        title.add_css_class("dim-label");
        let source = gtk::ToggleButton::with_label("Source");
        source.add_css_class("flat");
        source.add_css_class("sill-viewer-btn");
        source.set_tooltip_text(Some("Show the Markdown source"));
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("sill-viewer-btn");
        close.set_tooltip_text(Some("Close (Escape steps back)"));
        bar.append(&title);
        bar.append(&source);
        bar.append(&close);
        root.append(&bar);

        // text: read-only source view in the theme's scheme
        let text = sourceview5::Buffer::new(None);
        text.set_highlight_matching_brackets(false);
        let text_view = sourceview5::View::with_buffer(&text);
        text_view.set_editable(false);
        text_view.set_cursor_visible(false);
        text_view.set_monospace(true);
        text_view.set_show_line_numbers(true);
        text_view.set_wrap_mode(gtk::WrapMode::WordChar);
        text_view.set_left_margin(6);
        text_view.set_top_margin(6);
        text_view.add_css_class("sill-editor");
        let text_scroll = gtk::ScrolledWindow::builder().child(&text_view).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

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

        let stack = gtk::Stack::new();
        stack.add_named(&text_scroll, Some("text"));
        stack.add_named(&md_scroll, Some("markdown"));
        stack.add_named(&picture, Some("image"));
        stack.add_named(&info, Some("info"));
        stack.set_vexpand(true);
        root.append(&stack);

        let v = Rc::new(Self {
            root,
            title,
            source: source.clone(),
            stack,
            text,
            md,
            picture,
            info,
            history: RefCell::new(Vec::new()),
            weak: RefCell::new(Weak::new()),
            on_close: RefCell::new(None),
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
            source.connect_toggled(move |_| {
                if let Some(v) = weak.upgrade()
                    && let Some(p) = v.history.borrow().last().cloned()
                {
                    v.show(&p);
                }
            });
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
        let mut h = self.history.borrow_mut();
        if h.last().map(|l| l.as_path()) != Some(path) {
            h.push(path.to_path_buf());
        }
        drop(h);
        self.source.set_active(false);
        self.show(path);
    }

    /// One step back. False when nothing is left to show (the owner closes the viewer).
    pub fn back(&self) -> bool {
        let mut h = self.history.borrow_mut();
        h.pop();
        let prev = h.last().cloned();
        drop(h);
        match prev {
            Some(p) => {
                self.source.set_active(false);
                self.show(&p);
                true
            }
            None => false,
        }
    }

    /// Keyboard focus into the viewer (its close button: always there, never editable).
    pub fn focus(&self) {
        if let Some(bar) = self.root.first_child() {
            if let Some(close) = bar.last_child() {
                close.grab_focus();
            }
        }
    }

    /// Forget everything and ask the owner to close.
    pub fn close(&self) {
        self.history.borrow_mut().clear();
        if let Some(cb) = self.on_close.borrow().as_ref() {
            cb();
        }
    }

    fn show(&self, path: &Path) {
        let home = std::env::var("HOME").unwrap_or_default();
        self.title.set_text(&path.display().to_string().replacen(&home, "~", 1));
        self.title.set_tooltip_text(Some(&path.display().to_string()));
        self.source.set_visible(is_markdown(path));
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !path.is_file() {
            return self.say(&format!("{} is not a file (or is gone)", path.display()));
        }
        if size > MAX_BYTES {
            return self.say(&format!("{} MB: too large to show", size / 1_000_000));
        }
        if is_image(path) {
            self.picture.set_filename(Some(path));
            self.stack.set_visible_child_name("image");
            return;
        }
        let bytes = std::fs::read(path).unwrap_or_default();
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return self.say(&format!("binary file, {} KB", size.div_ceil(1000)));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if is_markdown(path) && !self.source.is_active() {
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let weak = self.weak.borrow().clone();
            markdown::set_context(
                &self.md,
                Rc::new(markdown::Ctx {
                    // images in the file: relative to the file, as any Markdown viewer does
                    resolve: Box::new(move |src| {
                        let p = Path::new(src);
                        let p = if p.is_absolute() { p.to_path_buf() } else { dir.join(p) };
                        p.is_file().then_some(p)
                    }),
                    open: Some(Box::new(move |p| {
                        if let Some(v) = weak.upgrade() {
                            v.open(p);
                        }
                    })),
                }),
            );
            self.stack.set_visible_child_name("markdown");
            markdown::render(&self.md, "sill-agent", &text);
            return;
        }
        let lang = sourceview5::LanguageManager::default().guess_language(Some(path), None);
        self.text.set_language(lang.as_ref());
        self.text.set_highlight_syntax(true);
        self.text.set_text(&text);
        self.stack.set_visible_child_name("text");
    }

    fn say(&self, what: &str) {
        self.info.set_text(what);
        self.stack.set_visible_child_name("info");
    }
}
