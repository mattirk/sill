//! One session tab: conversation + bottom area (prompt now; prompt/shell switcher in M3). Owns the
//! driver, persists the transcript, counts pending changes at turn end.

use std::collections::HashMap;
use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use sill_core::changes;
use gtk::gdk;
use gtk::glib;
use sill_core::acp::{AuthMethodInfo, ConfigKind, Controls, Event};
use sill_core::wire::{self, Attachment, AttachmentKind, Command, ConfigValue};
use super::remote::agents;
use sill_core::frontend::Request;
use super::remote;
use sill_core::store::{Entry, Session};
use super::remote_store::Store;

/// Entries rendered when a session opens; the rest come in chunks of [`HISTORY_CHUNK`] from the
/// "show earlier" row (SPEC §3.1 history: opening a long session costs the same as a short one).
const HISTORY_CHUNK: usize = 120;

use super::conversation::Conversation;

/// Refused hosts the conversation asks about per session start; the rest go to the log (S7).
const MAX_DENIED_PROMPTS: usize = 8;
use super::{markdown, session, theme};
use vte4::prelude::*;

/// The session's overlay as the shelf sees it: (layers, masked paths), for resolving a path the
/// agent names to the file it sees.
/// What gets a checkpoint's answer: the summary and the name the agent gave the session.
type Checkpointed = Box<dyn FnOnce(Option<String>, Option<String>)>;

/// A jail's overlay layers (host path, upper tree) and its masked paths, as the daemon plans them.

/// How long a turn may go without a word from the agent before the status counts the wait.
const SILENCE: std::time::Duration = std::time::Duration::from_secs(15);

pub struct SessionView {
    pub root: gtk::Box,
    prompt: gtk::TextView,
    /// What goes with the next prompt (SPEC §3.2 attachments) and the strip showing it.
    attachments: RefCell<Vec<Attachment>>,
    attach_strip: gtk::Box,
    prompt_queue: gtk::Box,
    /// The agent accepts image blocks (from `initialize`).
    images_ok: std::cell::Cell<bool>,
    /// The slash commands the agent offers, and the list that completes them as `/` is typed.
    commands: RefCell<Vec<sill_core::acp::AgentCommand>>,
    command_pop: gtk::Popover,
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
    /// The host shell's pid while it runs, and the directory it was last seen in: its own place,
    /// apart from the tree's selection; a shell started again (after `exit`) starts there.
    shell_bridge: RefCell<Option<super::streams::PtyBridge>>,
    host_bridge: RefCell<Option<super::streams::PtyBridge>>,
    host_cwd: RefCell<Option<std::path::PathBuf>>,
    /// When a pane last attached again to its running terminal (see `reattach`).
    last_reattach: std::cell::Cell<Option<std::time::Instant>>,
    /// The terminal is running a login; when it closes the session is opened again.
    login_pending: Rc<std::cell::Cell<bool>>,
    /// `Ready` arrived on the current connection.
    ready: std::cell::Cell<bool>,
    /// The launcher-mode note was shown in this view (once per tab, SPEC §4.7).
    mode_noted: std::cell::Cell<bool>,
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
    /// The snapshot's entries above what is shown, oldest first; "show earlier" takes from the end.
    older: RefCell<Vec<Entry>>,
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
    /// Open a managed process's output: title, session ID, process name.
    pub on_open_log: RefCell<Option<Box<dyn Fn(&str, String, String)>>>,
    pins_row: gtk::Box,
    /// Agent controls (mode, model, effort, toggles) + send button, under the prompt.
    controls_row: gtk::Box,
    /// Set while the controls are being rebuilt so their change signals don't echo back.
    rebuilding: std::cell::Cell<bool>,
    /// sill's own switch: answer permission requests with "allow once" without asking.
    auto_allow: std::cell::Cell<bool>,
    /// Send button; becomes a stop button while a turn is running.
    send_btn: RefCell<Option<gtk::Button>>,
    busy: std::cell::Cell<bool>,
    /// When the agent last said anything during a turn, and whether the status says it is quiet:
    /// after [`SILENCE`] the status counts the wait, so a slow or retrying agent never looks hung.
    last_event: std::cell::Cell<std::time::Instant>,
    silence_shown: std::cell::Cell<bool>,
    active_time: std::cell::Cell<(u64, Option<u64>)>,
    pub session: RefCell<Session>,
    pub conv: Rc<Conversation>,
    pub status: gtk::Label,
    driver: RefCell<Option<session::Driver>>,
    store: Rc<Store>,
    /// Markdown of the agent message being streamed (persisted at turn end).
    agent_md: RefCell<String>,
    /// *Summarize now* is running in the daemon (docs/memory.md §2.5): what gets the answer and the
    /// name it gave the session when the `Summarized` event arrives (None: it wrote nothing).
    checkpoint: RefCell<Option<Checkpointed>>,
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
    denied_monitor: RefCell<Option<glib::SourceId>>,
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

/// A prompt being written: its text and attachments (see [`SessionView::take_draft`]).
pub struct Draft {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
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
            // During a login the rules around a pasted code are `login_input`'s: Enter waits for
            // the code's last character, and a second Enter right after it is held.
            let entry = Rc::new(RefCell::new(super::login_input::CodeEntry::default()));
            let paste_into = {
                let login_pending = login_pending.clone();
                let show_toast = show_toast.clone();
                let entry = entry.clone();
                move |t: &vte4::Terminal| {
                    let t = t.clone();
                    let in_login = login_pending.get();
                    let show_toast = show_toast.clone();
                    if in_login && !entry.borrow_mut().begin() {
                        show_toast("still typing the pasted code");
                        return;
                    }
                    let entry = entry.clone();
                    t.clipboard().read_text_async(None::<&gtk::gio::Cancellable>, move |res| {
                        let text = res.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
                        if !in_login {
                            if !text.is_empty() {
                                t.paste_text(&text);
                            }
                            return;
                        }
                        // a login code: typed in one character at a time (a bracketed block is
                        // swallowed silently by the prompt) and acknowledged, not shown
                        if entry.borrow_mut().paste(&text) == 0 {
                            return;
                        }
                        glib::timeout_add_local(std::time::Duration::from_millis(4), move || {
                            let step = entry.borrow_mut().step(std::time::Instant::now());
                            match step {
                                super::login_input::Step::Type(c) => {
                                    t.feed_child(c.encode_utf8(&mut [0; 4]).as_bytes());
                                    glib::ControlFlow::Continue
                                }
                                super::login_input::Step::Done { typed, sent } => {
                                    if sent {
                                        t.feed_child(b"\r");
                                        show_toast(&format!("pasted {typed} characters and sent"));
                                    } else {
                                        show_toast(&format!("pasted {typed} characters — press Enter"));
                                    }
                                    glib::ControlFlow::Break
                                }
                                super::login_input::Step::Idle => glib::ControlFlow::Break,
                            }
                        });
                    });
                }
            };
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let t = terminal.clone();
            let paste = paste_into.clone();
            let login_keys = login_pending.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                let k = key.to_lower();
                if login_keys.get() {
                    let pressed = if key == gdk::Key::Return || key == gdk::Key::KP_Enter { super::login_input::Key::Enter } else { super::login_input::Key::Other };
                    if entry.borrow_mut().key(pressed, std::time::Instant::now()) == super::login_input::KeyAction::Hold {
                        return glib::Propagation::Stop;
                    }
                }
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
        let prompt_queue = gtk::Box::new(gtk::Orientation::Vertical, 0);
        prompt_queue.add_css_class("sill-prompt-queue");
        prompt_queue.set_visible(false);
        let queue_scroll = gtk::ScrolledWindow::new();
        queue_scroll.add_css_class("sill-prompt-queue-boundary");
        queue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        queue_scroll.set_max_content_height(100);
        queue_scroll.set_propagate_natural_height(true);
        queue_scroll.set_child(Some(&prompt_queue));
        prompt_queue.bind_property("visible", &queue_scroll, "visible").sync_create().build();
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
        chat.append(&queue_scroll);
        chat.append(&prompt_box);
        chat.set_vexpand(true);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&pins_row);
        root.append(&chat);

        let auto_allow_initial = session.auto_allow;
        let view = Rc::new(Self {
            root,
            prompt: prompt.clone(),
            attachments: RefCell::new(Vec::new()),
            attach_strip,
            prompt_queue,
            images_ok: std::cell::Cell::new(true),
            commands: RefCell::new(Vec::new()),
            command_pop: {
                let pop = gtk::Popover::new();
                pop.add_css_class("sill-menu");
                pop.set_has_arrow(false);
                pop.set_autohide(false);
                pop.set_can_focus(false);
                pop.set_position(gtk::PositionType::Top);
                pop.set_parent(&prompt);
                pop
            },
            bottom,
            pane,
            terminal: terminal.clone(),
            term_scroll: term_scroll.clone(),
            shell_open: std::cell::Cell::new(false),
            host_term: host_term.clone(),
            host_scroll: host_scroll.clone(),
            host_open: std::cell::Cell::new(false),
            shell_bridge: RefCell::new(None),
            host_bridge: RefCell::new(None),
            last_reattach: std::cell::Cell::new(None),
            host_cwd: RefCell::new(None),
            login_pending: login_pending.clone(),
            ready: std::cell::Cell::new(false),
            mode_noted: std::cell::Cell::new(false),
            startup_retried: std::cell::Cell::new(false),
            force_restart: std::cell::Cell::new(false),
            shell_btn: RefCell::new(None),
            attach_btn: RefCell::new(None),
            lock_btn: RefCell::new(None),
            usage: std::cell::Cell::new(None),
            usage_lbl: RefCell::new(None),
            earlier: RefCell::new(None),
            older: RefCell::new(Vec::new()),
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
            auto_allow: std::cell::Cell::new(auto_allow_initial),
            send_btn: RefCell::new(None),
            busy: std::cell::Cell::new(false),
            last_event: std::cell::Cell::new(std::time::Instant::now()),
            silence_shown: std::cell::Cell::new(false),
            active_time: std::cell::Cell::new((0, None)),
            session: RefCell::new(session),
            conv,
            status,
            driver: RefCell::new(None),
            store,
            agent_md: RefCell::new(String::new()),
            checkpoint: RefCell::new(None),
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

        // a turn with no word from the agent for a while: the status counts the wait
        {
            let weak = Rc::downgrade(&view);
            glib::timeout_add_seconds_local(1, move || {
                let Some(v) = weak.upgrade() else { return glib::ControlFlow::Break };
                // the person's time to answer is not the agent's silence: count from the answer
                if v.conv.asking() {
                    v.last_event.set(std::time::Instant::now());
                }
                let quiet = v.last_event.get().elapsed();
                if v.busy.get() && quiet >= SILENCE {
                    let secs = quiet.as_secs();
                    v.status.set_text(&format!("waiting on the agent · {}:{:02}", secs / 60, secs % 60));
                    v.status.set_tooltip_text(Some("The agent has sent nothing since; it may be waiting on its provider (a slow or retried request)"));
                    v.silence_shown.set(true);
                }
                glib::ControlFlow::Continue
            });
        }

        // the command list hangs off the prompt; let go of it with the prompt (GTK warns otherwise)
        {
            let pop = view.command_pop.clone();
            view.prompt.connect_destroy(move |_| pop.unparent());
        }

        // a prompt's images: inline data as sent, or the daemon's upload, as a local file to draw
        {
            let session_id = view.id();
            view.conv.set_image_loader(std::sync::Arc::new(move |a| {
                let data = match (&a.data_base64, &a.upload_id) {
                    (Some(data), _) => data.clone(),
                    (None, Some(upload_id)) => super::client::quick::<sill_proto::sfp::Upload>("_sill/uploads.get", sill_proto::sfp::UploadGet { session_id: session_id.clone(), upload_id: upload_id.clone() })
                        .map_err(|e| eprintln!("sill: loading {}: {e:#}", a.name)).ok()?.data_base64,
                    (None, None) => return None,
                };
                let bytes = glib::base64_decode(&data);
                let ext = a.mime.strip_prefix("image/").unwrap_or("img");
                remote::cache_image(&format!("upload:{}", a.upload_id.as_deref().unwrap_or(&a.name)), &bytes, ext)
            }));
        }

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

        {
            let weak = Rc::downgrade(&view);
            let mut sizes = [(0,0); 2];
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                let Some(v) = weak.upgrade() else { return glib::ControlFlow::Break };
                for (i, (term, slot)) in [(&v.terminal, &v.shell_bridge), (&v.host_term, &v.host_bridge)].into_iter().enumerate() {
                    if let Some(bridge) = slot.borrow().as_ref() {
                        let size = (term.column_count().max(20) as u16, term.row_count().max(5) as u16);
                        if sizes[i] != size { bridge.resize(size.0, size.1).ok(); sizes[i] = size; }
                    } else { sizes[i] = (0,0); }
                }
                let shell_closed = v.shell_bridge.borrow().as_ref().is_some_and(|b| b.closed());
                let host_closed = v.host_bridge.borrow().as_ref().is_some_and(|b| b.closed());
                if shell_closed && !v.reattach(&v.terminal, &v.shell_bridge) { v.shell_closed(); }
                if host_closed && !v.reattach(&v.host_term, &v.host_bridge) { v.host_shell_closed(); }
                glib::ControlFlow::Continue
            });
        }

        // The attach notification supplies an atomic transcript tail before live state.

        // Handle the TextView action, not a bubbling key event: GTK may consume Ctrl+V
        // before that event reaches us. This also covers the context-menu Paste action.
        {
            let weak = Rc::downgrade(&view);
            prompt.connect_paste_clipboard(move |prompt| {
                let Some(v) = weak.upgrade() else { return };
                let cb = prompt.clipboard();
                let formats = cb.formats();
                let has_image = formats.contains_type(gdk::Texture::static_type())
                    || formats.mime_types().iter().any(|mime| mime.starts_with("image/"));
                if !has_image { return; } // Let GTK paste ordinary text normally.
                prompt.stop_signal_emission_by_name("paste-clipboard");
                v.conv.add_note("reading image from clipboard…");
                let cancellable = gtk::gio::Cancellable::new();
                let finished = Rc::new(std::cell::Cell::new(false));
                let timeout_finished = finished.clone();
                let timeout_cancel = cancellable.clone();
                let timeout_view = Rc::downgrade(&v);
                glib::timeout_add_local_once(std::time::Duration::from_secs(15), move || {
                    if !timeout_finished.replace(true) {
                        timeout_cancel.cancel();
                        if let Some(v) = timeout_view.upgrade() {
                            v.conv.add_error("clipboard image read timed out; save the image and attach the file instead");
                        }
                    }
                });
                // Prefer explicit encoded image data. This separates transfer from GTK's
                // texture deserializer, which can otherwise stall without showing a stage.
                let image_mimes: Vec<&str> = ["image/png", "image/jpeg", "image/webp", "image/bmp", "image/gif"]
                    .into_iter().filter(|mime| formats.contain_mime_type(mime)).collect();
                if !image_mimes.is_empty() {
                    let read_cancel = cancellable.clone();
                    cb.read_async(&image_mimes, glib::Priority::DEFAULT, Some(&cancellable), move |res| {
                        if finished.get() { return; }
                        match res {
                            Ok((stream, mime)) => {
                                v.conv.add_note(&format!("clipboard {mime} stream opened; receiving image…"));
                                v.read_clipboard_image(stream, read_cancel, finished, Vec::new());
                            }
                            Err(e) => {
                                finished.set(true);
                                v.conv.add_error(&format!("could not open clipboard image stream: {e}"));
                            }
                        }
                    });
                } else {
                    cb.read_texture_async(Some(&cancellable), move |res| {
                        if finished.replace(true) { return; }
                        match res {
                            Ok(Some(tex)) => {
                                v.conv.add_note(&format!("clipboard image received ({}×{}); preparing attachment…", tex.width(), tex.height()));
                                v.attach_texture(&tex);
                            }
                            Ok(None) => v.conv.add_error("could not paste image: clipboard supplied no image; copy it again or attach the file"),
                            Err(e) => v.conv.add_error(&format!("could not paste image: {e}; copy it again or attach the file")),
                        }
                    });
                }
            });
        }

        // Enter sends, Shift+Enter inserts a newline
        let keys = gtk::EventControllerKey::new();
        {
            let weak = Rc::downgrade(&view);
            let prompt = prompt.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                let Some(v) = weak.upgrade() else { return glib::Propagation::Proceed };
                if key == gdk::Key::Tab && v.command_pop.is_visible() {
                    if let Some(name) = v.command_matches().first().map(|c| c.name.clone()) { v.complete_command(&name); }
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::Escape && v.command_pop.is_visible() {
                    v.command_pop.popdown();
                    return glib::Propagation::Stop;
                }
                if (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                    v.command_pop.popdown();
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
        {
            let weak = Rc::downgrade(&view);
            prompt.buffer().connect_changed(move |_| {
                if let Some(view) = weak.upgrade() { view.set_busy(view.busy.get()); view.show_command_matches(); }
            });
        }

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
            // where the host shell is now, read after each burst of output (a `cd` redraws the prompt)
            let weak = Rc::downgrade(&view);
            host_term.connect_contents_changed(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.note_host_cwd();
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
    pub fn toggle_host_shell(self: &Rc<Self>) {
        let on = self.bottom.visible_child_name().as_deref() != Some("host");
        self.show_host_shell(on);
    }

    /// Remember the host shell's working directory (its `/proc` cwd: the shelf runs on the host).
    fn note_host_cwd(&self) {
        if let Some(bridge) = self.host_bridge.borrow().as_ref() && let Ok(Some(cwd)) = bridge.cwd() {
            *self.host_cwd.borrow_mut() = Some(cwd);
        }
    }

    /// The host shell in the bottom slot: a login shell on the host, started on first use where the
    /// last one left off (else the session's first pinned folder, else home) and kept while hidden.
    /// Its directory is its own: the tree's selection does not move it. A host process by
    /// design — outside the jail, nothing it does is recorded; the pane says so above the terminal
    /// and the shell button turns the warning colour while it shows.
    pub fn show_host_shell(self: &Rc<Self>, on: bool) {
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
            let dir = self.host_cwd.borrow().clone().filter(|d| remote::is_dir(d))
                .or_else(|| self.store.pins(&self.id()).unwrap_or_default().into_iter().find(|p| remote::is_dir(p)))
                .or_else(|| session::home().ok());
            let env = [("TERM".into(), "xterm-256color".into()), ("COLORTERM".into(), "truecolor".into()),
                ("SILL_HOST_SHELL".into(), "1".into()), (changes::HOST_SHELL_GIT_ENV.0.into(), changes::HOST_SHELL_GIT_ENV.1.into())].into();
            let (cols, rows) = (self.host_term.column_count().max(20) as u16, self.host_term.row_count().max(5) as u16);
            let label = format!("host:{}", self.id());
            // the shelf came back (or its connection did): the shell it left is still running
            let found = super::streams::PtyBridge::find(None, &label).and_then(|t| super::streams::PtyBridge::reattach(&t, &self.host_term, cols, rows).ok());
            let opened = match found { Some(b) => Ok(b), None => super::streams::PtyBridge::open(None, &label, &[], &env, dir.as_deref(), cols, rows) };
            match opened {
                Ok(mut bridge) => match vte4::Pty::foreign_sync(bridge.take_fd(), None::<&gtk::gio::Cancellable>) {
                    Ok(pty) => { self.host_term.set_pty(Some(&pty)); *self.host_bridge.borrow_mut() = Some(bridge); }
                    Err(e) => { self.conv.add_error(&format!("host shell: {e}")); return; }
                },
                Err(e) => { self.conv.add_error(&format!("host shell: {e:#}")); return; }
            }
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

    /// A pane's stream ended. When the terminal itself still runs (the daemon cut a slow viewer
    /// off, the connection came back), attach again: the screen is redrawn from its recent output
    /// and a size nudge makes a full-screen program repaint. False when it is really gone.
    fn reattach(&self, term: &vte4::Terminal, slot: &RefCell<Option<super::streams::PtyBridge>>) -> bool {
        let Some(id) = slot.borrow().as_ref().map(|b| b.terminal_id.clone()) else { return false };
        // one attempt per few seconds: a stream that ends at once again is not worth a loop
        let now = std::time::Instant::now();
        if self.last_reattach.get().is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(3)) { return false; }
        if !matches!(super::streams::PtyBridge::fate(&id), super::streams::Fate::Running) { return false; }
        self.last_reattach.set(Some(now));
        let (cols, rows) = (term.column_count().max(20) as u16, term.row_count().max(5) as u16);
        let Ok(mut bridge) = super::streams::PtyBridge::reattach(&id, term, cols, rows) else { return false };
        let Ok(pty) = vte4::Pty::foreign_sync(bridge.take_fd(), None::<&gtk::gio::Cancellable>) else { return false };
        term.set_pty(Some(&pty));
        bridge.resize(cols, rows.saturating_sub(1).max(1)).ok();
        bridge.resize(cols, rows).ok();
        *slot.borrow_mut() = Some(bridge);
        true
    }

    /// End the pane terminals themselves (closing the tab), not only this view of them.
    fn close_terminals(&self) {
        for slot in [&self.shell_bridge, &self.host_bridge] {
            if let Some(b) = slot.borrow_mut().take() { close_in_background(b); }
        }
    }

    /// A pane's pty is done with: clear the screen and leave the dead pty attached until the next
    /// open replaces it. Never `set_pty(None)`: VTE 0.84's `Widget::set_pty` takes the new pty's
    /// impl without a null check, so detaching while a pty is set is a segfault in the main loop
    /// (seen 2026-11-03 when the daemon died under an open host pane and the bridge tick closed
    /// it). A dead master costs one fd; VTE already stopped reading it at EOF.
    fn drop_pty(term: &vte4::Terminal) {
        term.reset(true, true);
    }

    /// The host shell exited: forget it and show the prompt again.
    fn host_shell_closed(self: &Rc<Self>) {
        self.host_open.set(false);
        self.host_bridge.borrow_mut().take();
        Self::drop_pty(&self.host_term);
        self.show_host_shell(false);
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
                    self.conv.add_error(&format!("shell: {e:#}"));
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
        self.shell_bridge.borrow_mut().take();
        Self::drop_pty(&self.terminal);
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
        let cfg = remote::config::load();
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
                    let _ = remote::config::update(|c| c.host_terminal = Some(cmd.to_string()));
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
        self.open_pty(&[], &std::collections::BTreeMap::new())
    }

    /// Run `argv` on a fresh pty inside the jail, shown in the terminal pane.
    fn open_pty(&self, argv: &[String], env: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let home = session::home()?;
        let cwd = self.store.pins(&self.id()).unwrap_or_default().into_iter().find(|p| remote::is_dir(p)).unwrap_or(home);
        let cols = self.terminal.column_count().max(20) as u16;
        let rows = self.terminal.row_count().max(5) as u16;
        // the pane is a VTE terminal: say so, whatever the jail's default is
        let mut env = env.clone();
        env.entry("TERM".into()).or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into()).or_insert_with(|| "truecolor".into());
        let env = &env;
        // the plain shell the pane had before the shelf or its connection went is still running
        let found = argv.is_empty().then(|| super::streams::PtyBridge::find(Some(&self.id()), "shell")).flatten();
        let attached = found.and_then(|t| super::streams::PtyBridge::reattach(&t, &self.terminal, cols, rows).ok());
        let fresh = attached.is_none();
        let mut bridge = match attached {
            Some(b) => b,
            None => super::streams::PtyBridge::open(Some(&self.id()), "shell", argv, env, Some(&cwd), cols, rows)?,
        };
        let pty = vte4::Pty::foreign_sync(bridge.take_fd(), None::<&gtk::gio::Cancellable>)?;
        // a login command replaces the pane's shell: that one ends, it does not linger unseen
        let id = bridge.terminal_id.clone();
        let old = self.shell_bridge.borrow_mut().replace(bridge);
        if let Some(old) = old && old.terminal_id != id {
            close_in_background(old);
        }
        self.terminal.set_pty(Some(&pty));
        if fresh { self.terminal.feed(b"\x1b[2J\x1b[H"); }
        Ok(())
    }

    /// The person picked a login method. Agent-side methods go through the protocol; terminal
    /// methods run the agent's own command in the pane (Claude: `--cli auth login …`), and the
    /// session is opened again when that exits.
    /// `key`: the API key typed for a method that takes one (`AuthMethodInfo::api_key`).
    fn authenticate(self: &Rc<Self>, m: AuthMethodInfo, key: Option<String>) {
        if let Some(key) = key {
            self.status.set_text("logging in");
            self.command(Command::AuthenticateWithKey { method_id: m.id, key: wire::Secret(key) });
            return;
        }
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
        match self.open_pty(&argv, &e) {
            Ok(()) => {
                self.shell_open.set(true);
                self.login_pending.set(true);
                self.conv.add_note("finish the login in the terminal below (Ctrl+click opens a link); the session continues when it closes");
                self.show_shell(true);
            }
            Err(err) => self.conv.add_error(&format!("login: {err:#}")),
        }
    }

    /// Browser login with a localhost callback (Codex ChatGPT): run the agent on the host for the
    /// login only (`sill_core::login`), then rebuild the jail so the new login file is bound in.
    fn login_on_host(self: &Rc<Self>, agent: agents::Agent, m: AuthMethodInfo) {
        self.status.set_text("logging in");
        self.conv.add_note(&format!("{}: the login runs on the daemon host; follow the login link it reports", m.name));
        let (tx, rx) = async_channel::unbounded::<Result<String, String>>();
        let method = m.id.clone();
        std::thread::Builder::new()
            .name("sill-login".into())
            .spawn(move || {
                let r = (|| -> anyhow::Result<()> {
                    use std::io::BufRead;
                    let opened: sill_proto::host::HostStream = super::client::call("_sill/agents.login", sill_proto::host::AgentLogin { operation_id: None, agent_id: agent.id, method_id: method })?;
                    let stream = sill_proto::transport::open_stream(&opened.token)?;
                    struct Close(std::sync::Arc<dyn Fn() + Send + Sync>);
                    impl Drop for Close { fn drop(&mut self) { (self.0)(); } }
                    let _close = Close(stream.interrupt.clone());
                    for line in std::io::BufReader::new(stream.reader).lines() {
                        match serde_json::from_str::<sill_proto::host::Progress>(&line?)? {
                            sill_proto::host::Progress::Failed { message, .. } => anyhow::bail!("{message}"),
                            sill_proto::host::Progress::Completed { .. } => return Ok(()),
                            sill_proto::host::Progress::Status { text } | sill_proto::host::Progress::Stderr { text } => { let _ = tx.send_blocking(Ok(text)); }
                            _ => {}
                        }
                    }
                    anyhow::bail!("login connection closed; outcome unknown. Check login status before retrying")
                })();
                let _ = tx.send_blocking(r.map(|_| String::new()).map_err(|e| format!("{e:#}")));
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
                        v.conv.add_error(&format!("login failed: {e}"));
                        v.status.set_text("login needed");
                        v.command(Command::RetrySession);
                        break;
                    }
                }
            }
        });
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
        term.set_tooltip_text(Some("Shell inside this session's jail (Ctrl+T) · right-click: a terminal on the host (Ctrl+Shift+H)"));
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
                    d.connect_selected_notify(move |d| {
                        let Some(v) = weak.upgrade() else { return };
                        if v.rebuilding.get() {
                            return;
                        }
                        if let Some(id) = ids.get(d.selected() as usize) {
                            v.command(Command::SetConfig(oid.clone(), ConfigValue::Id(id.clone())));
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
                super::client::set(&v.id(), wire::SessionSet::AutoAllow(state));
                v.session.borrow_mut().auto_allow = state;
            });
            *self.lock_btn.borrow_mut() = Some(t.clone());
            right.append(&t);
        }
        // Keep each control centred on its own natural height, like the drawer toolbar.
        let mut child = right.first_child();
        while let Some(w) = child {
            w.set_valign(gtk::Align::Center);
            child = w.next_sibling();
        }
        self.controls_row.append(&right);
        let send = gtk::Button::new();
        send.add_css_class("sill-send");
        send.set_valign(gtk::Align::Center); // its own square, not the row's height
        {
            let weak = Rc::downgrade(self);
            send.connect_clicked(move |_| {
                let Some(v) = weak.upgrade() else { return };
                let buf = v.prompt.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
                if !text.is_empty() {
                    if v.send(&text) { buf.set_text(""); }
                } else if v.busy.get() {
                    v.cancel();
                }
            });
        }
        self.controls_row.append(&send);
        let mut child = self.controls_row.first_child();
        while let Some(w) = child {
            w.set_valign(gtk::Align::Center);
            child = w.next_sibling();
        }
        *self.send_btn.borrow_mut() = Some(send);
        self.set_busy(self.busy.get());
        self.rebuilding.set(false);
    }

    /// The snapshot's entries onto the conversation, in order: the last [`HISTORY_CHUNK`] now, the
    /// rest kept here for "show earlier" (the snapshot already holds them; nothing more is asked).
    fn replay_history(self: &Rc<Self>, mut entries: Vec<Entry>) {
        self.conv.clear();
        self.agent_md.borrow_mut().clear();
        self.earlier.borrow_mut().take();
        let tail = entries.split_off(entries.len().saturating_sub(HISTORY_CHUNK));
        let hidden = entries.len();
        *self.older.borrow_mut() = entries;
        let first_seq = tail.first().map(|e| e.seq);
        self.render_entries(&tail);
        if let Some(first) = first_seq
            && hidden > 0
        {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.set_halign(gtk::Align::Center);
            let b = gtk::Button::with_label("");
            b.add_css_class("flat");
            b.add_css_class("sill-earlier");
            row.append(&b);
            self.conv.prepend_top(&row);
            *self.earlier.borrow_mut() = Some((first, row, b.clone()));
            self.update_earlier(hidden);
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.show_earlier();
                }
            });
        }
    }

    fn render_entries(&self, entries: &[Entry]) {
        for e in entries {
            match e.kind.as_str() {
                "user" if !e.attachments.is_empty() => self.conv.add_user_full(&e.text, &e.attachments),
                "user" => self.conv.add_user_with(&e.text, &e.extra.lines().filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>()),
                "plan" => self.conv.set_plan_text(&e.text),
                "agent" | "subagent" if !e.attachments.is_empty() => {
                    if !e.text.is_empty() { self.conv.add_agent_block(&e.text); }
                    self.conv.add_agent_images(&e.attachments);
                }
                "agent" => self.conv.add_agent_block(&e.text),
                "thought" => self.conv.add_thought_block(&e.text),
                "subagent" => self.conv.add_subagent_text(&e.parent, &e.text),
                "tool" => self.conv.add_tool_call_full(
                    if e.call_id.is_empty() { format!("stored-{}", e.seq) } else { e.call_id.clone() },
                    &e.text,
                    &e.extra,
                    (!e.tool_kind.is_empty()).then_some(e.tool_kind.as_str()),
                    (!e.detail.is_empty()).then_some(e.detail.as_str()),
                    (!e.parent.is_empty()).then_some(e.parent.as_str()),
                    (!e.diff.is_empty()).then_some(e.diff.as_str()),
                ),
                _ => self.conv.add_note(&e.text),
            }
        }
    }

    /// The offered commands the prompt's `/word` starts, while that is all the prompt holds.
    fn command_matches(&self) -> Vec<sill_core::acp::AgentCommand> {
        let buf = self.prompt.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
        let Some(word) = text.strip_prefix('/').filter(|w| !w.contains(char::is_whitespace)) else { return vec![] };
        self.commands.borrow().iter().filter(|c| c.name.trim_start_matches('/').starts_with(word)).take(8).cloned().collect()
    }

    fn show_command_matches(self: &Rc<Self>) {
        let matches = self.command_matches();
        if matches.is_empty() {
            self.command_pop.popdown();
            return;
        }
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for c in matches {
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("sill-menu-item");
            b.set_can_focus(false);
            let line = match &c.hint { Some(h) if !h.is_empty() => format!("/{} {h}", c.name.trim_start_matches('/')), _ => format!("/{}", c.name.trim_start_matches('/')) };
            let l = gtk::Label::new(Some(&line));
            l.set_xalign(0.0);
            let d = gtk::Label::new(Some(&c.description));
            d.add_css_class("dim-label");
            d.set_xalign(0.0);
            d.set_ellipsize(gtk::pango::EllipsizeMode::End);
            d.set_max_width_chars(60);
            let both = gtk::Box::new(gtk::Orientation::Vertical, 0);
            both.append(&l);
            if !c.description.is_empty() { both.append(&d); }
            b.set_child(Some(&both));
            let weak = Rc::downgrade(self);
            let name = c.name.clone();
            b.connect_clicked(move |_| if let Some(v) = weak.upgrade() { v.complete_command(&name); });
            list.append(&b);
        }
        self.command_pop.set_child(Some(&list));
        if !self.command_pop.is_visible() { self.command_pop.popup(); }
    }

    fn complete_command(&self, name: &str) {
        self.command_pop.popdown();
        let buf = self.prompt.buffer();
        buf.set_text(&format!("/{} ", name.trim_start_matches('/')));
        buf.place_cursor(&buf.end_iter());
        self.prompt.grab_focus();
    }

    fn update_earlier(&self, left: usize) {
        if let Some((_, _, b)) = self.earlier.borrow().as_ref() {
            b.set_label(&format!("show earlier ({left} more)"));
        }
    }

    /// The chunk of history before what is shown, rendered above it; the row goes when none is left.
    fn show_earlier(self: &Rc<Self>) {
        let Some((_, row, _)) = self.earlier.borrow().clone() else { return };
        let chunk = {
            let mut older = self.older.borrow_mut();
            let at = older.len().saturating_sub(HISTORY_CHUNK);
            older.split_off(at)
        };
        let Some(new_first) = chunk.first().map(|e| e.seq) else { return };
        self.conv.begin_insert_after(&row);
        self.render_entries(&chunk);
        self.conv.end_insert();
        let left = self.older.borrow().len();
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
        if busy && !self.busy.get() {
            self.last_event.set(std::time::Instant::now());
        }
        self.busy.set(busy);
        if let Some(b) = self.send_btn.borrow().as_ref() {
            let buffer = self.prompt.buffer();
            let has_text = !buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).trim().is_empty();
            if busy && !has_text {
                // drawn glyphs (not text on a baseline) sit dead centre and size with the button
                let t = theme::current();
                let red = gtk::gdk::RGBA::parse(t.error).unwrap_or_else(|_| super::icons::fg_for(b, &t));
                b.set_child(Some(&super::icons::scalable("sill-stop", red)));
                b.set_tooltip_text(Some("Stop the agent and cancel all queued prompts; Enter queues text"));
                b.add_css_class("sill-stop");
            } else {
                b.set_child(Some(&super::icons::scalable("sill-send", super::icons::fg_for(b, &theme::current()))));
                b.set_tooltip_text(Some(if busy { "Queue prompt (Enter)" } else { "Send (Enter)" }));
                b.remove_css_class("sill-stop");
            }
        }
    }

    /// Stop the running turn and cancel unsent queued prompts.
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
        self.git_monitors.borrow_mut().clear();
        self.pins_row.set_visible(!pins.is_empty());
        if pins.is_empty() {
            self.chips.borrow_mut().clear();
            return;
        }
        let home = session::home().unwrap_or_default().to_string_lossy().into_owned();
        let mut summaries: Vec<(std::path::PathBuf, gtk::Box)> = Vec::new();
        for p in pins {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            chip.add_css_class("sill-chip");
            let l = gtk::Button::new();
            let lbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let is_dir = remote::is_dir(&p);
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
                    if remote::is_dir(&p) {
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
                if let Err(e) = super::client::pin(&v.id(), &p, false) {
                    v.conv.add_error(&format!("unpin: {e:#}"));
                }
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
        if let Some(timer) = self.denied_monitor.borrow_mut().take() { timer.remove(); }
        self.denied_seen.borrow_mut().clear();
        self.denied_asked.set(0);
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local(std::time::Duration::from_secs(3), move || {
            let Some(view) = weak.upgrade() else { return glib::ControlFlow::Break };
            view.check_denied();
            glib::ControlFlow::Continue
        });
        *self.denied_monitor.borrow_mut() = Some(timer);
        self.check_denied();
    }

    fn check_denied(self: &Rc<Self>) {
        let Ok(denied) = remote::request::<sill_core::frontend::Denied>(Request::Denied { session: self.id() }) else { return };
        for host in &denied.answered { self.denied_seen.borrow_mut().insert(host.trim().to_string()); }
        let text = denied.hosts.join("\n");
        let quiet = denied.quiet;
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
            let agent_name = denied.agent_name.clone();
            let mut actions: Vec<(String, Rc<dyn Fn()>)> = Vec::new();
            let remember = {
                let (session, host) = (self.id(), host.to_string());
                move || {
                    if let Err(e) = remote::request::<serde_json::Value>(Request::RememberDenied { session: session.clone(), host: host.clone() }) {
                        eprintln!("sill: remembering refused host: {e:#}");
                    }
                }
            };
            let exact = host.to_string();
            let weak = Rc::downgrade(self);
            let r = remember.clone();
            actions.push((
                format!("Allow {host}"),
                Rc::new(move || {
                    r();
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
                let r = remember.clone();
                actions.push((
                    format!("Allow {wild}"),
                    Rc::new(move || {
                        r();
                        if let Some(v) = weak.upgrade() {
                            v.allow_domain(&w);
                        }
                    }),
                ));
            }
            actions.push(("Keep blocked".into(), Rc::new(remember)));
            self.conv.add_actions(
                &format!("{agent_name} tried to reach {host} and the proxy refused it: the host is not on this agent's allow-list."),
                actions,
            );
        }
    }

    /// Add a host pattern to this agent's `domains` (its registry override) and rebuild the jail
    /// so the proxy picks it up.
    fn allow_domain(self: &Rc<Self>, pattern: &str) {
        match remote::request::<String>(Request::AllowHost { session: self.id(), host: pattern.into() }) {
            Ok(name) if name == "this session" => {
                self.conv.add_note(&format!("{pattern} allowed for this session"));
            }
            Ok(name) => {
                self.conv.add_note(&format!("{pattern} allowed for {name}; the session restarts"));
                self.force_restart.set(true);
                self.request_restart();
            }
            Err(e) => self.conv.add_error(&format!("could not save: {e:#}")),
        }
    }

    /// Drain asynchronously so large clipboard transfers never block the GTK event loop.
    fn read_clipboard_image(
        self: &Rc<Self>, stream: gtk::gio::InputStream,
        cancel: gtk::gio::Cancellable, finished: Rc<std::cell::Cell<bool>>,
        mut bytes: Vec<u8>,
    ) {
        let v = self.clone();
        let next_stream = stream.clone();
        let next_cancel = cancel.clone();
        stream.read_bytes_async(64 * 1024, glib::Priority::DEFAULT, Some(&cancel), move |res| {
            if finished.get() { return; }
            match res {
                Ok(chunk) if chunk.is_empty() => {
                    finished.set(true);
                    let _ = next_stream.close(Some(&next_cancel));
                    v.conv.add_note(&format!("clipboard transfer complete ({} bytes); decoding image…", bytes.len()));
                    match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                        Ok(tex) => v.attach_texture(&tex),
                        Err(e) => v.conv.add_error(&format!("could not decode clipboard image: {e}")),
                    }
                }
                Ok(chunk) => {
                    // Transport safety bound, independent of the decoded attachment limit.
                    if bytes.len().saturating_add(chunk.len()) > 16 * 1024 * 1024 {
                        finished.set(true);
                        next_cancel.cancel();
                        let _ = next_stream.close(None::<&gtk::gio::Cancellable>);
                        v.conv.add_error("clipboard image transfer exceeds 16 MiB");
                        return;
                    }
                    bytes.extend_from_slice(&chunk);
                    v.read_clipboard_image(next_stream, next_cancel, finished, bytes);
                }
                Err(e) => {
                    finished.set(true);
                    let _ = next_stream.close(None::<&gtk::gio::Cancellable>);
                    v.conv.add_error(&format!("clipboard image transfer failed after {} bytes: {e}", bytes.len()));
                }
            }
        });
    }

    /// A pasted image: PNG, inline, bounded by the daemon's decoded attachment limit.
    fn attach_texture(self: &Rc<Self>, tex: &gdk::Texture) {
        let png = tex.save_to_png_bytes();
        if png.len() > sill_proto::initialize::MAX_INLINE_BYTES {
            self.conv.add_note(&format!("image not attached: PNG is {:.2} MiB; image attachments are limited to 10 MiB per submission", png.len() as f64 / (1024.0 * 1024.0)));
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
                self.conv.add_error(&format!("could not read {}: {e}", path.display()));
                return;
            }
        };
        if let Some(mime) = image_mime {
            if bytes.len() > sill_proto::initialize::MAX_INLINE_BYTES {
                self.conv.add_note(&format!("{name} not attached: larger than 10 MiB"));
                return;
            }
            self.add_attachment(Attachment { name, mime: mime.into(), kind: AttachmentKind::Image { data_base64: sill_core::acp::base64(&bytes) } });
            return;
        }
        let upload_id = sill_core::sfp_client::new_id();
        match super::client::quick::<serde_json::Value>("_sill/uploads.create", serde_json::json!({"session_id":self.id(),"upload_id":upload_id,"name":name,"mime":"application/octet-stream","data_base64":sill_core::acp::base64(&bytes)})) {
            Ok(_) => self.add_attachment(Attachment { name, mime: "application/octet-stream".into(), kind: AttachmentKind::File { jail_path: upload_id.into() } }),
            Err(e) => self.conv.add_error(&format!("could not attach {name}: {e:#}")),
        }
    }

    fn add_attachment(self: &Rc<Self>, a: Attachment) {
        let image_bytes = |a: &Attachment| match &a.kind {
            AttachmentKind::Image { data_base64 } => data_base64.len() / 4 * 3
                - data_base64.bytes().rev().take_while(|b| *b == b'=').count(),
            _ => 0,
        };
        let total = self.attachments.borrow().iter().map(image_bytes).sum::<usize>()
            + image_bytes(&a);
        if total > sill_proto::initialize::MAX_INLINE_BYTES {
            self.conv.add_note("image not attached: images together exceed 10 MiB per submission; remove an image or use a smaller one");
            return;
        }
        if matches!(a.kind, AttachmentKind::Image { .. }) && !self.images_ok.get() {
            self.conv.add_note("this agent reports no image support; the image is sent anyway");
        }
        self.attachments.borrow_mut().push(a);
        self.rebuild_attach_strip();
        self.prompt.grab_focus();
    }

    /// What is typed and attached but not sent: taken out of the view (it is about to be closed and
    /// made anew, e.g. a pin restarting the session) and given back with [`Self::restore_draft`].
    pub fn take_draft(&self) -> Draft {
        let buf = self.prompt.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), true).to_string();
        Draft { text, attachments: std::mem::take(&mut *self.attachments.borrow_mut()) }
    }

    /// Put a draft back into a fresh view: the text, cursor at its end, and the attachment chips.
    pub fn restore_draft(self: &Rc<Self>, draft: Draft) {
        if !draft.text.is_empty() {
            let buf = self.prompt.buffer();
            buf.set_text(&draft.text);
            buf.place_cursor(&buf.end_iter());
        }
        if !draft.attachments.is_empty() {
            *self.attachments.borrow_mut() = draft.attachments;
            self.rebuild_attach_strip();
        }
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

    /// Read through the daemon's session-image API, then cache locally for GTK. Live images
    /// are opened by the jail's supervisor, including private mounts such as /tmp.
    fn resolve_jail_path(&self, src: &str) -> Option<std::path::PathBuf> {
        let bytes = match remote::session_image(&self.id(), src) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("sill: session {} image {src:?}: {e:#}", self.id());
                return None;
            }
        };
        let key = format!("{}:{}:{}", std::env::var("SILL_BOX").unwrap_or_default(), self.id(), src);
        remote::cache_image(&key, &bytes, std::path::Path::new(src).extension().and_then(|s| s.to_str()).unwrap_or("img"))
    }

    /// Have the pins changed since the jail started? Only folder pins are mounts; a pinned file never
    /// needs a new jail.
    pub fn pins_changed(&self) -> bool {
        let dirs = |v: &[std::path::PathBuf]| v.iter().filter(|p| remote::is_dir(p)).cloned().collect::<Vec<_>>();
        self.force_restart.get() || dirs(&self.store.pins(&self.id()).unwrap_or_default()) != dirs(&self.live_pins.borrow())
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
        super::client::set(&self.id(), wire::SessionSet::Desktop(id.to_string()));
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
        super::client::set(&self.id(), wire::SessionSet::MemoryScope(scope.to_string()));
        self.session.borrow_mut().memory_scope = scope.to_string();
        let resolved = sill_core::memory::Scope::resolve(scope, !self.store.pins(&self.id()).unwrap_or_default().is_empty());
        self.conv.add_note(&format!("recall: {}; told with the next prompt", resolved.label()));
    }

    /// The session's policy, from the session menu; the jail is rebuilt with it.
    pub fn set_policy(self: &Rc<Self>, name: &str) {
        if self.session.borrow().policy == name {
            return;
        }
        super::client::set(&self.id(), wire::SessionSet::Policy(name.to_string()));
        self.session.borrow_mut().policy = name.to_string();
        self.conv.add_note(&format!("policy: {name}; the session restarts"));
        self.restart_for_jail();
    }

    /// A jail property changed: rebuild the jail (the window waits for the turn to end).
    pub(super) fn restart_for_jail(self: &Rc<Self>) {
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
        self.ready.set(false);
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
                    v.conv.add_error(&format!("could not start the session: {e}"));
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

    /// The agent is up and between turns: it can be asked one more thing.
    pub fn agent_idle(&self) -> bool {
        self.driver.borrow().is_some() && self.ready.get() && !self.busy.get()
    }

    /// *Summarize now* (docs/memory.md §2.5): the daemon asks the agent for a checkpoint summary
    /// and stores it; `then` gets the text and the session's new name when the `Summarized` event
    /// comes back (`summarized`), or None at once when the daemon refuses (the agent busy or down).
    pub fn ask_checkpoint(self: &Rc<Self>, then: Box<dyn FnOnce(Option<String>, Option<String>)>) {
        if self.checkpoint.borrow().is_some() || !self.agent_idle() {
            then(None, None);
            return;
        }
        *self.checkpoint.borrow_mut() = Some(then);
        let id = self.id();
        let (tx, rx) = async_channel::bounded::<anyhow::Result<()>>(1);
        std::thread::Builder::new().name(format!("sill-checkpoint-{id}")).spawn(move || {
            let _ = tx.send_blocking(super::client::checkpoint(&id));
        }).ok();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(Err(e)) = rx.recv().await else { return };
            let Some(v) = weak.upgrade() else { return };
            v.conv.add_error(&format!("no checkpoint: {e:#}"));
            if let Some(cb) = v.checkpoint.borrow_mut().take() {
                cb(None, None);
            }
        });
    }

    /// The daemon's answer to `ask_checkpoint` (the window routes the `Summarized` event here).
    pub fn summarized(&self, text: Option<String>, title: Option<String>) {
        if let Some(t) = &title {
            self.title_changed(t, "agent");
        }
        if let Some(cb) = self.checkpoint.borrow_mut().take() {
            cb(text, title);
        }
    }

    /// A line of sill's own in the conversation (the daemon's note for a summary turn).
    pub fn note(&self, text: &str) {
        self.conv.add_note(text);
    }

    /// The session's name changed (here or in the daemon): the view's copy and the tab follow.
    pub fn title_changed(&self, title: &str, kind: &str) {
        let id = self.id();
        {
            let mut s = self.session.borrow_mut();
            s.title = title.to_string();
            s.title_kind = kind.to_string();
        }
        if let Some(cb) = self.on_title.borrow().as_ref() {
            cb(&id, title);
        }
    }

    /// A checkpoint is being written (its button waits).
    pub fn summary_pending(&self) -> bool {
        self.checkpoint.borrow().is_some()
    }

    fn show_prompt_queue(self: &Rc<Self>, rows: Vec<serde_json::Value>) {
        while let Some(child) = self.prompt_queue.first_child() {
            self.prompt_queue.remove(&child);
        }
        self.conv.set_held_prompts(rows.iter().filter(|r| r["held"].as_bool() == Some(true)).count());
        self.prompt_queue.set_visible(!rows.is_empty());
        if rows.is_empty() { return; }
        self.prompt_queue.set_margin_start(12);
        self.prompt_queue.set_margin_end(12);
        self.prompt_queue.set_margin_top(4);
        self.prompt_queue.set_margin_bottom(4);
        for (index, item) in rows.into_iter().enumerate() {
            let Some(id) = item["submission_id"].as_str() else { continue };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let text = item["text"].as_str().unwrap_or("");
            let count = item["attachments"].as_u64().unwrap_or(0);
            let held = item["held"].as_bool().unwrap_or(false);
            let preview = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let suffix = if count > 0 { format!(" · {count} attachments") } else { String::new() };
            let label = gtk::Label::new(Some(&format!("{}. {}{suffix}{}", index + 1, preview, if held { " · held" } else { "" })));
            label.add_css_class("dim-label");
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_width_chars(1);
            label.set_tooltip_text(Some(&format!("{text}\n{}", item["reason"].as_str().unwrap_or("Waiting for the agent to be ready"))));
            row.append(&label);
            if held {
                let resume = gtk::Button::from_icon_name("media-playback-start-symbolic");
                resume.add_css_class("flat");
                resume.set_tooltip_text(Some("Resume held prompt"));
                self.queue_action(&resume, id.to_string(), true);
                row.append(&resume);
            }
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("flat");
            if let Some(image) = remove.child().and_downcast::<gtk::Image>() { image.set_pixel_size(10); }
            remove.set_tooltip_text(Some("Cancel this prompt if it has not been dispatched"));
            self.queue_action(&remove, id.to_string(), false);
            row.append(&remove);
            self.prompt_queue.append(&row);
        }
    }

    fn queue_action(self: &Rc<Self>, button: &gtk::Button, submission: String, resume: bool) {
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |button| {
            let Some(view) = weak.upgrade() else { return };
            let sid = view.id();
            let sub = submission.clone();
            let weak = weak.clone();
            let button = button.clone();
            button.set_sensitive(false);
            glib::spawn_future_local(async move {
                let result = gtk::gio::spawn_blocking(move || {
                    if resume { super::client::resume_submission(&sid, &sub) }
                    else { super::client::cancel_submission(&sid, &sub) }
                }).await;
                let error = match result {
                    Ok(Ok(())) => return, // The canonical update removes or refreshes the row.
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(_) => "request worker failed".to_string(),
                };
                button.set_sensitive(true);
                if let Some(view) = weak.upgrade() {
                    view.conv.add_error(&format!("{} queued prompt: {error}", if resume { "resume" } else { "remove" }));
                }
            });
        });
    }

    /// Submit a prompt; the daemon queues it while the agent is working.
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
        // The daemon adds the standing instructions, scoped recall and current pins once for
        // every client. Keep the transcript and this request as the person wrote them.
        let attachments = std::mem::take(&mut *self.attachments.borrow_mut());
        let names: Vec<String> = attachments.iter().map(|a| a.name.clone()).collect();
        let entry = wire::Entry::User { text: text.to_string(), names: names.clone() };
        match d.try_send(Command::Prompt { text: text.to_string(), attachments: attachments.clone(), entry: Some(entry) }) {
            Ok(()) => {
                self.rebuild_attach_strip();
                // The accepted SFP user item renders the prompt for every frontend.
                lap("add_user");
                // the transcript entry is the daemon's to write, when it takes the prompt
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
    /// to the transcript, the view only drops its copy.
    fn flush_agent(&self) {
        self.agent_md.borrow_mut().clear();
    }

    pub fn timer_working(&self) -> bool { self.active_time.get().1.is_some() }

    pub fn active_seconds(&self) -> u64 {
        let (total, since) = self.active_time.get();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_millis() as u64;
        total.saturating_add(since.map_or(0, |start| now.saturating_sub(start))) / 1000
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        // any word from the agent ends a shown silence
        self.last_event.set(std::time::Instant::now());
        if self.silence_shown.replace(false) {
            self.status.set_tooltip_text(None);
            if self.busy.get() {
                self.status.set_text("working…");
            }
        }
        match &ev {
            Event::Ready { resumed, controls, .. } => {
                self.ready.set(true);
                self.startup_retried.set(false);
                *self.replaying.borrow_mut() = false;
                self.status.set_text(if *resumed { "resumed" } else { "ready" });
                self.build_controls(controls);
                return;
            }
            Event::Controls(c) => {
                self.build_controls(c);
                return;
            }
            // the daemon answered it (the session auto-allows): a quiet mark on the call's own line
            Event::AutoAnswered { tool_call_id, title, allowed } => {
                self.conv.mark_auto_allowed(tool_call_id, title, !*allowed);
                return;
            }
            Event::Other(t) if t.starts_with("sill:active-time:") => {
                if let Ok(time) = serde_json::from_str(&t["sill:active-time:".len()..]) {
                    self.active_time.set(time);
                }
                return;
            }
            Event::Other(t) if t.starts_with("sill:replay:") => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&t["sill:replay:".len()..])
                    && let Ok(entries) = serde_json::from_value::<Vec<Entry>>(value["entries"].clone())
                {
                    self.replay_history(entries);
                }
                return;
            }
            Event::Other(t) if t == "sill:authenticated" => {
                self.conv.auth_done();
                self.status.set_text("logged in");
                return;
            }
            Event::Other(t) if t.starts_with("sill:queue:") => {
                if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&t["sill:queue:".len()..]) {
                    self.show_prompt_queue(rows);
                }
                return;
            }
            // Held prompts now have their actions in the queue above the input.
            Event::Other(t) if t.starts_with("sill:held:") || t.starts_with("sill:held-resolved:") => return,
            Event::Other(t) if t == "sill:reconnecting" => {
                self.status.set_text("reconnecting");
                return;
            }
            // a command the daemon refused (a turn running, the session gone)
            Event::Other(t) if t.starts_with("refused: ") => {
                self.conv.add_note(&t["refused: ".len()..]);
                return;
            }
            Event::Capabilities { images } => self.images_ok.set(*images),
            Event::Commands(commands) => {
                *self.commands.borrow_mut() = commands.clone();
                return;
            }
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
                self.flush_agent();
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
                    Rc::new(move |m, key| {
                        if let Some(v) = weak.upgrade() {
                            v.authenticate(m, key);
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
                // an agent that went away mid-checkpoint: the daemon says so with an empty answer,
                // but its view may be gone by then
                if let Some(cb) = self.checkpoint.borrow_mut().take() {
                    glib::idle_add_local_once(move || cb(None, None));
                }
            }
            _ => {}
        }
        self.conv.handle(ev);
    }

    /// A turn ended: the chips and the ledger pages refresh (the daemon scans and notes the count).
    fn after_turn(self: &Rc<Self>, id: &str) {
        // the scan, the adoption of the agent's own state and the note with the count are the
        // daemon's after every turn (docs/daemon.md §7 M2); the view refreshes what it shows
        self.refresh_pins(); // git numbers on the chips
        if let Some(cb) = self.on_turn_end.borrow().as_ref() {
            cb(id);
        }
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
        self.close_terminals();
        for m in self.git_monitors.borrow_mut().drain(..) {
            m.cancel();
        }
        if let Some(m) = self.denied_monitor.borrow_mut().take() {
            m.remove();
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
        let session = self.id();
        let (tx, rx) = async_channel::bounded::<Option<Vec<sill_core::shim::Proc>>>(1);
        std::thread::spawn(move || {
            let list = remote::processes(&session).ok();
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
            // Automatic exposure must use the rules actually enforced at launch.
            let name = self.session.borrow().policy.clone();
            let snapshot = super::client::call::<sill_proto::admin::PolicySnapshot>(
                "_sill/policy.snapshot", sill_proto::sessions::SessionKey { session_id: self.id() });
            let text = match snapshot {
                Ok(snapshot) => match snapshot.snapshot {
                    Some(view) => view.text,
                    None => remote::request::<String>(Request::PolicyText { name }).ok(),
                },
                Err(_) => None, // failed inspection never grants automatic exposure
            };
            text.and_then(|text| sill_core::policy::Policy::parse(&text).ok())
                .map(|p| p.processes).unwrap_or_default()
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
        match super::processes::expose(self.id(), port) {
            Ok(b) => {
                let note = if b.host_port == port {
                    format!("{name}: port {port} is open on this machine at {}", b.url())
                } else {
                    format!("{name}: port {port} is open on this machine at {} ({port} was taken)", b.url())
                };
                self.conv.add_note(&note);
                self.bridges.borrow_mut().insert(port, b);
            }
            Err(e) => self.conv.add_error(&format!("{name}: port {port} could not be exposed: {e:#}")),
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
                    if let Some(cb) = v.on_open_log.borrow().as_ref() {
                        cb(&format!("{n} · process"), v.id(), n.clone());
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
        let session = self.id();
        let name = name.to_string();
        let (tx, rx) = async_channel::bounded::<Result<(), String>>(1);
        std::thread::spawn(move || {
            let request = if stop { Request::ProcStop { session, name } } else { Request::ProcDismiss { session, name } };
            let r = remote::request::<serde_json::Value>(request).map(|_| ());
            let _ = tx.send_blocking(r.map_err(|e| format!("{e:#}")));
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let r = rx.recv().await;
            let Some(v) = weak.upgrade() else { return };
            if let Ok(Err(e)) = r {
                v.conv.add_error(&format!("process: {e}"));
            }
            v.poll_procs();
        });
    }
}

/// (+insertions, −deletions, untracked files) vs HEAD: of a pinned folder's working tree, or of a
/// pinned file alone (in the repository that holds it, found by walking up); None outside a repo.
fn git_summary(path: &std::path::Path) -> Option<(u64,u64,u64)> {
    remote::git_summary(path)
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

/// `terminals.close` off the main loop: the daemon answers quickly, but the GTK thread never waits.
fn close_in_background(bridge: super::streams::PtyBridge) {
    let _ = std::thread::Builder::new().name("sill-terminal-close".into()).spawn(move || bridge.close());
}
