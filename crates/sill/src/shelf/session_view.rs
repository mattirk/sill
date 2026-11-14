//! One session tab: conversation + bottom area (prompt now; prompt/shell switcher in M3). Owns the
//! driver, persists the transcript, counts pending changes at turn end.

use std::collections::HashMap;
use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use sill_core::changes;
use gtk::gdk;
use gtk::glib;
use sill_core::acp::{Attachment, AttachmentKind, AuthMethodInfo, ConfigKind, ConfigValue, Controls, Event};
use sill_core::wire::{self, Command};
use sill_core::agents;
use sill_core::store::{Entry, Session, Store};

/// Entries rendered when a session opens; the rest come in chunks of [`HISTORY_CHUNK`] from the
/// "show earlier" row (SPEC §3.1 history: opening a long session costs the same as a short one).
const HISTORY_TAIL: usize = 60;
const HISTORY_CHUNK: usize = 120;

use super::conversation::Conversation;

/// Refused hosts the conversation asks about per session start; the rest go to the log (S7).
const MAX_DENIED_PROMPTS: usize = 8;
use super::viewer;
use super::{markdown, session, theme};
use vte4::prelude::*;

/// The session's overlay as the shelf sees it: (layers, masked paths), for resolving a path the
/// agent names to the file it sees.
type JailView = (Vec<sill_core::jail::Layer>, Vec<std::path::PathBuf>);

pub struct SessionView {
    pub root: gtk::Box,
    jail: RefCell<Option<JailView>>,
    prompt: gtk::TextView,
    /// What goes with the next prompt (SPEC §3.2 attachments) and the strip showing it.
    attachments: RefCell<Vec<Attachment>>,
    attach_strip: gtk::Box,
    /// The agent accepts image blocks (from `initialize`).
    images_ok: std::cell::Cell<bool>,
    /// Bottom slot: "prompt" or "shell" (SPEC §3.1 switcher).
    bottom: gtk::Stack,
    terminal: vte4::Terminal,
    term_scroll: gtk::ScrolledWindow,
    /// The shell's pty is opened on first use and kept while hidden.
    shell_open: std::cell::Cell<bool>,
    /// The person's shell on the host (Ctrl+Shift+H): the same pane, under a warning-coloured
    /// caption — a host process, outside the jail, unrecorded.
    host_term: vte4::Terminal,
    host_scroll: gtk::ScrolledWindow,
    host_open: std::cell::Cell<bool>,
    /// The terminal is running a login; when it closes the session is opened again.
    login_pending: Rc<std::cell::Cell<bool>>,
    /// `Ready` arrived on the current connection.
    ready: std::cell::Cell<bool>,
    /// The launcher-mode note was shown in this view (once per tab, SPEC §4.7).
    mode_noted: std::cell::Cell<bool>,
    /// The standing block (sill's own line, the person's instructions; `sill_core::instructions`)
    /// the current agent process has been sent: `None` after a (re)start so the next prompt carries
    /// it, compared on every send so an edited file reaches the agent with the next prompt.
    sent_standing: RefCell<Option<String>>,
    /// The connection died before `Ready` and the agent was restarted once already.
    startup_retried: std::cell::Cell<bool>,
    /// The jail must be rebuilt at the next restart even if the pins are unchanged (new login file).
    force_restart: std::cell::Cell<bool>,
    shell_btn: RefCell<Option<gtk::ToggleButton>>,
    /// Attach button and auto-allow lock: drawn glyphs, repainted on a theme change like the shell toggle.
    attach_btn: RefCell<Option<gtk::Button>>,
    lock_btn: RefCell<Option<gtk::ToggleButton>>,
    /// Context used of the model's window (tokens), from the agent's last usage update; shown in
    /// the controls row, hidden until the first one.
    usage: std::cell::Cell<Option<(u64, u64)>>,
    usage_lbl: RefCell<Option<gtk::Label>>,
    /// History not shown yet: the first shown entry's seq, and the "show earlier" row at the top.
    earlier: RefCell<Option<(i64, gtk::Box, gtk::Button)>>,
    /// Managed processes (docs/tasks.md §2.4, §2.5): the shim's list as last polled, the ports
    /// bridged to the host's loopback, the ports the person was asked about / said yes to, the
    /// indicator in the controls row and the poll timer.
    procs: RefCell<Vec<sill_core::shim::Proc>>,
    bridges: RefCell<HashMap<u16, super::processes::Bridge>>,
    port_asked: RefCell<std::collections::HashSet<u16>>,
    port_approved: RefCell<std::collections::HashSet<u16>>,
    proc_btn: RefCell<Option<gtk::Button>>,
    proc_tick: RefCell<Option<glib::SourceId>>,
    /// A log to show in the shelf's viewer: (title, source).
    pub on_open_log: RefCell<Option<Box<dyn Fn(&str, sill_core::session::LogSource)>>>,
    pins_row: gtk::Box,
    /// Agent controls (mode, model, effort, toggles) + send button, under the prompt.
    controls_row: gtk::Box,
    /// Set while the controls are being rebuilt so their change signals don't echo back.
    rebuilding: std::cell::Cell<bool>,
    /// The model the person just picked: when the agent reports it as current, the remembered mode
    /// and options are applied once more (the adapter re-seeds effort and may clamp the mode per model).
    reconcile_model: RefCell<Option<String>>,
    /// sill's own switch: answer permission requests with "allow once" without asking.
    auto_allow: std::cell::Cell<bool>,
    /// Send button; becomes a stop button while a turn is running.
    send_btn: RefCell<Option<gtk::Button>>,
    busy: std::cell::Cell<bool>,
    pub session: RefCell<Session>,
    pub conv: Rc<Conversation>,
    pub status: gtk::Label,
    driver: RefCell<Option<session::Driver>>,
    store: Rc<Store>,
    /// Markdown of the agent message being streamed (persisted at turn end).
    agent_md: RefCell<String>,
    /// The session is ending and its agent writes the summary (docs/memory.md §2.2): what gets the
    /// answer when the turn stops — or None when the agent is gone or too slow.
    summarizing: RefCell<Option<Box<dyn FnOnce(Option<String>)>>>,
    /// The running summary turn is *Summarize now* (docs/memory.md §2.5): the session goes on
    /// after it — unless the tab closes meanwhile, when the end adopts it (`adopt_summary`).
    checkpoint: std::cell::Cell<bool>,
    /// The summary turn's text so far, across its tool calls (each one flushes `agent_md`).
    summary_text: RefCell<String>,
    /// Prompts the person sent in this view's life, and whether the agent resumed its own context
    /// at Ready: together, whether the agent has anything to summarize (docs/memory.md decision 13).
    turns_this_run: std::cell::Cell<usize>,
    resumed: std::cell::Cell<bool>,
    /// After a resume with replay: ignore replayed events until the first turn starts.
    replaying: RefCell<bool>,
    /// Called when a turn ends (the window refreshes the ledger pages).
    pub on_busy: RefCell<Option<Box<dyn Fn(&str, bool)>>>,
    pub on_turn_end: RefCell<Option<Box<dyn Fn(&str)>>>,
    /// The agent wrote a file: the window refreshes the tree at once (the chips refresh here).
    pub on_disk_change: RefCell<Option<Box<dyn Fn()>>>,
    pub on_title: RefCell<Option<Box<dyn Fn(&str, &str)>>>,
    /// Click on a folder chip: the shelf shows that folder's tree beside the session.
    pub on_open_folder: RefCell<Option<Box<dyn Fn(&std::path::Path)>>>,
    /// A file to show in the shelf's viewer (a file chip, an image in a message); `full` = a pinned
    /// file, shown over the whole shelf with the tree closed.
    pub on_open_file: RefCell<Option<Box<dyn Fn(&std::path::Path)>>>,
    /// The terminal panes (`shell`, `host`): hosted by the window across its whole width while
    /// [`Self::pane_shown`] names one.
    pub pane: gtk::Stack,
    /// Shell toggle in the controls row (M3 wires the terminal pane to it).
    pub on_shell: RefCell<Option<Box<dyn Fn(bool)>>>,
    /// The shell button's right-click "terminal on the host": the window opens it (Ctrl+Shift+H) —
    /// the pane here, or (`true`) a separate window with the configured terminal.
    pub on_host_terminal: RefCell<Option<Box<dyn Fn(bool)>>>,
    /// Pins that were live when the jail started; changed pins need a session restart.
    live_pins: RefCell<Vec<std::path::PathBuf>>,
    /// Watches on pinned repos' `.git` (commits, index) so the chip numbers follow host activity too.
    git_monitors: RefCell<Vec<gtk::gio::FileMonitor>>,
    /// The chips as shown (pin path, the box holding the name and the git numbers): a refresh with
    /// the same pins updates the numbers in place instead of rebuilding.
    chips: RefCell<Vec<(std::path::PathBuf, gtk::Box)>>,
    /// Watches the session's runtime dir for `denied` (hosts the proxy refused).
    denied_monitor: RefCell<Option<gtk::gio::FileMonitor>>,
    denied_seen: RefCell<std::collections::HashSet<String>>,
    /// Refused hosts asked about since the session started; past `MAX_DENIED_PROMPTS` one note.
    denied_asked: std::cell::Cell<usize>,
    refresh_pending: std::cell::Cell<bool>,
    /// Rebuild this session's jail (the window: now, or after the turn). Behind an `Rc` and called
    /// through [`Self::request_restart`]: the callback may close this very view, whose `quit` takes
    /// the callbacks — a call made while the cell is borrowed is a crash (2026-11-01).
    pub on_restart: RefCell<Option<Rc<dyn Fn(&str)>>>,
    /// A start is under way on a worker thread (`connect`); a second call meanwhile runs once more
    /// after it (`Some(fresh)`); a prompt typed meanwhile waits here and goes when the agent is up.
    connecting: std::cell::Cell<bool>,
    connect_again: std::cell::Cell<Option<bool>>,
    pending_prompt: RefCell<Option<String>>,
}

impl SessionView {
    pub fn new(session: Session, store: Rc<Store>) -> Rc<Self> {
        let status = gtk::Label::new(Some("starting…"));
        status.add_css_class("dim-label");
        let conv = Conversation::new(status.clone());

        let prompt = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(10)
            .right_margin(10)
            .top_margin(8)
            .bottom_margin(8)
            .accepts_tab(false)
            .build();
        prompt.add_css_class("sill-prompt");
        let prompt_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .max_content_height(220)
            .propagate_natural_height(true)
            .child(&prompt)
            .margin_start(0)
            .margin_end(0)
            .margin_bottom(0)
            .build();
        prompt_scroll.add_css_class("sill-prompt-box");
        // the text view lays its lines out lazily, after the scrolled window has already measured
        // it, so a wrap that adds a line left the box a line short until the next relayout and
        // the new line clipped; worse, the view had scrolled to the cursor meanwhile and the
        // leftover scroll ate the top margin, the first line flush against the top edge. So the
        // box's minimum content height follows the content's own height — the last line's y-range
        // plus the margins, not the vadjustment's upper, which is max(content, allocation) and
        // so never shrinks once the box has grown (the box stayed tall after a send, 2026-11-02) —
        // up to the cap, and while the content fits the scroll is held at the top. Checked on
        // every signal the mismatch can show up on: the buffer changed (text in or out), upper
        // (lines validated), page-size (the box was allocated), value (the view scrolled); on 4.24
        // no single one is reliably last. The low-priority idle (markdown::settle_height) still
        // lands after GTK's validation for the final measure. SILL_PROMPT_DEBUG=1 traces it.
        {
            let sw = prompt_scroll.downgrade();
            let tv = prompt.downgrade();
            let debug = std::env::var_os("SILL_PROMPT_DEBUG").is_some();
            let fit = Rc::new(move |why: &str| {
                let (Some(sw), Some(tv)) = (sw.upgrade(), tv.upgrade()) else { return };
                let adj = tv.vadjustment().unwrap();
                let cap = sw.max_content_height();
                let (y, h) = tv.line_yrange(&tv.buffer().end_iter());
                let content = y + h + tv.top_margin() + tv.bottom_margin();
                let want = content.min(cap).max(0);
                if debug {
                    eprintln!(
                        "{} prompt {why}: content={content} upper={} page={} value={} min={} box={}",
                        super::trace_ts(), adj.upper(), adj.page_size(), adj.value(), sw.min_content_height(), sw.height()
                    );
                }
                if sw.min_content_height() != want {
                    sw.set_min_content_height(want);
                    sw.queue_resize();
                    super::markdown::settle_height(&sw);
                }
                if content <= cap && adj.value() != 0.0 {
                    adj.set_value(0.0);
                }
            });
            let adj = prompt.vadjustment().unwrap();
            let f = fit.clone();
            prompt.buffer().connect_changed(move |_| f("changed"));
            let f = fit.clone();
            adj.connect_upper_notify(move |_| f("upper"));
            let f = fit.clone();
            adj.connect_page_size_notify(move |_| f("page"));
            adj.connect_value_changed(move |_| fit("value"));
        }

        // no Box spacing: gaps are CSS margins in em, like every size in the row
        let controls_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        controls_row.add_css_class("sill-controls");
        controls_row.set_margin_start(8);
        controls_row.set_margin_end(8);
        controls_row.set_margin_top(4);
        controls_row.set_margin_bottom(4);
        // the person's shell inside the jail (SPEC §3.1): same mounts as the agent, own pty
        let (terminal, term_scroll) = Self::terminal_pane();
        // set while the pane runs a login (see shell_closed). Claude's code prompt never echoes
        // what it receives (a Bun readline quirk, the same in any terminal), so a paste into it is
        // acknowledged with a fading notice over the pane instead; the code itself stays unseen.
        let login_pending = Rc::new(std::cell::Cell::new(false));
        let toast = gtk::Label::new(None);
        toast.add_css_class("sill-toast");
        toast.set_halign(gtk::Align::End);
        toast.set_valign(gtk::Align::Start);
        toast.set_margin_top(6);
        toast.set_margin_end(10);
        toast.set_can_target(false);
        let show_toast = {
            let toast = toast.clone();
            move |text: &str| {
                toast.set_text(text);
                toast.add_css_class("sill-toast-show");
                let t = toast.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1800), move || {
                    t.remove_css_class("sill-toast-show");
                });
            }
        };
        // copy / paste: Ctrl+Shift+C / Ctrl+Shift+V as in every terminal, Ctrl+V pastes too (a
        // login code is pasted, not typed), Ctrl+C with a selection copies instead of interrupting;
        // a right-click menu for the mouse
        {
            // paste by reading the clipboard ourselves and feeding the text (bracketed paste and
            // newline handling stay VTE's); its own paste_clipboard() quietly did nothing here.
            // During a login the pane runs a line prompt that echoes keystrokes but shows nothing of
            // a pasted block (a bracketed paste arrives as one chunk): then the text is typed in,
            // one character at a time, so the person sees the code land.
            let paste_into = {
                let login_pending = login_pending.clone();
                let show_toast = show_toast.clone();
                move |t: &vte4::Terminal| {
                    let t = t.clone();
                    let in_login = login_pending.get();
                    let show_toast = show_toast.clone();
                    t.clipboard().read_text_async(None::<&gtk::gio::Cancellable>, move |res| {
                        let Ok(Some(text)) = res else { return };
                        if text.is_empty() {
                            return;
                        }
                        if !in_login {
                            t.paste_text(&text);
                            return;
                        }
                        // a login code: typed in one character at a time (a bracketed block is
                        // swallowed silently by the prompt) and acknowledged, not shown
                        let chars: Vec<String> = text.trim().chars().filter(|c| !c.is_control()).map(|c| c.to_string()).collect();
                        show_toast(&format!("pasted {} characters — press Enter", chars.len()));
                        let i = std::cell::Cell::new(0usize);
                        glib::timeout_add_local(std::time::Duration::from_millis(4), move || {
                            let n = i.get();
                            if n >= chars.len() {
                                return glib::ControlFlow::Break;
                            }
                            t.feed_child(chars[n].as_bytes());
                            i.set(n + 1);
                            glib::ControlFlow::Continue
                        });
                    });
                }
            };
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let t = terminal.clone();
            let paste = paste_into.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                let k = key.to_lower();
                if ctrl && (k == gdk::Key::v || (shift && key == gdk::Key::Insert)) {
                    paste(&t);
                    return glib::Propagation::Stop;
                }
                if ctrl && k == gdk::Key::c && (shift || t.has_selection()) {
                    t.copy_clipboard_format(vte4::Format::Text);
                    if !shift {
                        t.unselect_all();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            terminal.add_controller(keys);
            let menu = gtk::Popover::new();
            menu.add_css_class("sill-menu");
            menu.set_has_arrow(false);
            menu.set_parent(&terminal);
            let mbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let mut copy_btn = None;
            for (label, copy) in [("Copy", true), ("Paste", false)] {
                let b = gtk::Button::with_label(label);
                b.add_css_class("flat");
                b.add_css_class("sill-menu-item");
                if let Some(l) = b.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                    l.set_xalign(0.0);
                }
                let t = terminal.clone();
                let m = menu.clone();
                let paste = paste_into.clone();
                b.connect_clicked(move |_| {
                    m.popdown();
                    if copy {
                        t.copy_clipboard_format(vte4::Format::Text);
                    } else {
                        paste(&t);
                    }
                });
                if copy {
                    copy_btn = Some(b.clone());
                }
                mbox.append(&b);
            }
            menu.set_child(Some(&mbox));
            let right = gtk::GestureClick::new();
            right.set_button(3);
            let m = menu.clone();
            let t = terminal.clone();
            right.connect_pressed(move |g, _, x, y| {
                if let Some(c) = &copy_btn {
                    c.set_sensitive(t.has_selection());
                }
                m.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                m.popup();
                g.set_state(gtk::EventSequenceState::Claimed);
            });
            terminal.add_controller(right);
        }
        let bottom = gtk::Stack::new();
        // no transition: the prompt folds away in the same frame the terminal pane appears below,
        // one relayout the conversation follows to its end, instead of a slide on top of a jump
        bottom.set_transition_type(gtk::StackTransitionType::None);
        bottom.set_vhomogeneous(false);
        bottom.add_named(&prompt_scroll, Some("prompt"));
        // the terminals live in `pane`, which the window shows across its whole width under the
        // tree and the main pane; `bottom` keeps an empty page per terminal so its visible child
        // still says what the slot shows (the prompt folds away while a terminal is up)
        let pane = gtk::Stack::new();
        pane.set_vhomogeneous(false);
        let term_overlay = gtk::Overlay::new();
        term_overlay.set_child(Some(&term_scroll));
        term_overlay.add_overlay(&toast);
        // the jail shell under a caption and rule in the success colour: the counterpart of the
        // host shell's warning-coloured one, so the colour says at a glance which side a shell is on
        let term_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let term_caption = gtk::Label::new(Some("jail · inside the session"));
        term_caption.add_css_class("sill-jail-caption");
        term_caption.set_xalign(0.0);
        term_box.append(&term_caption);
        term_box.append(&term_overlay);
        pane.add_named(&term_box, Some("shell"));
        bottom.add_named(&gtk::Box::new(gtk::Orientation::Vertical, 0), Some("shell"));
        // the host shell: the same kind of pane, marked as host above it
        let (host_term, host_scroll) = Self::host_terminal_pane();
        let host_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let host_caption = gtk::Label::new(Some("host · outside the jail"));
        host_caption.add_css_class("sill-host-caption");
        host_caption.set_xalign(0.0);
        host_box.append(&host_caption);
        host_box.append(&host_scroll);
        pane.add_named(&host_box, Some("host"));
        bottom.add_named(&gtk::Box::new(gtk::Orientation::Vertical, 0), Some("host"));
        bottom.set_visible_child_name("prompt");

        let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        prompt_box.add_css_class("sill-prompt-box");
        prompt_scroll.remove_css_class("sill-prompt-box");
        let attach_strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        attach_strip.add_css_class("sill-attach-strip");
        attach_strip.set_visible(false);
        prompt_box.append(&attach_strip);
        prompt_box.append(&bottom);
        prompt_box.append(&controls_row);

        let pins_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pins_row.add_css_class("sill-pins");
        pins_row.set_margin_start(12);
        pins_row.set_margin_end(12);
        pins_row.set_margin_bottom(4);
        pins_row.set_visible(false);

        // the conversation with its prompt; the shelf lays its viewer over this and its tree beside it
        let chat = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat.append(&conv.root);
        chat.append(&prompt_box);
        chat.set_vexpand(true);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&pins_row);
        root.append(&chat);

        let auto_allow_initial = session.auto_allow;
        let view = Rc::new(Self {
            root,
            jail: RefCell::new(None),
            prompt: prompt.clone(),
            attachments: RefCell::new(Vec::new()),
            attach_strip,
            images_ok: std::cell::Cell::new(true),
            bottom,
            pane,
            terminal: terminal.clone(),
            term_scroll: term_scroll.clone(),
            shell_open: std::cell::Cell::new(false),
            host_term: host_term.clone(),
            host_scroll: host_scroll.clone(),
            host_open: std::cell::Cell::new(false),
            login_pending: login_pending.clone(),
            ready: std::cell::Cell::new(false),
            mode_noted: std::cell::Cell::new(false),
            sent_standing: RefCell::new(None),
            startup_retried: std::cell::Cell::new(false),
            force_restart: std::cell::Cell::new(false),
            shell_btn: RefCell::new(None),
            attach_btn: RefCell::new(None),
            lock_btn: RefCell::new(None),
            usage: std::cell::Cell::new(None),
            usage_lbl: RefCell::new(None),
            earlier: RefCell::new(None),
            procs: RefCell::new(Vec::new()),
            bridges: RefCell::new(HashMap::new()),
            port_asked: RefCell::new(Default::default()),
            port_approved: RefCell::new(Default::default()),
            proc_btn: RefCell::new(None),
            proc_tick: RefCell::new(None),
            on_open_log: RefCell::new(None),
            pins_row,
            controls_row,
            rebuilding: std::cell::Cell::new(false),
            reconcile_model: RefCell::new(None),
            auto_allow: std::cell::Cell::new(auto_allow_initial),
            send_btn: RefCell::new(None),
            busy: std::cell::Cell::new(false),
            session: RefCell::new(session),
            conv,
            status,
            driver: RefCell::new(None),
            store,
            agent_md: RefCell::new(String::new()),
            summarizing: RefCell::new(None),
            checkpoint: std::cell::Cell::new(false),
            summary_text: RefCell::new(String::new()),
            turns_this_run: std::cell::Cell::new(0),
            resumed: std::cell::Cell::new(false),
            replaying: RefCell::new(false),
            on_busy: RefCell::new(None),
            on_turn_end: RefCell::new(None),
            on_disk_change: RefCell::new(None),
            on_title: RefCell::new(None),
            on_open_folder: RefCell::new(None),
            on_open_file: RefCell::new(None),
            on_shell: RefCell::new(None),
            on_host_terminal: RefCell::new(None),
            live_pins: RefCell::new(Vec::new()),
            git_monitors: RefCell::new(Vec::new()),
            chips: RefCell::new(Vec::new()),
            denied_monitor: RefCell::new(None),
            denied_seen: RefCell::new(Default::default()),
            denied_asked: std::cell::Cell::new(0),
            refresh_pending: std::cell::Cell::new(false),
            on_restart: RefCell::new(None),
            connecting: std::cell::Cell::new(false),
            connect_again: std::cell::Cell::new(None),
            pending_prompt: RefCell::new(None),
        });

        // how messages find their images, and where a click on one goes — before the transcript is shown
        {
            let resolve_weak = Rc::downgrade(&view);
            let open_weak = Rc::downgrade(&view);
            view.conv.set_md_context(Rc::new(markdown::Ctx {
                resolve: Box::new(move |src| resolve_weak.upgrade()?.resolve_jail_path(src)),
                open: Some(Box::new(move |p| {
                    if let Some(v) = open_weak.upgrade() {
                        v.open_file(p);
                    }
                })),
            }));
        }

        // stored transcript: the tail now, earlier chunks when asked for
        {
            let id = view.session.borrow().id.clone();
            let total = view.store.entry_count(&id).unwrap_or(0);
            let tail = view.store.entries_before(&id, None, HISTORY_TAIL).unwrap_or_default();
            let first_seq = tail.first().map(|e| e.seq);
            view.render_entries(&tail);
            if let Some(first) = first_seq
                && total > tail.len()
            {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                row.set_halign(gtk::Align::Center);
                let b = gtk::Button::with_label("");
                b.add_css_class("flat");
                b.add_css_class("sill-earlier");
                row.append(&b);
                view.conv.prepend_top(&row);
                *view.earlier.borrow_mut() = Some((first, row, b.clone()));
                view.update_earlier(total - tail.len());
                let weak = Rc::downgrade(&view);
                b.connect_clicked(move |_| {
                    if let Some(v) = weak.upgrade() {
                        v.show_earlier();
                    }
                });
            }
        }

        // Enter sends, Shift+Enter inserts a newline
        let keys = gtk::EventControllerKey::new();
        {
            let weak = Rc::downgrade(&view);
            let prompt = prompt.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let Some(v) = weak.upgrade() else { return glib::Propagation::Proceed };
                // Ctrl+V with an image on the clipboard: attach it (text pastes as usual)
                if (key == gdk::Key::v || key == gdk::Key::V) && state.contains(gdk::ModifierType::CONTROL_MASK) {
                    let cb = prompt.clipboard();
                    if cb.formats().contains_type(gdk::Texture::static_type()) {
                        let v2 = v.clone();
                        cb.read_texture_async(None::<&gtk::gio::Cancellable>, move |res| {
                            if let Ok(Some(tex)) = res {
                                v2.attach_texture(&tex);
                            }
                        });
                        return glib::Propagation::Stop;
                    }
                    return glib::Propagation::Proceed;
                }
                if (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                    let buf = prompt.buffer();
                    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
                    if !text.is_empty() && v.send(&text) {
                        buf.set_text("");
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        prompt.add_controller(keys);

        view.refresh_pins();
        view.build_controls(&Controls::default());
        view.apply_terminal_theme(&theme::current());
        {
            let weak = Rc::downgrade(&view);
            theme::on_change(move |t| {
                if let Some(v) = weak.upgrade() {
                    v.apply_terminal_theme(t);
                    // drawn glyphs don't follow CSS: redraw them in the new foreground
                    if let Some(b) = v.shell_btn.borrow().as_ref() {
                        b.set_child(Some(&super::icons::scalable("sill-terminal", super::icons::fg_for(b, t))));
                    }
                    if let Some(b) = v.attach_btn.borrow().as_ref() {
                        b.set_child(Some(&super::icons::scalable("sill-plus", super::icons::fg_for(b, t))));
                    }
                    if let Some(b) = v.lock_btn.borrow().as_ref() {
                        b.set_child(Some(&lock_glyph(b.is_active(), super::icons::fg_for(b, t))));
                    }
                    v.set_busy(v.busy.get());
                    v.refresh_pins();
                }
            });
        }
        {
            // the shell toggle in the controls row drives the switcher
            let weak = Rc::downgrade(&view);
            *view.on_shell.borrow_mut() = Some(Box::new(move |on| {
                if let Some(v) = weak.upgrade() {
                    v.show_shell(on);
                }
            }));
        }
        {
            // the pty is foreign (the shell's pid is in the jail's namespace, VTE can't watch it), so
            // the shell going away shows up as EOF on the master, not as child-exited. Detach the dead
            // pty and drop back to the prompt; the next toggle opens a fresh shell.
            let weak = Rc::downgrade(&view);
            terminal.connect_eof(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.shell_closed();
                }
            });
        }
        {
            let weak = Rc::downgrade(&view);
            host_term.connect_child_exited(move |_, _| {
                if let Some(v) = weak.upgrade() {
                    v.host_shell_closed();
                }
            });
        }
        view.connect(false);
        view
    }

    /// Terminal colours from the theme palette: fg/bg plus the 16 ANSI slots mapped onto the theme's
    /// semantic colours (red=error, green=success, yellow=warning, blue=accent, cyan=link …).
    fn apply_terminal_theme(&self, t: &theme::Theme) {
        let font = theme::font();
        for term in [&self.terminal, &self.host_term] {
            term.set_font(Some(&gtk::pango::FontDescription::from_string(&format!("{} {}", font.mono, font.mono_pt()))));
            term.set_font_scale(theme::zoom());
            let parse = |c: &str| gtk::gdk::RGBA::parse(c).ok();
            let (Some(fg), Some(bg)) = (parse(t.fg), parse(t.bg)) else { continue };
            let slots = [
                t.surface, t.error, t.success, t.warning, t.accent, t.accent, t.link, t.fg, // normal 0–7
                t.fg_dim, t.error, t.success, t.warning, t.accent, t.accent, t.link, t.fg, // bright 8–15
            ];
            let palette: Vec<gtk::gdk::RGBA> = slots.iter().filter_map(|c| parse(c)).collect();
            if palette.len() == 16 {
                let refs: Vec<&gtk::gdk::RGBA> = palette.iter().collect();
                term.set_colors(Some(&fg), Some(&bg), &refs);
            } else {
                term.set_colors(Some(&fg), Some(&bg), &[]);
            }
            if let Some(cursor) = parse(t.accent) {
                term.set_color_cursor(Some(&cursor));
            }
        }
    }

    /// A terminal pane as both shells use it: VTE with the shelf's font and scale (VTE has its own
    /// font: it follows the text size by hand), 10k lines of scrollback, printed links open on the
    /// host with Ctrl+click (the jail has no browser, and logins start with a URL; 0x400 =
    /// PCRE2_MULTILINE, which VTE requires for match regexes), in a scrolled window that owns the
    /// height. Keys are the caller's: the jail pane pastes with login handling, the host pane plainly.
    fn terminal_pane() -> (vte4::Terminal, gtk::ScrolledWindow) {
        let term = vte4::Terminal::new();
        term.set_size_request(-1, 200);
        term.set_scrollback_lines(10_000);
        let font = theme::font();
        term.set_font(Some(&gtk::pango::FontDescription::from_string(&format!("{} {}", font.mono, font.mono_pt()))));
        term.set_font_scale(theme::zoom());
        term.set_scroll_on_output(false);
        term.set_scroll_on_keystroke(true);
        term.add_css_class("sill-terminal");
        if let Ok(re) = vte4::Regex::for_match(r#"https?://[^\s'"<>)\]]+"#, 0x400) {
            let tag = term.match_add_regex(&re, 0);
            term.match_set_cursor_name(tag, "pointer");
        }
        {
            let click = gtk::GestureClick::new();
            click.set_button(1);
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let t = term.clone();
            click.connect_pressed(move |g, _, x, y| {
                if !g.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                    return;
                }
                if let (Some(url), _) = t.check_match_at(x, y) {
                    gtk::UriLauncher::new(&url).launch(None::<&gtk::Window>, None::<&gtk::gio::Cancellable>, |_| {});
                    g.set_state(gtk::EventSequenceState::Claimed);
                }
            });
            term.add_controller(click);
        }
        let scroll = gtk::ScrolledWindow::builder()
            .child(&term)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .min_content_height(300)
            .build();
        (term, scroll)
    }

    /// The host shell's pane: [`Self::terminal_pane`] with plain Ctrl+Shift+C / Ctrl+Shift+V.
    fn host_terminal_pane() -> (vte4::Terminal, gtk::ScrolledWindow) {
        let (term, scroll) = Self::terminal_pane();
        {
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let t = term.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                let k = key.to_lower();
                if ctrl && shift && k == gdk::Key::v {
                    t.paste_clipboard();
                    return glib::Propagation::Stop;
                }
                if ctrl && shift && k == gdk::Key::c {
                    t.copy_clipboard_format(vte4::Format::Text);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            term.add_controller(keys);
        }
        (term, scroll)
    }

    /// Ctrl+Shift+H: the host shell pane, or back to the prompt when it is shown.
    pub fn toggle_host_shell(self: &Rc<Self>, cwd: Option<std::path::PathBuf>) {
        let on = self.bottom.visible_child_name().as_deref() != Some("host");
        self.show_host_shell(on, cwd);
    }

    /// The host shell in the bottom slot: a login shell on the host, started on first use in `cwd`
    /// (else the session's first pinned folder, else home) and kept while hidden. A host process by
    /// design — outside the jail, nothing it does is recorded; the pane says so above the terminal
    /// and the shell button turns the warning colour while it shows.
    pub fn show_host_shell(self: &Rc<Self>, on: bool, cwd: Option<std::path::PathBuf>) {
        if let Some(b) = self.shell_btn.borrow().as_ref() {
            let t = theme::current();
            let fg = super::icons::fg_for(b, &t);
            let color = if on { gtk::gdk::RGBA::parse(t.warning).unwrap_or(fg) } else { fg };
            b.set_child(Some(&super::icons::scalable("sill-terminal", color)));
        }
        if !on {
            if self.bottom.visible_child_name().as_deref() == Some("host") {
                self.bottom.set_visible_child_name("prompt");
                self.prompt.grab_focus();
            }
            return;
        }
        if !self.host_open.get() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            let git_env = format!("{}={}", changes::HOST_SHELL_GIT_ENV.0, changes::HOST_SHELL_GIT_ENV.1);
            let dir = cwd
                .or_else(|| self.store.pins(&self.id()).unwrap_or_default().into_iter().find(|p| p.is_dir()))
                .or_else(|| session::home().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            let weak = Rc::downgrade(self);
            self.host_term.spawn_async(
                vte4::PtyFlags::DEFAULT,
                Some(&dir.to_string_lossy()),
                &[&shell, "-l"],
                // SILL_HOST_SHELL: `sill restart` run here knows to step outside the shelf first;
                // the git env: the pin's .git/config is the agent's (S8)
                &["TERM=xterm-256color", "COLORTERM=truecolor", "SILL_HOST_SHELL=1", &git_env],
                glib::SpawnFlags::DEFAULT,
                || {},
                -1,
                None::<&gtk::gio::Cancellable>,
                move |res| {
                    let Some(v) = weak.upgrade() else { return };
                    if let Err(e) = res {
                        v.conv.add_note(&format!("host shell: {e}"));
                        v.host_open.set(false);
                        v.show_host_shell(false, None);
                    }
                },
            );
            self.host_open.set(true);
        }
        // a third of the window (the pane sits under the view now, so the view's own height shrinks)
        let h = self.root.root().map(|r| r.height()).unwrap_or(0);
        if h > 0 {
            self.host_scroll.set_min_content_height(h / 3);
        }
        self.bottom.set_visible_child_name("host");
        self.host_term.grab_focus();
    }

    /// The host shell exited: forget it and show the prompt again.
    fn host_shell_closed(self: &Rc<Self>) {
        self.host_open.set(false);
        self.host_term.set_pty(None);
        self.host_term.reset(true, true);
        self.show_host_shell(false, None);
    }

    /// Switch the bottom slot between prompt and shell; open the shell's pty on first use.
    pub fn show_shell(self: &Rc<Self>, on: bool) {
        if let Some(b) = self.shell_btn.borrow().as_ref()
            && b.is_active() != on
        {
            b.set_active(on);
        }
        if !on {
            self.bottom.set_visible_child_name("prompt");
            self.prompt.grab_focus();
            return;
        }
        if !self.shell_open.get() {
            match self.open_shell() {
                Ok(()) => self.shell_open.set(true),
                Err(e) => {
                    self.conv.add_note(&format!("shell: {e:#}"));
                    self.show_shell(false);
                    return;
                }
            }
        }
        // a third of the window (the pane sits under the view now, so the view's own height shrinks)
        let h = self.root.root().map(|r| r.height()).unwrap_or(0);
        if h > 0 {
            self.term_scroll.set_min_content_height(h / 3);
        }
        self.bottom.set_visible_child_name("shell");
        self.terminal.grab_focus();
    }

    /// The shell's pty hit EOF (exit, Ctrl+D): forget it and show the prompt again.
    fn shell_closed(self: &Rc<Self>) {
        self.shell_open.set(false);
        self.terminal.set_pty(None);
        self.terminal.reset(true, true);
        self.show_shell(false);
        if self.login_pending.replace(false) {
            self.status.set_text("retrying");
            self.command(Command::RetrySession);
        }
    }

    /// The shell button's right-click menu: a terminal on the host, and which one (the known
    /// terminals that are installed; `host_terminal` in `sill.toml`, free-form under Settings).
    fn fill_terminal_menu(self: &Rc<Self>, pop: &gtk::Popover) {
        let mbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let item = |label: &str| {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            b.add_css_class("sill-menu-item");
            if let Some(l) = b.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                l.set_xalign(0.0);
            }
            b
        };
        let open = item("Host shell here    Ctrl+Shift+H");
        open.set_tooltip_text(Some("The host's shell in this pane — a host process, outside the jail: nothing it does is recorded"));
        {
            let weak = Rc::downgrade(self);
            let p = pop.clone();
            open.connect_clicked(move |_| {
                p.popdown();
                let Some(v) = weak.upgrade() else { return };
                if let Some(cb) = v.on_host_terminal.borrow().as_ref() {
                    cb(false);
                }
            });
        }
        mbox.append(&open);
        let cfg = sill_core::config::load();
        let current = cfg.host_terminal();
        let window = item("Host terminal in a window");
        window.set_sensitive(current.is_some());
        window.set_tooltip_text(Some(&match &current {
            Some(c) => format!("Runs: {c}"),
            None => "No terminal found on the host; set one under Settings → Host terminal".to_string(),
        }));
        {
            let weak = Rc::downgrade(self);
            let p = pop.clone();
            window.connect_clicked(move |_| {
                p.popdown();
                let Some(v) = weak.upgrade() else { return };
                if let Some(cb) = v.on_host_terminal.borrow().as_ref() {
                    cb(true);
                }
            });
        }
        mbox.append(&window);
        let installed = sill_core::config::installed_terminals();
        // a hand-written command line (Settings) that isn't one of the known ones
        let custom = cfg.host_terminal.as_deref().map(str::trim).is_some_and(|t| !t.is_empty() && !installed.iter().any(|(_, c)| *c == t));
        if !installed.is_empty() || custom {
            let head = gtk::Label::new(Some("which terminal, in a window"));
            head.add_css_class("dim-label");
            head.set_xalign(0.0);
            head.set_margin_start(10);
            head.set_margin_top(6);
            mbox.append(&head);
        }
        let mut group: Option<gtk::CheckButton> = None;
        for (prog, cmd) in installed {
            let b = gtk::CheckButton::with_label(prog);
            b.add_css_class("sill-menu-item");
            b.set_group(group.as_ref());
            b.set_active(!custom && current.as_deref() == Some(cmd));
            let p = pop.clone();
            b.connect_toggled(move |b| {
                // connected after set_active: only the person's choice is saved
                if b.is_active() {
                    let _ = sill_core::config::update(|c| c.host_terminal = Some(cmd.to_string()));
                    p.popdown();
                }
            });
            mbox.append(&b);
            if group.is_none() {
                group = Some(b);
            }
        }
        if custom {
            let b = gtk::CheckButton::with_label("as set in Settings");
            b.add_css_class("sill-menu-item");
            b.set_group(group.as_ref());
            b.set_active(true);
            b.set_sensitive(false);
            mbox.append(&b);
        }
        pop.set_child(Some(&mbox));
    }

    pub fn toggle_shell(self: &Rc<Self>) {
        let on = self.bottom.visible_child_name().as_deref() != Some("shell");
        self.show_shell(on);
    }

    fn open_shell(&self) -> anyhow::Result<()> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        self.open_pty(&[shell, "-l".into()], &std::collections::BTreeMap::new())
    }

    /// Run `argv` on a fresh pty inside the jail, shown in the terminal pane.
    fn open_pty(&self, argv: &[String], env: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let sock = sill_core::session::socket_path(&self.id())?;
        let mut c = sill_core::shim::Control::connect(&sock).map_err(|_| anyhow::anyhow!("session is not running"))?;
        let home = session::home()?;
        let cwd = self.store.pins(&self.id()).unwrap_or_default().into_iter().find(|p| p.is_dir()).unwrap_or(home);
        let cols = self.terminal.column_count().max(20) as u16;
        let rows = self.terminal.row_count().max(5) as u16;
        // the pane is a VTE terminal: say so, whatever the jail's default is
        let mut env = env.clone();
        env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into()).or_insert_with(|| "truecolor".into());
        let env = &env;
        let (fd, _pid) = c.open_pty(argv, env, Some(&cwd), cols, rows)?;
        let pty = vte4::Pty::foreign_sync(fd, None::<&gtk::gio::Cancellable>)?;
        self.terminal.set_pty(Some(&pty));
        self.terminal.feed(b"\x1b[2J\x1b[H");
        Ok(())
    }

    /// The person picked a login method. Agent-side methods go through the protocol; terminal
    /// methods run the agent's own command in the pane (Claude: `--cli auth login …`), and the
    /// session is opened again when that exits.
    fn authenticate(self: &Rc<Self>, m: AuthMethodInfo) {
        let home = session::home().unwrap_or_default();
        let agent_id = self.session.borrow().agent.clone();
        let Some(agent) = agents::load(&home).remove(&agent_id) else {
            self.conv.add_note(&format!("unknown agent {agent_id}"));
            return;
        };
        // Host-side: the curated methods, and every protocol-level method of an agent installed
        // from the registry. Those agents aren't described (no home paths, no method list), and a
        // browser flow with a localhost callback silently hangs inside the jail; on the host it
        // opens the browser and writes its login under ~ where every session's overlay sees it.
        let unknown_agent = agent.home_paths.is_empty();
        if agent.host_login.contains(&m.id) || (unknown_agent && m.terminal.is_none()) {
            self.login_on_host(agent, m);
            return;
        }
        let Some((args, env)) = m.terminal else {
            self.command(Command::Authenticate(m.id));
            return;
        };
        let mut argv = agent.command.clone();
        argv.extend(args);
        let mut e = agent.env.clone();
        e.extend(env);
        self.terminal.set_pty(None);
        match self.open_pty(&argv, &e) {
            Ok(()) => {
                self.shell_open.set(true);
                self.login_pending.set(true);
                self.conv.add_note("finish the login in the terminal below (Ctrl+click opens a link); the session continues when it closes");
                self.show_shell(true);
            }
            Err(err) => self.conv.add_note(&format!("login: {err:#}")),
        }
    }

    /// Browser login with a localhost callback (Codex ChatGPT): run the agent on the host for the
    /// login only (`sill_core::login`), then rebuild the jail so the new login file is bound in.
    fn login_on_host(self: &Rc<Self>, agent: agents::Agent, m: AuthMethodInfo) {
        self.status.set_text("logging in");
        self.conv.add_note(&format!("{}: the login runs on this machine; finish it in the browser that opens", m.name));
        let (tx, rx) = async_channel::unbounded::<Result<String, String>>();
        let home = session::home().unwrap_or_default();
        let method = m.id.clone();
        std::thread::Builder::new()
            .name("sill-login".into())
            .spawn(move || {
                let progress = tx.clone();
                // the agent's stderr is noise (browser chatter) unless the login fails
                let mut stderr: std::collections::VecDeque<String> = std::collections::VecDeque::new();
                let r = sill_core::login::run(&agent, &home, &method, |line| match line {
                    sill_core::login::Line::Status(t) => {
                        let _ = progress.send_blocking(Ok(t));
                    }
                    sill_core::login::Line::Stderr(t) => {
                        if stderr.len() >= 12 {
                            stderr.pop_front();
                        }
                        stderr.push_back(t);
                    }
                });
                let _ = tx.send_blocking(r.map(|()| String::new()).map_err(|e| {
                    let tail: Vec<String> = stderr.into_iter().collect();
                    if tail.is_empty() { format!("{e:#}") } else { format!("{e:#}\n{}", tail.join("\n")) }
                }));
            })
            .ok();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                let Some(v) = weak.upgrade() else { break };
                match msg {
                    Ok(line) if !line.is_empty() => v.conv.add_note(&line),
                    Ok(_) => {
                        v.conv.add_note("logged in; the session starts again with the new login");
                        v.status.set_text("logged in");
                        v.force_restart.set(true);
                        v.request_restart();
                        break;
                    }
                    Err(e) => {
                        v.conv.add_note(&format!("login failed: {e}"));
                        v.status.set_text("login needed");
                        v.command(Command::RetrySession);
                        break;
                    }
                }
            }
        });
    }

    /// Remember a control change twice (SPEC §3.2): for this session, which keeps it when reopened,
    /// and as the default of its agent type, which the next new session starts from.
    fn remember(&self, f: impl Fn(&mut sill_core::store::AgentSettings)) {
        let agent = {
            let mut s = self.session.borrow_mut();
            f(&mut s.agent_settings);
            if let Err(e) = self.store.set_session_settings(&s.id, &s.agent_settings) {
                eprintln!("sill: could not remember session {} settings: {e:#}", s.id);
            }
            s.agent.clone()
        };
        let mut d = self.store.agent_settings(&agent);
        f(&mut d);
        if let Err(e) = self.store.set_agent_settings(&agent, &d) {
            eprintln!("sill: could not remember {agent} default settings: {e:#}");
        }
    }

    fn command(&self, c: Command) {
        if std::env::var_os("SILL_TRACE").is_some() && matches!(c, Command::SetMode(_) | Command::SetConfig(..)) {
            eprintln!("{} command {c:?}", super::trace_ts());
        }
        if let Some(d) = self.driver.borrow().as_ref()
            && let Err(e) = d.commands.try_send(c)
        {
            eprintln!("sill: command dropped, the session loop is gone: {e}");
        }
    }

    /// Bring the agent to the controls this session remembers, where they differ from what it
    /// reports and are still on offer. Model first: the adapter re-seeds effort and may clamp the
    /// mode for the model, so mode and the rest are set against the model they will run on.
    fn apply_remembered(&self, c: &Controls) {
        let remembered = self.session.borrow().agent_settings.clone();
        if std::env::var_os("SILL_TRACE").is_some() {
            let shown: Vec<String> = c
                .options
                .iter()
                .map(|o| match &o.kind {
                    ConfigKind::Select(cur, _) => format!("{}={cur}", o.id),
                    ConfigKind::Boolean(b) => format!("{}={b}", o.id),
                })
                .collect();
            eprintln!(
                "{} apply remembered mode={:?} config={:?} to mode={:?} {}",
                super::trace_ts(),
                remembered.mode,
                remembered.config,
                c.modes.as_ref().map(|(cur, _)| cur),
                shown.join(" ")
            );
        }
        let want_option = |opt: &sill_core::acp::ConfigOption| -> Option<Command> {
            let want = remembered.config.get(&opt.id)?;
            match &opt.kind {
                ConfigKind::Select(current, values) if want != current && values.iter().any(|(id, _)| id == want) => {
                    Some(Command::SetConfig(opt.id.clone(), ConfigValue::Id(want.clone())))
                }
                ConfigKind::Boolean(current) if (want == "true") != *current => {
                    Some(Command::SetConfig(opt.id.clone(), ConfigValue::Bool(want == "true")))
                }
                _ => None,
            }
        };
        let (models, others): (Vec<_>, Vec<_>) = c.options.iter().partition(|o| o.category == "model");
        for cmd in models.into_iter().filter_map(want_option) {
            self.command(cmd);
        }
        if let (Some(mode), Some((current, modes))) = (&remembered.mode, &c.modes)
            && mode != current
            && modes.iter().any(|(id, _)| id == mode)
        {
            self.command(Command::SetMode(mode.clone()));
        }
        for cmd in others.into_iter().filter_map(want_option) {
            self.command(cmd);
        }
    }

    /// Rebuild the controls row from what the agent advertises: a dropdown per mode set / select
    /// option, a switch per boolean, and the send button on the right.
    fn build_controls(self: &Rc<Self>, c: &Controls) {
        self.rebuilding.set(true);
        while let Some(w) = self.controls_row.first_child() {
            self.controls_row.remove(&w);
        }
        // left: attach (later: file upload) and the shell toggle (M3); everything else sits right
        let fg = super::icons::fg_for(&self.controls_row, &theme::current());
        let attach = gtk::Button::new();
        attach.set_child(Some(&super::icons::scalable("sill-plus", fg)));
        attach.add_css_class("flat");
        attach.add_css_class("sill-ctl-icon");
        attach.set_tooltip_text(Some("Attach a file or image to the next prompt (Ctrl+V pastes a screenshot)"));
        {
            let weak = Rc::downgrade(self);
            attach.connect_clicked(move |b| {
                let Some(v) = weak.upgrade() else { return };
                let dialog = gtk::FileDialog::builder().title("Attach to the prompt").modal(true).build();
                let parent = b.root().and_then(|r| r.downcast::<gtk::Window>().ok());
                let v = v.clone();
                dialog.open(parent.as_ref(), None::<&gtk::gio::Cancellable>, move |res| {
                    if let Ok(f) = res
                        && let Some(path) = f.path()
                    {
                        v.attach_path(&path);
                    }
                });
            });
        }
        self.controls_row.append(&attach);
        *self.attach_btn.borrow_mut() = Some(attach);
        let term = gtk::ToggleButton::new();
        term.set_child(Some(&super::icons::scalable("sill-terminal", fg)));
        term.add_css_class("flat");
        term.add_css_class("sill-ctl-icon");
        term.set_tooltip_text(Some("Shell inside this session's jail (Ctrl+`) · right-click: a terminal on the host"));
        term.set_active(self.bottom.visible_child_name().as_deref() == Some("shell"));
        *self.shell_btn.borrow_mut() = Some(term.clone());
        {
            let weak = Rc::downgrade(self);
            term.connect_toggled(move |t| {
                let Some(v) = weak.upgrade() else { return };
                if let Some(cb) = v.on_shell.borrow().as_ref() {
                    cb(t.is_active());
                }
            });
        }
        {
            // right-click: a terminal on the host, and which one
            let menu = gtk::Popover::new();
            menu.add_css_class("sill-menu");
            menu.set_has_arrow(false);
            menu.set_parent(&term);
            let weak = Rc::downgrade(self);
            menu.connect_show(move |pop| {
                if let Some(v) = weak.upgrade() {
                    v.fill_terminal_menu(pop);
                }
            });
            let right = gtk::GestureClick::new();
            right.set_button(3);
            let m = menu.clone();
            right.connect_pressed(move |g, _, _, _| {
                m.popup();
                g.set_state(gtk::EventSequenceState::Claimed);
            });
            term.add_controller(right);
        }
        self.controls_row.append(&term);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        self.controls_row.append(&spacer);
        // the agent's and sill's controls: their dropdowns ellipsize (`compact_dropdown`), so on a
        // narrow shelf the row truncates long names instead of setting the window's minimum width
        let right = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        right.set_halign(gtk::Align::End);
        right.set_valign(gtk::Align::Center);
        let dropdown = |names: &[String], current: usize| {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let d = gtk::DropDown::from_strings(&refs);
            compact_dropdown(&d);
            d.add_css_class("sill-control");
            d.set_selected(current as u32);
            d
        };
        // a current value the agent reports but does not offer (a resumed out-of-picker model) gets
        // its own row rather than the first entry's name
        let with_current = |current: &str, values: &[(String, String)]| -> Vec<(String, String)> {
            let mut v = values.to_vec();
            if !v.iter().any(|(id, _)| id == current) {
                v.push((current.to_string(), current.to_string()));
            }
            v
        };
        if let Some((current, modes)) = &c.modes {
            let modes = with_current(current, modes);
            let names: Vec<String> = modes.iter().map(|(_, n)| n.clone()).collect();
            let cur = modes.iter().position(|(id, _)| id == current).unwrap_or(0);
            let d = dropdown(&names, cur);
            d.set_tooltip_text(Some("Mode"));
            let weak = Rc::downgrade(self);
            let ids: Vec<String> = modes.iter().map(|(id, _)| id.clone()).collect();
            d.connect_selected_notify(move |d| {
                let Some(v) = weak.upgrade() else { return };
                if v.rebuilding.get() {
                    return;
                }
                if let Some(id) = ids.get(d.selected() as usize) {
                    v.command(Command::SetMode(id.clone()));
                    v.remember(|s| s.mode = Some(id.clone()));
                }
            });
            right.append(&d);
        }
        for opt in &c.options {
            // the mode set already covers a "mode"-category option (Claude Code sends both)
            if opt.category == "mode" && c.modes.is_some() {
                continue;
            }
            match &opt.kind {
                ConfigKind::Select(current, values) => {
                    let values = with_current(current, values);
                    let names: Vec<String> = values.iter().map(|(_, n)| n.clone()).collect();
                    let cur = values.iter().position(|(id, _)| id == current).unwrap_or(0);
                    let d = dropdown(&names, cur);
                    d.set_tooltip_text(Some(&opt.name));
                    let weak = Rc::downgrade(self);
                    let ids: Vec<String> = values.iter().map(|(id, _)| id.clone()).collect();
                    let oid = opt.id.clone();
                    let is_model = opt.category == "model";
                    d.connect_selected_notify(move |d| {
                        let Some(v) = weak.upgrade() else { return };
                        if v.rebuilding.get() {
                            return;
                        }
                        if let Some(id) = ids.get(d.selected() as usize) {
                            v.command(Command::SetConfig(oid.clone(), ConfigValue::Id(id.clone())));
                            if is_model {
                                *v.reconcile_model.borrow_mut() = Some(id.clone());
                            }
                            let (o, i) = (oid.clone(), id.clone());
                            v.remember(move |s| {
                                s.config.insert(o.clone(), i.clone());
                            });
                        }
                    });
                    right.append(&d);
                }
                ConfigKind::Boolean(on) => {
                    let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
                    let l = gtk::Label::new(Some(&opt.name));
                    l.add_css_class("dim-label");
                    let sw = gtk::Switch::new();
                    sw.set_active(*on);
                    sw.add_css_class("sill-control");
                    sw.set_valign(gtk::Align::Center);
                    let weak = Rc::downgrade(self);
                    let oid = opt.id.clone();
                    sw.connect_state_set(move |_, state| {
                        let Some(v) = weak.upgrade() else { return glib::Propagation::Proceed };
                        if !v.rebuilding.get() {
                            v.command(Command::SetConfig(oid.clone(), ConfigValue::Bool(state)));
                            let o = oid.clone();
                            v.remember(move |s| {
                                s.config.insert(o.clone(), state.to_string());
                            });
                        }
                        glib::Propagation::Proceed
                    });
                    b.append(&l);
                    b.append(&sw);
                    right.append(&b);
                }
            }
        }
        // managed processes: how many run (warning colour when one failed); the list on click
        {
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("sill-ctl-icon");
            b.add_css_class("sill-procs");
            b.set_visible(false);
            let menu = gtk::Popover::new();
            menu.add_css_class("sill-menu");
            menu.set_has_arrow(false);
            menu.set_parent(&b);
            let weak = Rc::downgrade(self);
            menu.connect_show(move |pop| {
                if let Some(v) = weak.upgrade() {
                    v.fill_proc_menu(pop);
                }
            });
            let m = menu.clone();
            b.connect_clicked(move |_| m.popup());
            right.append(&b);
            *self.proc_btn.borrow_mut() = Some(b);
            self.show_procs();
        }
        // context used, when the agent reports it
        {
            let l = gtk::Label::new(None);
            l.add_css_class("sill-usage");
            l.set_visible(false);
            right.append(&l);
            *self.usage_lbl.borrow_mut() = Some(l);
            self.show_usage();
        }
        // sill's own: auto-allow permission requests (the agent's "auto" modes still ask over ACP).
        // An open lock = requests are answered "allow once" by sill; closed = they wait for you.
        {
            let t = gtk::ToggleButton::new();
            t.add_css_class("flat");
            t.add_css_class("sill-ctl-icon");
            t.add_css_class("sill-lock");
            t.set_active(self.auto_allow.get());
            // the same glyph size in both states, so the swap doesn't move anything
            let set_look = move |t: &gtk::ToggleButton| {
                t.set_child(Some(&lock_glyph(t.is_active(), fg)));
                if t.is_active() {
                    t.set_tooltip_text(Some("Auto-allow is on: permission requests are answered \"allow once\" (each is still listed)"));
                } else {
                    t.set_tooltip_text(Some("Auto-allow is off: permission requests wait for your answer"));
                }
            };
            set_look(&t);
            let weak = Rc::downgrade(self);
            t.connect_toggled(move |t| {
                set_look(t);
                let Some(v) = weak.upgrade() else { return };
                let state = t.is_active();
                v.auto_allow.set(state);
                let _ = v.store.set_auto_allow(&v.id(), state);
                v.session.borrow_mut().auto_allow = state;
            });
            *self.lock_btn.borrow_mut() = Some(t.clone());
            right.append(&t);
        }
        self.controls_row.append(&right);
        let send = gtk::Button::new();
        send.add_css_class("sill-send");
        send.set_valign(gtk::Align::Center); // its own square, not the row's height
        {
            let weak = Rc::downgrade(self);
            send.connect_clicked(move |_| {
                let Some(v) = weak.upgrade() else { return };
                if v.busy.get() {
                    v.cancel();
                    return;
                }
                let buf = v.prompt.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
                if !text.is_empty() && v.send(&text) {
                    buf.set_text("");
                }
            });
        }
        self.controls_row.append(&send);
        *self.send_btn.borrow_mut() = Some(send);
        self.set_busy(self.busy.get());
        self.rebuilding.set(false);
    }

    /// Stored entries onto the conversation, in order.
    fn render_entries(&self, entries: &[Entry]) {
        for e in entries {
            match e.kind.as_str() {
                "user" => self.conv.add_user_with(&e.text, &e.extra.lines().filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>()),
                "agent" => self.conv.add_agent_block(&e.text),
                "thought" => self.conv.add_thought_block(&e.text),
                "tool" => self.conv.add_tool_call_full(
                    format!("stored-{}", e.seq),
                    &e.text,
                    &e.extra,
                    (!e.tool_kind.is_empty()).then_some(e.tool_kind.as_str()),
                    (!e.detail.is_empty()).then_some(e.detail.as_str()),
                    None,
                    (!e.diff.is_empty()).then_some(e.diff.as_str()),
                ),
                _ => self.conv.add_note(&e.text),
            }
        }
    }

    fn update_earlier(&self, left: usize) {
        if let Some((_, _, b)) = self.earlier.borrow().as_ref() {
            b.set_label(&format!("show earlier ({left} more)"));
        }
    }

    /// The chunk of history before what is shown, rendered above it; the row goes when none is left.
    fn show_earlier(self: &Rc<Self>) {
        let Some((first, row, _)) = self.earlier.borrow().clone() else { return };
        let id = self.id();
        let chunk = self.store.entries_before(&id, Some(first), HISTORY_CHUNK).unwrap_or_default();
        let Some(new_first) = chunk.first().map(|e| e.seq) else { return };
        self.conv.begin_insert_after(&row);
        self.render_entries(&chunk);
        self.conv.end_insert();
        let left = self.store.entries_before(&id, Some(new_first), usize::MAX).map(|v| v.len()).unwrap_or(0);
        if left > 0 {
            if let Some(e) = self.earlier.borrow_mut().as_mut() {
                e.0 = new_first;
            }
            self.update_earlier(left);
        } else {
            self.conv.remove_top(&row);
            self.earlier.borrow_mut().take();
        }
    }

    /// The usage label from the last usage update: percent on the row, tokens in the tooltip.
    fn show_usage(&self) {
        let Some(l) = self.usage_lbl.borrow().clone() else { return };
        match self.usage.get() {
            Some((used, size)) if size > 0 => {
                let pct = (used * 100 / size).min(999);
                l.set_text(&format!("{pct}%"));
                let k = |n: u64| if n >= 1000 { format!("{}k", n / 1000) } else { n.to_string() };
                l.set_tooltip_text(Some(&format!("context: {} of {} tokens used", k(used), k(size))));
                if pct >= 80 {
                    l.add_css_class("sill-usage-high");
                } else {
                    l.remove_css_class("sill-usage-high");
                }
                l.set_visible(true);
            }
            _ => l.set_visible(false),
        }
    }

    /// Flip the send button between "send" and "stop" (SPEC: one button, red while running).
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        if let Some(b) = self.send_btn.borrow().as_ref() {
            if busy {
                // drawn glyphs (not text on a baseline) sit dead centre and size with the button
                let t = theme::current();
                let red = gtk::gdk::RGBA::parse(t.error).unwrap_or_else(|_| super::icons::fg_for(b, &t));
                b.set_child(Some(&super::icons::scalable("sill-stop", red)));
                b.set_tooltip_text(Some("Stop the agent"));
                b.add_css_class("sill-stop");
            } else {
                b.set_child(Some(&super::icons::scalable("sill-send", super::icons::fg_for(b, &theme::current()))));
                b.set_tooltip_text(Some("Send (Enter)"));
                b.remove_css_class("sill-stop");
            }
        }
    }

    /// Ask the agent to stop the running turn.
    pub fn cancel(&self) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Cancel);
        }
    }

    /// Coalesced refresh of the chips (file monitors fire in bursts).
    fn refresh_pins_soon(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let Some(v) = weak.upgrade() else { return };
            v.refresh_pending.set(false);
            v.refresh_pins();
        });
    }

    /// The agent wrote a file (a tool call with a diff): the chips' git numbers and the tree follow
    /// now, not at the end of the turn.
    fn disk_changed(self: &Rc<Self>) {
        self.refresh_pins_soon();
        if let Some(cb) = self.on_disk_change.borrow().as_ref() {
            cb();
        }
    }

    /// Chips under the session bar, one per pinned folder, with an unpin ×.
    pub fn refresh_pins(self: &Rc<Self>) {
        let pins = self.store.pins(&self.id()).unwrap_or_default();
        // the pins the chips already show: only their git numbers are refreshed, in place — a
        // rebuild every time flickered, the numbers arriving a moment after the names
        let same = {
            let chips = self.chips.borrow();
            chips.len() == pins.len() && chips.iter().zip(&pins).all(|((p, _), q)| p == q)
        };
        if same {
            self.refresh_chip_numbers();
            return;
        }
        while let Some(c) = self.pins_row.first_child() {
            self.pins_row.remove(&c);
        }
        // watch each pinned repo's .git (index/HEAD/refs change on commit, checkout, host or jail)
        self.git_monitors.borrow_mut().clear();
        for p in &pins {
            let git = p.join(".git");
            if !git.is_dir() {
                continue;
            }
            for dir in [git.clone(), git.join("refs/heads")] {
                if let Ok(m) = gtk::gio::File::for_path(&dir).monitor_directory(gtk::gio::FileMonitorFlags::NONE, None::<&gtk::gio::Cancellable>) {
                    let weak = Rc::downgrade(self);
                    m.connect_changed(move |_, _, _, _| {
                        if let Some(v) = weak.upgrade() {
                            v.refresh_pins_soon();
                        }
                    });
                    self.git_monitors.borrow_mut().push(m);
                }
            }
        }
        self.pins_row.set_visible(!pins.is_empty());
        if pins.is_empty() {
            self.chips.borrow_mut().clear();
            return;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let mut summaries: Vec<(std::path::PathBuf, gtk::Box)> = Vec::new();
        for p in pins {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            chip.add_css_class("sill-chip");
            let l = gtk::Button::new();
            let lbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let is_dir = p.is_dir();
            let shown = p.to_string_lossy().replace(&home, "~");
            // a glyph says folder or file, the name alone says which; the whole path is the tooltip
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(shown.clone());
            let glyph = super::icons::glyph_image(if is_dir { "sill-files" } else { "sill-file" });
            glyph.add_css_class("sill-chip-glyph");
            lbox.append(&glyph);
            lbox.append(&gtk::Label::new(Some(&name)));
            summaries.push((p.clone(), lbox.clone()));
            l.set_child(Some(&lbox));
            l.add_css_class("flat");
            l.add_css_class("sill-chip-open");
            l.set_tooltip_text(Some(&if is_dir {
                format!("{shown} · show the folder's tree · +lines −lines vs HEAD, ?untracked files")
            } else {
                format!("{shown} · open the file (Escape returns) · +lines −lines vs HEAD, ? = untracked")
            }));
            {
                let weak = Rc::downgrade(self);
                let p = p.clone();
                l.connect_clicked(move |_| {
                    let Some(v) = weak.upgrade() else { return };
                    if p.is_dir() {
                        if let Some(cb) = v.on_open_folder.borrow().as_ref() {
                            cb(&p);
                        }
                    } else {
                        v.open_file(&p);
                    }
                });
            }
            let x = super::icons::glyph_button("sill-x");
            x.add_css_class("flat");
            x.add_css_class("sill-tab-close");
            let weak = Rc::downgrade(self);
            x.connect_clicked(move |_| {
                let Some(v) = weak.upgrade() else { return };
                let _ = v.store.unpin(&v.id(), &p);
                v.refresh_pins();
                // a folder pin is a mount: the jail is rebuilt (after the current turn if busy);
                // a file pin is only a chip
                if v.pins_changed() {
                    v.request_restart();
                }
            });
            chip.append(&l);
            chip.append(&x);
            self.pins_row.append(&chip);
        }
        *self.chips.borrow_mut() = summaries;
        self.refresh_chip_numbers();
    }

    /// The git numbers next to each chip's name: +added −deleted (vs HEAD), untracked count. git
    /// runs off the main thread (it walks the repo; on the main thread it stalled painting and
    /// scrolling for seconds); the labels are set in place and only when they changed. A chip box
    /// no longer in the row is a late result for chips that were rebuilt meanwhile.
    fn refresh_chip_numbers(self: &Rc<Self>) {
        let chips: Vec<(std::path::PathBuf, gtk::Box)> = self.chips.borrow().clone();
        if chips.is_empty() {
            return;
        }
        let (tx, rx) = async_channel::bounded::<Vec<Option<(u64, u64, u64)>>>(1);
        let paths: Vec<std::path::PathBuf> = chips.iter().map(|(p, _)| p.clone()).collect();
        std::thread::spawn(move || {
            let found: Vec<Option<(u64, u64, u64)>> = paths.iter().map(|p| git_summary(p)).collect();
            let _ = tx.send_blocking(found);
        });
        glib::spawn_future_local(async move {
            let Ok(found) = rx.recv().await else { return };
            for ((_, lbox), s) in chips.iter().zip(found) {
                if lbox.parent().is_none() {
                    continue;
                }
                let mut wanted: Vec<(String, &str)> = Vec::new();
                if let Some((add, del, untracked)) = s {
                    if add > 0 {
                        wanted.push((format!("+{add}"), "sill-git-add"));
                    }
                    if del > 0 {
                        wanted.push((format!("−{del}"), "sill-git-del"));
                    }
                    if untracked > 0 {
                        wanted.push((format!("?{untracked}"), "dim-label"));
                    }
                }
                // the labels after the glyph and the name
                let mut current: Vec<gtk::Widget> = Vec::new();
                let mut c = lbox.first_child().and_then(|g| g.next_sibling()).and_then(|n| n.next_sibling());
                while let Some(w) = c {
                    c = w.next_sibling();
                    current.push(w);
                }
                let same = current.len() == wanted.len()
                    && current.iter().zip(&wanted).all(|(w, (t, _))| w.downcast_ref::<gtk::Label>().is_some_and(|l| l.text() == t.as_str()));
                if same {
                    continue;
                }
                for w in current {
                    lbox.remove(&w);
                }
                for (t, class) in wanted {
                    let l = gtk::Label::new(Some(&t));
                    l.add_css_class(class);
                    lbox.append(&l);
                }
            }
        });
    }

    /// Watch `<runtime dir>/denied`: the proxy appends every host it refuses, and the person gets a
    /// one-click way to allow it for this agent (SPEC §4.8). Third-party agents start with an
    /// empty allow-list, so this is how their endpoints get in.
    fn watch_denied(self: &Rc<Self>) {
        self.denied_seen.borrow_mut().clear();
        self.denied_asked.set(0);
        let Ok(rt) = sill_core::shim::runtime_dir(&self.id()) else { return };
        let Ok(m) = gtk::gio::File::for_path(&rt).monitor_directory(gtk::gio::FileMonitorFlags::NONE, None::<&gtk::gio::Cancellable>) else {
            return;
        };
        let weak = Rc::downgrade(self);
        m.connect_changed(move |_, f, _, _| {
            if f.basename().is_some_and(|b| b == std::path::Path::new("denied"))
                && let Some(v) = weak.upgrade()
            {
                v.check_denied();
            }
        });
        *self.denied_monitor.borrow_mut() = Some(m);
        self.check_denied();
    }

    fn check_denied(self: &Rc<Self>) {
        let Ok(rt) = sill_core::shim::runtime_dir(&self.id()) else { return };
        let Ok(text) = std::fs::read_to_string(rt.join("denied")) else { return };
        let home = session::home().unwrap_or_default();
        let quiet = sill_core::policy::Policy::load(&self.session.borrow().policy, &home)
            .map(|p| p.network.quiet)
            .unwrap_or_else(|_| sill_core::policy::quiet_default());
        for host in text.lines().map(str::trim).filter(|h| !h.is_empty()) {
            if !self.denied_seen.borrow_mut().insert(host.to_string()) {
                continue;
            }
            // telemetry stays blocked without a question (`host:port` when the port is not the default one)
            let name = host.rsplit_once(':').filter(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok()).map(|(h, _)| h).unwrap_or(host);
            // the text shown is what a click saves: a plain name or nothing (security.md S7; the
            // launcher filters too, this is the shelf's own check on a file in the runtime dir)
            if !sill_core::proxy::valid_hostname(name) {
                continue;
            }
            if quiet.iter().any(|p| sill_core::proxy::host_matches(p, name)) {
                continue;
            }
            // an agent probing hosts in a loop must not fill the conversation with buttons: so
            // many questions per session start, then one line pointing at the log
            let asked = self.denied_asked.get();
            if asked >= MAX_DENIED_PROMPTS {
                if asked == MAX_DENIED_PROMPTS {
                    self.denied_asked.set(asked + 1);
                    self.conv.add_note(&format!("{MAX_DENIED_PROMPTS} refused hosts asked about; more are refused quietly — the proxy log (the session's log) lists them"));
                }
                continue;
            }
            self.denied_asked.set(asked + 1);
            let agent_name = agents::load(&session::home().unwrap_or_default())
                .get(&self.session.borrow().agent)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| self.session.borrow().agent.clone());
            let mut actions: Vec<(String, Rc<dyn Fn()>)> = Vec::new();
            let exact = host.to_string();
            let weak = Rc::downgrade(self);
            actions.push((
                format!("Allow {host}"),
                Rc::new(move || {
                    if let Some(v) = weak.upgrade() {
                        v.allow_domain(&exact);
                    }
                }),
            ));
            // `*.<parent>` for a deep name whose parent is a site, never a public suffix
            if let Some(wild) = sill_core::proxy::wildcard_offer(name) {
                let wild = format!("{wild}{}", host.strip_prefix(name).unwrap_or(""));
                let weak = Rc::downgrade(self);
                let w = wild.clone();
                actions.push((
                    format!("Allow {wild}"),
                    Rc::new(move || {
                        if let Some(v) = weak.upgrade() {
                            v.allow_domain(&w);
                        }
                    }),
                ));
            }
            actions.push(("Keep blocked".into(), Rc::new(|| {})));
            self.conv.add_actions(
                &format!("{agent_name} tried to reach {host} and the proxy refused it: the host is not on this agent's allow-list."),
                actions,
            );
        }
    }

    /// Add a host pattern to this agent's `domains` (its registry override) and rebuild the jail
    /// so the proxy picks it up.
    fn allow_domain(self: &Rc<Self>, pattern: &str) {
        let home = session::home().unwrap_or_default();
        let id = self.session.borrow().agent.clone();
        let Some(mut agent) = agents::load(&home).remove(&id) else { return };
        if !agent.domains.iter().any(|d| d == pattern) {
            agent.domains.push(pattern.to_string());
        }
        match agents::save_override(&home, &agent) {
            Ok(_) => {
                self.conv.add_note(&format!("{pattern} allowed for {}; the session restarts", agent.name));
                self.force_restart.set(true);
                self.request_restart();
            }
            Err(e) => self.conv.add_note(&format!("could not save: {e:#}")),
        }
    }

    /// A pasted image: PNG, inline. Refused above 8 MB (the agent would choke on it anyway).
    fn attach_texture(self: &Rc<Self>, tex: &gdk::Texture) {
        let png = tex.save_to_png_bytes();
        if png.len() > 8 << 20 {
            self.conv.add_note("image not attached: larger than 8 MB");
            return;
        }
        let n = self.attachments.borrow().iter().filter(|a| a.mime.starts_with("image/")).count() + 1;
        self.add_attachment(Attachment {
            name: format!("screenshot-{n}.png ({}×{})", tex.width(), tex.height()),
            mime: "image/png".into(),
            kind: AttachmentKind::Image { data_base64: sill_core::acp::base64(&png) },
        });
    }

    /// A chosen file: an image goes inline; anything else is copied into the session's upload
    /// directory (private `~/.cache`, visible in the jail) and sent as a link to that path.
    fn attach_path(self: &Rc<Self>, path: &std::path::Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let image_mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.conv.add_note(&format!("could not read {}: {e}", path.display()));
                return;
            }
        };
        if let Some(mime) = image_mime {
            if bytes.len() > 8 << 20 {
                self.conv.add_note(&format!("{name} not attached: larger than 8 MB"));
                return;
            }
            self.add_attachment(Attachment { name, mime: mime.into(), kind: AttachmentKind::Image { data_base64: sill_core::acp::base64(&bytes) } });
            return;
        }
        let home = session::home().unwrap_or_default();
        let s = self.session.borrow().clone();
        let Ok(policy) = sill_core::policy::Policy::load(&s.policy, &home) else { return };
        let Ok(sdir) = sill_core::session::state_dir(session::uid(), &s.id) else { return };
        let Some((host_dir, jail_dir)) = sill_core::session::uploads_dir(&policy, &sdir, &home) else {
            self.conv.add_note("files can't be attached: this policy keeps no private ~/.cache for the session");
            return;
        };
        // the uploads dir is inside the session's scratch, which the agent can reach: the copy
        // never follows a link it may have planted there (security.md S6)
        let rel = host_dir.join(&name);
        let rel = rel.strip_prefix(&sdir).unwrap_or(&rel);
        if let Err(e) = sill_core::safe_fs::write_file_beneath(&sdir, rel, &bytes, 0o644) {
            self.conv.add_note(&format!("could not copy {name} into the session: {e:#}"));
            return;
        }
        self.add_attachment(Attachment { name: name.clone(), mime: "application/octet-stream".into(), kind: AttachmentKind::File { jail_path: jail_dir.join(&name) } });
    }

    fn add_attachment(self: &Rc<Self>, a: Attachment) {
        if matches!(a.kind, AttachmentKind::Image { .. }) && !self.images_ok.get() {
            self.conv.add_note("this agent reports no image support; the image is sent anyway");
        }
        self.attachments.borrow_mut().push(a);
        self.rebuild_attach_strip();
        self.prompt.grab_focus();
    }

    /// The chips above the prompt: name and a cross to drop one.
    fn rebuild_attach_strip(self: &Rc<Self>) {
        while let Some(w) = self.attach_strip.first_child() {
            self.attach_strip.remove(&w);
        }
        let list = self.attachments.borrow().clone();
        self.attach_strip.set_visible(!list.is_empty());
        for (i, a) in list.iter().enumerate() {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            chip.add_css_class("sill-attach-chip");
            let glyph = gtk::Label::new(Some(if a.mime.starts_with("image/") { "▣" } else { "▤" }));
            glyph.add_css_class("dim-label");
            let l = gtk::Label::new(Some(&a.name));
            l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            l.set_max_width_chars(32);
            let x = super::icons::glyph_button("sill-x");
            x.add_css_class("flat");
            x.set_tooltip_text(Some("Remove"));
            let weak = Rc::downgrade(self);
            x.connect_clicked(move |_| {
                let Some(v) = weak.upgrade() else { return };
                let mut l = v.attachments.borrow_mut();
                if i < l.len() {
                    l.remove(i);
                }
                drop(l);
                v.rebuild_attach_strip();
            });
            chip.append(&glyph);
            chip.append(&l);
            chip.append(&x);
            self.attach_strip.append(&chip);
        }
    }

    /// A file for the shelf's viewer.
    fn open_file(&self, path: &std::path::Path) {
        if let Some(cb) = self.on_open_file.borrow().as_ref() {
            cb(path);
        }
    }

    /// The terminal pane the bottom slot shows (`shell`, `host`), `None` while it is the prompt.
    pub fn pane_shown(&self) -> Option<String> {
        self.bottom.visible_child_name().map(|n| n.to_string()).filter(|n| n != "prompt")
    }

    /// `f` whenever the bottom slot switches between the prompt and a terminal pane.
    pub fn connect_pane(&self, f: impl Fn() + 'static) {
        self.bottom.connect_visible_child_name_notify(move |_| f());
    }

    /// The session's overlay, planned once per jail.
    fn jail_view(&self) -> Option<JailView> {
        if let Some(j) = self.jail.borrow().as_ref() {
            return Some(j.clone());
        }
        let s = self.session.borrow().clone();
        let home = session::home().ok()?;
        let policy = sill_core::policy::Policy::load(&s.policy, &home).ok()?;
        let sdir = sill_core::session::state_dir(session::uid(), &s.id).ok()?;
        let binds = session::binds_of(&s, &policy, &home, &self.store);
        let user = std::env::var("USER").unwrap_or_default();
        let plan = sill_core::changes::session_plan(policy, sdir, &home, session::uid(), &user, binds).ok()?;
        let j = (plan.layers, plan.masked);
        *self.jail.borrow_mut() = Some(j.clone());
        Some(j)
    }

    /// An image path as the agent wrote it → the file it sees in its jail: relative to home, `..`
    /// folded lexically; nothing under a masked path; a pinned folder is the host's own; otherwise
    /// the session's overlay copy when it has one (a regular file — a whiteout means deleted), else
    /// the host file. Images only.
    fn resolve_jail_path(&self, src: &str) -> Option<std::path::PathBuf> {
        use std::os::unix::fs::FileTypeExt;
        use std::path::{Component, PathBuf};
        let src = src.strip_prefix("file://").unwrap_or(src);
        if src.contains("://") {
            return None;
        }
        let home = session::home().ok()?;
        let raw = PathBuf::from(src);
        let raw = if raw.is_absolute() { raw } else { home.join(raw) };
        let mut p = PathBuf::from("/");
        for c in raw.components() {
            match c {
                Component::ParentDir => {
                    p.pop();
                }
                Component::Normal(n) => p.push(n),
                _ => {}
            }
        }
        if !viewer::is_image(&p) {
            return None;
        }
        if self.store.pins(&self.id()).unwrap_or_default().iter().any(|pin| pin.is_dir() && p.starts_with(pin)) {
            return p.is_file().then_some(p);
        }
        let (layers, masked) = self.jail_view()?;
        if masked.iter().any(|m| p.starts_with(m)) {
            return None;
        }
        for l in &layers {
            if let Ok(rel) = p.strip_prefix(&l.host) {
                let upper = l.upper.join(rel);
                if let Ok(md) = upper.symlink_metadata() {
                    if md.file_type().is_char_device() {
                        return None; // deleted in the jail
                    }
                    if md.is_file() {
                        return Some(upper);
                    }
                }
            }
        }
        p.is_file().then_some(p)
    }

    /// Have the pins changed since the jail started? Only folder pins are mounts; a pinned file never
    /// needs a new jail.
    pub fn pins_changed(&self) -> bool {
        let dirs = |v: &[std::path::PathBuf]| v.iter().filter(|p| p.is_dir()).cloned().collect::<Vec<_>>();
        self.force_restart.get() || dirs(&self.store.pins(&self.id()).unwrap_or_default()) != dirs(&self.live_pins.borrow())
    }

    /// Context block prepended to prompts while pins exist (SPEC §3.1 pins).
    fn pin_prefix(&self) -> String {
        let pins = self.store.pins(&self.id()).unwrap_or_default();
        let (dirs, files): (Vec<_>, Vec<_>) = pins.iter().partition(|p| p.is_dir());
        let mut out = String::new();
        if !dirs.is_empty() {
            let list: Vec<String> = dirs.iter().map(|p| p.display().to_string()).collect();
            out.push_str(&format!("[Pinned folders — focus your work there unless told otherwise: {}]\n", list.join(", ")));
        }
        if !files.is_empty() {
            let list: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
            out.push_str(&format!("[Pinned files — the person keeps these at hand: {}]\n", list.join(", ")));
        }
        out
    }

    /// What goes before the person's text: the standing block when this agent process has not had
    /// this version of it (SPEC §3.1 instructions), then the pins, then a blank line. The caller
    /// commits the standing block to [`Self::sent_standing`] once the prompt was accepted.
    fn context_prefix(&self, standing: Option<&str>) -> String {
        let mut out = standing.unwrap_or_default().to_string();
        out.push_str(&self.pin_prefix());
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn focus_prompt(&self) {
        self.prompt.grab_focus();
    }

    /// Desktop access of this session's jail (SPEC §4.2), from the session menu: stored, noted in
    /// the conversation, and the jail rebuilt (after the turn when the agent is working).
    pub fn set_desktop(self: &Rc<Self>, id: &str) {
        if self.session.borrow().desktop == id {
            return;
        }
        let label = sill_core::config::DESKTOP_CHOICES.iter().find(|(i, _)| *i == id).map(|(_, l)| *l).unwrap_or(id);
        let _ = self.store.set_desktop(&self.id(), id);
        self.session.borrow_mut().desktop = id.to_string();
        self.conv.add_note(&format!("desktop access: {label}; the session restarts"));
        self.restart_for_jail();
    }

    /// Which earlier sessions the agent is told about (docs/memory.md §2.3), from the session menu:
    /// stored; the changed standing block goes with the next prompt, no restart.
    pub fn set_memory_scope(&self, scope: &str) {
        if self.session.borrow().memory_scope == scope {
            return;
        }
        let _ = self.store.set_memory_scope(&self.id(), scope);
        self.session.borrow_mut().memory_scope = scope.to_string();
        let resolved = sill_core::memory::Scope::resolve(scope, !self.store.pins(&self.id()).unwrap_or_default().is_empty());
        self.conv.add_note(&format!("recall: {}; told with the next prompt", resolved.label()));
    }

    /// The session's policy, from the session menu; the jail is rebuilt with it.
    pub fn set_policy(self: &Rc<Self>, name: &str) {
        if self.session.borrow().policy == name {
            return;
        }
        let _ = self.store.set_policy(&self.id(), name);
        self.session.borrow_mut().policy = name.to_string();
        self.conv.add_note(&format!("policy: {name}; the session restarts"));
        self.restart_for_jail();
    }

    /// A jail property changed: rebuild the jail (the window waits for the turn to end).
    fn restart_for_jail(self: &Rc<Self>) {
        self.force_restart.set(true);
        self.request_restart();
    }

    /// Call the window's restart callback with the cell released first: it may close this view
    /// synchronously (idle session), and `quit` then takes the callbacks.
    fn request_restart(&self) {
        let cb = self.on_restart.borrow().clone();
        if let Some(cb) = cb {
            cb(&self.id());
        }
    }

    pub fn id(&self) -> String {
        self.session.borrow().id.clone()
    }

    /// Start (or reconnect to) the session's jail and agent on a worker thread — `systemd-run`,
    /// the wait for the shim socket and the control calls take seconds at times, and the GTK loop
    /// must not wait for them (SPEC §4.11 #13) — then wire the driver up on the main loop.
    fn connect(self: &Rc<Self>, fresh: bool) {
        if self.connecting.replace(true) {
            // once more after the one under way (a restart asked for while starting)
            self.connect_again.set(Some(fresh || self.connect_again.get().unwrap_or(false)));
            return;
        }
        let s = self.session.borrow().clone();
        let pins = self.store.pins(&s.id).unwrap_or_default();
        *self.live_pins.borrow_mut() = pins.clone();
        self.jail.borrow_mut().take(); // the overlay is planned again with the current pins
        self.ready.set(false);
        self.sent_standing.borrow_mut().take(); // a new agent process: the standing block goes again
        self.status.set_text("starting");
        let (tx, rx) = async_channel::bounded::<Result<session::Driver, String>>(1);
        std::thread::Builder::new()
            .name(format!("sill-start-{}", s.id))
            .spawn(move || {
                let _ = tx.send_blocking(session::start(&s, pins, fresh).map_err(|e| format!("{e:#}")));
            })
            .ok();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(r) = rx.recv().await else { return };
            let Some(v) = weak.upgrade() else { return };
            v.connecting.set(false);
            match r {
                Ok(d) => v.connected(d),
                Err(e) => {
                    v.status.set_text("failed to start");
                    v.conv.add_note(&format!("could not start the session: {e}"));
                    // the prompt that waited for this goes back where it was typed
                    if let Some(text) = v.pending_prompt.borrow_mut().take() {
                        v.prompt.buffer().set_text(&text);
                    }
                }
            }
            if let Some(again) = v.connect_again.take() {
                v.connect(again);
            }
        });
    }

    /// The jail and the agent are up: events flow to `handle`, the prompt that waited goes.
    fn connected(self: &Rc<Self>, d: session::Driver) {
        self.start_proc_polling();
        if !self.mode_noted.replace(true)
            && let Some(n) = d.note.clone()
        {
            self.conv.add_note(&n);
        }
        self.watch_denied();
        let events = d.events.clone();
        *self.driver.borrow_mut() = Some(d);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(ev) = events.recv().await {
                let Some(v) = weak.upgrade() else { break };
                v.handle(ev);
            }
        });
        if let Some(text) = self.pending_prompt.borrow_mut().take() {
            self.send(&text);
        }
    }

    /// The agent saw this conversation: it resumed its own context, or the person sent it a prompt
    /// since this view opened. A cold agent after a failed `session/load` has neither, and must not
    /// be asked to summarize what it never saw.
    pub fn agent_saw_session(&self) -> bool {
        self.resumed.get() || self.turns_this_run.get() > 0
    }

    /// The agent is up and between turns: it can be asked one more thing.
    pub fn agent_idle(&self) -> bool {
        self.driver.borrow().is_some() && self.ready.get() && !self.busy.get()
    }

    /// Ask the agent for the session's summary as its last turn (docs/memory.md §2.2): the prompt
    /// goes as a plain turn — no standing block, no user entry (a note marks it in the transcript),
    /// the answer shows and is stored like any agent message — and `then` gets its text when the
    /// turn stops, None when the agent is gone, busy, or has not answered within 90 seconds (the
    /// turn is cancelled then; the caller writes the mechanical summary).
    pub fn ask_summary(self: &Rc<Self>, prompt: &str, then: Box<dyn FnOnce(Option<String>)>) {
        self.ask_summary_turn(prompt, "the session ends: the agent writes its summary for memory", false, then);
    }

    /// *Summarize now* (docs/memory.md §2.5): the same turn as a checkpoint — the answer goes to
    /// `then`, the session goes on. Refused (None at once) while the agent is busy or down.
    pub fn ask_checkpoint(self: &Rc<Self>, prompt: &str, then: Box<dyn FnOnce(Option<String>)>) {
        self.ask_summary_turn(prompt, "summarize now: the agent writes a checkpoint summary for memory", true, then);
    }

    /// Name the session (store, the view's copy, the tab); `kind` as in `Session::title_kind`.
    pub fn rename(&self, title: &str, kind: &str) {
        let id = self.id();
        let _ = self.store.set_title(&id, title, kind);
        {
            let mut s = self.session.borrow_mut();
            s.title = title.to_string();
            s.title_kind = kind.to_string();
        }
        if let Some(cb) = self.on_title.borrow().as_ref() {
            cb(&id, title);
        }
    }

    /// A checkpoint is being written.
    pub fn checkpoint_pending(&self) -> bool {
        self.summarizing.borrow().is_some() && self.checkpoint.get()
    }

    /// The tab closed during a checkpoint: its answer becomes the session's final summary —
    /// `then` replaces the page's callback, and the turn counts as the end's.
    pub fn adopt_summary(&self, then: Box<dyn FnOnce(Option<String>)>) {
        if self.summarizing.borrow().is_some() {
            *self.summarizing.borrow_mut() = Some(then);
            self.checkpoint.set(false);
        } else {
            then(None);
        }
    }

    fn ask_summary_turn(self: &Rc<Self>, prompt: &str, note: &str, checkpoint: bool, then: Box<dyn FnOnce(Option<String>)>) {
        let Some(d) = self.driver.borrow().as_ref().map(|d| d.commands.clone()) else {
            then(None);
            return;
        };
        let ask = Command::Prompt { text: prompt.to_string(), attachments: vec![], entry: Some(wire::Entry::Note(note.to_string())) };
        if !self.agent_idle() || self.summarizing.borrow().is_some() || d.try_send(ask).is_err() {
            then(None);
            return;
        }
        self.conv.add_note(note);
        self.checkpoint.set(checkpoint);
        *self.summarizing.borrow_mut() = Some(then);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_secs(90), move || {
            let Some(v) = weak.upgrade() else { return };
            v.end_summary_turn("cut short: the agent had not finished after 90 seconds");
        });
    }

    /// Stop the summary turn before the agent stopped it: the turn is cancelled and what it wrote
    /// so far, marked with `why`, is the answer (None when it had written nothing).
    fn end_summary_turn(&self, why: &str) {
        let Some(cb) = self.summarizing.borrow_mut().take() else { return };
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Cancel);
        }
        let partial = self.take_summary_text();
        let answer = (!partial.is_empty()).then(|| format!("{partial}\n({why})"));
        glib::idle_add_local_once(move || cb(answer));
    }

    /// The summary turn's text: what its tool calls flushed, then what is still streaming.
    fn take_summary_text(&self) -> String {
        let mut text = std::mem::take(&mut *self.summary_text.borrow_mut());
        text.push_str(&self.agent_md.borrow());
        text.trim().to_string()
    }

    /// Send a prompt; false if the agent is busy or the session is down.
    /// A summary turn is running (the end's, or a checkpoint): the agent is writing.
    pub fn summary_pending(&self) -> bool {
        self.summarizing.borrow().is_some()
    }

    pub fn send(self: &Rc<Self>, text: &str) -> bool {
        let trace = std::env::var_os("SILL_TRACE").is_some();
        let t0 = std::time::Instant::now();
        let lap = |what: &str| {
            if trace {
                eprintln!("{} send: {what} at {} ms", super::trace_ts(), t0.elapsed().as_millis());
            }
        };
        if self.driver.borrow().is_none() {
            // after Ended, or while the jail is still starting: the shim respawns the agent and
            // session/load resumes; the prompt goes as soon as the agent is up (`connected`)
            let mut pending = self.pending_prompt.borrow_mut();
            *pending = Some(match pending.take() {
                Some(earlier) => format!("{earlier}\n\n{text}"),
                None => text.to_string(),
            });
            drop(pending);
            self.conv.add_note("the session is starting; the prompt is sent when the agent is up");
            if !self.connecting.get() {
                self.connect(false);
            }
            return true;
        }
        let Some(d) = self.driver.borrow().as_ref().map(|d| d.commands.clone()) else {
            self.conv.add_note("session is not running");
            return false;
        };
        // sill's facts about this jail come from the session's policy (none if it can't be read)
        let facts = session::home()
            .ok()
            .and_then(|h| sill_core::policy::Policy::load(&self.session.borrow().policy, &h).ok().map(|p| sill_core::instructions::facts_block(&p, &h)))
            .unwrap_or_default();
        // what earlier sessions in these folders did (docs/memory.md §2.3): changes only when one
        // in scope ends or the scope changes, so the block stays the same across prompts
        let memory = match session::home() {
            Ok(h) => {
                let pins = self.store.pins(&self.id()).unwrap_or_default();
                sill_core::memory::recall(&self.store, &self.session.borrow(), &pins, &h, sill_core::config::load().memory_recall())
            }
            Err(_) => String::new(),
        };
        let standing = sill_core::instructions::standing_block(&facts, &memory, &sill_core::instructions::load_global());
        let standing_due = self.sent_standing.borrow().as_deref() != Some(standing.as_str());
        let outgoing = format!("{}{}", self.context_prefix(standing_due.then_some(standing.as_str())), text);
        lap("prefix");
        let attachments = std::mem::take(&mut *self.attachments.borrow_mut());
        let names: Vec<String> = attachments.iter().map(|a| a.name.clone()).collect();
        let entry = wire::Entry::User { text: text.to_string(), names: names.clone() };
        match d.try_send(Command::Prompt { text: outgoing, attachments: attachments.clone(), entry: Some(entry) }) {
            Ok(()) => {
                if standing_due {
                    *self.sent_standing.borrow_mut() = Some(standing);
                }
                self.rebuild_attach_strip();
                self.conv.add_user_with(text, &names);
                lap("add_user");
                // the transcript entry is the daemon's to write, when it takes the prompt
                self.turns_this_run.set(self.turns_this_run.get() + 1);
                lap("done");
                true
            }
            Err(_) => {
                *self.attachments.borrow_mut() = attachments; // keep them for the retry
                self.rebuild_attach_strip();
                self.conv.add_note("the agent is busy; wait for the turn to end");
                false
            }
        }
    }

    /// The agent's text so far is closed (a tool call or the end of the turn): the daemon writes it
    /// to the transcript; the view keeps it only for a summary turn, whose text survives its tool calls.
    fn flush_agent(&self) {
        let md = std::mem::take(&mut *self.agent_md.borrow_mut());
        if !md.trim().is_empty() && self.summarizing.borrow().is_some() {
            self.summary_text.borrow_mut().push_str(&md);
        }
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        let replaying = *self.replaying.borrow();
        match &ev {
            Event::Ready { session_id, resumed, controls } => {
                self.ready.set(true);
                self.startup_retried.set(false);
                // the daemon stored the agent's session id; the view's copy follows
                self.session.borrow_mut().acp_session_id = Some(session_id.clone());
                *self.replaying.borrow_mut() = *resumed;
                self.resumed.set(*resumed);
                if !*resumed {
                    // a fresh ACP session (also after a login's retry): its context has no standing
                    // block yet, whatever this agent process was sent before
                    self.sent_standing.borrow_mut().take();
                }
                self.status.set_text(if *resumed { "resumed" } else { "ready" });
                self.build_controls(controls);
                // bring the agent back to the settings the person last chose in this session
                self.apply_remembered(controls);
                return;
            }
            Event::Controls(c) => {
                self.build_controls(c);
                // the model the person picked is current now: the adapter rebuilt the options for it
                // (effort re-seeded, "auto" possibly clamped) — the remembered picks go once more
                let picked = self.reconcile_model.borrow().clone();
                if let Some(model) = picked
                    && c.options.iter().any(|o| o.category == "model" && matches!(&o.kind, ConfigKind::Select(cur, _) if *cur == model))
                {
                    *self.reconcile_model.borrow_mut() = None;
                    self.apply_remembered(c);
                }
                return;
            }
            Event::PermissionRequest { .. } if self.auto_allow.get() => {
                let Event::PermissionRequest { tool_call_id, title, options, reply } = ev else { unreachable!() };
                use sill_core::acp::PermissionOptionKind as K;
                let pick = options
                    .iter()
                    .find(|o| o.kind == K::AllowOnce)
                    .or_else(|| options.iter().find(|o| o.kind == K::AllowAlways))
                    .map(|o| o.id.clone());
                let _ = reply.send(pick.clone());
                // a quiet mark on the call's own line, not a note with the whole command
                self.conv.mark_auto_allowed(&tool_call_id, &title, pick.is_none());
                return;
            }
            // the daemon answered it (the session auto-allows): a quiet mark on the call's own line
            Event::AutoAnswered { tool_call_id, title, allowed } => {
                self.conv.mark_auto_allowed(tool_call_id, title, !*allowed);
                return;
            }
            // a command the daemon refused (a turn running, the session gone)
            Event::Other(t) if t.starts_with("refused: ") => {
                self.conv.add_note(&t["refused: ".len()..]);
                return;
            }
            Event::Capabilities { images } => self.images_ok.set(*images),
            Event::Usage { used, size } => {
                self.usage.set(Some((*used, *size)));
                self.show_usage();
                return;
            }
            Event::TurnStarted => {
                *self.replaying.borrow_mut() = false;
                self.set_busy(true);
                if let Some(cb) = self.on_busy.borrow().as_ref() {
                    cb(&self.id(), true);
                }
            }
            Event::UserText(_) | Event::Text(_) | Event::Thought(_) | Event::ToolCall { .. } | Event::ToolCallUpdate { .. }
                if replaying =>
            {
                return; // the stored transcript is already on screen
            }
            // some agents (Mistral Vibe) echo the prompt back as a user message chunk during the
            // turn; the shelf already shows what it sent, so outside a replay it is dropped
            Event::UserText(_) => return,
            Event::Text(t) => self.agent_md.borrow_mut().push_str(t),
            // the daemon writes tool calls and their updates to the transcript
            Event::ToolCall { diff, .. } => {
                self.flush_agent();
                if diff.is_some() {
                    self.disk_changed();
                }
            }
            Event::ToolCallUpdate { diff, .. } => {
                if diff.is_some() {
                    self.disk_changed();
                }
            }
            Event::Stopped(_) => {
                self.set_busy(false);
                if let Some(cb) = self.on_busy.borrow().as_ref() {
                    cb(&self.id(), false);
                }
                // the summary turn: its text goes to whoever asked, once this event is handled
                self.flush_agent();
                let summary_cb = self.summarizing.borrow_mut().take();
                if let Some(cb) = summary_cb {
                    let answer = self.take_summary_text();
                    glib::idle_add_local_once(move || cb(Some(answer)));
                }
                let id = self.id();
                let weak = Rc::downgrade(self);
                glib::idle_add_local_once(move || {
                    if let Some(v) = weak.upgrade() {
                        v.after_turn(&id);
                    }
                });
                self.poll_procs();
            }
            Event::AuthRequired { message, methods } => {
                self.status.set_text("login needed");
                let weak = Rc::downgrade(self);
                self.conv.add_auth(
                    message,
                    methods.clone(),
                    Rc::new(move |m| {
                        if let Some(v) = weak.upgrade() {
                            v.authenticate(m);
                        }
                    }),
                );
                return;
            }
            Event::Ended(err) => {
                // the connection failed before the session was even open: usually an agent that was
                // still running from an earlier shelf and refuses a second `initialize`. Start it
                // again, once; session/load brings the conversation back.
                if err.is_some() && !self.ready.get() && !self.startup_retried.replace(true) {
                    *self.driver.borrow_mut() = None;
                    self.conv.add_note("the agent did not accept the connection; starting it again");
                    self.connect(true);
                    return;
                }
                // the agent or its stream is gone (crash, OOM, kill): the turn is over whether or not
                // Stopped arrived, and the next send reconnects instead of waiting for it
                let was_busy = self.busy.get();
                self.set_busy(false);
                if was_busy && let Some(cb) = self.on_busy.borrow().as_ref() {
                    cb(&self.id(), false);
                }
                self.flush_agent();
                *self.driver.borrow_mut() = None;
                // an agent that went away mid-summary: the mechanical one it is
                let summary_cb = self.summarizing.borrow_mut().take();
                if let Some(cb) = summary_cb {
                    self.summary_text.borrow_mut().clear();
                    glib::idle_add_local_once(move || cb(None));
                }
            }
            _ => {}
        }
        self.conv.handle(ev);
    }

    /// Count the session's pending changes (SPEC §4.4) and tell the window to refresh the ledger pages.
    fn after_turn(self: &Rc<Self>, id: &str) {
        let s = self.session.borrow().clone();
        let store2 = Store::open_default().ok();
        let (tx, rx) = async_channel::bounded::<String>(1);
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        std::thread::spawn(move || {
            let msg = (|| -> anyhow::Result<String> {
                let policy = sill_core::policy::Policy::load(&s.policy, &home)?;
                let state_dir = sill_core::session::state_dir(uid, &s.id)?;
                let binds = match store2.as_ref() {
                    Some(st) => session::binds_of(&s, &policy, &home, st),
                    None => vec![],
                };
                let plan = sill_core::changes::session_plan(policy.clone(), state_dir, &home, uid, &user, binds)?;
                let changes = sill_core::changes::scan(&policy, &plan, None)?;
                // the agent's own new dot-paths move into its sill-owned home (SPEC §4.2)
                let mut note = String::new();
                if let Some(agent) = sill_core::agents::load(&home).get(&s.agent) {
                    let pins = store2.as_ref().and_then(|st| st.pins(&s.id).ok()).unwrap_or_default();
                    let adopted = sill_core::session::adopt_agent_state(agent, &home, &changes, &pins)?;
                    if !adopted.is_empty() {
                        let names: Vec<String> = adopted
                            .iter()
                            .map(|p| format!("~/{}", p.strip_prefix(&home).unwrap_or(p).display()))
                            .collect();
                        note = format!("{} kept as {}'s own state (in its jail home from the next start)", names.join(", "), agent.name);
                    }
                }
                let state = sill_core::agents::load(&home).get(&s.agent).map(|a| sill_core::session::state_paths(a, &home)).unwrap_or_default();
                let pending = changes
                    .iter()
                    .filter(|c| !c.ignored && !sill_core::changes::is_agent_state(&c.path, &state, &home))
                    .count();
                let mut msg = if pending > 0 { format!("{pending} change(s) pending in Changes") } else { String::new() };
                if !note.is_empty() {
                    if !msg.is_empty() {
                        msg.push_str(" · ");
                    }
                    msg.push_str(&note);
                }
                Ok(msg)
            })();
            let _ = tx.send_blocking(msg.unwrap_or_else(|e| format!("ledger: {e:#}")));
        });
        let weak = Rc::downgrade(self);
        let id = id.to_string();
        glib::spawn_future_local(async move {
            if let Ok(msg) = rx.recv().await {
                let Some(v) = weak.upgrade() else { return };
                if !msg.is_empty() {
                    v.conv.add_note(&msg);
                }
                v.refresh_pins(); // git numbers on the chips
                if let Some(cb) = v.on_turn_end.borrow().as_ref() {
                    cb(&id);
                }
            }
        });
    }

    /// The tab is closing: tell the agent, stop the timers, cancel the monitors and drop the
    /// window's callbacks — every closure the view hands out holds it weakly, so after this the
    /// last `Rc` is the window's and the view goes with it (SPEC §4.11 #15).
    pub fn quit(&self) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Quit);
        }
        self.driver.borrow_mut().take();
        if let Some(id) = self.proc_tick.borrow_mut().take() {
            id.remove();
        }
        self.bridges.borrow_mut().clear();
        for m in self.git_monitors.borrow_mut().drain(..) {
            m.cancel();
        }
        if let Some(m) = self.denied_monitor.borrow_mut().take() {
            m.cancel();
        }
        self.on_busy.borrow_mut().take();
        self.on_turn_end.borrow_mut().take();
        self.on_disk_change.borrow_mut().take();
        self.on_title.borrow_mut().take();
        self.on_open_folder.borrow_mut().take();
        self.on_open_file.borrow_mut().take();
        self.on_shell.borrow_mut().take();
        self.on_host_terminal.borrow_mut().take();
        self.on_restart.borrow_mut().take();
    }

    // --- managed processes (docs/tasks.md §2.4, §2.5) -------------------------------------------

    /// Ask the shim for its process list every few seconds while the view lives (cheap: one line
    /// over a unix socket); also right after a turn.
    fn start_proc_polling(self: &Rc<Self>) {
        if self.proc_tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local(3, move || match weak.upgrade() {
            Some(v) => {
                v.poll_procs();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        *self.proc_tick.borrow_mut() = Some(id);
        self.poll_procs();
    }

    fn poll_procs(self: &Rc<Self>) {
        let Ok(sock) = sill_core::session::socket_path(&self.id()) else { return };
        let (tx, rx) = async_channel::bounded::<Option<Vec<sill_core::shim::Proc>>>(1);
        std::thread::spawn(move || {
            let list = sill_core::shim::Control::connect(&sock).and_then(|mut c| c.proc_list()).ok();
            let _ = tx.send_blocking(list);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let Ok(list) = rx.recv().await
                && let Some(v) = weak.upgrade()
            {
                v.procs_updated(list);
            }
        });
    }

    /// The list came back (`None`: no shim to ask — the jail is down, so are its ports).
    fn procs_updated(self: &Rc<Self>, list: Option<Vec<sill_core::shim::Proc>>) {
        let list = list.unwrap_or_default();
        // ports: bridges for running processes that declared one and may be exposed; the rest go
        let running_ports: Vec<(u16, String)> = list.iter().filter(|p| p.running()).filter_map(|p| p.port.map(|port| (port, p.name.clone()))).collect();
        self.bridges.borrow_mut().retain(|port, _| running_ports.iter().any(|(p, _)| p == port));
        let allowed = {
            let s = self.session.borrow();
            let home = session::home().unwrap_or_default();
            sill_core::policy::Policy::load(&s.policy, &home).map(|p| p.processes).unwrap_or_default()
        };
        for (port, name) in running_ports {
            if self.bridges.borrow().contains_key(&port) {
                continue;
            }
            if allowed.allows(port) || self.port_approved.borrow().contains(&port) {
                self.expose_port(port, &name);
            } else if self.port_asked.borrow_mut().insert(port) {
                let weak = Rc::downgrade(self);
                let n = name.clone();
                self.conv.add_actions(
                    &format!("{name} listens on port {port} inside the jail. Expose it on this machine, loopback only (http://127.0.0.1:{port})?"),
                    vec![
                        ("Expose".into(), Rc::new(move || {
                            let Some(v) = weak.upgrade() else { return };
                            v.port_approved.borrow_mut().insert(port);
                            v.expose_port(port, &n);
                        })),
                        ("Not now".into(), Rc::new(|| {})),
                    ],
                );
            }
        }
        *self.procs.borrow_mut() = list;
        self.show_procs();
    }

    fn expose_port(self: &Rc<Self>, port: u16, name: &str) {
        let Ok(sock) = sill_core::session::socket_path(&self.id()) else { return };
        match super::processes::expose(sock, port) {
            Ok(b) => {
                let note = if b.host_port == port {
                    format!("{name}: port {port} is open on this machine at {}", b.url())
                } else {
                    format!("{name}: port {port} is open on this machine at {} ({port} was taken)", b.url())
                };
                self.conv.add_note(&note);
                self.bridges.borrow_mut().insert(port, b);
            }
            Err(e) => self.conv.add_note(&format!("{name}: port {port} could not be exposed: {e:#}")),
        }
        self.show_procs();
    }

    /// The indicator: the count of running processes; warning colour while an ended one failed.
    fn show_procs(&self) {
        let Some(b) = self.proc_btn.borrow().clone() else { return };
        let procs = self.procs.borrow();
        let running = procs.iter().filter(|p| p.running()).count();
        let failed = procs.iter().any(|p| p.failed());
        if procs.is_empty() {
            b.set_visible(false);
            return;
        }
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        row.append(&super::icons::glyph_image("sill-processes"));
        row.append(&gtk::Label::new(Some(&running.to_string())));
        b.set_child(Some(&row));
        if failed {
            b.add_css_class("sill-procs-fail");
        } else {
            b.remove_css_class("sill-procs-fail");
        }
        b.set_tooltip_text(Some(&format!("{running} managed process(es) running{}", if failed { " · one has failed" } else { "" })));
        b.set_visible(true);
    }

    /// The list: name, state and uptime, the last output line, the URL when exposed; Log, Stop /
    /// Dismiss per row, Stop all below.
    fn fill_proc_menu(self: &Rc<Self>, pop: &gtk::Popover) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let procs = self.procs.borrow().clone();
        for p in &procs {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
            row.add_css_class("sill-proc-row");
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let name = gtk::Label::new(Some(&p.name));
            name.add_css_class("sill-proc-name");
            name.set_xalign(0.0);
            let state = match (p.running(), p.exit_code(), p.state.as_str()) {
                (true, _, _) => format!("running · {}", super::processes::uptime(now.saturating_sub(p.started))),
                (_, Some(0), _) => "exited".to_string(),
                (_, Some(c), _) => format!("failed ({c})"),
                _ => "stopped".to_string(),
            };
            let st = gtk::Label::new(Some(&state));
            st.add_css_class("sill-proc-state");
            st.set_hexpand(true);
            st.set_xalign(0.0);
            line.append(&name);
            line.append(&st);
            let log = gtk::Button::with_label("Log");
            log.add_css_class("flat");
            log.add_css_class("sill-change-btn");
            {
                let weak = Rc::downgrade(self);
                let n = p.name.clone();
                let pop = pop.clone();
                log.connect_clicked(move |_| {
                    pop.popdown();
                    let Some(v) = weak.upgrade() else { return };
                    if let Ok(socket) = sill_core::session::socket_path(&v.id())
                        && let Some(cb) = v.on_open_log.borrow().as_ref()
                    {
                        cb(&format!("{n} · process"), sill_core::session::LogSource::Proc { socket, name: n.clone() });
                    }
                });
            }
            line.append(&log);
            let act = gtk::Button::with_label(if p.running() { "Stop" } else { "Dismiss" });
            act.add_css_class("flat");
            act.add_css_class("sill-change-btn");
            {
                let weak = Rc::downgrade(self);
                let n = p.name.clone();
                let running = p.running();
                let pop = pop.clone();
                act.connect_clicked(move |_| {
                    pop.popdown();
                    if let Some(v) = weak.upgrade() {
                        v.proc_action(&n, running);
                    }
                });
            }
            line.append(&act);
            row.append(&line);
            let cmd = gtk::Label::new(Some(&p.argv.join(" ")));
            cmd.add_css_class("sill-proc-last");
            cmd.set_xalign(0.0);
            cmd.set_ellipsize(gtk::pango::EllipsizeMode::End);
            cmd.set_max_width_chars(60);
            row.append(&cmd);
            if !p.last_line.is_empty() {
                let last = gtk::Label::new(Some(&p.last_line));
                last.add_css_class("sill-proc-last");
                last.set_xalign(0.0);
                last.set_ellipsize(gtk::pango::EllipsizeMode::End);
                last.set_max_width_chars(60);
                row.append(&last);
            }
            if let Some(port) = p.port {
                let bridged = self.bridges.borrow().get(&port).map(|b| b.url());
                match bridged {
                    Some(url) => {
                        let link = gtk::LinkButton::with_label(&url, &url);
                        link.set_halign(gtk::Align::Start);
                        row.append(&link);
                    }
                    None if p.running() => {
                        let l = gtk::Label::new(Some(&format!("port {port} inside the jail, not exposed")));
                        l.add_css_class("sill-proc-last");
                        l.set_xalign(0.0);
                        row.append(&l);
                    }
                    None => {}
                }
            }
            col.append(&row);
        }
        if procs.iter().filter(|p| p.running()).count() > 1 {
            let all = gtk::Button::with_label("Stop all");
            all.add_css_class("flat");
            all.add_css_class("sill-menu-item");
            let weak = Rc::downgrade(self);
            let pop = pop.clone();
            all.connect_clicked(move |_| {
                pop.popdown();
                if let Some(v) = weak.upgrade() {
                    v.proc_action("", true);
                }
            });
            col.append(&all);
        }
        pop.set_child(Some(&col));
    }

    /// Stop one (or all, with an empty name) or dismiss an ended one; the list is polled again after.
    fn proc_action(self: &Rc<Self>, name: &str, stop: bool) {
        let Ok(sock) = sill_core::session::socket_path(&self.id()) else { return };
        let name = name.to_string();
        let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
        std::thread::spawn(move || {
            let r = sill_core::shim::Control::connect(&sock).and_then(|mut c| match (stop, name.is_empty()) {
                (true, true) => c.proc_stop_all().map(|_| ()),
                (true, false) => c.proc_stop(&name),
                (false, _) => c.proc_dismiss(&name),
            });
            let _ = tx.send_blocking(r.map_err(|e| format!("{e:#}")));
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let r = rx.recv().await;
            let Some(v) = weak.upgrade() else { return };
            if let Ok(Err(e)) = r {
                v.conv.add_note(&format!("process: {e}"));
            }
            v.poll_procs();
        });
    }
}

/// (+insertions, −deletions, untracked files) vs HEAD: of a pinned folder's working tree, or of a
/// pinned file alone (in the repository that holds it, found by walking up); None outside a repo.
fn git_summary(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let (repo, spec): (std::path::PathBuf, Option<std::path::PathBuf>) = if path.is_dir() {
        (path.to_path_buf(), None)
    } else {
        let root = path.ancestors().skip(1).find(|a| a.join(".git").exists())?;
        (root.to_path_buf(), Some(path.strip_prefix(root).ok()?.to_path_buf()))
    };
    if !repo.join(".git").exists() {
        return None;
    }
    let git = sill_core::changes::Git::open(&repo).ok()?;
    let mut cmd = git.cmd();
    cmd.args(["diff", "--numstat"]).args(sill_core::changes::Git::DIFF_SAFE).args(["HEAD", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    let (mut add, mut del) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        add += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        del += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    }
    let mut cmd = git.cmd();
    cmd.args(["ls-files", "--others", "--exclude-standard", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    let untracked = String::from_utf8_lossy(&out.stdout).lines().count() as u64;
    Some((add, del, untracked))
}

/// Let a dropdown shrink: its button shows the selection in a label that ellipsizes, so a tight row
/// truncates a long name ("Default (recom…") instead of widening the window. The popup keeps full
/// names and the check mark on the selected one.
fn compact_dropdown(d: &gtk::DropDown) {
    fn text_of(item: &gtk::ListItem) -> String {
        item.item().and_downcast::<gtk::StringObject>().map(|s| s.string().to_string()).unwrap_or_default()
    }
    let button = gtk::SignalListItemFactory::new();
    button.connect_setup(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let l = gtk::Label::new(None);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_xalign(0.0);
        item.set_child(Some(&l));
    });
    button.connect_bind(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        if let Some(l) = item.child().and_downcast::<gtk::Label>() {
            l.set_text(&text_of(item));
        }
    });
    // The check marks the dropdown's *chosen* item (`selected` on the DropDown). A list item's own
    // `selected` is the popup's cursor, which follows the pointer, so a check bound to it wandered
    // with the mouse.
    fn mark(check: &gtk::Image, item: &gtk::ListItem, d: &gtk::DropDown) {
        check.set_opacity(if item.position() == d.selected() { 1.0 } else { 0.0 });
    }
    let list = gtk::SignalListItemFactory::new();
    let dd = d.downgrade();
    list.connect_setup(move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let l = gtk::Label::new(None);
        l.set_xalign(0.0);
        l.set_hexpand(true);
        let check = super::icons::glyph_image("sill-check");
        row.append(&l);
        row.append(&check);
        item.set_child(Some(&row));
        // rows stay bound while the popup is hidden: follow later choices too (one handler per
        // row ever created; rows are recycled, so a handful per dropdown)
        if let Some(d) = dd.upgrade() {
            let (item, check) = (item.downgrade(), check.downgrade());
            d.connect_selected_notify(move |d| {
                if let (Some(item), Some(check)) = (item.upgrade(), check.upgrade()) {
                    mark(&check, &item, d);
                }
            });
        }
    });
    let dd = d.downgrade();
    list.connect_bind(move |_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else { return };
        if let Some(l) = row.first_child().and_downcast::<gtk::Label>() {
            l.set_text(&text_of(item));
        }
        if let (Some(c), Some(d)) = (row.last_child().and_downcast::<gtk::Image>(), dd.upgrade()) {
            mark(&c, item, &d);
        }
    });
    d.set_factory(Some(&button));
    d.set_list_factory(Some(&list));
}

/// The auto-allow lock: open when requests are answered by sill, closed when they wait.
fn lock_glyph(open: bool, color: gtk::gdk::RGBA) -> gtk::Image {
    super::icons::scalable(if open { "sill-lock-open" } else { "sill-lock" }, color)
}
