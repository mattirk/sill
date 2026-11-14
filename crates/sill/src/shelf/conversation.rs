//! The conversation view (SPEC §3.1): the ACP stream as a column of blocks — user prompts, agent
//! messages (streamed markdown), thoughts (collapsed), tool calls with status, permission requests
//! with buttons.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::oneshot;
use gtk4 as gtk;
use gtk::prelude::*;
use sill_core::acp::{Event, PermissionOption};

use super::markdown;

pub struct Conversation {
    pub root: gtk::ScrolledWindow,
    column: gtk::Box,
    /// The agent message being streamed, with its accumulated markdown.
    current: RefCell<Option<(gtk::TextView, String)>>,
    current_thought: RefCell<Option<(gtk::TextView, String)>>,
    tool_rows: RefCell<HashMap<String, (gtk::Label, gtk::Label)>>,
    status: gtk::Label,
}

impl Conversation {
    pub fn new(status: gtk::Label) -> Rc<Self> {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.set_margin_top(8);
        column.set_margin_bottom(8);
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&column)
            .build();
        Rc::new(Self {
            root,
            column,
            current: RefCell::new(None),
            current_thought: RefCell::new(None),
            tool_rows: RefCell::new(HashMap::new()),
            status,
        })
    }

    fn scroll_to_end(&self) {
        let adj = self.root.vadjustment();
        // after layout: defer one idle so the new height is known
        let adj2 = adj.clone();
        gtk::glib::idle_add_local_once(move || adj2.set_value(adj2.upper() - adj2.page_size()));
    }

    fn text_view(css: &str) -> gtk::TextView {
        let tv = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .right_margin(8)
            .top_margin(6)
            .bottom_margin(6)
            .build();
        tv.add_css_class(css);
        markdown::install_tags(&tv.buffer());
        tv
    }

    pub fn add_user(&self, text: &str) {
        self.finish_current();
        let l = gtk::Label::new(Some(text));
        l.add_css_class("sill-user");
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_selectable(true);
        l.set_xalign(0.0);
        l.set_halign(gtk::Align::End);
        l.set_max_width_chars(70);
        self.column.append(&l);
        self.scroll_to_end();
    }

    fn finish_current(&self) {
        self.current.borrow_mut().take();
        self.current_thought.borrow_mut().take();
    }

    fn append_markdown(&self, slot: &RefCell<Option<(gtk::TextView, String)>>, css: &str, chunk: &str) {
        let mut cur = slot.borrow_mut();
        if cur.is_none() {
            let tv = Self::text_view(css);
            self.column.append(&tv);
            *cur = Some((tv, String::new()));
        }
        let (tv, md) = cur.as_mut().unwrap();
        md.push_str(chunk);
        markdown::render(&tv.buffer(), md);
        drop(cur);
        self.scroll_to_end();
    }

    pub fn add_tool_call(&self, id: String, title: &str, status: &str) {
        self.finish_current();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("sill-tool");
        let icon = gtk::Label::new(Some("⚙"));
        let title_l = gtk::Label::new(Some(title));
        title_l.set_xalign(0.0);
        title_l.set_wrap(true);
        title_l.set_hexpand(true);
        let status_l = gtk::Label::new(Some(status));
        status_l.add_css_class("dim-label");
        row.append(&icon);
        row.append(&title_l);
        row.append(&status_l);
        self.column.append(&row);
        self.tool_rows.borrow_mut().insert(id, (title_l, status_l));
        self.scroll_to_end();
    }

    pub fn update_tool_call(&self, id: &str, status: Option<&str>, title: Option<&str>) {
        if let Some((title_l, status_l)) = self.tool_rows.borrow().get(id) {
            if let Some(s) = status {
                status_l.set_text(s);
            }
            if let Some(t) = title {
                title_l.set_text(t);
            }
        }
    }

    pub fn add_permission(&self, title: &str, options: Vec<PermissionOption>, reply: oneshot::Sender<Option<String>>) {
        self.finish_current();
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
        frame.add_css_class("sill-permission");
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_wrap(true);
        frame.append(&label);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let reply = Rc::new(RefCell::new(Some(reply)));
        for opt in options {
            let b = gtk::Button::with_label(&opt.name);
            match opt.kind {
                sill_core::acp::PermissionOptionKind::AllowOnce | sill_core::acp::PermissionOptionKind::AllowAlways => {
                    b.add_css_class("suggested-action")
                }
                _ => b.add_css_class("destructive-action"),
            }
            let reply = reply.clone();
            let buttons2 = buttons.clone();
            let label2 = label.clone();
            let id = opt.id.clone();
            let name = opt.name.clone();
            b.connect_clicked(move |_| {
                if let Some(tx) = reply.borrow_mut().take() {
                    let _ = tx.send(Some(id.clone()));
                    buttons2.set_sensitive(false);
                    label2.set_text(&format!("{} → {name}", label2.text()));
                }
            });
            buttons.append(&b);
        }
        frame.append(&buttons);
        self.column.append(&frame);
        self.scroll_to_end();
    }

    pub fn add_note(&self, text: &str) {
        self.finish_current();
        let l = gtk::Label::new(Some(text));
        l.add_css_class("dim-label");
        l.set_xalign(0.0);
        l.set_wrap(true);
        self.column.append(&l);
        self.scroll_to_end();
    }

    /// Apply one session event.
    pub fn handle(&self, ev: Event) {
        match ev {
            Event::Ready(id) => self.status.set_text(&format!("ready · session {}", &id[..id.len().min(8)])),
            Event::TurnStarted => self.status.set_text("working…"),
            Event::Text(t) => self.append_markdown(&self.current, "sill-agent", &t),
            Event::Thought(t) => self.append_markdown(&self.current_thought, "sill-thought", &t),
            Event::ToolCall { id, title, status, .. } => self.add_tool_call(id, &title, &status),
            Event::ToolCallUpdate { id, status, title } => self.update_tool_call(&id, status.as_deref(), title.as_deref()),
            Event::PermissionRequest { title, options, reply } => {
                self.status.set_text("waiting for your answer");
                self.add_permission(&title, options, reply)
            }
            Event::Other(_) => {}
            Event::Stopped(reason) => {
                self.finish_current();
                self.status.set_text(&format!("idle · last turn: {reason:?}"));
            }
            Event::Ended(err) => {
                self.finish_current();
                match err {
                    Some(e) => {
                        self.add_note(&format!("session ended: {e}"));
                        self.status.set_text("session ended");
                    }
                    None => self.status.set_text("session closed"),
                }
            }
        }
    }
}

pub const CSS: &str = r#"
.sill-user { background: alpha(@accent_bg_color, 0.18); border-radius: 10px; padding: 6px 10px; }
.sill-agent { background: alpha(@view_fg_color, 0.05); border-radius: 10px; }
.sill-thought { background: transparent; color: alpha(@view_fg_color, 0.6); font-size: 90%; }
.sill-tool { background: alpha(@view_fg_color, 0.07); border-radius: 8px; padding: 6px 10px; }
.sill-permission { background: alpha(@warning_bg_color, 0.2); border-radius: 8px; padding: 8px 10px; }
.sill-prompt { border-radius: 8px; }
"#;
