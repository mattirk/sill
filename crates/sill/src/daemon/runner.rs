//! One running session's conversation, owned by the daemon (docs/daemon.md §3.3, decision 2): the
//! ACP client (`acp::run_session` on a thread, as the shelf ran it), the transcript it writes, and
//! the frontends attached to it. It runs whether or not anyone is attached.
//!
//! What the shelf did per event before the split is done here, once: the agent's text is gathered
//! and written as an `agent` entry when a tool call or the end of the turn closes it, tool calls are
//! written and updated, the agent's session id is kept for `session/load`, and a session/load's
//! replay is neither written nor sent (the stored transcript is already what a frontend shows). A
//! permission request is answered here when the session auto-allows, else it goes to every attached
//! frontend and the first answer counts; with nobody attached it waits for the next attach. The
//! summary turn (the session's end, or a checkpoint) is a prompt of the daemon's own, with a cap.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use sill_core::acp::{self, Controls};
use sill_core::wire::{self, Answer, Entry, Reply};

/// What a summary turn is for (docs/memory.md §2.2, §2.5): the session's end, or a checkpoint
/// while it goes on. A checkpoint becomes the end's when the tab closes during it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    End,
    Checkpoint,
}

/// How long the agent gets for its summary before the turn is cut short.
pub const SUMMARY_CAP: std::time::Duration = std::time::Duration::from_secs(90);

/// A summary turn's outcome, handed to the hub once the runner's lock is released.
pub struct SummaryDone {
    pub kind: SummaryKind,
    /// What the agent wrote; None when it wrote nothing (or was gone).
    pub text: Option<String>,
}

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
    /// When the last turn stopped (the daemon's idle timer counts from it too).
    last_turn_end: Option<i64>,
    /// Replaced by a newer runner (a fresh start): its events go nowhere and write nothing.
    retired: bool,
    /// The summary turn in flight, if any, and the text it has produced so far (its tool calls
    /// flush the agent's text to the store; the summary keeps a copy). `generation` tells the 90 s cap
    /// whether the turn it was started for is still the one running.
    summary: Option<(SummaryKind, String, u64)>,
    summary_gen: u64,
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

    fn handle(&self, hub: &Arc<Hub>, ev: acp::Event) {
        let done = self.handle_locked(hub, ev);
        // the lock is released: the hub may lock this runner again (retire it, broadcast to it)
        if let Some(d) = done {
            hub.summary_done(&self.id, d);
        }
    }

    fn handle_locked(&self, hub: &Arc<Hub>, ev: acp::Event) -> Option<SummaryDone> {
        let mut st = self.st.lock().unwrap();
        if st.retired {
            return None;
        }
        let mut done = None;
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
                return None;
            }
            // some agents echo the prompt back during the turn; the frontend shows what it sent
            acp::Event::UserText(_) => return None,
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
                st.last_turn_end = Some(sill_core::store::now());
                flush(hub, &id, &mut st);
                // the summary turn: its text goes to the hub once this event is out
                if let Some((kind, text, _)) = st.summary.take() {
                    done = Some(SummaryDone { kind, text: Some(text.trim().to_string()) });
                }
            }
            acp::Event::Ended(_) => {
                st.busy = false;
                st.ended = true;
                flush(hub, &id, &mut st);
                st.pending.clear(); // the agent is gone: its questions with it
                // an agent that went away mid-summary: the mechanical one it is
                if let Some((kind, _, _)) = st.summary.take() {
                    done = Some(SummaryDone { kind, text: None });
                }
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
        done
    }

    /// The agent saw this conversation: it resumed its own context, or it has had a turn since
    /// this runner started. A cold agent after a failed `session/load` has neither, and must not be
    /// asked to summarize what it never saw (docs/memory.md decision 13).
    pub fn agent_saw_session(&self) -> bool {
        let st = self.st.lock().unwrap();
        st.ready.as_ref().is_some_and(|(_, resumed)| *resumed) || st.turns > 0
    }

    /// The agent is up and between turns: it can be asked one more thing.
    pub fn agent_idle(&self) -> bool {
        let st = self.st.lock().unwrap();
        st.ready.is_some() && !st.busy && !st.ended && !st.retired
    }

    /// How many frontends are attached.
    pub fn sub_count(&self) -> usize {
        self.st.lock().unwrap().subs.len()
    }

    /// A turn is running (a summary turn included).
    pub fn busy(&self) -> bool {
        self.st.lock().unwrap().busy
    }

    /// When the last turn stopped, if one did in this runner's life.
    pub fn last_turn_end(&self) -> Option<i64> {
        self.st.lock().unwrap().last_turn_end
    }

    /// The summary turn running, if any.
    pub fn summarizing(&self) -> Option<SummaryKind> {
        self.st.lock().unwrap().summary.as_ref().map(|(k, _, _)| *k)
    }

    /// The tab closed during a checkpoint: its answer becomes the session's final summary.
    pub fn adopt_checkpoint(&self) -> bool {
        let mut st = self.st.lock().unwrap();
        match st.summary.as_mut() {
            Some((kind, _, _)) if *kind == SummaryKind::Checkpoint => {
                *kind = SummaryKind::End;
                true
            }
            _ => false,
        }
    }

    /// Ask the agent for a summary as a plain turn (docs/memory.md §2.2): no standing block, no
    /// user entry — a note marks it in the transcript and for the attached frontends — and the
    /// answer is stored like any agent message. The hub gets [`SummaryDone`] when the turn stops,
    /// when the agent goes, or when [`SUMMARY_CAP`] passes (the turn is cancelled then; what was
    /// written so far, marked as cut short, is the answer). Refused while the agent is busy, down,
    /// or already summarizing.
    pub fn ask_summary(self: &Arc<Self>, hub: &Arc<Hub>, prompt: &str, note: &str, kind: SummaryKind) -> Result<()> {
        let generation = {
            let mut st = self.st.lock().unwrap();
            if st.ended || st.retired || st.ready.is_none() {
                bail!("the agent is not running");
            }
            if st.busy {
                bail!("the agent is busy");
            }
            if st.summary.is_some() {
                bail!("a summary is being written already");
            }
            st.busy = true;
            st.summary_gen += 1;
            let generation = st.summary_gen;
            st.summary = Some((kind, String::new(), generation));
            broadcast(&self.id, &mut st.subs, &wire::Event::Note(note.to_string()));
            generation
        };
        if self.commands.try_send(acp::Command::Prompt(prompt.to_string(), vec![])).is_err() {
            let mut st = self.st.lock().unwrap();
            st.busy = false;
            st.summary = None;
            bail!("the session loop is gone");
        }
        hub.with_store(|s| s.append(&self.id, "note", note, "")).ok();
        let (me, hub) = (self.clone(), hub.clone());
        std::thread::Builder::new()
            .name(format!("sill-summary-cap-{}", self.id))
            .spawn(move || {
                std::thread::sleep(SUMMARY_CAP);
                me.cut_summary(&hub, generation, &format!("cut short: the agent had not finished after {} seconds", SUMMARY_CAP.as_secs()));
            })
            .ok();
        Ok(())
    }

    /// Stop the summary turn started as generation `generation` before the agent stopped it: the turn is
    /// cancelled and what it wrote so far, marked with `why`, is the answer.
    fn cut_summary(&self, hub: &Arc<Hub>, generation: u64, why: &str) {
        let done = {
            let mut st = self.st.lock().unwrap();
            match st.summary.as_ref() {
                Some((_, _, g)) if *g == generation => {}
                _ => return,
            }
            let (kind, mut text, _) = st.summary.take().unwrap();
            text.push_str(&st.agent_md);
            let text = text.trim().to_string();
            let _ = self.commands.try_send(acp::Command::Cancel);
            SummaryDone { kind, text: (!text.is_empty()).then(|| format!("{text}\n({why})")) }
        };
        hub.summary_done(&self.id, done);
    }

    /// One event to every attached frontend (the hub's own: the end, a checkpoint's answer).
    pub fn notify(&self, event: &wire::Event) {
        let mut st = self.st.lock().unwrap();
        broadcast(&self.id, &mut st.subs, event);
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

/// The agent's gathered text becomes an `agent` entry; a summary turn keeps its copy.
fn flush(hub: &Hub, id: &str, st: &mut State) {
    let md = std::mem::take(&mut st.agent_md);
    if !md.trim().is_empty() {
        if let Some((_, text, _)) = st.summary.as_mut() {
            text.push_str(&md);
        }
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
