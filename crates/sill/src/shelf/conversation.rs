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
    /// Follow new content unless the person scrolled up.
    stick_to_bottom: std::cell::Cell<bool>,
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
        let conv = Rc::new(Self {
            root,
            column,
            current: RefCell::new(None),
            current_thought: RefCell::new(None),
            tool_rows: RefCell::new(HashMap::new()),
            status,
            stick_to_bottom: std::cell::Cell::new(true),
        });
        // The content height is only known after layout: follow the adjustment's upper bound instead of
        // guessing when to scroll. Scrolling up by hand releases it; reaching the bottom re-engages it.
        let adj = conv.root.vadjustment();
        {
            let c = conv.clone();
            adj.connect_upper_notify(move |adj| {
                if c.stick_to_bottom.get() {
                    adj.set_value(adj.upper() - adj.page_size());
                }
            });
        }
        {
            let c = conv.clone();
            adj.connect_value_changed(move |adj| {
                let at_bottom = adj.value() >= adj.upper() - adj.page_size() - 24.0;
                c.stick_to_bottom.set(at_bottom);
            });
        }
        conv
    }

    fn scroll_to_end(&self) {
        self.stick_to_bottom.set(true);
        let adj = self.root.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
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

    /// A complete agent message (stored transcript).
    pub fn add_agent_block(&self, md: &str) {
        self.finish_current();
        let tv = Self::text_view("sill-agent");
        markdown::render(&tv.buffer(), md);
        self.column.append(&tv);
        self.scroll_to_end();
    }

    pub fn add_thought_block(&self, md: &str) {
        self.finish_current();
        let tv = Self::text_view("sill-thought");
        markdown::render(&tv.buffer(), md);
        self.column.append(&tv);
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
            Event::Ready { session_id, resumed } => {
                self.status.set_text(&format!("{} · {}", if resumed { "resumed" } else { "ready" }, &session_id[..session_id.len().min(8)]))
            }
            Event::TurnStarted => self.status.set_text("working…"),
            Event::UserText(t) => self.add_user(&t),
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
/* flat and square: no bubbles, no rounded corners, hierarchy by tone and a thin left rule */
window, .background { border-radius: 0; }
tabbar tab, tabbar .tab { border-radius: 0; }
.boxed-list, .boxed-list row, .frame, scrolledwindow.frame { border-radius: 0; }
popover > contents { border-radius: 0; }
button { border-radius: 0; }
entry { border-radius: 0; }

.sill-user { background: alpha(@view_fg_color, 0.07); border-radius: 0; padding: 8px 12px; }
.sill-user text { background: transparent; }
.sill-agent { background: transparent; border-radius: 0; }
.sill-agent text { background: transparent; }
.sill-thought { background: transparent; color: alpha(@view_fg_color, 0.55); font-size: 90%; }
.sill-thought text { background: transparent; }
.sill-tool { background: alpha(@view_fg_color, 0.045); border-radius: 0; padding: 6px 10px;
             border-left: 2px solid alpha(@accent_bg_color, 0.8); }
.sill-permission { background: alpha(@warning_bg_color, 0.10); border-radius: 0; padding: 8px 10px;
                   border-left: 2px solid @warning_bg_color; }
.sill-permission button { border-radius: 0; }
.sill-prompt { border-radius: 0; }
.sill-prompt text { background: transparent; }
.sill-prompt-box { border-radius: 0; border: none; border-top: 1px solid alpha(@view_fg_color, 0.15);
                   background: alpha(@view_fg_color, 0.035); }
"#;
