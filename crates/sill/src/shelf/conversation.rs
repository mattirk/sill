//! The conversation view (SPEC §3.1): the ACP stream as a column of blocks — user prompts, agent
//! messages (streamed markdown), thoughts (collapsed), tool calls with status, permission requests
//! with buttons.
//!
//! Layout (docs/design.md): the column is clamped to a reading measure so prose doesn't run the
//! full width of a wide shelf; a user turn is plain text with an accent rule, not a bubble; a tool
//! call is one line (glyph, title, status mark) whose body opens on click; consecutive tool calls
//! fold into one "n steps" card so a long turn stays a short list.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::oneshot;
use gtk4 as gtk;
use gtk::prelude::*;
use libadwaita as adw;
use sill_core::acp::{Event, PermissionOption};

use super::{markdown, theme};

/// Reading measure of the column in logical px at text scale 1: wide enough that a shelf at the
/// default width fills it edge to edge (no side margins); only a wide shelf gets centred prose.
const MEASURE: f64 = 800.0;
/// Width below which the clamp stops centring and just fills.
const MEASURE_TIGHT: f64 = 640.0;

/// One tool call on screen.
struct ToolRow {
    row: gtk::Box,
    title: gtk::Label,
    mark: gtk::Label,
    body: gtk::Label,
    status: RefCell<String>,
    group: RefCell<Option<Rc<ToolGroup>>>,
}

/// Consecutive tool calls folded into one card: a header line (count, latest title, aggregate
/// mark) that opens the list of rows.
struct ToolGroup {
    header_title: gtk::Label,
    header_mark: gtk::Label,
    rows: gtk::Box,
    ids: RefCell<Vec<String>>,
}

pub struct Conversation {
    pub root: gtk::ScrolledWindow,
    column: gtk::Box,
    /// The agent message being streamed, with its accumulated markdown.
    current: RefCell<Option<(gtk::TextView, String)>>,
    current_thought: RefCell<Option<(gtk::TextView, String)>>,
    tool_rows: RefCell<HashMap<String, Rc<ToolRow>>>,
    /// The tool call (or group) the next tool call would join; cleared by any other block.
    last_tool: RefCell<Option<String>>,
    status: gtk::Label,
    /// Follow new content unless the person scrolled up.
    stick_to_bottom: std::cell::Cell<bool>,
    ticking: std::cell::Cell<bool>,
    tick_frames: std::cell::Cell<u32>,
    weak: RefCell<std::rc::Weak<Conversation>>,
    /// Set while we move the adjustment ourselves, so it doesn't count as the person scrolling.
    following: std::cell::Cell<bool>,
    /// Previous adjustment value, to tell a scroll-up from everything else that moves it.
    last_value: std::cell::Cell<f64>,
    last_upper: std::cell::Cell<f64>,
    last_page: std::cell::Cell<f64>,
}

impl Conversation {
    pub fn new(status: gtk::Label) -> Rc<Self> {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.set_margin_top(8);
        column.set_margin_bottom(8);
        let clamp = adw::Clamp::builder().child(&column).build();
        Self::size_clamp(&clamp);
        {
            let weak = clamp.downgrade();
            theme::on_change(move |_| {
                if let Some(c) = weak.upgrade() {
                    Self::size_clamp(&c);
                }
            });
        }
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&clamp)
            .build();
        let conv = Rc::new(Self {
            root,
            column,
            current: RefCell::new(None),
            current_thought: RefCell::new(None),
            tool_rows: RefCell::new(HashMap::new()),
            last_tool: RefCell::new(None),
            status,
            stick_to_bottom: std::cell::Cell::new(true),
            ticking: std::cell::Cell::new(false),
            tick_frames: std::cell::Cell::new(0),
            weak: RefCell::new(std::rc::Weak::new()),
            following: std::cell::Cell::new(false),
            last_value: std::cell::Cell::new(0.0),
            last_upper: std::cell::Cell::new(0.0),
            last_page: std::cell::Cell::new(0.0),
        });
        // The content height is only known after layout: follow the adjustment's geometry (`changed` covers
        // both the content growing and the viewport shrinking, e.g. the prompt box getting taller) instead
        // of guessing when to scroll. Any scroll up that isn't ours releases it — by direction, not by
        // distance, so a touchpad's first few pixels aren't yanked back — and reaching the bottom
        // re-engages it.
        *conv.weak.borrow_mut() = Rc::downgrade(&conv);
        let adj = conv.root.vadjustment();
        {
            let c = conv.clone();
            adj.connect_changed(move |_| {
                // `changed` arrives inside the viewport's allocation; a set_value there is clamped
                // away when the allocation finishes. Follow from the next frames instead.
                if c.stick_to_bottom.get() {
                    c.request_follow();
                }
            });
        }
        {
            let c = conv.clone();
            adj.connect_value_changed(move |adj| {
                let v = adj.value();
                let upper = adj.upper();
                let page = adj.page_size();
                // a drop in value while the geometry is changing is GTK clamping/relayout (text views
                // validate lines lazily; the viewport shrinks when the prompt box grows), not the
                // person scrolling up
                let relayout = upper != c.last_upper.get() || page != c.last_page.get();
                if !c.following.get() && !relayout {
                    if v >= upper - page - 1.0 {
                        c.stick_to_bottom.set(true);
                    } else if v < c.last_value.get() {
                        c.stick_to_bottom.set(false);
                    }
                }
                c.last_value.set(v);
                c.last_upper.set(upper);
                c.last_page.set(page);
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
        self.request_follow();
    }

    /// Follow new content once layout has caught up: a frame-clock tick runs after each content
    /// change, pins the view to the bottom for the next frames, and stops by itself (no permanent
    /// tick: that would keep the frame clock awake). Streaming chunks re-arm it on every append.
    fn request_follow(&self) {
        if self.ticking.get() {
            self.tick_frames.set(0);
            return;
        }
        let weak = self.weak.borrow().clone();
        self.ticking.set(true);
        self.tick_frames.set(0);
        self.root.add_tick_callback(move |_, _| {
            let Some(c) = weak.upgrade() else { return gtk::glib::ControlFlow::Break };
            if !c.stick_to_bottom.get() {
                c.ticking.set(false);
                return gtk::glib::ControlFlow::Break;
            }
            let adj = c.root.vadjustment();
            c.follow(&adj);
            let n = c.tick_frames.get() + 1;
            c.tick_frames.set(n);
            if n > 40 {
                c.ticking.set(false);
                return gtk::glib::ControlFlow::Break;
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    /// The measure follows the text size, so it stays the same number of characters.
    fn size_clamp(clamp: &adw::Clamp) {
        let k = theme::text_scale();
        clamp.set_maximum_size((MEASURE * k) as i32);
        clamp.set_tightening_threshold((MEASURE_TIGHT * k) as i32);
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
            .pixels_inside_wrap(3) // a little leading: prose, not a terminal
            .pixels_below_lines(2)
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
        l.set_halign(gtk::Align::Fill);
        l.set_margin_top(6);
        self.column.append(&l);
        self.scroll_to_end();
    }

    fn finish_current(&self) {
        self.current.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        self.last_tool.borrow_mut().take();
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
        tv.queue_resize(); // the view must re-measure: lazy line validation leaves stale heights behind
        drop(cur);
        self.request_follow();
    }

    /// A complete agent message (stored transcript).
    pub fn add_agent_block(&self, md: &str) {
        self.finish_current();
        let tv = Self::text_view("sill-agent");
        markdown::render(&tv.buffer(), md);
        tv.queue_resize(); // the view must re-measure: lazy line validation leaves stale heights behind
        self.column.append(&tv);
        self.request_follow();
    }

    pub fn add_thought_block(&self, md: &str) {
        self.finish_current();
        let tv = Self::text_view("sill-thought");
        markdown::render(&tv.buffer(), md);
        tv.queue_resize(); // the view must re-measure: lazy line validation leaves stale heights behind
        self.column.append(&tv);
        self.request_follow();
    }

    /// Glyph for an ACP tool kind (`Debug` name); stored rows don't keep the kind.
    fn kind_glyph(kind: Option<&str>) -> &'static str {
        match kind {
            Some("Read") => "›",
            Some("Edit") => "✎",
            Some("Delete") => "✕",
            Some("Move") => "→",
            Some("Search") => "⌕",
            Some("Execute") => "$",
            Some("Fetch") => "↓",
            Some("Think") => "…",
            _ => "⚙",
        }
    }

    /// Status mark and CSS class for an ACP tool status (`Debug` name).
    fn status_mark(status: &str) -> (&'static str, &'static str) {
        match status {
            "Pending" => ("·", "sill-mark-pending"),
            "InProgress" => ("…", "sill-mark-running"),
            "Completed" => ("✓", "sill-mark-done"),
            "Failed" => ("✕", "sill-mark-failed"),
            _ => ("·", "sill-mark-pending"),
        }
    }

    fn set_mark(label: &gtk::Label, status: &str) {
        let (glyph, class) = Self::status_mark(status);
        label.set_text(glyph);
        for c in ["sill-mark-pending", "sill-mark-running", "sill-mark-done", "sill-mark-failed"] {
            label.remove_css_class(c);
        }
        label.add_css_class(class);
    }

    pub fn add_tool_call(&self, id: String, title: &str, status: &str) {
        self.add_tool_call_full(id, title, status, None, None);
    }

    /// One line: glyph, title, status mark; the body (what the call does) opens on click.
    pub fn add_tool_call_full(&self, id: String, title: &str, status: &str, kind: Option<&str>, detail: Option<&str>) {
        self.current.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("sill-tool");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("sill-tool-line");
        let glyph = gtk::Label::new(Some(Self::kind_glyph(kind)));
        glyph.add_css_class("sill-tool-glyph");
        glyph.set_width_chars(1);
        let title_l = gtk::Label::new(Some(title));
        title_l.set_xalign(0.0);
        title_l.set_single_line_mode(true);
        title_l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_l.set_hexpand(true);
        title_l.set_tooltip_text(Some(title));
        let mark = gtk::Label::new(None);
        mark.add_css_class("sill-tool-mark");
        mark.set_width_chars(1);
        Self::set_mark(&mark, status);
        row.append(&glyph);
        row.append(&title_l);
        row.append(&mark);
        card.append(&row);
        let body = gtk::Label::new(detail);
        body.add_css_class("sill-tool-body");
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        body.set_selectable(true);
        let body_rev = gtk::Revealer::builder().child(&body).reveal_child(false).transition_type(gtk::RevealerTransitionType::SlideDown).build();
        card.append(&body_rev);
        {
            let rev = body_rev.clone();
            let b = body.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                if !b.text().is_empty() {
                    rev.set_reveal_child(!rev.reveals_child());
                }
            });
            row.add_controller(click);
        }
        let tr = Rc::new(ToolRow {
            row: card,
            title: title_l,
            mark,
            body,
            status: RefCell::new(status.to_string()),
            group: RefCell::new(None),
        });
        // consecutive calls fold into one card
        let prev = self.last_tool.borrow().clone();
        let prev_row = prev.and_then(|pid| self.tool_rows.borrow().get(&pid).cloned()); // borrow ends here
        self.tool_rows.borrow_mut().insert(id.clone(), tr.clone()); // before the header counts it
        match prev_row {
            Some(prev_row) => {
                let existing = prev_row.group.borrow().clone(); // borrow ends here: start_group writes it
                let group = match existing {
                    Some(g) => g,
                    None => self.start_group(&prev_row),
                };
                group.rows.append(&tr.row);
                group.ids.borrow_mut().push(id.clone());
                *tr.group.borrow_mut() = Some(group.clone());
                self.refresh_group(&group);
            }
            None => self.column.append(&tr.row),
        }
        *self.last_tool.borrow_mut() = Some(id);
        self.request_follow();
    }

    /// Turn a lone tool row into a group card in its place; the row moves inside.
    fn start_group(&self, first: &Rc<ToolRow>) -> Rc<ToolGroup> {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("sill-tool");
        card.add_css_class("sill-tool-group");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("sill-tool-line");
        let glyph = gtk::Label::new(Some("⚙"));
        glyph.add_css_class("sill-tool-glyph");
        glyph.set_width_chars(1);
        let header_title = gtk::Label::new(None);
        header_title.set_xalign(0.0);
        header_title.set_single_line_mode(true);
        header_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header_title.set_hexpand(true);
        let header_mark = gtk::Label::new(None);
        header_mark.add_css_class("sill-tool-mark");
        header_mark.set_width_chars(1);
        header.append(&glyph);
        header.append(&header_title);
        header.append(&header_mark);
        card.append(&header);
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
        rows.add_css_class("sill-tool-rows");
        let rows_rev = gtk::Revealer::builder().child(&rows).reveal_child(false).transition_type(gtk::RevealerTransitionType::SlideDown).build();
        card.append(&rows_rev);
        {
            let rev = rows_rev.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| rev.set_reveal_child(!rev.reveals_child()));
            header.add_controller(click);
        }
        // the card takes the first row's place
        self.column.insert_child_after(&card, Some(&first.row));
        self.column.remove(&first.row);
        rows.append(&first.row);
        let first_id = self.tool_rows.borrow().iter().find(|(_, r)| Rc::ptr_eq(r, first)).map(|(id, _)| id.clone());
        let g = Rc::new(ToolGroup { header_title, header_mark, rows, ids: RefCell::new(first_id.into_iter().collect()) });
        *first.group.borrow_mut() = Some(g.clone());
        g
    }

    /// Header of a group: "n steps · latest title", mark = running if any runs, failed if any
    /// failed, else done.
    fn refresh_group(&self, g: &Rc<ToolGroup>) {
        let rows = self.tool_rows.borrow();
        let ids = g.ids.borrow();
        let members: Vec<&Rc<ToolRow>> = ids.iter().filter_map(|id| rows.get(id)).collect();
        let latest = members.last().map(|r| r.title.text().to_string()).unwrap_or_default();
        g.header_title.set_text(&format!("{} steps · {latest}", members.len()));
        let statuses: Vec<String> = members.iter().map(|r| r.status.borrow().clone()).collect();
        let agg = if statuses.iter().any(|s| s == "InProgress" || s == "Pending") {
            "InProgress"
        } else if statuses.iter().any(|s| s == "Failed") {
            "Failed"
        } else {
            "Completed"
        };
        Self::set_mark(&g.header_mark, agg);
    }

    pub fn update_tool_call_full(&self, id: &str, status: Option<&str>, title: Option<&str>, detail: Option<&str>) {
        let row = self.tool_rows.borrow().get(id).cloned();
        let Some(r) = row else { return };
        if let Some(s) = status {
            *r.status.borrow_mut() = s.to_string();
            Self::set_mark(&r.mark, s);
        }
        if let Some(t) = title {
            r.title.set_text(t);
            r.title.set_tooltip_text(Some(t));
        }
        if let Some(d) = detail {
            // an update's detail (output) is added under what the call already showed
            let cur = r.body.text();
            if cur.is_empty() {
                r.body.set_text(d);
            } else if !cur.ends_with(d) {
                r.body.set_text(&format!("{cur}\n\n{d}"));
            }
        }
        let group = r.group.borrow().clone();
        if let Some(g) = group {
            self.refresh_group(&g);
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
        self.request_follow();
    }

    /// A question from sill with a few actions (same look as a permission request). Each button
    /// disables the row and runs its action.
    pub fn add_actions(&self, message: &str, actions: Vec<(String, Rc<dyn Fn()>)>) {
        self.finish_current();
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
        frame.add_css_class("sill-permission");
        let label = gtk::Label::new(Some(message));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_selectable(true);
        frame.append(&label);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for (text, act) in actions {
            let b = gtk::Button::with_label(&text);
            let buttons2 = buttons.clone();
            let label2 = label.clone();
            b.connect_clicked(move |_| {
                buttons2.set_sensitive(false);
                label2.set_text(&format!("{} → {}", label2.text(), text));
                act();
            });
            buttons.append(&b);
        }
        frame.append(&buttons);
        self.column.append(&frame);
        self.request_follow();
    }

    /// The agent wants a login first: one button per method it offers.
    pub fn add_auth(&self, message: &str, methods: Vec<sill_core::acp::AuthMethodInfo>, pick: Rc<dyn Fn(sill_core::acp::AuthMethodInfo)>) {
        self.finish_current();
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
        frame.add_css_class("sill-permission");
        let text = if message.is_empty() { "Login needed".to_string() } else { format!("Login needed: {message}") };
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        frame.append(&label);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for m in methods {
            let b = gtk::Button::with_label(&m.name);
            b.set_tooltip_text(m.description.as_deref());
            b.add_css_class("suggested-action");
            let pick = pick.clone();
            let buttons2 = buttons.clone();
            let label2 = label.clone();
            b.connect_clicked(move |_| {
                buttons2.set_sensitive(false);
                label2.set_text(&format!("{} → {}", label2.text(), m.name));
                pick(m.clone());
            });
            buttons.append(&b);
        }
        frame.append(&buttons);
        self.column.append(&frame);
        self.request_follow();
    }

    pub fn add_note(&self, text: &str) {
        self.finish_current();
        let l = gtk::Label::new(Some(text));
        l.add_css_class("dim-label");
        l.set_xalign(0.0);
        l.set_wrap(true);
        self.column.append(&l);
        self.request_follow();
    }

    /// Apply one session event.
    pub fn handle(&self, ev: Event) {
        match ev {
            Event::Controls(_) | Event::AuthRequired { .. } => {}
            Event::Ready { session_id, resumed, .. } => {
                self.status.set_text(&format!("{} · {}", if resumed { "resumed" } else { "ready" }, &session_id[..session_id.len().min(8)]))
            }
            Event::TurnStarted => self.status.set_text("working…"),
            Event::UserText(t) => self.add_user(&t),
            Event::Text(t) => self.append_markdown(&self.current, "sill-agent", &t),
            Event::Thought(t) => self.append_markdown(&self.current_thought, "sill-thought", &t),
            Event::ToolCall { id, title, status, kind, detail } => {
                self.add_tool_call_full(id, &title, &status, Some(&kind), detail.as_deref())
            }
            Event::ToolCallUpdate { id, status, title, detail } => {
                self.update_tool_call_full(&id, status.as_deref(), title.as_deref(), detail.as_deref())
            }
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
/* no client-side shadow or resize margin around the toplevel: the content would sit a few pixels in
   on the first frame and hop to the edge once the window is placed */
window, window.csd, window.csd.background { margin: 0; box-shadow: none; outline: none; }
tabbar tab, tabbar .tab { border-radius: 0; }
.boxed-list, .boxed-list row, .frame, scrolledwindow.frame { border-radius: 0; }
popover > contents { border-radius: 0; }
button { border-radius: 0; }
entry { border-radius: 0; }

/* a user turn: plain text behind an accent rule, no bubble */
.sill-user { background: transparent; border-radius: 0; padding: 4px 6px; border-left: 2px solid alpha(@accent_bg_color, 0.9); }
.sill-user text { background: transparent; }
.sill-agent { background: transparent; border-radius: 0; }
.sill-agent text { background: transparent; }
.sill-thought { background: transparent; color: alpha(@view_fg_color, 0.55); font-size: 90%; }
.sill-thought text { background: transparent; }
/* a tool call: one quiet line; the body opens under it */
.sill-tool { background: transparent; border-radius: 0; padding: 0; border-left: 2px solid alpha(@view_fg_color, 0.18); }
.sill-tool-line { padding: 3px 6px; font-size: 90%; color: alpha(@view_fg_color, 0.75); }
.sill-tool-line:hover { background: alpha(@view_fg_color, 0.04); color: @view_fg_color; }
.sill-tool-glyph { font-family: monospace; opacity: 0.6; }
.sill-tool-mark { font-family: monospace; }
.sill-mark-pending { opacity: 0.4; }
.sill-mark-running { color: @accent_color; }
.sill-mark-done { opacity: 0.5; }
.sill-mark-failed { color: @error_color; }
.sill-tool-body { font-family: monospace; font-size: 85%; padding: 4px 10px 6px 28px; color: alpha(@view_fg_color, 0.8); }
.sill-tool-rows { margin-left: 10px; border-left: 1px solid alpha(@view_fg_color, 0.12); }
.sill-tool-rows .sill-tool { border-left: none; }
.sill-permission { background: alpha(@warning_bg_color, 0.10); border-radius: 0; padding: 8px 10px;
                   border-left: 2px solid @warning_bg_color; }
.sill-permission button { border-radius: 0; }
.sill-prompt { border-radius: 0; }
.sill-prompt text { background: transparent; }
/* "+" menu: a plain list, icon + name, left-aligned */
popover.sill-menu > contents { padding: 4px; min-width: 180px; }
.sill-menu-item { padding: 5px 10px; min-height: 0; background: transparent; border: none; box-shadow: none; }
.sill-menu-item:hover { background: alpha(@view_fg_color, 0.07); }
.sill-menu-item label { color: inherit; font-weight: normal; }
.sill-menu-icon { opacity: 0.9; }
.sill-menu-item:hover .sill-menu-icon { opacity: 1; }
.sill-menu-sep { margin: 4px 6px; opacity: 0.5; }
.sill-menu-caption { font-size: 80%; opacity: 0.5; padding: 2px 10px; }
.sill-menu-trash { padding: 0 6px; min-height: 0; opacity: 0.35; background: transparent; border: none; box-shadow: none; }
.sill-menu-trash:hover { opacity: 1; color: @error_color; }
/* top-bar status: a small dot at rest, a spinner while working; the text lives in the tooltip */
.sill-status-slot { padding: 6px 8px; }
.sill-status { min-width: 8px; min-height: 8px; border-radius: 4px; background: alpha(@view_fg_color, 0.3); }
.sill-status.sill-attention { background: @warning_color; }
.sill-spinner { min-width: 15px; min-height: 15px; opacity: 0.8; }
/* Agents page: one quiet row per registry entry */
.sill-agents { background: transparent; }
.sill-agents > row { padding: 0; }
.sill-agents > row:hover { background: alpha(@view_fg_color, 0.03); }
/* (scoped: the conversation's agent text view carries the same class) */
.sill-agents .sill-agent { padding: 7px 10px 7px 8px; border-bottom: 1px solid alpha(@view_fg_color, 0.08); }
.sill-agent-name { font-weight: 600; }
.sill-agent-version { font-size: 85%; }
.sill-agent-id { font-family: monospace; font-size: 80%; opacity: 0.5; }
.sill-agent-link { padding: 0 4px; min-height: 0; min-width: 0; opacity: 0.55; }
.sill-agent-link:hover { opacity: 1; }
.sill-agent-log { font-family: monospace; font-size: 80%; opacity: 0.7; }
button.sill-install { padding: 1px 10px; min-height: 22px; font-weight: normal; background: alpha(@accent_bg_color, 0.25); border: none; box-shadow: none; }
button.sill-install:hover { background: alpha(@accent_bg_color, 0.45); }
.sill-filter { padding: 2px 10px; min-height: 0; font-weight: normal; }
.sill-files > row { padding: 1px 4px; min-height: 0; }
.sill-files > row:selected { background: alpha(@accent_bg_color, 0.25); }
.sill-files treeexpander { padding: 1px 0; }
.sill-file-icon { opacity: 0.7; -gtk-icon-size: 14px; }
.sill-changed { color: @warning_color; }
.sill-new { color: @success_color; }
.sill-dim { opacity: 0.45; }
.sill-pinned { color: @accent_color; }
.sill-editor gutter { border: none; }
/* the "sill" toggle: quiet, underlined like a selected tab while the backend is open */
button.sill-action { padding: 2px 8px; min-height: 0; background: transparent; box-shadow: none; border-bottom: 2px solid transparent; }
button.sill-action:hover { background: alpha(@view_fg_color, 0.07); }
button.sill-action:checked { background: alpha(@view_fg_color, 0.09); border-bottom: 2px solid alpha(@accent_bg_color, 0.9); }
.sill-pins { padding: 2px 0; }
.sill-chip { background: alpha(@view_fg_color, 0.07); padding: 0 2px 0 0; }
.sill-chip-open { padding: 1px 8px; min-height: 0; }
.sill-apply { color: @warning_color; padding: 0 8px; min-height: 0; }
.sill-git-add { color: @success_color; font-size: 90%; }
.sill-git-del { color: @error_color; font-size: 90%; }
.sill-scope { padding: 2px 0; }
.sill-pin { padding: 0 2px; min-height: 0; min-width: 0; opacity: 0.4; -gtk-icon-size: 11px; }
.sill-pin:hover { opacity: 1; }
.sill-badge { font-size: 80%; color: alpha(@accent_color, 0.9); font-family: monospace; }
.sill-controls { font-size: 85%; }
.sill-ctl-icon { min-height: 0; min-width: 0; padding: 2px 6px; opacity: 0.6; -gtk-icon-size: 13px; }
.sill-ctl-icon:hover, .sill-ctl-icon:checked { opacity: 1; }
/* the lock: same colour in both states, fixed width so open/closed don't shift the row */
button.sill-lock { min-width: 24px; padding: 2px 0; opacity: 0.6; }
button.sill-lock:checked { background: transparent; opacity: 0.9; }
button.sill-lock:hover { opacity: 1; }
.sill-controls .dim-label { opacity: 0.7; }
.sill-control { min-height: 0; padding: 0; color: alpha(@view_fg_color, 0.6); }
.sill-control > button { min-height: 0; padding: 1px 4px; background: transparent; border: none; box-shadow: none; color: inherit; }
.sill-control > button:hover { background: alpha(@view_fg_color, 0.07); color: @view_fg_color; }
.sill-control > button arrow { -gtk-icon-size: 10px; opacity: 0.6; }
switch.sill-control { min-width: 30px; min-height: 14px; opacity: 0.7; }
switch.sill-control:hover, switch.sill-control:checked { opacity: 1; }
switch.sill-control > slider { min-width: 12px; min-height: 12px; }
button.sill-send { min-height: 22px; min-width: 22px; padding: 0; font-size: 170%; line-height: 1;
                   background: alpha(@accent_bg_color, 0.25); }
button.sill-send > label { padding: 0; margin: 0; }
button.sill-send:hover { background: alpha(@accent_bg_color, 0.45); }
button.sill-send.sill-stop { color: @error_color; background: alpha(@error_bg_color, 0.25); }
button.sill-send.sill-stop > image { margin: 0; padding: 0; }
button.sill-send.sill-stop:hover { background: alpha(@error_bg_color, 0.45); }
.sill-terminal { padding: 4px; }
/* a notice over the terminal that fades away */
.sill-toast { padding: 3px 10px; font-size: 90%; color: @view_fg_color; background: alpha(@accent_bg_color, 0.35);
              opacity: 0; transition: opacity 700ms ease; }
.sill-toast.sill-toast-show { opacity: 1; transition: opacity 120ms ease; }
.sill-hotkey { padding: 4px 14px; font-family: monospace; }
.sill-capturing { background: alpha(@accent_bg_color, 0.3); }
.sill-prompt-box { border-radius: 0; border: none; border-top: 1px solid alpha(@view_fg_color, 0.15);
                   background: alpha(@view_fg_color, 0.035); }
"#;
