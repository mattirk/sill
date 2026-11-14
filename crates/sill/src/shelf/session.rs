//! Shelf-side session driver: ensure the session's unit runs, start the agent through the shim, run
//! the ACP loop on a thread, hand events to the GTK main loop.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sill_core::acp::{self, AutoPermission, Command, Event, SessionOptions};
use sill_core::agents;
use sill_core::session::{StartOptions, ensure_started, state_dir};
use sill_core::shim;
use sill_core::store;

pub struct Driver {
    pub commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<Event>,
}

pub fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME")?))
}

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Start (or reconnect to) a stored session. The jail's cwd is the home directory (SPEC §3.1).
pub fn start(s: &store::Session) -> Result<Driver> {
    let home = home()?;
    let registry = agents::load(&home);
    let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
    let opts = StartOptions {
        session: s.id.clone(),
        policy: s.policy.clone(),
        cwd: home.clone(),
        state_dir: Some(state_dir(uid(), &s.id)?),
        unsafe_state: false,
        share_net: false,
        env: vec![],
        shim_bin: None,
    };
    let sock = ensure_started(&opts, &home, &std::env::current_exe()?)?;
    let mut control = shim::Control::connect(&sock)?;
    if !control.status()?.running {
        let mut env = agent.env.clone();
        env.extend(agent.sandbox_off_env.clone());
        control.spawn(&agent.command, &env, Some(&home))?;
    }
    let stream = shim::agent_stream(&sock)?;
    let (cmd_tx, cmd_rx) = async_channel::bounded(8);
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let sopts = SessionOptions {
        cwd: home,
        auto: AutoPermission::Ask,
        resume: if agent.supports_load { s.acp_session_id.clone() } else { None },
    };
    std::thread::Builder::new().name(format!("sill-acp-{}", s.id)).spawn(move || {
        if let Err(e) = acp::run_session(stream, sopts, cmd_rx, ev_tx.clone()) {
            let _ = ev_tx.send_blocking(Event::Ended(Some(format!("{e:#}"))));
        }
    })?;
    Ok(Driver { commands: cmd_tx, events: ev_rx })
}

/// Revert a path inside a running session's jail.
pub fn revert(session: &str, path: &std::path::Path) -> Result<String> {
    let sock = sill_core::session::socket_path(session)?;
    let mut c = shim::Control::connect(&sock).context("session is not running")?;
    c.revert(path)
}
