//! Shelf-side session driver: ensure the unit runs, start the agent through the shim, run the ACP
//! loop on a thread, and hand events to the GTK main loop.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sill_core::acp::{self, AutoPermission, Command, Event};
use sill_core::session::{StartOptions, ensure_started};
use sill_core::shim;

pub struct Driver {
    pub commands: async_channel::Sender<Command>,
    pub events: async_channel::Receiver<Event>,
}

/// The main shelf session. Returns channels; the ACP loop runs on a background thread.
pub fn start(session: &str, cwd: &Path, agent: &str) -> Result<Driver> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME")?);
    let opts = StartOptions {
        session: session.to_string(),
        policy: "default".into(),
        cwd: cwd.to_path_buf(),
        state_dir: None,
        unsafe_state: false,
        share_net: false,
        env: vec![],
        shim_bin: None,
    };
    let sock = ensure_started(&opts, &home, &std::env::current_exe()?)?;
    let mut control = shim::Control::connect(&sock)?;
    if !control.status()?.running {
        let argv: Vec<String> = agent.split_whitespace().map(str::to_string).collect();
        control.spawn(&argv, &BTreeMap::new(), Some(cwd))?;
    }
    let stream = shim::agent_stream(&sock)?;
    let (cmd_tx, cmd_rx) = async_channel::bounded(8);
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let cwd = cwd.to_path_buf();
    std::thread::Builder::new().name("sill-acp".into()).spawn(move || {
        if let Err(e) = acp::run_session(stream, &cwd, AutoPermission::Ask, cmd_rx, ev_tx.clone()) {
            let _ = ev_tx.send_blocking(Event::Ended(Some(format!("{e:#}"))));
        }
    })?;
    Ok(Driver { commands: cmd_tx, events: ev_rx })
}
