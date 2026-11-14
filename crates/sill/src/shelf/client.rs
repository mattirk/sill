//! The shelf's connection to the daemon (docs/daemon.md §3.4): one client, connected on first use,
//! made again when the daemon left (a restart keeps every session; only the socket is new).
//! Blocking — callers run it off the main thread, as they did `ensure_started`.
//!
//! Sessions' conversations arrive as `_sill/session.event` notifications on that one connection; a
//! dispatcher thread per connection hands each to the view attached to its session, as the
//! `acp::Event` the view has always rendered. When the connection goes, every view attached through
//! it gets `Ended` and attaches again on its next prompt.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sill_core::acp::Event;
use sill_proto::Client;

static CLIENT: OnceLock<Mutex<Option<Arc<Client>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Client>>> {
    CLIENT.get_or_init(|| Mutex::new(None))
}

/// Session id → (the view's event channel, the connection it attached through).
type Subs = Mutex<HashMap<String, (async_channel::Sender<Event>, usize)>>;
static SUBS: OnceLock<Subs> = OnceLock::new();

fn subs() -> &'static Subs {
    SUBS.get_or_init(Default::default)
}

/// What the window, not a view, handles: a session's lifecycle events (`SessionEnded`,
/// `Summarized`, `Note` — the view may be gone or never attached) and a question another frontend
/// answered, named by the tool call its dialog belongs to.
pub enum Lifecycle {
    Event(sill_core::wire::Event),
    Resolved { tool_call_id: String },
    /// `changes.changed`: the overlays changed; whatever shows them rescans.
    ChangesChanged,
}

type LifecycleTx = Mutex<Option<async_channel::Sender<(String, Lifecycle)>>>;
static LIFECYCLE: OnceLock<LifecycleTx> = OnceLock::new();

pub fn set_lifecycle(tx: async_channel::Sender<(String, Lifecycle)>) {
    *LIFECYCLE.get_or_init(Default::default).lock().unwrap() = Some(tx);
}

fn lifecycle(key: String, l: Lifecycle) {
    if let Some(tx) = LIFECYCLE.get().and_then(|l| l.lock().unwrap().clone()) {
        let _ = tx.send_blocking((key, l));
    }
}

/// Open permission requests by (session, request number) → the tool call whose dialog shows them,
/// so a `Resolved` from another frontend finds the dialog to close.
type Open = Mutex<HashMap<(String, u64), String>>;
static OPEN: OnceLock<Open> = OnceLock::new();

fn open() -> &'static Open {
    OPEN.get_or_init(Default::default)
}

/// A connection's identity in `SUBS` (its address: the `Arc` lives as long as it is in use).
fn tag(c: &Arc<Client>) -> usize {
    Arc::as_ptr(c) as usize
}

fn connected() -> Result<Arc<Client>> {
    let mut s = slot().lock().unwrap();
    if let Some(c) = s.as_ref() {
        return Ok(c.clone());
    }
    let c = Arc::new(Client::connect_or_start()?);
    *s = Some(c.clone());
    let d = c.clone();
    std::thread::Builder::new().name("sill-client-events".into()).spawn(move || dispatch(d))?;
    Ok(c)
}

/// A connection of its own, made on first use and again when the daemon has closed it. The daemon
/// answers one connection's requests in order, so what must not wait gets a lane apart from the
/// first one, which carries attaches (seconds while a jail starts), ends and overlay scans.
type Lane = OnceLock<Mutex<Option<Arc<Client>>>>;

/// The shelf's own writes (`sessions.set`, pins, notes…), from the main loop.
static WRITER: Lane = OnceLock::new();
/// Answers to the agent's questions and the view's one blocking read (`sessions.jail_view`): the
/// person just clicked, or the GTK thread waits — never behind a scan or a delete.
static QUICK: Lane = OnceLock::new();

fn lane(lane: &'static Lane) -> Result<Arc<Client>> {
    let slot = lane.get_or_init(|| Mutex::new(None));
    let mut s = slot.lock().unwrap();
    if let Some(c) = s.as_ref() {
        return Ok(c.clone());
    }
    let c = Arc::new(Client::connect_or_start()?);
    *s = Some(c.clone());
    Ok(c)
}

/// One request on a lane; on a connection the daemon has closed, once more on a fresh one.
fn call_on<T: DeserializeOwned>(l: &'static Lane, method: &str, params: impl Serialize + Clone) -> Result<T> {
    let c = lane(l)?;
    match c.call(method, params.clone()) {
        Err(e) if Client::is_closed(&e) => {
            let slot = l.get_or_init(|| Mutex::new(None));
            let mut s = slot.lock().unwrap();
            if s.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &c)) {
                *s = None;
            }
            drop(s);
            lane(l)?.call(method, params)
        }
        r => r,
    }
}

fn write<T: DeserializeOwned>(method: &str, params: impl Serialize + Clone) -> Result<T> {
    call_on(&WRITER, method, params)
}

/// A request that must not queue (see [`QUICK`]).
pub fn quick<T: DeserializeOwned>(method: &str, params: impl Serialize + Clone) -> Result<T> {
    call_on(&QUICK, method, params)
}

/// One field of a session's row (docs/daemon.md §7 M1c: the daemon is the store's only writer).
/// An error is logged, not raised: the view has already changed what it shows.
pub fn set(id: &str, set: sill_core::wire::SessionSet) {
    let what = format!("{set:?}");
    match serde_json::to_value(&set).map_err(anyhow::Error::from).and_then(|set| write::<serde_json::Value>(sill_proto::method::SESSIONS_SET, sill_proto::SessionSet { key: id.to_string(), set })) {
        Ok(_) => {}
        Err(e) => eprintln!("sill: session {id} {what}: {e:#}"),
    }
}

/// A new session row; its id.
pub fn new_session(n: sill_proto::NewSession) -> Result<String> {
    Ok(write::<sill_proto::Key>(sill_proto::method::SESSIONS_NEW, n)?.key)
}

/// The trash can: jail, state and row go. Blocking for as long as the stop takes.
pub fn delete(id: &str) -> Result<()> {
    write::<serde_json::Value>(sill_proto::method::SESSIONS_DELETE, sill_proto::Key { key: id.to_string() })?;
    Ok(())
}

pub fn pin(id: &str, path: &std::path::Path, on: bool) -> Result<()> {
    let m = if on { sill_proto::method::SESSIONS_PIN } else { sill_proto::method::SESSIONS_UNPIN };
    write::<serde_json::Value>(m, sill_proto::Pin { key: id.to_string(), path: path.to_path_buf() })?;
    Ok(())
}

/// A global pin added or removed (docs/pins.md); the daemon canonicalizes and refuses nested ones.
pub fn global_pin(path: &std::path::Path, on: bool) -> Result<()> {
    let m = if on { sill_proto::method::PINS_ADD } else { sill_proto::method::PINS_REMOVE };
    write::<serde_json::Value>(m, sill_proto::GlobalPin { path: path.to_path_buf() })?;
    Ok(())
}

pub fn note_add(id: &str, text: &str) -> Result<()> {
    write::<serde_json::Value>(sill_proto::method::MEMORY_NOTE_ADD, sill_proto::Note { key: id.to_string(), text: text.to_string(), ts: 0 })?;
    Ok(())
}

pub fn note_remove(id: &str, ts: i64, text: &str) -> Result<()> {
    write::<serde_json::Value>(sill_proto::method::MEMORY_NOTE_REMOVE, sill_proto::Note { key: id.to_string(), text: text.to_string(), ts })?;
    Ok(())
}

pub fn set_agent_settings(agent: &str, settings: &sill_core::store::AgentSettings) -> Result<()> {
    write::<serde_json::Value>(sill_proto::method::AGENTS_SET_SETTINGS, sill_proto::AgentSettingsSet { agent: agent.to_string(), settings: serde_json::to_value(settings)? })?;
    Ok(())
}

pub fn compact() -> Result<sill_proto::Compacted> {
    write(sill_proto::method::MEMORY_COMPACT, sill_proto::Empty::default())
}

pub fn forget_ended() -> Result<usize> {
    Ok(write::<sill_proto::Count>(sill_proto::method::MEMORY_FORGET, sill_proto::Empty::default())?.n)
}

fn forget(c: &Arc<Client>) {
    let mut s = slot().lock().unwrap();
    if s.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, c)) {
        *s = None;
    }
}

/// One request; on a connection the daemon has closed, once more on a fresh one.
pub fn call<T: DeserializeOwned>(method: &str, params: impl Serialize + Clone) -> Result<T> {
    let c = connected()?;
    match c.call(method, params.clone()) {
        Err(e) if Client::is_closed(&e) => {
            forget(&c);
            connected()?.call(method, params)
        }
        r => r,
    }
}

/// `daemon.status`, for the Settings page and the session note.
pub fn status() -> Result<sill_proto::DaemonStatus> {
    call(sill_proto::method::DAEMON_STATUS, sill_proto::Empty::default())
}

/// A session's log tail, as text — the viewer's reader.
pub fn session_log(id: &str, lines: usize) -> String {
    match call::<sill_proto::Text>(sill_proto::method::SESSIONS_LOG, sill_proto::LogRequest { key: id.to_string(), lines }) {
        Ok(t) => t.text,
        Err(e) => format!("{e:#}"),
    }
}

/// Attach to a session's conversation (the daemon starts jail and agent when needed): its events
/// go to `events` from now on — the session's current state first. Registered before the call, so
/// nothing the daemon sends ahead of its answer is lost.
pub fn attach(id: &str, fresh: bool, events: async_channel::Sender<Event>) -> Result<()> {
    let req = sill_proto::Attach { key: id.to_string(), fresh };
    let mut tries = 0;
    loop {
        let c = connected()?;
        subs().lock().unwrap().insert(id.to_string(), (events.clone(), tag(&c)));
        match c.call::<serde_json::Value>(sill_proto::method::SESSIONS_ATTACH, req.clone()) {
            Err(e) if Client::is_closed(&e) && tries == 0 => {
                forget(&c);
                tries += 1;
            }
            Err(e) => {
                subs().lock().unwrap().remove(id);
                return Err(e);
            }
            Ok(_) => return Ok(()),
        }
    }
}

/// The view is done with the session (its tab closed or its driver replaced).
pub fn detach(id: &str) {
    subs().lock().unwrap().remove(id);
    let _ = call::<serde_json::Value>(sill_proto::method::SESSIONS_DETACH, sill_proto::Key { key: id.to_string() });
}

/// A command to the session: a refusal (a turn running, the session gone) comes back as the error.
pub fn command(id: &str, cmd: &sill_core::wire::Command) -> Result<()> {
    let command = serde_json::to_value(cmd)?;
    call::<serde_json::Value>(sill_proto::method::SESSIONS_COMMAND, sill_proto::SessionCommand { key: id.to_string(), command })?;
    Ok(())
}

/// End a session as closing its tab does (docs/memory.md §2.2); the daemon writes the summary or
/// asks the agent for it first (`pending`), then sends `SessionEnded`.
pub fn end(id: &str, reason: &str) -> Result<sill_proto::Ending> {
    call(sill_proto::method::SESSIONS_END, sill_proto::End { key: id.to_string(), reason: reason.to_string() })
}

/// *Summarize now*: the answer comes as a `Summarized` event.
pub fn checkpoint(id: &str) -> Result<()> {
    call::<serde_json::Value>(sill_proto::method::SESSIONS_CHECKPOINT, sill_proto::Key { key: id.to_string() })?;
    Ok(())
}

/// Route one connection's notifications to the attached views until it closes; then every view
/// that attached through it learns the session's conversation is gone from here.
fn dispatch(c: Arc<Client>) {
    let me = tag(&c);
    loop {
        let next = c.notifications.lock().unwrap().recv();
        let Ok((method, params)) = next else { break };
        if method == sill_proto::method::CHANGES_CHANGED {
            lifecycle(String::new(), Lifecycle::ChangesChanged);
            continue;
        }
        if method != sill_proto::method::SESSION_EVENT {
            continue;
        }
        let Ok(n) = serde_json::from_value::<sill_proto::SessionEvent>(params) else { continue };
        let Ok(wire) = serde_json::from_value::<sill_core::wire::Event>(n.event) else { continue };
        if matches!(wire, sill_core::wire::Event::SessionEnded { .. } | sill_core::wire::Event::Summarized { .. } | sill_core::wire::Event::Note(_)) {
            lifecycle(n.key, Lifecycle::Event(wire));
            continue;
        }
        match &wire {
            sill_core::wire::Event::PermissionRequest { request, tool_call_id, .. } => {
                open().lock().unwrap().insert((n.key.clone(), *request), tool_call_id.clone());
            }
            sill_core::wire::Event::Resolved { request } => {
                if let Some(tool_call_id) = open().lock().unwrap().remove(&(n.key.clone(), *request)) {
                    lifecycle(n.key, Lifecycle::Resolved { tool_call_id });
                }
                continue;
            }
            _ => {}
        }
        let Some(tx) = subs().lock().unwrap().get(&n.key).filter(|(_, t)| *t == me).map(|(tx, _)| tx.clone()) else { continue };
        let key = n.key.clone();
        // a question: the view answers through a reply channel, the answer goes to the daemon
        let ev = wire.into_acp(move |request, answer| {
            std::thread::spawn(move || {
                let Ok(answer) = futures::executor::block_on(answer) else { return };
                let Ok(answer) = serde_json::to_value(answer) else { return };
                if let Err(e) = quick::<serde_json::Value>(sill_proto::method::SESSIONS_ANSWER, sill_proto::SessionAnswer { key, request, answer }) {
                    eprintln!("sill: answering the agent: {e:#}");
                }
            });
        });
        if let Some(ev) = ev {
            let _ = tx.send_blocking(ev);
        }
    }
    forget(&c);
    open().lock().unwrap().clear();
    let gone: Vec<async_channel::Sender<Event>> = {
        let mut s = subs().lock().unwrap();
        let ids: Vec<String> = s.iter().filter(|(_, (_, t))| *t == me).map(|(k, _)| k.clone()).collect();
        ids.into_iter().filter_map(|k| s.remove(&k)).map(|(tx, _)| tx).collect()
    };
    for tx in gone {
        let _ = tx.send_blocking(Event::Ended(Some("the connection to the sill daemon closed".into())));
    }
}
