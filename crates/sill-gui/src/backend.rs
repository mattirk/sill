//! Blocking SFP I/O stays off the GUI thread. Each attachment owns its connection;
//! dropping it cancels the subscription without ending the session or agent.
use std::{sync::mpsc, time::Duration};

use sill_proto::{Client, sessions, sfp::SessionEvent};

use crate::model::Conversation;

#[derive(Debug)]
pub enum Command {
    Refresh,
    Attach { id: String, generation: u64 },
    Reconnect(u64),
}

#[derive(Debug, Clone)]
pub enum Update {
    Sessions(Vec<sessions::Session>),
    Snapshot(Box<Conversation>),
    Event(Box<SessionEvent>),
    Status(String),
    Error(String),
}

pub fn start(
    initial: Option<String>,
) -> (
    mpsc::Sender<Command>,
    async_channel::Receiver<(u64, Update)>,
) {
    let (commands, receiver) = mpsc::channel();
    let (updates, events) = async_channel::bounded(128);
    std::thread::Builder::new()
        .name("sill-gui-sfp".into())
        .spawn(move || {
            run(receiver, updates, initial);
        })
        .expect("start SFP worker");
    (commands, events)
}

fn run(
    commands: mpsc::Receiver<Command>,
    updates: async_channel::Sender<(u64, Update)>,
    initial: Option<String>,
) {
    let send = |generation, update| updates.send_blocking((generation, update)).is_ok();
    let mut generation = 0;
    let mut active = initial;
    let mut client: Option<Client> = None;
    let mut retry = Duration::from_secs(1);
    loop {
        if updates.is_closed() {
            return;
        }
        if client.is_none() {
            if !send(generation, Update::Status("Connecting…".into())) {
                return;
            }
            // Never invoke connect_or_start: its executable is sill-gui, not sill daemon.
            let connected = (|| -> anyhow::Result<Client> {
                let c = Client::connect()?;
                let sessions = c.list_sessions()?;
                if !send(generation, Update::Sessions(sessions)) {
                    anyhow::bail!("window closed");
                }
                if let Some(key) = &active {
                    let session = c.resolve_session(key)?;
                    active = Some(session.session_id.clone());
                    if !send(generation, Update::Status("Loading conversation…".into())) {
                        anyhow::bail!("window closed");
                    }
                    let snapshot = Conversation::attach(&c, &session.session_id)?;
                    if !send(generation, Update::Snapshot(Box::new(snapshot))) {
                        anyhow::bail!("window closed");
                    }
                }
                Ok(c)
            })();
            match connected {
                Ok(c) => {
                    client = Some(c);
                    retry = Duration::from_secs(1);
                    if !send(generation, Update::Status("Connected".into())) {
                        return;
                    }
                }
                Err(error) => {
                    if !send(generation, Update::Error(format!("{error:#}"))) {
                        return;
                    }
                    match commands.recv_timeout(retry) {
                        Ok(Command::Attach {
                            id,
                            generation: next,
                        }) => {
                            active = Some(id);
                            generation = next;
                        }
                        Ok(Command::Reconnect(next)) => generation = next,
                        Ok(_) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    retry = (retry * 2).min(Duration::from_secs(10));
                    continue;
                }
            }
        }
        match commands.recv_timeout(Duration::from_millis(30)) {
            Ok(Command::Attach {
                id,
                generation: next,
            }) => {
                active = Some(id);
                generation = next;
                client = None;
                continue;
            }
            Ok(Command::Reconnect(next)) => {
                generation = next;
                client = None;
                continue;
            }
            Ok(Command::Refresh) => match client.as_ref().unwrap().list_sessions() {
                Ok(sessions) => {
                    if !send(generation, Update::Sessions(sessions)) {
                        return;
                    }
                }
                Err(error) => {
                    if !send(generation, Update::Error(format!("{error:#}"))) {
                        return;
                    }
                    client = None;
                    continue;
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let c = client.as_ref().unwrap();
        let mut disconnected = false;
        // Bound work between commands so a busy stream cannot starve session switches.
        for _ in 0..128 {
            let next = c.notifications.lock().unwrap().try_recv();
            match next {
                Ok((method, value)) if method == "_sill/session.event" => {
                    match serde_json::from_value::<SessionEvent>(value) {
                        Ok(event) => {
                            if !send(generation, Update::Event(Box::new(event))) {
                                return;
                            }
                        }
                        Err(error) => {
                            if !send(
                                generation,
                                Update::Error(format!("Invalid conversation event: {error}")),
                            ) {
                                return;
                            }
                            disconnected = true;
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            if !send(
                generation,
                Update::Status("Connection lost; restoring conversation…".into()),
            ) {
                return;
            }
            client = None;
        }
    }
}
