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

/// Route one connection's notifications to the attached views until it closes; then every view
/// that attached through it learns the session's conversation is gone from here.
fn dispatch(c: Arc<Client>) {
    let me = tag(&c);
    loop {
        let next = c.notifications.lock().unwrap().recv();
        let Ok((method, params)) = next else { break };
        if method != sill_proto::method::SESSION_EVENT {
            continue;
        }
        let Ok(n) = serde_json::from_value::<sill_proto::SessionEvent>(params) else { continue };
        let Ok(wire) = serde_json::from_value::<sill_core::wire::Event>(n.event) else { continue };
        let Some(tx) = subs().lock().unwrap().get(&n.key).filter(|(_, t)| *t == me).map(|(tx, _)| tx.clone()) else { continue };
        let key = n.key.clone();
        // a question: the view answers through a reply channel, the answer goes to the daemon
        let ev = wire.into_acp(move |request, answer| {
            std::thread::spawn(move || {
                let Ok(answer) = futures::executor::block_on(answer) else { return };
                let Ok(answer) = serde_json::to_value(answer) else { return };
                if let Err(e) = call::<serde_json::Value>(sill_proto::method::SESSIONS_ANSWER, sill_proto::SessionAnswer { key, request, answer }) {
                    eprintln!("sill: answering the agent: {e:#}");
                }
            });
        });
        if let Some(ev) = ev {
            let _ = tx.send_blocking(ev);
        }
    }
    forget(&c);
    let gone: Vec<async_channel::Sender<Event>> = {
        let mut s = subs().lock().unwrap();
        let ids: Vec<String> = s.iter().filter(|(_, (_, t))| *t == me).map(|(k, _)| k.clone()).collect();
        ids.into_iter().filter_map(|k| s.remove(&k)).map(|(tx, _)| tx).collect()
    };
    for tx in gone {
        let _ = tx.send_blocking(Event::Ended(Some("the connection to the sill daemon closed".into())));
    }
}
