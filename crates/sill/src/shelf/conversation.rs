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
/// Streamed chunks are rendered at most this often (the whole message each time).
const STREAM_MS: u64 = 80;

/// One tool call on screen.
struct ToolRow {
    row: gtk::Box,
    title: gtk::Label,
    mark: gtk::Label,
    body: gtk::Label,
    /// A file edit as a unified diff (hidden until there is one), filled live as updates arrive.
    diff: sourceview5::View,
    /// A file edit (ACP kind Edit/Delete/Move): folds with edits, never with other steps.
    edit: bool,
    status: RefCell<String>,
    group: RefCell<Option<Rc<ToolGroup>>>,
    /// A delegation's children: the sub-agent's own calls (rows), its text (said), and the count
    /// on the line; opened together with the body.
    children: RefCell<Option<Children>>,
}

struct Children {
    rows: gtk::Box,
    said: gtk::Label,
    count: gtk::Label,
    rev: gtk::Revealer,
    n: std::cell::Cell<usize>,
}

/// Consecutive tool calls folded into one card: a header line (count, latest title, aggregate
/// mark) that opens the list of rows. File edits and other steps make separate cards.
struct ToolGroup {
    header_title: gtk::Label,
    header_mark: gtk::Label,
    rows: gtk::Box,
    ids: RefCell<Vec<String>>,
    edit: bool,
}

pub struct Conversation {
    pub root: gtk::ScrolledWindow,
    column: gtk::Box,
    /// The agent message being streamed, with its accumulated markdown.
    current: RefCell<Option<(gtk::Box, String)>>,
    current_thought: RefCell<Option<(gtk::Box, String)>>,
    /// Image context for every message (paths as the agent sees them; a click opens the viewer).
    md_ctx: RefCell<Option<Rc<markdown::Ctx>>>,
    /// A render of the streamed message/thought is scheduled: chunks arrive many times a second
    /// and every render is the whole message, so they are coalesced (STREAM_MS).
    render_due: [std::cell::Cell<bool>; 2],
    tool_rows: RefCell<HashMap<String, Rc<ToolRow>>>,
    /// The tool call (or group) the next tool call would join; cleared by any other block.
    last_tool: RefCell<Option<String>>,
    status: gtk::Label,
    /// Follow new content unless the person scrolled up.
    stick_to_bottom: std::cell::Cell<bool>,
    ticking: std::cell::Cell<bool>,
    tick_frames: std::cell::Cell<u32>,
    /// Which follow tick is the live one; an older one still registered stops at its next frame.
    tick_gen: std::cell::Cell<u32>,
    /// `settled`: the content height last frame, and for how many frames it has not changed.
    settle_upper: std::cell::Cell<f64>,
    settle_frames: std::cell::Cell<u32>,
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
            md_ctx: RefCell::new(None),
            render_due: [std::cell::Cell::new(false), std::cell::Cell::new(false)],
            tool_rows: RefCell::new(HashMap::new()),
            last_tool: RefCell::new(None),
            status,
            stick_to_bottom: std::cell::Cell::new(true),
            ticking: std::cell::Cell::new(false),
            tick_frames: std::cell::Cell::new(0),
            tick_gen: std::cell::Cell::new(0),
            settle_upper: std::cell::Cell::new(-1.0),
            settle_frames: std::cell::Cell::new(0),
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
        {
            // the column's width changed (shelf resized, a scrollbar came or went): tables are laid
            // out for a width, so they follow — on idle, never inside the allocation reporting it
            let last = std::cell::Cell::new(0.0f64);
            conv.root.hadjustment().connect_changed(move |a| {
                let w = a.page_size();
                if (w - last.get()).abs() > 2.0 {
                    last.set(w);
                    gtk::glib::idle_add_local_once(markdown::relayout);
                }
            });
        }
        let adj = conv.root.vadjustment();
        {
            // shown again (shelf re-opened, tab switched back): a follow tick armed while hidden may
            // never have run to its end and left `ticking` set, which would block every later follow
            // — the view then stops short of the end. Start a fresh one.
            let weak = Rc::downgrade(&conv);
            conv.root.connect_map(move |_| {
                if let Some(c) = weak.upgrade() {
                    c.ticking.set(false);
                    if c.stick_to_bottom.get() {
                        c.request_follow();
                    }
                }
            });
        }
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

    pub fn reset_settle(&self) {
        self.settle_upper.set(-1.0);
        self.settle_frames.set(0);
    }

    /// Called once per frame while the shelf waits to be revealed: true once the content height has
    /// held for two frames and, when following, the view is at the bottom.
    pub fn settled(&self) -> bool {
        let adj = self.root.vadjustment();
        let upper = adj.upper();
        let n = if (upper - self.settle_upper.get()).abs() < 0.5 { self.settle_frames.get() + 1 } else { 0 };
        self.settle_upper.set(upper);
        self.settle_frames.set(n);
        let in_place = !self.stick_to_bottom.get() || adj.value() >= upper - adj.page_size() - 1.0;
        n >= 2 && in_place && self.root.is_mapped()
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
        let generation = self.tick_gen.get().wrapping_add(1);
        self.tick_gen.set(generation);
        self.root.add_tick_callback(move |_, _| {
            let Some(c) = weak.upgrade() else { return gtk::glib::ControlFlow::Break };
            if c.tick_gen.get() != generation {
                return gtk::glib::ControlFlow::Break; // superseded: the live tick owns `ticking`
            }
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

    /// Set before the transcript is shown: how messages resolve and open their images.
    pub fn set_md_context(&self, ctx: Rc<markdown::Ctx>) {
        *self.md_ctx.borrow_mut() = Some(ctx);
    }

    /// An empty message: a vertical box the Markdown renderer fills with prose, code and image segments.
    fn message_box(&self) -> gtk::Box {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        b.add_css_class("sill-msg");
        if let Some(ctx) = self.md_ctx.borrow().clone() {
            markdown::set_context(&b, ctx);
        }
        b
    }

    pub fn add_user(&self, text: &str) {
        self.add_user_with(text, &[]);
    }

    /// A user turn; `attachments` are the names of what went with it, shown as a dim line under it.
    pub fn add_user_with(&self, text: &str, attachments: &[String]) {
        self.finish_current();
        let block = gtk::Box::new(gtk::Orientation::Vertical, 2);
        block.add_css_class("sill-user");
        block.set_margin_top(6);
        let l = gtk::Label::new(Some(text));
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_selectable(true);
        l.set_xalign(0.0);
        l.set_halign(gtk::Align::Fill);
        block.append(&l);
        if !attachments.is_empty() {
            let a = gtk::Label::new(Some(&format!("⌁ {}", attachments.join(" · "))));
            a.add_css_class("dim-label");
            a.add_css_class("sill-user-attach");
            a.set_xalign(0.0);
            a.set_wrap(true);
            a.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            block.append(&a);
        }
        self.column.append(&block);
        self.scroll_to_end();
    }

    fn finish_current(&self) {
        self.flush_streams();
        self.current.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        self.last_tool.borrow_mut().take();
    }

    /// Render whatever is still pending for the streamed message and thought, now.
    fn flush_streams(&self) {
        for which in 0..2 {
            if self.render_due[which].replace(false) {
                self.render_stream(which);
            }
        }
    }

    fn render_stream(&self, which: usize) {
        let (slot, class) = if which == 0 { (&self.current, "sill-agent") } else { (&self.current_thought, "sill-thought") };
        if let Some((msg, md)) = slot.borrow().as_ref() {
            markdown::render(msg, class, md);
            msg.queue_resize(); // re-measure: lazy line validation leaves stale heights behind
        }
        self.request_follow();
    }

    fn append_markdown(&self, slot: &RefCell<Option<(gtk::Box, String)>>, css: &str, chunk: &str) {
        let which = if css == "sill-thought" { 1 } else { 0 };
        let mut cur = slot.borrow_mut();
        let fresh = cur.is_none();
        if fresh {
            let msg = self.message_box();
            self.column.append(&msg);
            *cur = Some((msg, String::new()));
        }
        cur.as_mut().unwrap().1.push_str(chunk);
        drop(cur);
        if fresh {
            self.render_stream(which); // the first chunk shows at once
        } else if !self.render_due[which].replace(true) {
            let weak = self.weak.borrow().clone();
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(STREAM_MS), move || {
                if let Some(c) = weak.upgrade()
                    && c.render_due[which].replace(false)
                {
                    c.render_stream(which);
                }
            });
        }
    }

    /// A complete agent message (stored transcript).
    pub fn add_agent_block(&self, md: &str) {
        self.add_message_block("sill-agent", md);
    }

    pub fn add_thought_block(&self, md: &str) {
        self.add_message_block("sill-thought", md);
    }

    /// A complete message: in the column first, so the renderer measures with the real font.
    fn add_message_block(&self, class: &str, md: &str) {
        self.finish_current();
        let msg = self.message_box();
        self.column.append(&msg);
        markdown::render(&msg, class, md);
        msg.queue_resize();
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
            Some("Delegate") => "⤷", // a sub-agent (sill's own kind, see acp::is_delegation)
            _ => "⚙",
        }
    }

    /// A file edit's diff into the row's diff view, shown from then on; nothing without one.
    fn set_diff(r: &ToolRow, diff: Option<&str>) {
        if let Some(d) = diff.filter(|d| !d.trim().is_empty()) {
            markdown::fill_code_view(&r.diff, "diff", d);
            r.diff.set_visible(true);
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

    /// The children slot of a delegation row, made on first use.
    fn children_of(&self, r: &Rc<ToolRow>) -> Children {
        let mut slot = r.children.borrow_mut();
        if slot.is_none() {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let said = gtk::Label::new(None);
            said.add_css_class("sill-tool-said");
            said.set_xalign(0.0);
            said.set_wrap(true);
            said.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            said.set_selectable(true);
            said.set_visible(false);
            let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
            rows.add_css_class("sill-tool-rows");
            col.append(&rows);
            col.append(&said);
            let rev = gtk::Revealer::builder().child(&col).reveal_child(false).transition_type(gtk::RevealerTransitionType::SlideDown).build();
            r.row.append(&rev);
            let count = gtk::Label::new(None);
            count.add_css_class("dim-label");
            count.add_css_class("sill-tool-count");
            // on the line, before the mark
            if let Some(line) = r.row.first_child() {
                if let Some(line) = line.downcast_ref::<gtk::Box>() {
                    line.insert_child_after(&count, Some(&r.title));
                }
            }
            {
                let rev = rev.clone();
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, _, _| rev.set_reveal_child(!rev.reveals_child()));
                if let Some(line) = r.row.first_child() {
                    line.add_controller(click);
                }
            }
            *slot = Some(Children { rows, said, count, rev, n: std::cell::Cell::new(0) });
        }
        let c = slot.as_ref().unwrap();
        Children { rows: c.rows.clone(), said: c.said.clone(), count: c.count.clone(), rev: c.rev.clone(), n: std::cell::Cell::new(c.n.get()) }
    }

    /// A permission request answered by the session's auto-allow: the call's line gets a small
    /// `auto` tag (tooltip: the request) instead of a note with the whole command. Without a row
    /// (a request before its call, or a cancelled one) a short note is kept.
    pub fn mark_auto_allowed(&self, tool_call_id: &str, title: &str, cancelled: bool) {
        let row = self.tool_rows.borrow().get(tool_call_id).cloned();
        match row {
            Some(r) if !cancelled => {
                if r.row.first_child().and_then(|l| l.downcast::<gtk::Box>().ok()).is_some_and(|l| l.css_classes().iter().any(|c| c == "sill-tool-auto")) {
                    return;
                }
                let tag = gtk::Label::new(Some("auto"));
                tag.add_css_class("sill-tool-auto-tag");
                tag.set_tooltip_text(Some(&format!("auto-allowed: {title}")));
                if let Some(line) = r.row.first_child().and_then(|l| l.downcast::<gtk::Box>().ok()) {
                    line.add_css_class("sill-tool-auto");
                    line.insert_child_after(&tag, Some(&r.title));
                }
            }
            _ => {
                let short: String = title.chars().take(90).collect();
                let more = if title.chars().count() > 90 { "…" } else { "" };
                self.add_note(&format!("auto-allowed: {short}{more}{}", if cancelled { " (no allow option; cancelled)" } else { "" }));
            }
        }
    }

    /// Text a sub-agent produced goes under its delegation, folded.
    pub fn add_subagent_text(&self, parent: &str, text: &str) {
        let row = self.tool_rows.borrow().get(parent).cloned();
        let Some(r) = row else { return };
        let c = self.children_of(&r);
        let cur = c.said.text();
        c.said.set_text(&format!("{cur}{text}"));
        c.said.set_visible(true);
        self.request_follow();
    }

    /// One line: glyph, title, status mark; the body (what the call does, and a file edit's diff)
    /// opens on click. With a `parent` (a sub-agent's own call) the row goes under that delegation
    /// instead of the column.
    pub fn add_tool_call_full(&self, id: String, title: &str, status: &str, kind: Option<&str>, detail: Option<&str>, parent: Option<&str>, diff: Option<&str>) {
        self.flush_streams();
        self.current.borrow_mut().take();
        self.current_thought.borrow_mut().take();
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("sill-tool");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("sill-tool-line");
        let glyph = gtk::Label::new(Some(Self::kind_glyph(kind)));
        glyph.add_css_class("sill-tool-glyph");
        glyph.set_width_chars(1);
        let delegation = kind == Some("Delegate") || title.starts_with("sub-agent · ");
        if delegation {
            // a hand-off stands out from the agent's own steps: accent glyph, accent rule
            glyph.set_text("⤷");
            glyph.add_css_class("sill-tool-delegate");
            card.add_css_class("sill-tool-delegation");
        }
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
        body.set_visible(detail.is_some_and(|d| !d.is_empty()));
        // a file edit's diff under the text: the code segments' view with the diff language
        let diff_view = markdown::code_view();
        diff_view.add_css_class("sill-tool-diff");
        diff_view.set_visible(false);
        let body_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body_box.append(&body);
        body_box.append(&diff_view);
        let body_rev = gtk::Revealer::builder().child(&body_box).reveal_child(false).transition_type(gtk::RevealerTransitionType::SlideDown).build();
        card.append(&body_rev);
        {
            let rev = body_rev.clone();
            let b = body.clone();
            let d = diff_view.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                if !b.text().is_empty() || d.is_visible() {
                    rev.set_reveal_child(!rev.reveals_child());
                }
            });
            row.add_controller(click);
        }
        let is_edit = matches!(kind, Some("Edit" | "Delete" | "Move"));
        let tr = Rc::new(ToolRow {
            row: card,
            title: title_l,
            mark,
            body,
            diff: diff_view,
            edit: is_edit,
            status: RefCell::new(status.to_string()),
            group: RefCell::new(None),
            children: RefCell::new(None),
        });
        Self::set_diff(&tr, diff);
        // a sub-agent's own call: under its delegation, counted on the line
        let parent_row = parent.and_then(|p| self.tool_rows.borrow().get(p).cloned());
        if let Some(pr) = parent_row {
            let c = self.children_of(&pr);
            c.rows.append(&tr.row);
            let n = pr.children.borrow().as_ref().map(|c| c.n.get()).unwrap_or(0) + 1;
            if let Some(cc) = pr.children.borrow().as_ref() {
                cc.n.set(n);
            }
            c.count.set_text(&format!("{n} steps"));
            self.tool_rows.borrow_mut().insert(id, tr);
            self.request_follow();
            return;
        }
        // consecutive calls fold into one card — file edits with edits, other steps with steps; a
        // delegation stands alone (its mark must show)
        let prev = if delegation { None } else { self.last_tool.borrow().clone() };
        let prev_row = prev.and_then(|pid| self.tool_rows.borrow().get(&pid).cloned()).filter(|p| p.edit == is_edit); // borrow ends here
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
        *self.last_tool.borrow_mut() = if delegation { None } else { Some(id) };
        self.request_follow();
    }

    /// Turn a lone tool row into a group card in its place; the row moves inside.
    fn start_group(&self, first: &Rc<ToolRow>) -> Rc<ToolGroup> {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("sill-tool");
        card.add_css_class("sill-tool-group");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("sill-tool-line");
        let glyph = gtk::Label::new(Some(if first.edit { "✎" } else { "⚙" }));
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
        let g = Rc::new(ToolGroup { header_title, header_mark, rows, ids: RefCell::new(first_id.into_iter().collect()), edit: first.edit });
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
        g.header_title.set_text(&format!("{} {} · {latest}", members.len(), if g.edit { "edits" } else { "steps" }));
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

    pub fn update_tool_call_full(&self, id: &str, status: Option<&str>, title: Option<&str>, detail: Option<&str>, diff: Option<&str>) {
        let row = self.tool_rows.borrow().get(id).cloned();
        let Some(r) = row else { return };
        Self::set_diff(&r, diff); // an edit's diff arrives with the update: live
        if let Some(s) = status {
            *r.status.borrow_mut() = s.to_string();
            Self::set_mark(&r.mark, s);
        }
        if let Some(t) = title {
            let t = if r.row.has_css_class("sill-tool-delegation") && !t.starts_with("sub-agent · ") && !t.to_lowercase().contains("sub-agent") {
                format!("sub-agent · {t}")
            } else {
                t.to_string()
            };
            r.title.set_text(&t);
            r.title.set_tooltip_text(Some(&t));
        }
        if let Some(d) = detail.filter(|d| !d.trim().is_empty()) {
            // an update's detail (output) is added under what the call already showed
            let cur = r.body.text();
            if cur.is_empty() {
                r.body.set_text(d);
            } else if !cur.ends_with(d) {
                r.body.set_text(&format!("{cur}\n\n{d}"));
            }
            r.body.set_visible(true);
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
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        frame.append(&label);
        let buttons = answer_row();
        let reply = Rc::new(RefCell::new(Some(reply)));
        for opt in options {
            let b = answer_button(&opt.name);
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
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_selectable(true);
        frame.append(&label);
        let buttons = answer_row();
        for (text, act) in actions {
            let b = answer_button(&text);
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
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        frame.append(&label);
        let buttons = answer_row();
        for m in methods {
            let b = answer_button(&m.name);
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
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
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
            Event::ToolCall { id, title, status, kind, detail, diff, parent } => {
                self.add_tool_call_full(id, &title, &status, Some(&kind), detail.as_deref(), parent.as_deref(), diff.as_deref())
            }
            Event::SubagentText { parent, text } => self.add_subagent_text(&parent, &text),
            Event::ToolCallUpdate { id, status, title, detail, diff } => {
                self.update_tool_call_full(&id, status.as_deref(), title.as_deref(), detail.as_deref(), diff.as_deref())
            }
            Event::PermissionRequest { title, options, reply, .. } => {
                self.status.set_text("waiting for your answer");
                self.add_permission(&title, options, reply)
            }
            Event::Other(_) | Event::Capabilities { .. } => {}
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

/// A row of answer buttons (permission, actions, login) that wraps onto more lines rather than
/// widening the column.
fn answer_row() -> gtk::FlowBox {
    let f = gtk::FlowBox::new();
    f.set_selection_mode(gtk::SelectionMode::None);
    f.set_homogeneous(false);
    f.set_column_spacing(6);
    f.set_row_spacing(6);
    f.set_max_children_per_line(8);
    f.set_halign(gtk::Align::Start);
    f
}

/// An answer button whose text wraps inside it (agents' option texts can be long).
fn answer_button(text: &str) -> gtk::Button {
    let l = gtk::Label::new(Some(text));
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_max_width_chars(40);
    l.set_justify(gtk::Justification::Center);
    let b = gtk::Button::new();
    b.set_child(Some(&l));
    b
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

/* a user turn: a framed box on the code background behind the accent rule, so your own words stand out */
/* the tint is the box; no frame: a border meeting the rule at the corners is drawn as a mitred joint */
.sill-user { background: @sill_code_bg; border: none; border-left: 3px solid alpha(@accent_bg_color, 0.9);
             border-radius: 0; padding: 10px 14px; }
.sill-user text { background: transparent; }
.sill-user-attach { font-size: 85%; }
/* an image in a message: no button chrome, a click opens it in the viewer */
button.sill-md-image { padding: 0; margin: 4px 8px; background: transparent; border: none; box-shadow: none; }
button.sill-md-image:hover { opacity: 0.85; }
/* the viewer (left) and the folder tree (right) of a session */
/* the viewer lies over the conversation: opaque */
.sill-viewer { background: @window_bg_color; }
.sill-viewer-bar { padding: 3px 6px 3px 12px; border-bottom: 1px solid alpha(@view_fg_color, 0.12); }
.sill-viewer-btn { padding: 2px 8px; min-height: 0; min-width: 0; font-size: 90%; }
.sill-tree-panel { border-left: 1px solid alpha(@view_fg_color, 0.12); }
.sill-tree-panel .sill-pin { opacity: 0; }
.sill-tree-panel row:hover .sill-pin { opacity: 0.6; }
.sill-tree-panel row:hover .sill-pin:hover { opacity: 1; }
/* attachments waiting above the prompt: small chips with a remove cross */
.sill-attach-strip { padding: 4px 8px 0 8px; }
.sill-attach-chip { background: alpha(@view_fg_color, 0.08); padding: 1px 2px 1px 8px; font-size: 85%; }
.sill-attach-chip button { min-height: 0; min-width: 0; padding: 0 4px; opacity: 0.5; background: transparent; border: none; box-shadow: none; }
.sill-attach-chip button:hover { opacity: 1; }
.sill-agent { background: transparent; border-radius: 0; }
.sill-agent text { background: transparent; }
.sill-thought { background: transparent; color: alpha(@view_fg_color, 0.55); font-size: 90%; }
.sill-thought text { background: transparent; }
/* a fenced code block: the theme's source scheme, a quiet frame */
.sill-code { font-size: 92%; margin: 2px 8px; }
.sill-code, .sill-code text { background: @sill_code_bg; }
/* a tool call: one quiet line; the body opens under it */
.sill-tool { background: transparent; border-radius: 0; padding: 0; border-left: 2px solid alpha(@view_fg_color, 0.18); }
.sill-tool-line { padding: 3px 6px; font-size: 90%; color: alpha(@view_fg_color, 0.75); }
.sill-tool-line:hover { background: alpha(@view_fg_color, 0.04); color: @view_fg_color; }
.sill-tool-glyph { font-family: monospace; opacity: 0.6; }
/* a hand-off to a sub-agent */
.sill-tool-delegation { border-left-color: alpha(@accent_bg_color, 0.7); }
.sill-tool-delegate { color: @accent_color; opacity: 1; }
.sill-tool-count { font-size: 85%; }
.sill-tool-auto-tag { font-size: 75%; opacity: 0.45; padding: 0 5px; border: 1px solid alpha(@view_fg_color, 0.3); }
.sill-tool-said { font-size: 90%; padding: 4px 10px 6px 28px; color: alpha(@view_fg_color, 0.7); }
.sill-tool-mark { font-family: monospace; }
.sill-mark-pending { opacity: 0.4; }
.sill-mark-running { color: @accent_color; }
.sill-mark-done { opacity: 0.5; }
.sill-mark-failed { color: @error_color; }
.sill-tool-body { font-family: monospace; font-size: 85%; padding: 4px 10px 6px 28px; color: alpha(@view_fg_color, 0.8); }
.sill-tool-diff { margin: 0 10px 6px 28px; font-size: 85%; }
/* the viewer's Markdown preview: a code block or image that differs from git HEAD */
.sill-md-changed { border-left: 3px solid @success_color; }
/* the host shell pane: the same terminal, under a warning-coloured caption and rule */
.sill-host-caption { font-size: 85%; color: @warning_color; padding: 3px 10px; border-top: 2px solid alpha(@warning_color, 0.8); }
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
/* session menu (tab glyph / right-click): key–value rows, the jail's whereabouts as a caption */
popover.sill-session-menu > contents { padding: 10px 12px; min-width: 260px; }
.sill-session-key { opacity: 0.6; }
.sill-session-menu dropdown > button, .sill-session-menu entry { min-height: 0; padding: 3px 8px; }
.sill-menu-trash { padding: 0 6px; min-height: 0; opacity: 0.35; background: transparent; border: none; box-shadow: none; }
.sill-menu-trash > image { -gtk-icon-size: 0.8em; }
.sill-menu-trash:hover { opacity: 1; color: @error_color; }
/* "delete all" beside the Reopen caption: caption-sized, armed in the error colour */
.sill-menu-delete-all { font-size: 80%; padding: 0 8px; min-height: 0; opacity: 0.5; background: transparent; border: none; box-shadow: none; }
.sill-menu-delete-all:hover, .sill-menu-delete-all.sill-armed { opacity: 1; color: @error_color; }
/* top-bar status: a small dot at rest, a spinner while working; the text lives in the tooltip */
.sill-status-slot { padding: 6px 8px; }
.sill-status { min-width: 8px; min-height: 8px; border-radius: 4px; background: alpha(@view_fg_color, 0.3); }
.sill-status.sill-attention { background: @warning_color; }
.sill-spinner { min-width: 15px; min-height: 15px; color: @accent_color; }
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
/* Changes page: flat rows, quiet buttons that show on hover, badges for where a change lives */
.sill-changes { background: transparent; }
.sill-changes > row { padding: 2px 6px; min-height: 0; border-radius: 0; }
.sill-changes > row:hover { background: alpha(@view_fg_color, 0.04); }
.sill-changes > row:selected { background: alpha(@accent_bg_color, 0.2); }
.sill-changes > row.sill-changes-header { padding-top: 10px; padding-bottom: 2px; border-bottom: 1px solid alpha(@view_fg_color, 0.12); }
.sill-changes > row.sill-changes-header:hover { background: transparent; }
.sill-change-btn { padding: 0 8px; min-height: 0; opacity: 0.35; font-size: 90%; }
.sill-changes > row:hover .sill-change-btn, .sill-changes > row:selected .sill-change-btn, .sill-changes-header:hover .sill-change-btn { opacity: 1; }
.sill-change-note { font-size: 85%; }
.sill-badge.sill-badge-git { color: @success_color; }
.sill-badge.sill-badge-live { color: @accent_color; }
.sill-badge.sill-badge-nogit { color: @warning_color; }
.sill-badge.sill-badge-kept { color: alpha(@success_color, 0.75); }
.sill-diff-frame { border-top: 1px solid alpha(@view_fg_color, 0.12); }
.sill-diff { font-size: 90%; }
.sill-files > row { padding: 1px 4px; min-height: 0; }
.sill-files > row:selected { background: alpha(@accent_bg_color, 0.25); }
.sill-files treeexpander { padding: 1px 0; }
.sill-file-icon { opacity: 0.7; -gtk-icon-size: 0.95em; }
.sill-changed { color: @warning_color; }
.sill-below { color: @warning_color; }
.sill-new { color: @success_color; }
.sill-pending { color: @accent_color; }
.sill-dim { opacity: 0.45; }
.sill-pinned { color: @accent_color; }
.sill-editor gutter { border: none; }
/* the "sill" toggle: quiet, underlined like a selected tab while the backend is open */
button.sill-action { padding: 2px 8px; min-height: 0; background: transparent; box-shadow: none; border-bottom: 2px solid transparent; }
button.sill-action:hover { background: alpha(@view_fg_color, 0.07); }
button.sill-action:checked { background: alpha(@view_fg_color, 0.09); border-bottom: 2px solid alpha(@accent_bg_color, 0.9); }
.sill-pins { padding: 6px 0; }
.sill-chip { background: alpha(@view_fg_color, 0.07); padding: 0 2px 0 0; }
.sill-chip-open { padding: 3px 8px; min-height: 0; }
.sill-apply { color: @warning_color; padding: 0 8px; min-height: 0; }
.sill-git-add { color: @success_color; font-size: 90%; }
.sill-git-del { color: @error_color; font-size: 90%; }
.sill-scope { padding: 2px 0; }
.sill-pin { padding: 0 2px; min-height: 0; min-width: 0; opacity: 0.4; -gtk-icon-size: 0.75em; }
.sill-pin:hover { opacity: 1; }
.sill-badge { font-size: 80%; color: alpha(@accent_color, 0.9); font-family: monospace; }
/* the ACP controls row (mode, model, reasoning, …): small, light and quiet until hovered */
.sill-controls { font-size: 80%; }
.sill-controls label { font-weight: normal; }
/* Everything in the row is sized in em — buttons, glyphs, gaps — and nothing in px: em follows the
   text scale and the display's font DPI, px only the integer scale factor, so px rows come out
   different on every machine. The glyphs are sill's own textures sized by -gtk-icon-size. */
.sill-controls > *, .sill-controls > box > * { margin-left: 0.3em; }
.sill-controls > *:first-child, .sill-controls > box > *:first-child { margin-left: 0; }
.sill-ctl-icon { min-width: 2em; min-height: 2em; padding: 0; opacity: 0.6; }
.sill-ctl-icon > image { -gtk-icon-size: 1.1em; }
.sill-ctl-icon:hover, .sill-ctl-icon:checked { opacity: 1; }
/* the lock keeps the row's colour when on: the state is the glyph, not a highlight */
button.sill-lock:checked { background: transparent; opacity: 0.9; }
.sill-controls .dim-label { opacity: 0.7; }
.sill-control { min-height: 0; padding: 0; color: alpha(@view_fg_color, 0.5); }
.sill-control:hover { color: alpha(@view_fg_color, 0.85); }
.sill-control > button { min-height: 2em; padding: 0 0.3em; background: transparent; border: none; box-shadow: none; color: inherit; }
.sill-control > button:hover { background: alpha(@view_fg_color, 0.07); color: @view_fg_color; }
.sill-control > button arrow { -gtk-icon-size: 0.6em; opacity: 0.45; }
switch.sill-control { min-width: 2.2em; min-height: 1.1em; opacity: 0.7; }
switch.sill-control:hover, switch.sill-control:checked { opacity: 1; }
switch.sill-control > slider { min-width: 0.9em; min-height: 0.9em; }
button.sill-send { min-width: 1.6em; min-height: 1.6em; padding: 0; background: alpha(@accent_bg_color, 0.25); }
button.sill-send > image { -gtk-icon-size: 1.1em; margin: 0; padding: 0; }
button.sill-send:hover { background: alpha(@accent_bg_color, 0.45); }
button.sill-send.sill-stop { min-width: 1.6em; min-height: 1.6em; background: alpha(@error_bg_color, 0.25); }
button.sill-send.sill-stop > image { -gtk-icon-size: 1em; }
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
