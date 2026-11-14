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
use sill_core::wire::{self, Answer, Entry};
use sill_core::store::Store;
use super::{context, relay};

/// What a summary turn is for (docs/memory.md §2.2, §2.5): the session's end, or a checkpoint
/// while it goes on. A checkpoint becomes the end's when the tab closes during it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SummaryKind {
    End,
    Checkpoint,
}

/// How long the agent gets for its summary before the turn is cut short.
pub const SUMMARY_CAP: std::time::Duration = std::time::Duration::from_secs(90);

/// A summary turn's outcome, handed to the hub once the runner's lock is released.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct SummaryDone {
    pub kind: SummaryKind,
    /// What the agent wrote; None when it wrote nothing (or was gone).
    pub text: Option<String>,
}

use super::Conn;
use super::hub::Hub;

pub struct Runner {
    pub id: String,
    relay: Arc<relay::Client>,
    st: Mutex<State>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct State {
    cursor: u64,
    next_command: u64,
    outbox: BTreeMap<u64, wire::Command>,
    context: context::Context,
    completed: Option<SummaryDone>,
    #[serde(skip)]
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
    pending: BTreeMap<u64, wire::Event>,
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
    summary_deadline: Option<i64>,
}

impl Runner {
    pub fn load(id: &str, store: &Store) -> Result<serde_json::Value> {
        use rusqlite::OptionalExtension;
        store.conn().execute_batch("CREATE TABLE IF NOT EXISTS daemon_runners (id TEXT PRIMARY KEY, state TEXT NOT NULL)")?;
        let state: Option<String> = store.conn().query_row("SELECT state FROM daemon_runners WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(state.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or(serde_json::Value::Null))
    }

    pub fn new(id: &str, relay: Arc<relay::Client>, snapshot: serde_json::Value) -> Result<Arc<Runner>> {
        let state: State = if snapshot.is_null() { State::default() } else { serde_json::from_value(snapshot)? };
        // Subscribers are frontend connections; they are never carried across a daemon restart.
        // Keep retirement durable until the relay's final event is drained: an ended session may
        // outlive the main daemon briefly, and must never replay as a live conversation.
        Ok(Arc::new(Runner { id: id.into(), relay, st: Mutex::new(state) }))
    }

    fn save(&self, store: &Store, state: &State) -> Result<()> {
        store.conn().execute("INSERT INTO daemon_runners(id,state) SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM sessions WHERE id=?1) ON CONFLICT(id) DO UPDATE SET state=excluded.state", rusqlite::params![self.id, serde_json::to_string(state)?])?;
        Ok(())
    }

    pub fn cursor(snapshot: &serde_json::Value) -> u64 { snapshot.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0) }

    pub fn alive(&self) -> bool {
        let st = self.st.lock().unwrap();
        !st.ended && !st.retired
    }

    /// Consume the session loop's events until it ends. Runs on its own thread.
    pub fn run(self: Arc<Self>, hub: Arc<Hub>, events: std::sync::mpsc::Receiver<relay::Delivered>) {
        // A summary result and its end intent remain recoverable if the daemon stopped between
        // committing the event and applying the end. Finishing an end is idempotent.
        let completed = self.st.lock().unwrap().completed.clone();
        if let Some(done) = completed { hub.summary_done(&self.id, done); self.summary_applied(&hub); }
        hub.with_store(|store| { let mut st = self.st.lock().unwrap(); self.drain(store, &mut st) }).ok();
        self.resume_summary_cap(&hub);
        while let Ok(delivery) = events.recv() {
            let seq = delivery.seq;
            let end = matches!(delivery.event, wire::Event::Ended(_));
            let stopped = matches!(delivery.event, wire::Event::Stopped(_));
            let auto = hub.auto_allow(&self.id);
            let done = hub.with_store(|store| {
                let tx = store.conn().unchecked_transaction()?;
                let mut st = self.st.lock().unwrap();
                if seq > 0 && seq <= st.cursor { return Ok(None); }
                if st.retired {
                    if seq > 0 { st.cursor = seq; }
                    if end { st.ended = true; st.outbox.clear(); }
                    self.save(store, &st)?;
                    tx.commit()?;
                    return Ok(None);
                }
                let done = self.handle_locked(store, &mut st, delivery.event, auto)?;
                if seq > 0 { st.cursor = seq; }
                if let Some(done) = &done { st.completed = Some(done.clone()); }
                self.save(store, &st)?;
                tx.commit()?;
                Ok(done)
            });
            match done {
                Ok(done) => {
                    // Ack only after transcript and snapshot are committed together.
                    self.relay.ack(seq).ok();
                    hub.with_store(|store| { let mut st = self.st.lock().unwrap(); self.drain(store, &mut st) })
                        .unwrap_or_else(|e| { eprintln!("sill daemon: delivering queued control: {e:#}"); });
                    if let Some(done) = done { hub.summary_done(&self.id, done); self.summary_applied(&hub); }
                }
                Err(e) => { eprintln!("sill daemon: saving session {}: {e:#}", self.id); std::process::exit(103); }
            }
            if stopped && !self.st.lock().unwrap().retired { hub.turn_ended(&self.id); }
            if end { break; }
        }
        // A relay connection disappearing is not an agent ending: leave the snapshot intact and
        // let restart reconnect. Sweeps treat this missing runner as unknown, not idle.
        let retry = { let st = self.st.lock().unwrap(); !st.ended && !st.retired };
        hub.runner_ended(&self);
        if retry { hub.reconnect_runner(&self.id); }
    }

    fn handle_locked(&self, store: &Store, st: &mut State, ev: wire::Event, auto: bool) -> Result<Option<SummaryDone>> {
        if st.retired {
            return Ok(None);
        }
        let mut done = None;
        let id = self.id.clone();
        match &ev {
            wire::Event::Ready { session_id, resumed, controls } => {
                store.set_acp_session_id(&id, session_id)?;
                st.context.ready(*resumed);
                self.restore(store, st, controls, true)?;
                st.ready = Some((session_id.clone(), *resumed));
                st.controls = Some(controls.clone());
                st.replaying = *resumed;
            }
            wire::Event::Capabilities { images } => st.images = Some(*images),
            wire::Event::Controls(c) => { self.restore(store, st, c, false)?; st.controls = Some(c.clone()); },
            wire::Event::Usage { used, size } => st.usage = Some((*used, *size)),
            // the loop drops a prompt that arrives while it waits for a login (no Stopped follows),
            // so the session is not busy any more — the prompt after the login must not be refused
            wire::Event::AuthRequired { .. } => st.busy = false,
            wire::Event::TurnStarted => {
                st.replaying = false;
                st.busy = true;
                st.turns += 1;
                st.auth = None;
            }
            // a session/load's replay: what it says is in the store already, and on screen
            wire::Event::UserText(_)
            | wire::Event::Text(_)
            | wire::Event::Thought(_)
            | wire::Event::ToolCall { .. }
            | wire::Event::ToolCallUpdate { .. }
            | wire::Event::SubagentText { .. }
                if st.replaying =>
            {
                return Ok(None);
            }
            // some agents echo the prompt back during the turn; the frontend shows what it sent
            wire::Event::UserText(_) => return Ok(None),
            wire::Event::Text(t) => st.agent_md.push_str(t),
            wire::Event::ToolCall { id: call, title, status, kind, detail, diff, parent } => {
                flush(store, &id, st)?;
                if let Ok(seq) = store.append_tool(&id, title, status, kind, detail.as_deref(), diff.as_deref()) {
                    store.set_tool_identity(&id, seq, call, parent.as_deref())?;
                    st.tool_seqs.insert(call.clone(), seq);
                }
            }
            wire::Event::SubagentText { parent, text } => {
                flush(store, &id, st)?;
                store.append_subagent_text(&id, parent, text)?;
            }
            wire::Event::ToolCallUpdate { id: call, status, title, detail, diff } => {
                if let Some(seq) = st.tool_seqs.get(call).copied() {
                    store.update_tool(&id, seq, title.as_deref(), status.as_deref(), detail.as_deref(), diff.as_deref())?;
                }
            }
            wire::Event::Stopped(_) => {
                st.busy = false;
                st.last_turn_end = Some(sill_core::store::now());
                flush(store, &id, st)?;
                // the summary turn: its text goes to the hub once this event is out
                if let Some((kind, text, _)) = st.summary.take() {
                    done = Some(SummaryDone { kind, text: Some(text.trim().to_string()) });
                }
            }
            wire::Event::Ended(_) => {
                st.busy = false;
                st.ended = true;
                flush(store, &id, st)?;
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
            wire::Event::PermissionRequest { request, tool_call_id, title, options } if auto => {
                use acp::PermissionOptionKind as K;
                let pick = options
                    .iter()
                    .find(|o| o.kind == K::AllowOnce)
                    .or_else(|| options.iter().find(|o| o.kind == K::AllowAlways))
                    .map(|o| o.id.clone());
                let allowed = pick.is_some();
                self.relay.answer(request, Answer::Permission(pick))?;
                wire::Event::AutoAnswered { tool_call_id, title, allowed }
            }
            other => other,
        };
        match &ev {
            wire::Event::PermissionRequest { request, .. } | wire::Event::Elicitation { request, .. } => { st.pending.insert(*request, ev.clone()); }
            wire::Event::Resolved { request } => { st.pending.remove(request); }
            wire::Event::AuthRequired { .. } => st.auth = Some(ev.clone()),
            _ => {}
        }
        broadcast(&self.id, &mut st.subs, &ev);
        Ok(done)
    }

    fn restore(&self, store: &Store, st: &mut State, controls: &Controls, initial: bool) -> Result<()> {
        let settings = store.sessions()?.into_iter().find(|s| s.id == self.id).map(|s| s.agent_settings).unwrap_or_default();
        for command in st.context.restore(controls, &settings, initial) { Self::queue(st, from_acp(command)); }
        Ok(())
    }

    fn summary_applied(&self, hub: &Hub) {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            st.completed = None;
            if !st.retired { self.save(store, &st)?; }
            Ok(())
        }).ok();
    }

    fn queue(st: &mut State, command: wire::Command) {
        st.next_command += 1;
        st.outbox.insert(st.next_command, command);
    }

    fn drain(&self, store: &Store, st: &mut State) -> Result<()> {
        while let Some((id, command)) = st.outbox.first_key_value().map(|(id, cmd)| (*id, cmd.clone())) {
            self.relay.command(id, command)?;
            st.outbox.remove(&id);
            self.save(store, st)?;
        }
        Ok(())
    }

    fn resume_summary_cap(self: &Arc<Self>, hub: &Arc<Hub>) {
        let timer = { let st = self.st.lock().unwrap(); st.summary.as_ref().map(|(_,_,g)| (*g, st.summary_deadline)) };
        if let Some((generation, deadline)) = timer {
            let remaining = deadline.map(|end| std::time::Duration::from_secs(end.saturating_sub(sill_core::store::now()).max(0) as u64)).unwrap_or(SUMMARY_CAP);
            let (me, hub) = (self.clone(), hub.clone());
            std::thread::spawn(move || { std::thread::sleep(remaining); me.cut_summary(&hub, generation, "cut short: summary deadline reached"); });
        }
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
        let st = self.st.lock().unwrap();
        st.summary.as_ref().map(|(k, _, _)| *k).or_else(|| st.completed.as_ref().map(|done| done.kind))
    }

    /// The tab closed during a checkpoint: its answer becomes the session's final summary.
    pub fn adopt_checkpoint(&self, hub: &Hub, end_context: &str) -> Result<bool> {
        hub.with_store(|store| {
        let mut st = self.st.lock().unwrap();
        if !matches!(st.summary, Some((SummaryKind::Checkpoint, _, _))) { return Ok(false); }
        let transaction = store.conn().unchecked_transaction()?;
        st.summary.as_mut().unwrap().0 = SummaryKind::End;
        let result = (|| -> Result<()> {
            store.conn().execute("INSERT INTO daemon_ends(id,context) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET context=excluded.context", rusqlite::params![self.id, end_context])?;
            self.save(store, &st)?;
            transaction.commit()?;
            Ok(())
        })();
        if result.is_err() { st.summary.as_mut().unwrap().0 = SummaryKind::Checkpoint; }
        result.map(|()| true)
        })
    }

    /// Ask the agent for a summary as a plain turn (docs/memory.md §2.2): no standing block, no
    /// user entry — a note marks it in the transcript and for the attached frontends — and the
    /// answer is stored like any agent message. The hub gets [`SummaryDone`] when the turn stops,
    /// when the agent goes, or when [`SUMMARY_CAP`] passes (the turn is cancelled then; what was
    /// written so far, marked as cut short, is the answer). Refused while the agent is busy, down,
    /// or already summarizing.
    pub fn ask_summary(self: &Arc<Self>, hub: &Arc<Hub>, prompt: &str, note: &str, kind: SummaryKind) -> Result<()> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            if st.ended || st.retired || st.ready.is_none() { bail!("the agent is not running"); }
            if st.busy { bail!("the agent is busy"); }
            if st.summary.is_some() { bail!("a summary is being written already"); }
            let tx = store.conn().unchecked_transaction()?;
            st.busy = true;
            st.summary_gen += 1;
            st.summary_deadline = Some(sill_core::store::now() + SUMMARY_CAP.as_secs() as i64);
            st.summary = Some((kind, String::new(), st.summary_gen));
            Self::queue(&mut st, wire::Command::Prompt { text: prompt.into(), attachments: vec![], entry: None });
            store.append(&self.id, "note", note, "")?;
            self.save(store, &st)?;
            tx.commit()?;
            broadcast(&self.id, &mut st.subs, &wire::Event::Note(note.into()));
            self.drain(store, &mut st)?;
            Ok(())
        })?;
        self.resume_summary_cap(hub);
        Ok(())
    }

    /// Stop the summary turn started as generation `generation` before the agent stopped it: the turn is
    /// cancelled and what it wrote so far, marked with `why`, is the answer.
    fn cut_summary(&self, hub: &Arc<Hub>, generation: u64, why: &str) {
        let done = hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            if !st.summary.as_ref().is_some_and(|(_,_,g)| *g == generation) { return Ok(None); }
            let (kind, mut text, _) = st.summary.take().unwrap();
            text.push_str(&st.agent_md);
            let text = text.trim().to_string();
            Self::queue(&mut st, wire::Command::Cancel);
            let done = SummaryDone { kind, text: (!text.is_empty()).then(|| format!("{text}\n({why})")) };
            st.completed = Some(done.clone());
            self.save(store, &st)?;
            self.drain(store, &mut st)?;
            Ok(Some(done))
        });
        match done {
            Ok(Some(done)) => { hub.summary_done(&self.id, done); self.summary_applied(hub); }
            Ok(None) => {}
            Err(e) => eprintln!("sill daemon: summary deadline for {}: {e:#}", self.id),
        }
    }

    /// One event to every attached frontend (the hub's own: the end, a checkpoint's answer).
    pub fn notify(&self, event: &wire::Event) {
        let mut st = self.st.lock().unwrap();
        broadcast(&self.id, &mut st.subs, event);
    }

    /// Subscribe `conn` and bring it up to date: what a frontend that just opened needs to show the
    /// session as it is — the agent's capabilities and controls, a login it waits for, the turn in
    /// flight with its text so far, the questions not yet answered.
    pub fn attach(&self, conn: Arc<Conn>, store: &Store) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        st.subs.retain(|c| c.id != conn.id);
        // Store and runner are locked together by the hub. A turn cannot finish between loading
        // the transcript and taking the live snapshot, so no final text/tool update can fall into
        // the gap between the two views of the conversation.
        let entries = store.entries_before(&self.id, None, 60)?;
        let total = store.entry_count(&self.id)?;
        let replay = sill_proto::rpc::Request::new(None, "_sill/session.replay", serde_json::json!({"key":self.id,"entries":entries,"total":total}));
        conn.send(serde_json::to_string(&replay)?);
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
        evs.extend(st.pending.values().cloned());
        let mut one = vec![conn];
        for e in &evs {
            broadcast(&self.id, &mut one, e);
        }
        st.subs.extend(one);
        Ok(())
    }

    /// The watchdog's probe of this runner's lock (`Hub::health`).
    pub fn probe(&self, deadline: std::time::Instant) -> bool {
        super::hub::probe(&self.st, deadline)
    }

    pub fn detach(&self, conn: u64) {
        self.st.lock().unwrap().subs.retain(|c| c.id != conn);
    }

    /// A frontend's command. A prompt is refused while a turn runs; when it is taken, its entry is
    /// written — the person's text, or the note for a turn sill asked for.
    pub fn command(&self, hub: &Hub, origin: u64, cmd: wire::Command) -> Result<()> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            let user = self.commit_command(store, &hub.home, &mut st, cmd)?;
            if let Some(text) = user {
                let mut others: Vec<_> = st.subs.iter().filter(|connection| connection.id != origin).cloned().collect();
                broadcast(&self.id, &mut others, &wire::Event::UserText(text));
            }
            // Once committed, a transport failure keeps the outbox and busy state for recovery.
            // It must never roll back a command the worker may already have accepted.
            self.drain(store, &mut st)?;
            Ok(())
        })
    }

    fn commit_command(&self, store: &Store, home: &std::path::Path, st: &mut State, mut cmd: wire::Command) -> Result<Option<String>> {
        let before = st.clone();
        let result = (|| -> Result<Option<String>> {
            if st.ended || st.retired { bail!("the session is not running"); }
            let mut prepared = None;
            let mut entry = None;
            if let wire::Command::Prompt { text, entry: e, .. } = &mut cmd {
                if st.ready.is_none() { bail!("the agent is starting or needs a login"); }
                if st.busy { bail!("the agent is busy; wait for the turn to end"); }
                if st.context.restoring() { bail!("the agent is restoring its model settings; send again when it is ready"); }
                let p = st.context.prepare(store, &self.id, home, text)?;
                *text = p.text.clone();
                prepared = Some(p);
                entry = e.clone();
                st.busy = true;
            }
            let remembered = wire::command_into_acp(cmd.clone()).0;
            let controls = st.controls.clone();
            st.context.chosen(controls.as_ref(), &remembered);
            // Keep the accepted command and its plain transcript entry together before sending.
            let tx = store.conn().unchecked_transaction()?;
            if let Some(p) = prepared { st.context.commit(p); }
            let user = match &entry { Some(Entry::User { text, .. }) => Some(text.clone()), _ => None };
            match entry {
                Some(Entry::User { text, names }) => { store.append(&self.id, "user", &text, &names.join("\n"))?; store.touch(&self.id)?; }
                Some(Entry::Note(note)) => { store.append(&self.id, "note", &note, "")?; }
                None => {}
            }
            Self::queue(st, cmd);
            context::remember_choice(store, &self.id, &remembered)?;
            self.save(store, st)?;
            tx.commit()?;
            Ok(user)
        })();
        if result.is_err() { *st = before; }
        result
    }

    /// The answer to question `request`: to the agent, and every attached frontend drops its dialog.
    pub fn answer(&self, request: u64, answer: Answer) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let Some(question) = st.pending.get(&request) else { bail!("no open question {request} (answered already?)") };
        if !matches!((question, &answer), (wire::Event::PermissionRequest { .. }, Answer::Permission(_)) | (wire::Event::Elicitation { .. }, Answer::Form(_))) {
            bail!("the answer is not of the question's kind");
        }
        self.relay.answer(request, answer)?;
        st.pending.remove(&request);
        broadcast(&self.id, &mut st.subs, &wire::Event::Resolved { request });
        Ok(())
    }

    /// Hand the session over to a newer runner, or end it: no more events out, the loop told to quit.
    pub fn retire(&self, hub: &Hub) -> Result<()> {
        hub.with_store(|store| {
            let mut st = self.st.lock().unwrap();
            if st.retired { return self.drain(store, &mut st); }
            st.retired = true;
            st.busy = false;
            st.summary = None;
            st.summary_deadline = None;
            st.completed = None;
            st.outbox.clear();
            st.subs.clear();
            st.pending.clear();
            // Save retirement and the Quit outbox before sending it. The terminal cursor remains
            // until deletion/retention, allowing another daemon to drain an unacknowledged Ended.
            Self::queue(&mut st, wire::Command::Quit);
            self.save(store, &st)?;
            self.drain(store, &mut st)
        })
    }

    pub fn mark_terminal(store: &Store, id: &str) -> Result<()> {
        let snapshot = Self::load(id, store)?;
        if snapshot.is_null() { return Ok(()); }
        let mut state: State = serde_json::from_value(snapshot)?;
        state.retired = true;
        state.ended = true;
        state.busy = false;
        state.summary = None;
        state.completed = None;
        state.pending.clear();
        state.outbox.clear();
        store.conn().execute("UPDATE daemon_runners SET state=?2 WHERE id=?1", rusqlite::params![id, serde_json::to_string(&state)?])?;
        Ok(())
    }
}

/// The agent's gathered text becomes an `agent` entry; a summary turn keeps its copy.
fn flush(store: &Store, id: &str, st: &mut State) -> Result<()> {
    let md = std::mem::take(&mut st.agent_md);
    if !md.trim().is_empty() {
        if let Some((_, text, _)) = st.summary.as_mut() {
            text.push_str(&md);
        }
        store.append(id, "agent", &md, "")?;
    }
    Ok(())
}

fn from_acp(cmd: acp::Command) -> wire::Command {
    match cmd {
        acp::Command::SetMode(m) => wire::Command::SetMode(m),
        acp::Command::SetConfig(k,v) => wire::Command::SetConfig(k,v),
        _ => unreachable!("control restoration only emits control commands"),
    }
}

/// One event to every subscriber; a connection that is gone or stuck is dropped from the list.
fn broadcast(id: &str, subs: &mut Vec<Arc<Conn>>, event: &wire::Event) {
    if subs.is_empty() {
        return;
    }
    let params = sill_proto::SessionEvent { key: id.to_string(), event: event.clone() };
    let req = sill_proto::rpc::Request::new(None, sill_proto::method::SESSION_EVENT, serde_json::to_value(params).unwrap_or_default());
    let Ok(line) = serde_json::to_string(&req) else { return };
    subs.retain(|c| c.send(line.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn relay_client() -> (Arc<relay::Client>, std::thread::JoinHandle<()>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sill-runner-snapshot-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("relay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut hello = String::new();
            reader.read_line(&mut hello).unwrap();
            stream.write_all(b"{\"Hello\":{\"acknowledged\":0,\"last\":0}}\n").unwrap();
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                if let Some(command) = message.get("Command") {
                    writeln!(stream, "{}", serde_json::json!({"Accepted":{"id":command["id"],"error":null}})).unwrap();
                }
            }
        });
        let (client, _) = relay::connect(&socket, 0).unwrap();
        (client, server, dir)
    }

    #[test]
    fn snapshot_restores_partial_summary_questions_and_tool_updates() {
        let (client, server, dir) = relay_client();
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("fake-agent", "default").unwrap();
        let snapshot = Runner::load(&session.id, &store).unwrap();
        let runner = Runner::new(&session.id, client.clone(), snapshot).unwrap();
        {
            let mut state = runner.st.lock().unwrap();
            state.ready = Some(("agent-session".into(), false));
            state.busy = true;
            state.summary = Some((SummaryKind::End, String::new(), 7));
            runner.handle_locked(&store, &mut state, wire::Event::Text("first summary segment".into()), false).unwrap();
            runner.handle_locked(&store, &mut state, wire::Event::ToolCall {
                id: "tool-1".into(), title: "inspect".into(), status: "running".into(), kind: "read".into(), detail: None, diff: None, parent: Some("delegation".into()),
            }, false).unwrap();
            runner.handle_locked(&store, &mut state, wire::Event::PermissionRequest { request: 19, tool_call_id: "tool-1".into(), title: "inspect".into(), options: vec![] }, false).unwrap();
            state.cursor = 12;
            runner.save(&store, &state).unwrap();
        }
        drop(runner);
        let restored = Runner::new(&session.id, client.clone(), Runner::load(&session.id, &store).unwrap()).unwrap();
        {
            let mut state = restored.st.lock().unwrap();
            assert_eq!(state.cursor, 12);
            assert!(state.busy);
            assert!(state.pending.contains_key(&19));
            assert!(matches!(&state.summary, Some((SummaryKind::End, text, 7)) if text == "first summary segment"));
            restored.handle_locked(&store, &mut state, wire::Event::ToolCallUpdate { id: "tool-1".into(), status: Some("completed".into()), title: None, detail: None, diff: None }, false).unwrap();
            restored.handle_locked(&store, &mut state, wire::Event::Resolved { request: 19 }, false).unwrap();
            restored.handle_locked(&store, &mut state, wire::Event::Text(" plus second segment".into()), false).unwrap();
            let done = restored.handle_locked(&store, &mut state, wire::Event::Stopped(acp::StopReason::EndTurn), false).unwrap().unwrap();
            assert_eq!(done.kind, SummaryKind::End);
            assert_eq!(done.text.as_deref(), Some("first summary segment plus second segment"));
            assert!(!state.busy);
            assert!(state.pending.is_empty());
        }
        let entries = store.entries_before(&session.id, None, usize::MAX).unwrap();
        let tool = entries.iter().find(|entry| entry.kind == "tool").unwrap();
        assert_eq!(tool.call_id, "tool-1");
        assert_eq!(tool.parent, "delegation");
        assert_eq!(tool.extra, "completed");
        assert_eq!(entries.iter().filter(|entry| entry.kind == "agent").count(), 2);
        drop(restored);
        drop(client);
        server.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn committed_outbox_keeps_its_command_id_until_acknowledged() {
        let (client, server, dir) = relay_client();
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("fake-agent", "default").unwrap();
        Runner::load(&session.id, &store).unwrap();
        let runner = Runner::new(&session.id, client.clone(), serde_json::Value::Null).unwrap();
        {
            let mut state = runner.st.lock().unwrap();
            state.busy = true;
            Runner::queue(&mut state, wire::Command::Prompt { text: "plain".into(), attachments: vec![], entry: None });
            runner.save(&store, &state).unwrap();
        }
        drop(runner);
        let restored = Runner::new(&session.id, client.clone(), Runner::load(&session.id, &store).unwrap()).unwrap();
        {
            let mut state = restored.st.lock().unwrap();
            assert_eq!(state.next_command, 1);
            assert_eq!(state.outbox.keys().copied().collect::<Vec<_>>(), vec![1]);
            restored.drain(&store, &mut state).unwrap();
            assert!(state.outbox.is_empty());
        }
        let snapshot = Runner::load(&session.id, &store).unwrap();
        assert_eq!(snapshot["next_command"], 1);
        assert_eq!(snapshot["outbox"], serde_json::json!({}));
        assert_eq!(snapshot["busy"], true);
        drop(restored);
        drop(client);
        server.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_cursor_and_retirement_survive_another_daemon() {
        let (client, server, dir) = relay_client();
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("fake-agent", "default").unwrap();
        Runner::load(&session.id, &store).unwrap();
        let runner = Runner::new(&session.id, client.clone(), serde_json::Value::Null).unwrap();
        {
            let mut state = runner.st.lock().unwrap();
            state.cursor = 42;
            state.busy = true;
            state.summary = Some((SummaryKind::End, "partial".into(), 1));
            runner.save(&store, &state).unwrap();
        }
        Runner::mark_terminal(&store, &session.id).unwrap();
        let snapshot = Runner::load(&session.id, &store).unwrap();
        assert_eq!(Runner::cursor(&snapshot), 42);
        let restored = Runner::new(&session.id, client.clone(), snapshot).unwrap();
        assert!(!restored.alive());
        let state = restored.st.lock().unwrap();
        assert!(state.retired && state.ended);
        assert!(!state.busy);
        assert!(state.summary.is_none());
        drop(state);
        drop(restored);
        drop(runner);
        drop(client);
        server.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_late_event_cannot_recreate_the_snapshot_of_a_deleted_session() {
        let (client, server, dir) = relay_client();
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("fake-agent", "default").unwrap();
        Runner::load(&session.id, &store).unwrap();
        let runner = Runner::new(&session.id, client.clone(), serde_json::Value::Null).unwrap();
        store.delete(&session.id).unwrap();
        {
            let mut state = runner.st.lock().unwrap();
            state.cursor = 17;
            runner.save(&store, &state).unwrap();
        }
        assert!(Runner::load(&session.id, &store).unwrap().is_null());
        drop(runner);
        drop(client);
        server.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_command_rolls_back_busy_context_outbox_and_remembered_settings() {
        let (client, server, dir) = relay_client();
        let store = Store::open(&dir.join("test.db")).unwrap();
        let session = store.create("fake-agent", "default").unwrap();
        Runner::load(&session.id, &store).unwrap();
        let runner = Runner::new(&session.id, client.clone(), serde_json::Value::Null).unwrap();
        let mut state = runner.st.lock().unwrap();
        state.ready = Some(("fake-agent-session".into(), false));
        let before = serde_json::to_value(&*state).unwrap();
        store.conn().execute_batch("CREATE TRIGGER reject_user BEFORE INSERT ON entries WHEN NEW.kind='user' BEGIN SELECT RAISE(ABORT,'reject user for test'); END").unwrap();
        let prompt = || wire::Command::Prompt { text: "work".into(), attachments: vec![], entry: Some(Entry::User { text: "work".into(), names: vec![] }) };
        assert!(runner.commit_command(&store, &dir, &mut state, prompt()).is_err());
        assert_eq!(serde_json::to_value(&*state).unwrap(), before);
        assert_eq!(store.entry_count(&session.id).unwrap(), 0);
        assert!(Runner::load(&session.id, &store).unwrap().is_null());
        store.conn().execute_batch("DROP TRIGGER reject_user; CREATE TRIGGER reject_snapshot BEFORE INSERT ON daemon_runners BEGIN SELECT RAISE(ABORT,'reject snapshot for test'); END").unwrap();
        assert!(runner.commit_command(&store, &dir, &mut state, wire::Command::SetMode("auto".into())).is_err());
        assert_eq!(serde_json::to_value(&*state).unwrap(), before);
        assert!(store.agent_settings("fake-agent").mode.is_none());
        assert!(store.sessions().unwrap().into_iter().find(|s| s.id == session.id).unwrap().agent_settings.mode.is_none());
        store.conn().execute_batch("DROP TRIGGER reject_snapshot").unwrap();
        assert_eq!(runner.commit_command(&store, &dir, &mut state, prompt()).unwrap().as_deref(), Some("work"));
        assert!(state.busy);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(store.entry_count(&session.id).unwrap(), 1);
        drop(state);
        drop(runner);
        drop(client);
        server.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
