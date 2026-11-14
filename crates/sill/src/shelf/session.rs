//! Shelf-side session driver: attach the view to the session's conversation, which the daemon runs
//! (docs/daemon.md §3.3) — jail, agent, ACP client and transcript are the daemon's — and hand its
//! events to the GTK main loop as the `acp::Event`s the view renders; the view's commands go back
//! over the socket.

use std::path::PathBuf;

use anyhow::{Context, Result};
use sill_core::acp::Event;
use sill_core::agents;
use sill_core::session::state_dir;
use sill_core::shim;
use sill_core::store;
use sill_core::wire::Command;

pub struct Driver {
    pub commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<Event>,
    /// What the conversation says about the jail's limits, if anything (`mode_note`, taken here
    /// on the worker thread).
    pub note: Option<String>,
}

pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME")?))
}

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Attach to a stored session's conversation; the daemon starts its jail and agent from the
/// stored row when they are not running. `fresh`: an agent still running in the jail is started
/// again instead of being reattached (adapters that refuse a second `initialize`, Codex's "Already
/// initialized"). Blocking — the daemon may be starting the jail — so the view runs it on a worker
/// thread (SPEC §4.11 #13). A command the daemon refuses (a turn running, the session gone) comes
/// back as an `Event::Other("refused: …")` the view shows as a note.
pub fn start(s: &store::Session, _pins: Vec<PathBuf>, fresh: bool) -> Result<Driver> {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    super::client::attach(&s.id, fresh, ev_tx.clone())?;
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    let id = s.id.clone();
    std::thread::Builder::new().name(format!("sill-cmd-{id}")).spawn(move || {
        while let Ok(c) = cmd_rx.recv_blocking() {
            if matches!(c, Command::Quit) {
                break;
            }
            if let Err(e) = super::client::command(&id, &c) {
                let _ = ev_tx.send_blocking(Event::Other(format!("refused: {e:#}")));
            }
        }
        // the tab closed (Quit) or the view dropped its driver: the agent stays the daemon's
        super::client::detach(&id);
    })?;
    Ok(Driver { commands: cmd_tx, events: ev_rx, note: mode_note() })
}

/// What a session's conversation says about how its jail runs: nothing in the normal case, one
/// line when the daemon could give it no resource limits. Blocking (a daemon call): off the main thread.
pub fn mode_note() -> Option<String> {
    match super::client::status() {
        Ok(s) if !s.limits => Some(format!("no resource limits for this jail — a busy agent can starve the host ({})", s.launcher)),
        _ => None,
    }
}

/// The binds a stored session runs with (for ledger scans on the host side).
pub fn binds_of(s: &store::Session, policy: &sill_core::policy::Policy, home: &std::path::Path, store: &store::Store) -> Vec<(PathBuf, PathBuf)> {
    let registry = agents::load(home);
    let Some(agent) = registry.get(&s.agent) else { return vec![] };
    let Ok(sdir) = state_dir(uid(), &s.id) else { return vec![] };
    let pins = store.pins(&s.id).unwrap_or_default();
    sill_core::session::binds_for(agent, policy, &sdir, home, &pins, s.ghost).unwrap_or_default()
}

/// Revert a path inside a running session's jail.
pub fn revert(session: &str, path: &std::path::Path) -> Result<String> {
    let sock = sill_core::session::socket_path(session)?;
    let mut c = shim::Control::connect(&sock).context("session is not running")?;
    c.revert(path)
}
