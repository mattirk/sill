//! One running session's conversation, owned by the daemon (docs/daemon.md §3.3, decision 2): the
//! ACP client (`acp::run_session` on a thread, as the shelf ran it), the transcript it writes, and
//! the frontends attached to it. It runs whether or not anyone is attached.
//!
//! What the shelf did per event before the split is done here, once: the agent's text is gathered
//! and written as an `agent` entry when a tool call or the end of the turn closes it, tool calls are
//! written and updated, the agent's session id is kept for `session/load`, and a session/load's
//! replay is neither written nor sent (the stored transcript is already what a frontend shows). A
//! permission request is answered here when the session auto-allows, else it goes to every attached
//! frontend and the first answer counts; with nobody attached it waits for the next attach.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use sill_core::acp::{self, Controls};
use sill_core::wire::{self, Answer, Entry, Reply};

use super::Conn;
use super::hub::Hub;

pub struct Runner {
    pub id: String,
    commands: async_channel::Sender<acp::Command>,
    st: Mutex<State>,
}

#[derive(Default)]
struct State {
    subs: Vec<Arc<Conn>>,
    /// The agent's session id, whether it was resumed, and the controls it opened with.
    ready: Option<(String, bool)>,
    controls: Option<Controls>,
    images: Option<bool>,
    usage: Option<(u64, u64)>,
    /// A login the agent asked for and nobody did yet.
    auth: Option<wire::Event>,
    busy: bool,
    /// After a `session/load`, until the first turn: the agent replays the old conversation.
    replaying: bool,
    turns: u32,
    /// The agent's text since the last tool call or turn end (not yet in the store).
    agent_md: String,
    /// Tool call id → its entry's `seq`, for the updates.
    tool_seqs: HashMap<String, i64>,
    /// Questions waiting for an answer: the event as sent, and where the answer goes.
    pending: BTreeMap<u64, (wire::Event, Reply)>,
    next_request: u64,
    ended: bool,
    /// Replaced by a newer runner (a fresh start): its events go nowhere and write nothing.
    retired: bool,
}

impl Runner {
    pub fn new(id: &str, commands: async_channel::Sender<acp::Command>, first: Arc<Conn>) -> Arc<Runner> {
        Arc::new(Runner { id: id.to_string(), commands, st: Mutex::new(State { subs: vec![first], next_request: 1, ..Default::default() }) })
    }

    pub fn alive(&self) -> bool {
        let st = self.st.lock().unwrap();
        !st.ended && !st.retired
    }

    /// Consume the session loop's events until it ends. Runs on its own thread.
    pub fn run(self: Arc<Self>, hub: Arc<Hub>, events: async_channel::Receiver<acp::Event>) {
        while let Ok(ev) = events.recv_blocking() {
            let end = matches!(ev, acp::Event::Ended(_));
            self.handle(&hub, ev);
            if end {
                break;
            }
        }
        // a loop that stopped without saying so has ended all the same
        if !self.st.lock().unwrap().ended {
            self.handle(&hub, acp::Event::Ended(Some("the session loop stopped".into())));
        }
        hub.runner_ended(&self);
    }

    fn handle(&self, hub: &Hub, ev: acp::Event) {
        let mut st = self.st.lock().unwrap();
        if st.retired {
            return;
        }
        let id = self.id.clone();
        match &ev {
            acp::Event::Ready { session_id, resumed, controls } => {
                hub.with_store(|s| s.set_acp_session_id(&id, session_id)).ok();
                st.ready = Some((session_id.clone(), *resumed));
                st.controls = Some(controls.clone());
                st.replaying = *resumed;
            }
            acp::Event::Capabilities { images } => st.images = Some(*images),
            acp::Event::Controls(c) => st.controls = Some(c.clone()),
            acp::Event::Usage { used, size } => st.usage = Some((*used, *size)),
            // the loop drops a prompt that arrives while it waits for a login (no Stopped follows),
            // so the session is not busy any more — the prompt after the login must not be refused
            acp::Event::AuthRequired { .. } => st.busy = false,
            acp::Event::TurnStarted => {
                st.replaying = false;
                st.busy = true;
                st.turns += 1;
                st.auth = None;
            }
            // a session/load's replay: what it says is in the store already, and on screen
            acp::Event::UserText(_)
            | acp::Event::Text(_)
            | acp::Event::Thought(_)
            | acp::Event::ToolCall { .. }
            | acp::Event::ToolCallUpdate { .. }
            | acp::Event::SubagentText { .. }
                if st.replaying =>
            {
                return;
            }
            // some agents echo the prompt back during the turn; the frontend shows what it sent
            acp::Event::UserText(_) => return,
            acp::Event::Text(t) => st.agent_md.push_str(t),
            acp::Event::ToolCall { id: call, title, status, kind, detail, diff, .. } => {
                flush(hub, &id, &mut st);
                if let Ok(seq) = hub.with_store(|s| s.append_tool(&id, title, status, kind, detail.as_deref(), diff.as_deref())) {
                    st.tool_seqs.insert(call.clone(), seq);
                }
            }
            acp::Event::ToolCallUpdate { id: call, status, title, detail, diff } => {
                if let Some(seq) = st.tool_seqs.get(call).copied() {
                    hub.with_store(|s| s.update_tool(&id, seq, title.as_deref(), status.as_deref(), detail.as_deref(), diff.as_deref())).ok();
                }
            }
            acp::Event::Stopped(_) => {
                st.busy = false;
                flush(hub, &id, &mut st);
            }
            acp::Event::Ended(_) => {
                st.busy = false;
                st.ended = true;
                flush(hub, &id, &mut st);
                st.pending.clear(); // the agent is gone: its questions with it
            }
            _ => {}
        }
        // the session's auto-allow answers here, attached or not (SPEC §3.3)
        let ev = match ev {
            acp::Event::PermissionRequest { tool_call_id, title, options, reply } if hub.auto_allow(&id) => {
                use acp::PermissionOptionKind as K;
                let pick = options
                    .iter()
                    .find(|o| o.kind == K::AllowOnce)
                    .or_else(|| options.iter().find(|o| o.kind == K::AllowAlways))
                    .map(|o| o.id.clone());
                let allowed = pick.is_some();
                let _ = reply.send(pick);
                acp::Event::AutoAnswered { tool_call_id, title, allowed }
            }
            other => other,
        };
        let request = st.next_request;
        let (wire, reply) = wire::Event::from_acp(ev, request);
        if let Some(r) = reply {
            st.next_request += 1;
            st.pending.insert(request, (wire.clone(), r));
        }
        if matches!(wire, wire::Event::AuthRequired { .. }) {
            st.auth = Some(wire.clone());
        }
        broadcast(&self.id, &mut st.subs, &wire);
    }

    /// Subscribe `conn` and bring it up to date: what a frontend that just opened needs to show the
    /// session as it is — the agent's capabilities and controls, a login it waits for, the turn in
    /// flight with its text so far, the questions not yet answered.
    pub fn attach(&self, conn: Arc<Conn>) {
        let mut st = self.st.lock().unwrap();
        st.subs.retain(|c| c.id != conn.id);
        let mut evs = Vec::new();
        if let Some(images) = st.images {
            evs.push(wire::Event::Capabilities { images });
        }
        if let Some((session_id, resumed)) = st.ready.clone() {
            // the agent has this conversation in its context: resumed, or it has had a turn
            evs.push(wire::Event::Ready { session_id, resumed: resumed || st.turns > 0, controls: st.controls.clone().unwrap_or_default() });
        }
        if let Some((used, size)) = st.usage {
            evs.push(wire::Event::Usage { used, size });
        }
        if let Some(a) = st.auth.clone() {
            evs.push(a);
        }
        if st.busy {
            evs.push(wire::Event::TurnStarted);
            if !st.agent_md.is_empty() {
                evs.push(wire::Event::Text(st.agent_md.clone()));
            }
        }
        evs.extend(st.pending.values().map(|(e, _)| e.clone()));
        let mut one = vec![conn];
        for e in &evs {
            broadcast(&self.id, &mut one, e);
        }
        st.subs.extend(one);
    }

    pub fn detach(&self, conn: u64) {
        self.st.lock().unwrap().subs.retain(|c| c.id != conn);
    }

    /// A frontend's command. A prompt is refused while a turn runs; when it is taken, its entry is
    /// written — the person's text, or the note for a turn sill asked for.
    pub fn command(&self, hub: &Hub, cmd: wire::Command) -> Result<()> {
        let (cmd, entry) = cmd.into_acp();
        let prompt = matches!(cmd, acp::Command::Prompt(..));
        {
            let mut st = self.st.lock().unwrap();
            if st.ended || st.retired {
                bail!("the session is not running");
            }
            if prompt {
                if st.busy {
                    bail!("the agent is busy; wait for the turn to end");
                }
                st.busy = true; // TurnStarted confirms it; a second prompt before then is refused too
            }
        }
        if self.commands.try_send(cmd).is_err() {
            bail!("the session loop is gone");
        }
        match entry {
            Some(Entry::User { text, names }) => {
                hub.with_store(|s| {
                    s.append(&self.id, "user", &text, &names.join("\n"))?;
                    s.touch(&self.id)
                })
                .ok();
            }
            Some(Entry::Note(n)) => {
                hub.with_store(|s| s.append(&self.id, "note", &n, "")).ok();
            }
            None => {}
        }
        Ok(())
    }

    /// The answer to question `request`: to the agent, and every attached frontend drops its dialog.
    pub fn answer(&self, request: u64, answer: Answer) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let Some((_, reply)) = st.pending.remove(&request) else { bail!("no open question {request} (answered already?)") };
        if !reply.send(answer) {
            bail!("the answer is not of the question's kind");
        }
        broadcast(&self.id, &mut st.subs, &wire::Event::Resolved { request });
        Ok(())
    }

    /// Hand the session over to a newer runner, or end it: no more events out, the loop told to quit.
    pub fn retire(&self) {
        {
            let mut st = self.st.lock().unwrap();
            st.retired = true;
            st.subs.clear();
            st.pending.clear();
        }
        let _ = self.commands.try_send(acp::Command::Quit);
    }
}

/// The agent's gathered text becomes an `agent` entry.
fn flush(hub: &Hub, id: &str, st: &mut State) {
    let md = std::mem::take(&mut st.agent_md);
    if !md.trim().is_empty() {
        hub.with_store(|s| s.append(id, "agent", &md, "")).ok();
    }
}

/// One event to every subscriber; a connection that is gone or stuck is dropped from the list.
fn broadcast(id: &str, subs: &mut Vec<Arc<Conn>>, event: &wire::Event) {
    if subs.is_empty() {
        return;
    }
    let params = sill_proto::SessionEvent { key: id.to_string(), event: serde_json::to_value(event).unwrap_or_default() };
    let req = sill_proto::rpc::Request::new(None, sill_proto::method::SESSION_EVENT, serde_json::to_value(params).unwrap_or_default());
    let Ok(line) = serde_json::to_string(&req) else { return };
    subs.retain(|c| c.send(line.clone()));
}
