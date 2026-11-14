//! Shelf-side session driver: have the daemon start the session's jail (docs/daemon.md §3.4), start
//! the agent through the shim, run the ACP loop on a thread, hand events to the GTK main loop.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sill_core::acp::{self, AutoPermission, Command, Event, SessionOptions};
use sill_core::agents;
use sill_core::session::state_dir;
use sill_core::shim;
use sill_core::store;

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

/// Start (or reconnect to) a stored session. The daemon starts the jail from the stored row — its
/// pins, policy and desktop grants (`_pins` is what the caller believes they are; the daemon reads
/// the store) — and answers with the shim socket. `fresh`: an agent still running in the jail is
/// stopped and started again instead of being reattached (for adapters that refuse a second
/// `initialize`, Codex's "Already initialized"). Blocking — the daemon call, the control calls —
/// so the view runs it on a worker thread (SPEC §4.11 #13).
pub fn start(s: &store::Session, _pins: Vec<PathBuf>, fresh: bool) -> Result<Driver> {
    let home = home()?;
    let registry = agents::load(&home);
    let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
    let started: sill_proto::Started = super::client::call(sill_proto::method::SESSIONS_START, sill_proto::Key { key: s.id.clone() })?;
    let sock = started.socket;
    let mut control = shim::Control::connect(&sock)?;
    if fresh && control.status()?.running {
        control.kill(3000)?;
    }
    if !control.status()?.running {
        let mut env = agent.env.clone();
        env.extend(agent.sandbox_off_env.clone());
        control.spawn(&agent.command, &env, Some(&home))?;
    }
    let stream = shim::agent_stream(&sock)?;
    // a control channel: a reconnect re-applies the remembered settings as one command each, and none
    // of them may be dropped for want of room
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let sopts = SessionOptions {
        cwd: home,
        auto: AutoPermission::Ask,
        resume: if agent.supports_load { s.acp_session_id.clone() } else { None },
    };
    std::thread::Builder::new().name(format!("sill-acp-{}", s.id)).spawn(move || {
        // run_session sends Event::Ended itself, error included
        let _ = acp::run_session(stream, sopts, cmd_rx, ev_tx);
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
    sill_core::session::binds_for(agent, policy, &sdir, home, &pins).unwrap_or_default()
}

/// Revert a path inside a running session's jail.
pub fn revert(session: &str, path: &std::path::Path) -> Result<String> {
    let sock = sill_core::session::socket_path(session)?;
    let mut c = shim::Control::connect(&sock).context("session is not running")?;
    c.revert(path)
}
