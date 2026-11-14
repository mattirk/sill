//! One session tab: conversation + bottom area (prompt now; prompt/shell switcher in M3). Owns the
//! driver, persists the transcript, runs auto-keep at turn end.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use sill_core::acp::{Command, Event};
use sill_core::store::{Session, Store};

use super::conversation::Conversation;
use super::session;

pub struct SessionView {
    pub root: gtk::Box,
    prompt: gtk::TextView,
    pub session: RefCell<Session>,
    pub conv: Rc<Conversation>,
    pub status: gtk::Label,
    driver: RefCell<Option<session::Driver>>,
    store: Rc<Store>,
    /// Markdown of the agent message being streamed (persisted at turn end).
    agent_md: RefCell<String>,
    /// After a resume with replay: ignore replayed events until the first turn starts.
    replaying: RefCell<bool>,
    /// Called when a turn ends (the window refreshes the ledger pages).
    pub on_turn_end: RefCell<Option<Box<dyn Fn(&str)>>>,
    pub on_title: RefCell<Option<Box<dyn Fn(&str, &str)>>>,
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
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        prompt_scroll.add_css_class("frame");

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&conv.root);
        root.append(&prompt_scroll);

        let view = Rc::new(Self {
            root,
            prompt: prompt.clone(),
            session: RefCell::new(session),
            conv,
            status,
            driver: RefCell::new(None),
            store,
            agent_md: RefCell::new(String::new()),
            replaying: RefCell::new(false),
            on_turn_end: RefCell::new(None),
            on_title: RefCell::new(None),
        });

        // stored transcript
        if let Ok(entries) = view.store.entries(&view.session.borrow().id) {
            for e in entries {
                match e.kind.as_str() {
                    "user" => view.conv.add_user(&e.text),
                    "agent" => view.conv.add_agent_block(&e.text),
                    "thought" => view.conv.add_thought_block(&e.text),
                    "tool" => view.conv.add_tool_call(format!("stored-{}", e.seq), &e.text, &e.extra),
                    _ => view.conv.add_note(&e.text),
                }
            }
        }

        // Enter sends, Shift+Enter inserts a newline
        let keys = gtk::EventControllerKey::new();
        {
            let v = view.clone();
            let prompt = prompt.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
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

        view.connect();
        view
    }

    pub fn focus_prompt(&self) {
        self.prompt.grab_focus();
    }

    pub fn id(&self) -> String {
        self.session.borrow().id.clone()
    }

    fn connect(self: &Rc<Self>) {
        let s = self.session.borrow().clone();
        match session::start(&s) {
            Ok(d) => {
                let events = d.events.clone();
                *self.driver.borrow_mut() = Some(d);
                let v = self.clone();
                glib::spawn_future_local(async move {
                    while let Ok(ev) = events.recv().await {
                        v.handle(ev);
                    }
                });
            }
            Err(e) => {
                self.status.set_text("failed to start");
                self.conv.add_note(&format!("could not start the session: {e:#}"));
            }
        }
    }

    /// Send a prompt; false if the agent is busy or the session is down.
    pub fn send(&self, text: &str) -> bool {
        let Some(d) = self.driver.borrow().as_ref().map(|d| d.commands.clone()) else {
            self.conv.add_note("session is not running");
            return false;
        };
        match d.try_send(Command::Prompt(text.to_string())) {
            Ok(()) => {
                self.conv.add_user(text);
                let id = self.id();
                let _ = self.store.append(&id, "user", text, "");
                let _ = self.store.touch(&id);
                // first prompt names the session (keywords), unless it has nothing to say
                let first = self.store.entries(&id).map(|e| e.iter().filter(|e| e.kind == "user").count() <= 1).unwrap_or(false);
                if first && let Some(title) = sill_core::title::from_prompt(text) {
                    let _ = self.store.set_title(&id, &title);
                    self.session.borrow_mut().title = title.clone();
                    if let Some(cb) = self.on_title.borrow().as_ref() {
                        cb(&id, &title);
                    }
                }
                true
            }
            Err(_) => {
                self.conv.add_note("the agent is busy; wait for the turn to end");
                false
            }
        }
    }

    fn flush_agent(&self) {
        let md = std::mem::take(&mut *self.agent_md.borrow_mut());
        if !md.trim().is_empty() {
            let _ = self.store.append(&self.id(), "agent", &md, "");
        }
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        let replaying = *self.replaying.borrow();
        match &ev {
            Event::Ready { session_id, resumed } => {
                let _ = self.store.set_acp_session_id(&self.id(), session_id);
                self.session.borrow_mut().acp_session_id = Some(session_id.clone());
                *self.replaying.borrow_mut() = *resumed;
                self.status.set_text(if *resumed { "resumed" } else { "ready" });
                return;
            }
            Event::TurnStarted => {
                *self.replaying.borrow_mut() = false;
            }
            Event::UserText(_) | Event::Text(_) | Event::Thought(_) | Event::ToolCall { .. } | Event::ToolCallUpdate { .. }
                if replaying =>
            {
                return; // the stored transcript is already on screen
            }
            Event::Text(t) => self.agent_md.borrow_mut().push_str(t),
            Event::ToolCall { title, status, .. } => {
                self.flush_agent();
                let _ = self.store.append(&self.id(), "tool", title, status);
            }
            Event::Stopped(_) => {
                self.flush_agent();
                let id = self.id();
                let v = self.clone();
                glib::idle_add_local_once(move || v.after_turn(&id));
            }
            _ => {}
        }
        self.conv.handle(ev);
    }

    /// Auto-keep repo changes (SPEC §4.4) and tell the window to refresh the ledger pages.
    fn after_turn(self: &Rc<Self>, id: &str) {
        let s = self.session.borrow().clone();
        let (tx, rx) = async_channel::bounded::<String>(1);
        let home = session::home().unwrap_or_default();
        let uid = session::uid();
        let user = std::env::var("USER").unwrap_or_default();
        std::thread::spawn(move || {
            let msg = (|| -> anyhow::Result<String> {
                let policy = sill_core::policy::Policy::load(&s.policy, &home)?;
                let state_dir = sill_core::session::state_dir(uid, &s.id)?;
                let plan = sill_core::changes::session_plan(policy.clone(), state_dir, &home, uid, &user)?;
                let changes = sill_core::changes::scan(&policy, &plan, None)?;
                let (kept, skipped) = sill_core::changes::auto_keep(&policy, &home, &changes);
                let pending = changes.len() - kept.len();
                let mut msg = String::new();
                if !kept.is_empty() {
                    msg.push_str(&format!("kept {} file(s) in repo roots", kept.len()));
                }
                if !skipped.is_empty() {
                    msg.push_str(&format!("{}{} not auto-kept", if msg.is_empty() { "" } else { "; " }, skipped.len()));
                }
                if pending > 0 {
                    msg.push_str(&format!("{}{} change(s) pending in Changes", if msg.is_empty() { "" } else { "; " }, pending));
                }
                Ok(msg)
            })();
            let _ = tx.send_blocking(msg.unwrap_or_else(|e| format!("ledger: {e:#}")));
        });
        let v = self.clone();
        let id = id.to_string();
        glib::spawn_future_local(async move {
            if let Ok(msg) = rx.recv().await {
                if !msg.is_empty() {
                    v.conv.add_note(&msg);
                }
                if let Some(cb) = v.on_turn_end.borrow().as_ref() {
                    cb(&id);
                }
            }
        });
    }

    pub fn quit(&self) {
        if let Some(d) = self.driver.borrow().as_ref() {
            let _ = d.commands.try_send(Command::Quit);
        }
    }
}
