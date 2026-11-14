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
/// `fresh`: an agent still running in the jail is stopped and started again instead of being
/// reattached (for adapters that refuse a second `initialize`, Codex's "Already initialized").
pub fn start(s: &store::Session, store: &store::Store, fresh: bool) -> Result<Driver> {
    let home = home()?;
    let registry = agents::load(&home);
    let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
    let sdir = state_dir(uid(), &s.id)?;
    let policy = sill_core::policy::Policy::load(&s.policy, &home)?;
    let pins = store.pins(&s.id).unwrap_or_default();
    let binds = sill_core::session::binds_for(agent, &policy, &sdir, &home, &pins)?;
    let opts = StartOptions {
        session: s.id.clone(),
        policy: s.policy.clone(),
        cwd: home.clone(),
        state_dir: Some(sdir),
        unsafe_state: false,
        share_net: false,
        env: vec![],
        shim_bin: None,
        agent: Some(s.agent.clone()),
        binds,
        x11: sill_core::config::desktop_grants(&s.desktop, &sill_core::config::load()).0,
        dbus: sill_core::config::desktop_grants(&s.desktop, &sill_core::config::load()).1,
    };
    let sock = ensure_started(&opts, &home, &sill_core::session::staged_exe()?)?;
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
    Ok(Driver { commands: cmd_tx, events: ev_rx })
}

/// What a session's conversation says about how its jail runs: nothing in `unit` mode (the
/// normal case), one line in `direct` mode — with the warning when there are no limits.
pub fn mode_note() -> Option<String> {
    let l = sill_core::launcher::current();
    match l.mode {
        sill_core::launcher::Mode::Unit => None,
        sill_core::launcher::Mode::Direct if l.has_limits() => Some(format!("jail runs detached, direct mode ({}); limits in its own cgroup", l.reason)),
        sill_core::launcher::Mode::Direct => Some(format!(
            "jail runs detached, direct mode ({}): no resource limits — a busy agent can starve the host (sudo sill init)",
            l.reason
        )),
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
