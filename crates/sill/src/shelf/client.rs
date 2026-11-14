//! The shelf's connection to the daemon (docs/daemon.md §3.4): one client, connected on first use,
//! made again when the daemon left (a restart keeps every session; only the socket is new).
//! Blocking — callers run it off the main thread, as they did `ensure_started`.

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sill_proto::Client;

static CLIENT: OnceLock<Mutex<Option<Arc<Client>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Client>>> {
    CLIENT.get_or_init(|| Mutex::new(None))
}

fn connected() -> Result<Arc<Client>> {
    let mut s = slot().lock().unwrap();
    if let Some(c) = s.as_ref() {
        return Ok(c.clone());
    }
    let c = Arc::new(Client::connect_or_start()?);
    *s = Some(c.clone());
    Ok(c)
}

/// One request; on a connection the daemon has closed, once more on a fresh one.
pub fn call<T: DeserializeOwned>(method: &str, params: impl Serialize + Clone) -> Result<T> {
    let c = connected()?;
    match c.call(method, params.clone()) {
        Err(e) if Client::is_closed(&e) => {
            let mut s = slot().lock().unwrap();
            if s.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &c)) {
                *s = None;
            }
            drop(s);
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
