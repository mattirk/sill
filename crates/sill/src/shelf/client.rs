//! The shelf's connection to the daemon (docs/daemon.md §3.4): one client, connected on first use,
//! made again when the daemon left (a restart keeps every session; only the socket is new).
//! Blocking — callers run it off the main thread, as they did `ensure_started`.
//!
//! Sessions' conversations arrive as `_sill/session.event` notifications on that one connection; a
//! dispatcher thread per connection hands each to the view attached to its session, as the
//! `acp::Event` the view has always rendered. When the connection goes, every view attached through
//! it reconnects with backoff and restores live subscriptions without ending their views.

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
#[derive(Clone)]
struct Subscription {
    events: async_channel::Sender<Event>,
    connection: usize,
    generation: u64,
    view: Arc<Mutex<sill_core::sfp_client::View>>,
}
static NEXT_SUB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
type Subs = Mutex<HashMap<String, Subscription>>;
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
    let result = call_on(&WRITER, method, params);
    if result.is_ok() { super::remote::invalidate(); }
    result
}

/// A request that must not queue (see [`QUICK`]).
pub fn quick<T: DeserializeOwned>(method: &str, params: impl Serialize + Clone) -> Result<T> {
    call_on(&QUICK, method, params)
}

/// One field of a session's row (docs/daemon.md §7 M1c: the daemon is the store's only writer).
/// An error is logged, not raised: the view has already changed what it shows.
pub fn set(id: &str, set: sill_core::wire::SessionSet) {
    let what = format!("{set:?}");
    match write::<serde_json::Value>(sill_proto::method::SESSIONS_SET, sill_proto::SessionSet { key: id.to_string(), set }) {
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
pub fn attach(id: &str, fresh: bool, events: async_channel::Sender<Event>) -> Result<u64> {
    if fresh {
        let c = connected()?;
        c.call::<serde_json::Value>(sill_proto::method::SESSIONS_ATTACH, sill_proto::Attach { key: id.into(), fresh: true })?;
        c.call::<serde_json::Value>(sill_proto::method::SESSIONS_DETACH, sill_proto::Key { key: id.into() })?;
    }
    let view = Arc::new(Mutex::new(sill_core::sfp_client::View::default()));
    let generation = NEXT_SUB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut c = connected()?;
    subs().lock().unwrap().insert(id.to_string(), Subscription { events: events.clone(), connection: tag(&c), generation, view: view.clone() });
    let mut tries = 0;
    loop {
        let result = {
            let mut view = view.lock().unwrap();
            view.attach(&c, id).map(|initial| { for event in initial { deliver(&c, id, &events, &view, event); } })
        };
        match result {
            Err(e) if Client::is_closed(&e) && tries == 0 => {
                forget(&c);
                tries += 1;
                let next = connected()?;
                let mut all = subs().lock().unwrap();
                let Some(sub) = all.get_mut(id).filter(|s| s.generation == generation) else { return Ok(generation) };
                sub.connection = tag(&next);
                drop(all);
                c = next;
            }
            Err(e) => {
                let mut all = subs().lock().unwrap();
                if all.get(id).is_some_and(|s| s.generation == generation) { all.remove(id); }
                return Err(e);
            }
            Ok(_) => {
                detach_abandoned(&c, id, generation, &view);
                return Ok(generation);
            }
        }
    }
}

fn detach_abandoned(c: &Arc<Client>, id: &str, generation: u64, view: &Arc<Mutex<sill_core::sfp_client::View>>) {
    let still_used = subs().lock().unwrap().get(id).is_some_and(|s| s.connection == tag(c) && s.generation == generation);
    if !still_used { let _ = view.lock().unwrap().detach(c); }
}

/// The view is done with the session (its tab closed or its driver replaced).
pub fn detach(id: &str, generation: u64) {
    let current = slot().lock().unwrap().clone();
    let mut all = subs().lock().unwrap();
    if !all.get(id).is_some_and(|s| s.generation == generation) { return; }
    let sub = all.remove(id).unwrap();
    drop(all);
    // Serialize removal with the next driver's registration. A delayed old driver's Quit must
    // not unsubscribe a replacement that has already attached through the same connection.
    if let Some(c) = current.filter(|c| tag(c) == sub.connection) {
        let _ = sub.view.lock().unwrap().detach(&c);
    }
}

/// A command to the session: a refusal (a turn running, the session gone) comes back as the error.
pub fn command(id: &str, cmd: &sill_core::wire::Command) -> Result<()> {
    let view = subs().lock().unwrap().get(id).map(|s|s.view.clone());
    let view = view.as_ref().map(|v|v.lock().unwrap());
    let (method, params) = sill_core::sfp_client::command_request(id, cmd, view.as_deref())?;
    drop(view);
    if method == "_sill/sessions.submit" {
        // Retrying this exact intent is safe: params retains the generated submission ID.
        call::<serde_json::Value>(method, params)?;
    } else {
        // An uncertain control RPC is not permission to repeat authentication or session retry.
        connected()?.call::<serde_json::Value>(method, params)?;
    }
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
            super::remote::invalidate();
            lifecycle(String::new(), Lifecycle::ChangesChanged);
            continue;
        }
        if method == "_sill/session.event" {
            let session_id = params.get("session_id").and_then(|v|v.as_str()).map(str::to_owned);
            let notification = match serde_json::from_value::<sill_proto::sfp::SessionEvent>(params) {
                Ok(notification) => notification,
                Err(error) => {
                    if let Some(id) = session_id {
                        let sub = subs().lock().unwrap().get(&id).filter(|s|s.connection == me).cloned();
                        if let Some(sub) = sub {
                            let mut view = sub.view.lock().unwrap();
                            let _ = view.detach(&c);
                            view.subscription_id.clear();
                            let _ = sub.events.send_blocking(Event::Other(format!("refused: unsupported SFP event; synchronization stopped: {error}")));
                        }
                    }
                    continue;
                }
            };
            let id = notification.session_id.clone();
            let sub = subs().lock().unwrap().get(&id).filter(|s|s.connection == me).cloned();
            if let Some(sub) = sub {
                let mut view = sub.view.lock().unwrap();
                let result = view.accept(notification).or_else(|_| view.attach(&c, &id));
                match result {
                    Ok(events) => for event in events { deliver(&c, &id, &sub.events, &view, event); },
                    Err(e) => { let _ = sub.events.send_blocking(Event::Other(format!("refused: synchronizing session: {e:#}"))); }
                }
            }
            continue;
        }
        if method != sill_proto::method::SESSION_EVENT {
            continue;
        }
        let Ok(n) = serde_json::from_value::<sill_proto::SessionEvent>(params) else { continue };
        let wire = n.event;
        if matches!(wire, sill_core::wire::Event::SessionEnded { .. } | sill_core::wire::Event::Summarized { .. } | sill_core::wire::Event::Note(_)) {
            super::remote::invalidate();
            lifecycle(n.key, Lifecycle::Event(wire));
            continue;
        }
        // Internal notifications are retained only for window lifecycle while host methods
        // are migrated. Conversation state comes exclusively from the public SFP subscription.

    }
    forget(&c);
    super::remote::invalidate();
    let waiting: Vec<(String, u64)> = {
        let mut all = subs().lock().unwrap();
        all.retain(|_, sub| !sub.events.is_closed());
        all.iter_mut().filter_map(|(id, sub)| {
            if sub.connection != me { return None; }
            sub.connection = 0;
            let _ = sub.events.try_send(Event::Other("sill:reconnecting".into()));
            Some((id.clone(), sub.generation))
        }).collect()
    };
    // Request numbers belong to the restored runner. Replayed pending questions install their
    // new reply channels; never forward an answer through a stale permission widget.
    let ids: std::collections::HashSet<&str> = waiting.iter().map(|(id, _)| id.as_str()).collect();
    open().lock().unwrap().retain(|(id, _), _| !ids.contains(id.as_str()));
    if !waiting.is_empty() {
        let _ = std::thread::Builder::new().name("sill-client-reconnect".into()).spawn(move || reconnect(waiting));
    }
}

/// A stale reconnect may race a tab closing or a new driver. Generation checks ensure it cannot
/// resurrect that old subscription or remove its replacement.
fn reconnect(mut waiting: Vec<(String, u64)>) {
    let mut delay = std::time::Duration::from_millis(100);
    while !waiting.is_empty() {
        waiting.retain(|(id, generation)| subs().lock().unwrap().get(id)
            .is_some_and(|s| s.generation == *generation && !s.events.is_closed() && s.connection == 0));
        if waiting.is_empty() { return; }
        std::thread::sleep(delay);
        let c = match connected() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("sill: reconnecting to the daemon: {e:#}");
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
        };
        let mut retry = Vec::new();
        for (id, generation) in waiting {
            let (events, view) = {
                let mut all = subs().lock().unwrap();
                let Some(sub) = all.get_mut(&id).filter(|s| s.generation == generation && s.connection == 0) else { continue };
                sub.connection = tag(&c); // register before replay starts
                (sub.events.clone(), sub.view.clone())
            };
            let result = (|| -> Result<()> {
                let session: sill_proto::Session = c.call(sill_proto::method::SESSIONS_GET, sill_proto::Key { key: id.clone() })?;
                if session.ended_at.is_some() {
                    lifecycle(id.clone(), Lifecycle::Event(sill_core::wire::Event::SessionEnded { deleted: false, reason: "ended".into() }));
                    anyhow::bail!("the session ended while the daemon reconnected");
                }
                let mut state = view.lock().unwrap();
                for event in state.attach(&c, &id)? { deliver(&c, &id, &events, &state, event); }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    // A detach during the request must still win. Do not detach a replacement
                    // driver which has already attached through this same connection.
                    detach_abandoned(&c, &id, generation, &view);
                }
                Err(e) if Client::is_closed(&e) => {
                    forget(&c);
                    let mut all = subs().lock().unwrap();
                    if let Some(sub) = all.get_mut(&id).filter(|s| s.generation == generation) {
                        sub.connection = 0;
                        retry.push((id, generation));
                    }
                }
                Err(e) => {
                    let mut all = subs().lock().unwrap();
                    if all.get(&id).is_some_and(|s| s.generation == generation) { all.remove(&id); }
                    drop(all);
                    let _ = events.send_blocking(Event::Ended(Some(format!("reattaching: {e:#}"))));
                }
            }
        }
        waiting = retry;
        delay = (delay * 2).min(std::time::Duration::from_secs(5));
    }
}

fn deliver(_client: &Arc<Client>, id: &str, tx: &async_channel::Sender<Event>, view: &sill_core::sfp_client::View, wire: sill_core::wire::Event) {
    if matches!(&wire, sill_core::wire::Event::SessionEnded { .. } | sill_core::wire::Event::Summarized { .. } | sill_core::wire::Event::Note(_)) {
        super::remote::invalidate();
        lifecycle(id.into(), Lifecycle::Event(wire));
        return;
    }
    match &wire {
        sill_core::wire::Event::PermissionRequest { request, tool_call_id, .. } => { open().lock().unwrap().insert((id.into(), *request), tool_call_id.clone()); }
        sill_core::wire::Event::Resolved { request } => {
            if let Some(tool_call_id) = open().lock().unwrap().remove(&(id.into(), *request)) { lifecycle(id.into(), Lifecycle::Resolved { tool_call_id }); }
            return;
        }
        _ => {}
    }
    let request = match &wire { sill_core::wire::Event::PermissionRequest { request, .. } | sill_core::wire::Event::Elicitation { request, .. } => Some(*request), _ => None };
    let question_id = request.and_then(|r|view.question_id(r)).map(str::to_owned);
    let session_id = id.to_owned();
    let event = sill_core::wire::event_into_acp(wire, move |_request, answer| {
        std::thread::spawn(move || {
            let Some(question_id) = question_id else { return };
            let Ok(answer) = futures::executor::block_on(answer) else { return };
            let input = sill_core::sfp_client::answer(question_id, session_id, answer);
            if let Err(e) = quick::<serde_json::Value>("_sill/questions.answer", input) { eprintln!("sill: answering the agent: {e:#}"); }
        });
    });
    if let Some(event) = event { let _ = tx.send_blocking(event); }
}
