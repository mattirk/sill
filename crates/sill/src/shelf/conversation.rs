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
    /// Set while we move the adjustment ourselves, so it doesn't count as the person scrolling.
    following: std::cell::Cell<bool>,
    /// Previous adjustment value, to tell a scroll-up from everything else that moves it.
    last_value: std::cell::Cell<f64>,
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
            following: std::cell::Cell::new(false),
            last_value: std::cell::Cell::new(0.0),
        });
        // The content height is only known after layout: follow the adjustment's geometry (`changed` covers
        // both the content growing and the viewport shrinking, e.g. the prompt box getting taller) instead
        // of guessing when to scroll. Any scroll up that isn't ours releases it — by direction, not by
        // distance, so a touchpad's first few pixels aren't yanked back — and reaching the bottom
        // re-engages it.
        let adj = conv.root.vadjustment();
        {
            let c = conv.clone();
            adj.connect_changed(move |adj| {
                if c.stick_to_bottom.get() {
                    c.follow(adj);
                }
            });
        }
        {
            let c = conv.clone();
            adj.connect_value_changed(move |adj| {
                let v = adj.value();
                if !c.following.get() {
                    if v >= adj.upper() - adj.page_size() - 1.0 {
                        c.stick_to_bottom.set(true);
                    } else if v < c.last_value.get() {
                        c.stick_to_bottom.set(false);
                    }
                }
                c.last_value.set(v);
            });
        }
        conv
    }

    /// Move to the bottom without it counting as the person scrolling.
    fn follow(&self, adj: &gtk::Adjustment) {
        let bottom = (adj.upper() - adj.page_size()).max(adj.lower());
        if adj.value() != bottom {
            self.following.set(true);
            adj.set_value(bottom);
            self.following.set(false);
        }
    }

    fn scroll_to_end(&self) {
        self.stick_to_bottom.set(true);
        self.follow(&self.root.vadjustment());
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
            Event::Controls(_) => {}
            Event::Ready { session_id, resumed, .. } => {
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
.sill-files > row { padding: 1px 4px; min-height: 0; }
.sill-files > row:selected { background: alpha(@accent_bg_color, 0.25); }
.sill-files treeexpander { padding: 1px 0; }
.sill-file-icon { opacity: 0.7; -gtk-icon-size: 14px; }
.sill-changed { color: @warning_color; }
.sill-new { color: @success_color; }
.sill-dim { opacity: 0.45; }
.sill-pinned { color: @accent_color; }
.sill-editor gutter { border: none; }
.sill-page-head { border-bottom: 1px solid alpha(@view_fg_color, 0.12); padding-bottom: 4px; }
.sill-back { padding: 2px 10px 2px 6px; background: alpha(@accent_bg_color, 0.18); color: @view_fg_color; }
.sill-back:hover { background: alpha(@accent_bg_color, 0.32); }
.sill-pins { padding: 2px 0; }
.sill-chip { background: alpha(@view_fg_color, 0.07); padding: 0 2px 0 0; }
.sill-chip-open { padding: 1px 8px; min-height: 0; }
.sill-git-add { color: @success_color; font-size: 90%; }
.sill-git-del { color: @error_color; font-size: 90%; }
.sill-scope { padding: 2px 0; }
.sill-pin { padding: 0 2px; min-height: 0; min-width: 0; opacity: 0.4; -gtk-icon-size: 11px; }
.sill-pin:hover { opacity: 1; }
.sill-badge { font-size: 80%; color: alpha(@accent_color, 0.9); font-family: monospace; }
.sill-controls { font-size: 85%; }
.sill-controls .dim-label { opacity: 0.7; }
.sill-control { min-height: 0; padding: 0; color: alpha(@view_fg_color, 0.6); }
.sill-control > button { min-height: 0; padding: 1px 6px; background: transparent; border: none; box-shadow: none; color: inherit; }
.sill-control > button:hover { background: alpha(@view_fg_color, 0.07); color: @view_fg_color; }
.sill-control > button arrow { -gtk-icon-size: 10px; opacity: 0.6; }
switch.sill-control { min-width: 30px; min-height: 14px; opacity: 0.7; }
switch.sill-control:hover, switch.sill-control:checked { opacity: 1; }
switch.sill-control > slider { min-width: 12px; min-height: 12px; }
button.sill-send { min-height: 22px; min-width: 22px; padding: 0 5px; font-size: 170%; line-height: 1;
                   background: alpha(@accent_bg_color, 0.25); }
button.sill-send > label { padding: 0; margin: 0; }
button.sill-send:hover { background: alpha(@accent_bg_color, 0.45); }
button.sill-send.sill-stop { font-size: 140%; color: @error_color; background: alpha(@error_bg_color, 0.25); }
button.sill-send.sill-stop:hover { background: alpha(@error_bg_color, 0.45); }
.sill-prompt-box { border-radius: 0; border: none; border-top: 1px solid alpha(@view_fg_color, 0.15);
                   background: alpha(@view_fg_color, 0.035); }
"#;
