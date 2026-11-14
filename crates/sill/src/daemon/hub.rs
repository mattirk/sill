//! The daemon's state and its methods: one store, the launcher, the sessions it supervises and the
//! conversations it runs (`runner`), a dispatch by method name. `serve` hands it the connection, a
//! method and a value and writes back what it says; events go to the connection's queue.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sill_core::launcher::Launcher;
use sill_core::policy::Policy;
use sill_core::session::StartOptions;
use sill_core::store::Store;
use sill_proto::rpc::Error;
use sill_proto::{
    AgentSettingsSet, Attach, Compacted, Count, DaemonStatus, End, Ending, Key, LogRequest, MemorySearch, MemorySession, NewSession, Note, Pin, Session,
    SessionAnswer, SessionCommand, SessionSet, Started, Text, Transcript, method,
};

use super::Conn;
use super::runner::{Runner, SummaryDone, SummaryKind};

// the Changes page's methods: a child module, so it reaches the hub's own fields
#[path = "changes.rs"]
mod changes;

/// What an end that waits for the agent's summary needs once the answer is in (the facts as they
/// were when the end was asked for; the row's summary is replaced only when memory is on).
struct EndCtx {
    facts: sill_core::memory::Facts,
    memory: bool,
    /// The old summary stays: memory is off, or a reopened session was closed without a turn.
    keep_old: bool,
    /// Why the end was asked for (`close`, `idle`, `quit`): the `SessionEnded` event carries it.
    reason: String,
}

pub struct Hub {
    store: Mutex<Store>,
    started: i64,
    mode: &'static str,
    launcher: &'static Launcher,
    /// The copy of this binary that sessions are started from (`session::staged_exe`).
    exe: PathBuf,
    home: PathBuf,
    /// Session id → pid of its `sill launch`, for every jail this daemon started or adopted; the
    /// wait threads take an entry out when its process ends.
    children: Arc<Mutex<HashMap<String, u32>>>,
    /// Session id → the conversation the daemon runs for it (docs/daemon.md §3.3).
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    /// Starting a conversation (jail, agent, ACP) is one at a time: two attaches of one session
    /// must not start two agents.
    starting: Mutex<()>,
    /// Sessions whose end waits for the agent's summary turn (`sessions.end`).
    ending: Mutex<HashMap<String, EndCtx>>,
    /// When each running jail was started or adopted by this daemon: the idle timer counts from
    /// the latest of this, the last prompt and the last turn's end, so a session just resumed is
    /// not ended a minute in.
    started_at: Mutex<HashMap<String, i64>>,
}

impl Hub {
    pub fn new(mode: &'static str) -> Result<Hub> {
        let store = Store::open_default()?;
        let launcher = sill_core::launcher::current();
        eprintln!("sill daemon: {}", launcher.describe());
        let hub = Hub {
            store: Mutex::new(store),
            started: sill_core::store::now(),
            mode,
            launcher,
            exe: sill_core::session::staged_exe()?,
            home: sill_core::changes::home()?,
            children: Arc::default(),
            runners: Mutex::new(HashMap::new()),
            starting: Mutex::new(()),
            ending: Mutex::new(HashMap::new()),
            started_at: Mutex::new(HashMap::new()),
        };
        hub.adopt_running();
        hub.sweep_dead_ghosts();
        hub.sweep_orphan_runtime();
        Ok(hub)
    }

    pub fn session_count(&self) -> usize {
        self.store.lock().unwrap().sessions().map(|s| s.len()).unwrap_or(0)
    }

    /// The store, for one call (the runners write the transcript through it).
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        f(&self.store.lock().unwrap())
    }

    /// The session answers permission requests itself (its auto-allow, read at the moment asked).
    pub fn auto_allow(&self, id: &str) -> bool {
        self.store.lock().unwrap().sessions().ok().and_then(|v| v.into_iter().find(|s| s.id == id)).is_some_and(|s| s.auto_allow)
    }

    /// A connection closed: it is subscribed nowhere any more.
    pub fn disconnect(&self, conn: u64) {
        let ids: Vec<String> = {
            let runners = self.runners.lock().unwrap();
            runners
                .iter()
                .filter(|(_, r)| {
                    let before = r.sub_count();
                    r.detach(conn);
                    r.sub_count() != before
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.presence(&id);
        }
    }

    /// Tell the session's shim how many frontends are attached (docs/daemon.md §3.6): the agent
    /// stream is the daemon's and never drops while it lives, so the shim's agent-idle rule needs
    /// this to know nobody is watching. On a thread: the shim may be slow, the caller is a request.
    fn presence(&self, id: &str) {
        let n = self.runners.lock().unwrap().get(id).filter(|r| r.alive()).map(|r| r.sub_count()).unwrap_or(0);
        let id = id.to_string();
        let _ = std::thread::Builder::new().name(format!("sill-presence-{id}")).spawn(move || {
            let Ok(sock) = sill_core::session::socket_path(&id) else { return };
            match sill_core::shim::Control::connect(&sock).and_then(|mut c| c.attached(n as u32)) {
                Ok(()) => {}
                Err(e) => eprintln!("sill daemon: session {id}: telling the shim {n} attached: {e:#}"),
            }
        });
    }

    /// Once a minute (docs/daemon.md §3.6): the policy's idle timeout ends a running session whose
    /// person has sent nothing for `idle_timeout_minutes` and whose agent is between turns, as
    /// closing its tab would (`sessions.end`, reason `idle`; 0 = never) — attached or not, the
    /// daemon's sessions are its own. Then memory's nightly run when it is due.
    pub fn sweep(self: &Arc<Self>) {
        let now = sill_core::store::now();
        let mut ids: Vec<String> = self.children.lock().unwrap().keys().cloned().collect();
        ids.extend(self.runners.lock().unwrap().keys().cloned());
        ids.sort();
        ids.dedup();
        let sessions = self.with_store(|s| s.sessions()).unwrap_or_default();
        for id in ids {
            let Some(s) = sessions.iter().find(|x| x.id == id) else { continue };
            if s.ended_at.is_some() || self.ending.lock().unwrap().contains_key(&id) {
                continue;
            }
            let runner = self.runners.lock().unwrap().get(&id).filter(|r| r.alive()).cloned();
            if runner.as_ref().is_some_and(|r| r.busy()) {
                continue;
            }
            let Ok(mut policy) = Policy::load(&s.policy, &self.home) else { continue };
            if s.ghost {
                policy.ghostly(&self.home); // at most an hour (docs/memory.md §3.2)
            }
            let limit = policy.limits.idle_timeout_minutes as i64 * 60;
            if limit <= 0 {
                continue;
            }
            let started = self.started_at.lock().unwrap().get(&id).copied().unwrap_or(now);
            let since = s.last_used.max(started).max(runner.and_then(|r| r.last_turn_end()).unwrap_or(0));
            if now - since > limit {
                eprintln!("sill daemon: session {id} idle for {} minutes, ending it", policy.limits.idle_timeout_minutes);
                if let Err(e) = self.end_session(&id, "idle") {
                    eprintln!("sill daemon: ending idle session {id}: {e:#}");
                }
            }
        }
        self.revert_ghosts();
        self.auto_revert();
        let uid = rustix::process::getuid().as_raw();
        match sill_core::memory::nightly_if_due(&|id| {
            let _ = sill_core::session::remove_state(uid, id);
        }) {
            Ok(Some(done)) => eprintln!("sill daemon: memory nightly: {} compacted, {} trimmed, {} deleted", done.compacted, done.trimmed, done.deleted),
            Ok(None) => {}
            Err(e) => eprintln!("sill daemon: memory nightly: {e:#}"),
        }
    }

    /// A conversation's loop ended: forget it unless a newer one took its place.
    pub fn runner_ended(&self, r: &Arc<Runner>) {
        let mut runners = self.runners.lock().unwrap();
        if runners.get(&r.id).is_some_and(|cur| Arc::ptr_eq(cur, r)) {
            runners.remove(&r.id);
        }
        eprintln!("sill daemon: session {} conversation ended", r.id);
    }

    fn runner(&self, id: &str) -> Result<Arc<Runner>, Error> {
        match self.runners.lock().unwrap().get(id) {
            Some(r) if r.alive() => Ok(r.clone()),
            _ => Err(Error::failed("the session is not running")),
        }
    }

    pub fn dispatch(self: &Arc<Self>, conn: &Arc<Conn>, m: &str, params: Value) -> Result<Value, Error> {
        match m {
            method::DAEMON_STATUS => to_value(self.status()?),
            method::DAEMON_SHUTDOWN => Ok(json!({})),
            method::SESSIONS_LIST => to_value(self.sessions()?),
            method::SESSIONS_GET => {
                let k: Key = args(params)?;
                let all = self.sessions()?;
                let found = all.iter().find(|s| s.id == k.key).or_else(|| all.iter().find(|s| s.id.starts_with(&k.key) || s.title == k.key));
                match found {
                    Some(s) => to_value(s),
                    None => Err(Error::failed(format!("no session {}", k.key))),
                }
            }
            method::SESSIONS_START => {
                let k: Key = args(params)?;
                to_value(self.start_session(&k.key).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::SESSIONS_STOP => {
                let k: Key = args(params)?;
                self.retire_runner(&k.key);
                sill_core::session::stop(&k.key).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_NEW => {
                let n: NewSession = args(params)?;
                to_value(Key { key: self.new_session(n).map_err(|e| Error::failed(format!("{e:#}")))? })
            }
            method::SESSIONS_SET => {
                let r: SessionSet = args(params)?;
                let set: sill_core::wire::SessionSet = serde_json::from_value(r.set).map_err(Error::invalid_params)?;
                self.set_session(&r.key, set).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_DELETE => {
                let k: Key = args(params)?;
                self.delete_session(&k.key).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_PIN | method::SESSIONS_UNPIN => {
                let p: Pin = args(params)?;
                if m == method::SESSIONS_PIN && self.is_ghost(&p.key) {
                    return Err(Error::failed(GHOST_NO_PINS));
                }
                self.with_session(&p.key, |s, id| if m == method::SESSIONS_PIN { s.pin(id, &p.path) } else { s.unpin(id, &p.path) })
                    .map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::MEMORY_NOTE_ADD => {
                let n: Note = args(params)?;
                if self.is_ghost(&n.key) {
                    return Err(Error::failed("a ghost session is not remembered: it takes no notes"));
                }
                self.with_session(&n.key, |s, id| s.add_memory(id, "note", &n.text)).map_err(|e| Error::failed(format!("{e:#}")))?;
                refresh_memory();
                Ok(json!({}))
            }
            method::MEMORY_NOTE_REMOVE => {
                let n: Note = args(params)?;
                self.with_session(&n.key, |s, id| s.remove_memory(id, n.ts, &n.text)).map_err(|e| Error::failed(format!("{e:#}")))?;
                refresh_memory();
                Ok(json!({}))
            }
            method::MEMORY_COMPACT => {
                let uid = rustix::process::getuid().as_raw();
                let done = self
                    .with_store(|s| {
                        let done = sill_core::memory::compact_all(s, sill_core::store::now(), &|id| {
                            let _ = sill_core::session::remove_state(uid, id);
                        })?;
                        s.vacuum()?;
                        Ok(done)
                    })
                    .map_err(|e| Error::failed(format!("{e:#}")))?;
                refresh_memory();
                to_value(Compacted { compacted: done.compacted, trimmed: done.trimmed, deleted: done.deleted })
            }
            method::MEMORY_FORGET => {
                let ended: Vec<String> = self.with_store(|s| s.sessions()).map_err(|e| Error::failed(format!("{e:#}")))?.into_iter().filter(|s| s.ended_at.is_some()).map(|s| s.id).collect();
                for id in &ended {
                    self.delete_session(id).map_err(|e| Error::failed(format!("{e:#}")))?;
                }
                self.with_store(|s| s.vacuum()).ok();
                to_value(Count { n: ended.len() })
            }
            method::AGENTS_SET_SETTINGS => {
                let a: AgentSettingsSet = args(params)?;
                let settings: sill_core::store::AgentSettings = serde_json::from_value(a.settings).map_err(Error::invalid_params)?;
                self.with_store(|s| s.set_agent_settings(&a.agent, &settings)).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_CHANGES => {
                let k: Key = args(params)?;
                let s = self.with_store(|st| st.sessions()).map_err(|e| Error::failed(format!("{e:#}")))?.into_iter().find(|s| s.id == k.key);
                let Some(s) = s else { return Err(Error::failed(format!("no session {}", k.key))) };
                let uid = rustix::process::getuid().as_raw();
                let user = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
                let changes = self.session_changes(&s, uid, &user).map_err(|e| Error::failed(format!("{e:#}")))?;
                // what the person would want to look at: build output is not asked about
                to_value(Count { n: changes.iter().filter(|c| !c.ignored).count() })
            }
            method::CHANGES_HISTORY => {
                let f: sill_core::history::Filter = args(params)?;
                to_value(self.changes_history(f).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::CHANGES_LIST => to_value(self.changes_list().map_err(|e| Error::failed(format!("{e:#}")))?),
            method::CHANGES_DIFF => {
                let of: sill_core::wire::DiffOf = args(params)?;
                to_value(Text { text: self.changes_diff(of).map_err(|e| Error::failed(format!("{e:#}")))? })
            }
            method::CHANGES_KEEP => {
                let k: sill_proto::ChangesKeep = args(params)?;
                to_value(self.changes_keep(&k.session, k.items).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::CHANGES_REVERT => {
                let r: sill_proto::ChangesPaths = args(params)?;
                to_value(self.changes_revert(&r.session, r.paths).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::CHANGES_UNDO => {
                let u: sill_proto::ChangesUndo = args(params)?;
                to_value(self.changes_undo(&u.session, u.ids, u.force).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::CHANGES_GIT_REVERT => {
                let g: sill_proto::GitRevert = args(params)?;
                to_value(Text { text: self.changes_git_revert(&g.repo, &g.rel).map_err(|e| Error::failed(format!("{e:#}")))? })
            }
            method::SESSIONS_END => {
                let e: End = args(params)?;
                to_value(self.end_session(&e.key, &e.reason).map_err(|e| Error::failed(format!("{e:#}")))?)
            }
            method::SESSIONS_CHECKPOINT => {
                let k: Key = args(params)?;
                self.checkpoint(&k.key).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_ATTACH => {
                let a: Attach = args(params)?;
                self.attach(conn, &a.key, a.fresh).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_DETACH => {
                let k: Key = args(params)?;
                if let Some(r) = self.runners.lock().unwrap().get(&k.key) {
                    r.detach(conn.id);
                }
                self.presence(&k.key);
                Ok(json!({}))
            }
            method::SESSIONS_COMMAND => {
                let c: SessionCommand = args(params)?;
                let cmd: sill_core::wire::Command = serde_json::from_value(c.command).map_err(Error::invalid_params)?;
                let r = self.runner(&c.key)?;
                if matches!(cmd, sill_core::wire::Command::Quit) {
                    r.detach(conn.id);
                    self.presence(&c.key);
                    return Ok(json!({}));
                }
                r.command(self, cmd).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_ANSWER => {
                let a: SessionAnswer = args(params)?;
                let answer: sill_core::wire::Answer = serde_json::from_value(a.answer).map_err(Error::invalid_params)?;
                self.runner(&a.key)?.answer(a.request, answer).map_err(|e| Error::failed(format!("{e:#}")))?;
                Ok(json!({}))
            }
            method::SESSIONS_LOG => {
                let r: LogRequest = args(params)?;
                if !sill_core::session::valid_id(&r.key) {
                    return Err(Error::invalid_params("not a session id"));
                }
                to_value(Text { text: sill_core::session::read_log(&sill_core::session::log_source(&r.key), r.lines.clamp(1, 20_000)) })
            }
            method::MEMORY_LIST => {
                let store = self.store.lock().unwrap();
                // a ghost between its end and its deletion is nobody's memory
                let ghosts: Vec<String> = store.sessions().map_err(Error::failed)?.into_iter().filter(|s| s.ghost).map(|s| s.id).collect();
                let mut ended: Vec<MemorySession> = sill_memory::sessions(store.conn())
                    .map_err(Error::failed)?
                    .into_iter()
                    .filter(|s| !ghosts.contains(&s.id))
                    .filter_map(|s| {
                        Some(MemorySession { id: s.id, agent: s.agent, title: s.title, ended_at: s.ended_at?, summary: sill_memory::first_line(&s.summary).to_string() })
                    })
                    .collect();
                ended.sort_by_key(|s| std::cmp::Reverse(s.ended_at));
                to_value(ended)
            }
            method::MEMORY_SHOW => {
                let k: Key = args(params)?;
                let store = self.store.lock().unwrap();
                let s = find(&store, &k.key)?;
                let text = sill_memory::show(store.conn(), &s, &sill_core::memory::kept_of(&s.id), &self.home).map_err(Error::failed)?;
                to_value(Text { text })
            }
            method::MEMORY_SEARCH => {
                let q: MemorySearch = args(params)?;
                let mut text = q.words;
                if let Some(f) = q.folder {
                    text.push(format!("folder:{f}"));
                }
                if let Some(a) = q.agent {
                    text.push(format!("agent:{a}"));
                }
                if let Some(s) = q.since {
                    if sill_memory::parse_age(&s).is_none() {
                        return Err(Error::invalid_params("since expects 30d, 12h, 2w …"));
                    }
                    text.push(format!("since:{s}"));
                }
                let query = sill_memory::Query::parse(&text.join(" "), &self.home);
                if query.is_empty() {
                    return Err(Error::invalid_params("search for what?"));
                }
                let store = self.store.lock().unwrap();
                let hits = sill_memory::search(store.conn(), &query, sill_core::store::now(), 50).map_err(Error::failed)?;
                let text = if hits.is_empty() { "nothing found\n".to_string() } else { hits.iter().map(|h| sill_memory::hit_lines(h, &self.home)).collect() };
                to_value(Text { text })
            }
            method::MEMORY_TRANSCRIPT => {
                let t: Transcript = args(params)?;
                let store = self.store.lock().unwrap();
                let s = find(&store, &t.key)?;
                let text = sill_memory::transcript(store.conn(), &s, t.grep.as_deref()).map_err(Error::failed)?;
                to_value(Text { text })
            }
            method::SETTINGS_GET => to_value(sill_core::config::load()),
            _ => Err(Error::method_not_found(m)),
        }
    }

    fn status(&self) -> Result<DaemonStatus, Error> {
        let sessions = self.sessions()?;
        Ok(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: sill_proto::PROTOCOL,
            pid: std::process::id(),
            started: self.started,
            mode: self.mode.to_string(),
            launcher: self.launcher.describe(),
            limits: self.launcher.has_limits(),
            journal: self.launcher.journal,
            socket: sill_proto::socket_path().map(|p| p.display().to_string()).unwrap_or_default(),
            running: sessions.iter().filter(|s| s.running).count(),
            sessions: sessions.len(),
        })
    }

    fn sessions(&self) -> Result<Vec<Session>, Error> {
        let store = self.store.lock().unwrap();
        let rows = store.sessions().map_err(Error::failed)?;
        rows.into_iter()
            .map(|s| {
                let pins = store.pins(&s.id).unwrap_or_default();
                // an ended session has no jail by definition; the probe is a socket connect
                let running = s.ended_at.is_none() && sill_core::session::is_running(&s.id);
                Ok(Session {
                    id: s.id,
                    agent: s.agent,
                    title: s.title,
                    title_kind: s.title_kind,
                    created_at: s.created_at,
                    last_used: s.last_used,
                    policy: s.policy,
                    auto_allow: s.auto_allow,
                    desktop: s.desktop,
                    ended_at: s.ended_at,
                    summary: s.summary,
                    summary_kind: s.summary_kind,
                    memory_scope: s.memory_scope,
                    pins,
                    running,
                    ghost: s.ghost,
                })
            })
            .collect()
    }

    /// Attach `conn` to the session's conversation, starting jail, agent and ACP client when there
    /// is none (or `fresh`: the agent started again, for adapters that refuse a second `initialize`).
    fn attach(self: &Arc<Self>, conn: &Arc<Conn>, id: &str, fresh: bool) -> Result<()> {
        let _one_at_a_time = self.starting.lock().unwrap();
        // the guard goes before `presence` locks the runners again
        let running = self.runners.lock().unwrap().get(id).filter(|r| r.alive()).cloned();
        if !fresh && let Some(r) = running {
            r.attach(conn.clone());
            self.presence(id);
            return Ok(());
        }
        let started = self.start_session(id)?;
        let (s, agent) = {
            let store = self.store.lock().unwrap();
            let Some(s) = store.sessions()?.into_iter().find(|x| x.id == id) else { bail!("no session {id}") };
            let registry = sill_core::agents::load(&self.home);
            let Some(agent) = registry.get(&s.agent).cloned() else { bail!("unknown agent {}", s.agent) };
            (s, agent)
        };
        let mut control = sill_core::shim::Control::connect(&started.socket)?;
        if fresh && control.status()?.running {
            control.kill(3000)?;
        }
        if !control.status()?.running {
            let mut env = agent.env.clone();
            env.extend(agent.sandbox_off_env.clone());
            control.spawn(&agent.command, &env, Some(&self.home))?;
        }
        let stream = sill_core::shim::agent_stream(&started.socket)?;
        // a control channel: a reconnect re-applies the remembered settings as one command each, and
        // none of them may be dropped for want of room
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let runner = Runner::new(id, cmd_tx, conn.clone());
        if let Some(old) = self.runners.lock().unwrap().insert(id.to_string(), runner.clone()) {
            old.retire();
        }
        let sopts = sill_core::acp::SessionOptions {
            cwd: self.home.clone(),
            auto: sill_core::acp::AutoPermission::Ask,
            resume: if agent.supports_load { s.acp_session_id.clone() } else { None },
        };
        std::thread::Builder::new().name(format!("sill-acp-{id}")).spawn(move || {
            // run_session sends Event::Ended itself, error included
            let _ = sill_core::acp::run_session(stream, sopts, cmd_rx, ev_tx);
        })?;
        let hub = self.clone();
        std::thread::Builder::new().name(format!("sill-conv-{id}")).spawn(move || runner.run(hub, ev_rx))?;
        eprintln!("sill daemon: session {id} conversation started{}", if fresh { " (agent restarted)" } else { "" });
        self.presence(id);
        Ok(())
    }

    /// A store write on a session that exists (the id checked first: a write to a missing row
    /// would silently touch nothing).
    fn with_session<T>(&self, id: &str, f: impl FnOnce(&Store, &str) -> Result<T>) -> Result<T> {
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let store = self.store.lock().unwrap();
        if !store.sessions()?.iter().any(|s| s.id == id) {
            bail!("no session {id}");
        }
        f(&store, id)
    }

    /// A new session row (what `Store::create` and the shelf's `create_session` / `new_here` did).
    fn new_session(&self, n: NewSession) -> Result<String> {
        if n.ghost && !n.pins.is_empty() {
            bail!("{GHOST_NO_PINS}");
        }
        let store = self.store.lock().unwrap();
        let s = store.create_with(&n.agent, &n.policy, n.ghost)?;
        let pins = if n.ghost {
            vec![]
        } else if n.pins.is_empty() && n.remember_pins {
            // the pins of the latest session that had any (the sessions come newest first)
            store.sessions()?.into_iter().filter(|x| x.id != s.id).find_map(|x| store.pins(&x.id).ok().filter(|p| !p.is_empty())).unwrap_or_default()
        } else {
            n.pins
        };
        for p in &pins {
            store.pin(&s.id, p)?;
        }
        if !n.desktop.is_empty() {
            store.set_desktop(&s.id, &n.desktop)?;
        }
        if !n.memory_scope.is_empty() {
            store.set_memory_scope(&s.id, &n.memory_scope)?;
        }
        if !n.follows.is_empty() {
            store.set_follows(&s.id, &n.follows)?;
        }
        Ok(s.id)
    }

    fn set_session(&self, id: &str, set: sill_core::wire::SessionSet) -> Result<()> {
        use sill_core::wire::SessionSet as S;
        if matches!(set, S::Summary { .. }) && self.is_ghost(id) {
            bail!("a ghost session is not remembered: it has no summary");
        }
        let summary = matches!(set, S::Summary { .. } | S::Title { .. });
        self.with_session(id, |s, id| match set {
            S::Title { title, kind } => s.set_title(id, &title, &kind),
            S::AutoAllow(on) => s.set_auto_allow(id, on),
            S::Desktop(d) => s.set_desktop(id, &d),
            S::MemoryScope(m) => s.set_memory_scope(id, &m),
            S::Policy(p) => s.set_policy(id, &p),
            S::Summary { text, kind } => s.set_summary(id, &text, &kind),
            S::Settings(a) => s.set_session_settings(id, &a),
            S::Reopen => s.reopen(id),
        })?;
        if summary {
            refresh_memory();
        }
        Ok(())
    }

    /// The trash can: the conversation and jail go, then the session's state on the host and its
    /// row; pins of deleted sessions are pruned so they do not haunt the tree.
    fn delete_session(&self, id: &str) -> Result<()> {
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let ghost = self.is_ghost(id);
        // the history (SPEC §3.6): the pinned repositories' range and the deletion itself, before
        // the row goes; a ghost leaves neither
        if !ghost && let Some(s) = self.with_store(|st| st.sessions()).ok().and_then(|v| v.into_iter().find(|s| s.id == id)) {
            let e = sill_core::history::event(&s, sill_core::history::Kind::Deleted, std::path::Path::new(""));
            if let Err(err) = self.with_store(|st| {
                sill_core::history::close_pins(st, &s)?;
                sill_core::history::record(st, &e)
            }) {
                eprintln!("sill daemon: history: {err:#}");
            }
        }
        self.retire_runner(id);
        self.ending.lock().unwrap().remove(id);
        if let Err(e) = sill_core::session::stop(id) {
            eprintln!("sill daemon: stopping session {id} for deletion: {e:#}");
        }
        let uid = rustix::process::getuid().as_raw();
        if let Err(e) = sill_core::session::remove_state(uid, id) {
            eprintln!("sill daemon: removing session {id}'s state: {e:#}");
        }
        self.with_store(|s| {
            // a ghost's rows are overwritten as they go and the file is rewritten without them,
            // its WAL emptied: the depth docs/memory.md §3.4 promises, no more
            if ghost {
                s.conn().execute_batch("PRAGMA secure_delete = ON")?;
            }
            let r = s.delete(id).and_then(|_| s.prune_pins().map(|_| ()));
            if ghost {
                let _ = s.conn().execute_batch("PRAGMA secure_delete = OFF");
                if r.is_ok() {
                    s.vacuum()?;
                    s.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                }
            }
            r
        })?;
        // its runtime dir goes too: the jail's log (a ghost's), and `denied`, the hosts its proxy
        // refused — a trace of what the session tried
        if let Ok(rt) = sill_core::shim::runtime_dir(id) {
            let _ = std::fs::remove_dir_all(rt);
        }
        refresh_memory();
        eprintln!("sill daemon: session {id} deleted");
        Ok(())
    }

    /// A ghost's hour (docs/memory.md §3.2), from the minute sweep: in every running ghost that is
    /// not mid-turn, each write whose last change is an hour old reverts through the jail's shim —
    /// repositories, build output and conflicts too (`changes::ghost_due`). The attached frontends
    /// get a note saying how many went.
    fn revert_ghosts(&self) {
        let after = std::time::Duration::from_secs(sill_core::policy::GHOST_IDLE_MINUTES as u64 * 60);
        let ghosts: Vec<sill_core::store::Session> =
            self.with_store(|s| s.sessions()).unwrap_or_default().into_iter().filter(|s| s.ghost && s.ended_at.is_none()).collect();
        let uid = rustix::process::getuid().as_raw();
        let user = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
        for s in ghosts {
            let runner = self.runners.lock().unwrap().get(&s.id).filter(|r| r.alive()).cloned();
            if runner.as_ref().is_some_and(|r| r.busy()) || !sill_core::session::is_running(&s.id) {
                continue;
            }
            let due = match self.session_changes(&s, uid, &user) {
                Ok(changes) => sill_core::changes::ghost_due(&changes, std::time::SystemTime::now(), after),
                Err(e) => {
                    eprintln!("sill daemon: ghost {}: scanning its writes: {e:#}", s.id);
                    continue;
                }
            };
            if due.is_empty() {
                continue;
            }
            let Ok(sock) = sill_core::session::socket_path(&s.id) else { continue };
            let mut control = match sill_core::shim::Control::connect(&sock) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("sill daemon: ghost {}: {e:#}", s.id);
                    continue;
                }
            };
            let mut n = 0;
            for c in &due {
                match control.revert(&c.path) {
                    Ok(_) => n += 1,
                    Err(e) => eprintln!("sill daemon: ghost {}: reverting {}: {e:#}", s.id, c.path.display()),
                }
            }
            eprintln!("sill daemon: ghost {}: reverted {n} of {} path(s) an hour after their last write", s.id, due.len());
            if n > 0 && let Some(r) = runner {
                r.notify(&sill_core::wire::Event::Note(format!("ghost: {n} path(s) reverted, an hour after their last write")));
            }
        }
    }

    /// A session's writes as the Changes page lists them: its upper trees against its policy (a
    /// ghost's tightened).
    fn session_changes(&self, s: &sill_core::store::Session, uid: u32, user: &str) -> Result<Vec<sill_core::ledger::Change>> {
        let policy = sill_core::session::policy_of(s, &self.home)?;
        let registry = sill_core::agents::load(&self.home);
        let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
        let sdir = sill_core::session::state_dir(uid, &s.id)?;
        if !sdir.is_dir() {
            return Ok(vec![]);
        }
        let pins = self.with_store(|st| st.pins(&s.id)).unwrap_or_default();
        let binds = sill_core::session::binds_for(agent, &policy, &sdir, &self.home, &pins, s.ghost)?;
        let plan = sill_core::changes::session_plan(policy.clone(), sdir, &self.home, uid, user, binds)?;
        sill_core::changes::scan(&policy, &plan, None)
    }

    /// The row says ghost (a missing row: no).
    fn is_ghost(&self, id: &str) -> bool {
        self.with_store(|s| s.sessions()).ok().and_then(|v| v.into_iter().find(|s| s.id == id)).is_some_and(|s| s.ghost)
    }

    /// At start, after adoption (docs/memory.md §3.3, docs/daemon.md §3.6): a ghost whose jail is
    /// not running ended with a crash or a logout, and one whose end was recorded but not finished
    /// (the last daemon left during the deletion) — both are deleted now, jail and all, as their end
    /// would have. A ghost still running and never ended was adopted and stays.
    fn sweep_dead_ghosts(&self) {
        let ghosts: Vec<(String, bool)> =
            self.with_store(|s| s.sessions()).unwrap_or_default().into_iter().filter(|s| s.ghost).map(|s| (s.id, s.ended_at.is_some())).collect();
        for (id, ended) in ghosts {
            let running = self.children.lock().unwrap().contains_key(&id) || sill_core::session::is_running(&id);
            if running && !ended {
                continue;
            }
            eprintln!("sill daemon: ghost session {id} {}: deleting it", if ended { "ended before its deletion was done" } else { "has no jail any more" });
            if let Err(e) = self.delete_session(&id) {
                eprintln!("sill daemon: deleting ghost {id}: {e:#}");
            }
        }
    }

    /// At start: runtime dirs of sessions that have no row any more and no live launcher — left by
    /// deletions before the runtime dir went with them — are removed.
    fn sweep_orphan_runtime(&self) {
        let Ok(dir) = sill_proto::runtime_dir() else { return };
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let rows: Vec<String> = self.with_store(|s| s.sessions()).unwrap_or_default().into_iter().map(|s| s.id).collect();
        for e in rd.flatten() {
            let Some(id) = e.file_name().to_str().map(str::to_string) else { continue };
            if id == "dev" || !sill_core::session::valid_id(&id) || !e.path().is_dir() || rows.contains(&id) {
                continue;
            }
            if sill_core::launcher::pid_file_alive(&e.path().join("launch.pid")) || self.children.lock().unwrap().contains_key(&id) {
                continue;
            }
            if std::fs::remove_dir_all(e.path()).is_ok() {
                eprintln!("sill daemon: removed the runtime dir of deleted session {id}");
            }
        }
    }

    /// The session's conversation is over as far as the daemon is concerned: no more events out.
    fn retire_runner(&self, id: &str) {
        if let Some(r) = self.runners.lock().unwrap().remove(id) {
            r.retire();
        }
    }

    /// The session ends as closing its tab does (docs/memory.md §2.2). With memory on, its agent —
    /// when it is up, idle and saw the conversation — writes the summary as its last turn and the
    /// end follows the answer (`pending`); without the agent, sill's own summary from the facts and
    /// the end at once. A reopened session closed without a turn keeps the summary it had
    /// (decision 13). With memory off only `ended_at` is set. A session without a single prompt is
    /// deleted instead: there is nothing to remember. Asked again while the agent writes: still
    /// pending. Asked during a checkpoint: that checkpoint becomes the end's summary.
    fn end_session(self: &Arc<Self>, id: &str, reason: &str) -> Result<Ending> {
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let runner = self.runners.lock().unwrap().get(id).filter(|r| r.alive()).cloned();
        if let Some(r) = &runner {
            match r.summarizing() {
                Some(SummaryKind::End) => return Ok(Ending { pending: true, deleted: false }),
                Some(SummaryKind::Checkpoint) => {
                    let mut ctx = self.end_ctx(id)?;
                    ctx.reason = reason.to_string();
                    if r.adopt_checkpoint() {
                        self.ending.lock().unwrap().insert(id.to_string(), ctx);
                        eprintln!("sill daemon: session {id} ends ({reason}): its checkpoint becomes the summary");
                        return Ok(Ending { pending: true, deleted: false });
                    }
                }
                None => {}
            }
        }
        // a ghost leaves nothing (docs/memory.md §3.2): no summary turn, the row and its state go
        if self.is_ghost(id) {
            eprintln!("sill daemon: ghost session {id} ends ({reason}): deleting it");
            self.finish_end(id, None, true, reason);
            return Ok(Ending { pending: false, deleted: true });
        }
        let mut ctx = self.end_ctx(id)?;
        ctx.reason = reason.to_string();
        // nothing was asked: nothing to remember (docs/memory.md §2.1), the row would only crowd
        // every search and recall with "0 prompts"
        if ctx.facts.prompts == 0 {
            eprintln!("sill daemon: session {id} ends ({reason}): nothing was asked, deleting it");
            self.finish_end(id, None, true, reason);
            return Ok(Ending { pending: false, deleted: true });
        }
        let saw = runner.as_ref().is_some_and(|r| r.agent_saw_session());
        ctx.keep_old = ctx.keep_old && !saw;
        let said = self.with_store(|s| s.count_kind(id, "agent")).unwrap_or(0) > 0;
        if let Some(r) = runner
            && ctx.memory
            && !ctx.keep_old
            && said
            && saw
            && r.agent_idle()
            && agent_process_up(id)
        {
            let prompt = sill_core::memory::summary_prompt(&ctx.facts, &self.home);
            match r.ask_summary(self, &prompt, "the session ends: the agent writes its summary for memory", SummaryKind::End) {
                Ok(()) => {
                    self.ending.lock().unwrap().insert(id.to_string(), ctx);
                    eprintln!("sill daemon: session {id} ends ({reason}): the agent writes its summary");
                    return Ok(Ending { pending: true, deleted: false });
                }
                Err(e) => eprintln!("sill daemon: session {id}: the agent could not be asked for a summary ({e:#})"),
            }
        }
        eprintln!("sill daemon: session {id} ends ({reason})");
        self.finish_end(id, Some((ctx, None)), false, reason);
        Ok(Ending { pending: false, deleted: false })
    }

    fn end_ctx(&self, id: &str) -> Result<EndCtx> {
        let store = self.store.lock().unwrap();
        let Some(s) = store.sessions()?.into_iter().find(|x| x.id == id) else { bail!("no session {id}") };
        let uid = rustix::process::getuid().as_raw();
        let kept_dir = sill_core::session::state_dir(uid, id).map(|d| d.join("kept")).unwrap_or_default();
        let facts = sill_core::memory::collect(&store, &s, &kept_dir);
        Ok(EndCtx { facts, memory: sill_core::config::load().memory(), keep_old: !s.summary.is_empty(), reason: String::new() })
    }

    /// *Summarize now* (docs/memory.md §2.5): the summary turn, the session going on; the answer
    /// is stored and comes to the attached frontends as `Summarized`.
    fn checkpoint(self: &Arc<Self>, id: &str) -> Result<()> {
        if self.is_ghost(id) {
            bail!("a ghost session is not remembered: there is nothing to summarize for");
        }
        let r = self.runner(id).map_err(|e| anyhow::anyhow!("{}", e.message))?;
        let ctx = self.end_ctx(id)?;
        let prompt = sill_core::memory::checkpoint_prompt(&ctx.facts, &self.home);
        r.ask_summary(self, &prompt, "summarize now: the agent writes a checkpoint summary for memory", SummaryKind::Checkpoint)
    }

    /// A summary turn is over (the runner's lock is not held): the end completes, or the
    /// checkpoint is stored and announced.
    pub fn summary_done(self: &Arc<Self>, id: &str, done: SummaryDone) {
        let answer = done.text.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
        match done.kind {
            SummaryKind::End => {
                let ctx = self.ending.lock().unwrap().remove(id);
                let Some(ctx) = ctx else {
                    eprintln!("sill daemon: session {id}: a summary came in with no end waiting for it");
                    return;
                };
                let ctx_reason = ctx.reason.clone();
                self.finish_end(id, Some((ctx, answer)), false, &ctx_reason);
            }
            SummaryKind::Checkpoint => {
                let mut title = None;
                let text = answer.map(|a| {
                    let (t, body) = sill_core::memory::split_title(&a);
                    title = t;
                    if body.is_empty() { a } else { body }
                });
                if let Some(text) = &text {
                    self.with_store(|s| {
                        if let Some(t) = &title {
                            s.set_title(id, t, "agent")?;
                        }
                        s.set_summary(id, text, "agent")
                    })
                    .ok();
                    refresh_memory();
                }
                if let Some(r) = self.runners.lock().unwrap().get(id) {
                    r.notify(&sill_core::wire::Event::Summarized { text, title });
                }
            }
        }
    }

    /// Record the end — the row written from `ctx` and the agent's `answer`, or deleted — tell
    /// the attached frontends, and stop the jail on a thread of its own (a stop can take seconds).
    fn finish_end(self: &Arc<Self>, id: &str, ctx: Option<(EndCtx, Option<String>)>, delete: bool, reason: &str) {
        // a deletion takes seconds (the jail's stop) and this daemon may be gone before it is done
        // (`sill restart`): the end is on the row at once, and the next daemon's start finishes it
        // for a ghost (`sweep_dead_ghosts`) as the nightly sweep does for an unprompted session
        if delete {
            self.with_store(|s| s.end_only(id)).ok();
        }
        if !delete && let Some(s) = self.with_store(|st| st.sessions()).ok().and_then(|v| v.into_iter().find(|s| s.id == id)) {
            // the pinned repositories' commits in this open period (SPEC §3.6 decision 2)
            if let Err(err) = self.with_store(|st| sill_core::history::close_pins(st, &s)) {
                eprintln!("sill daemon: history: {err:#}");
            }
        }
        if !delete && let Some((ctx, answer)) = ctx {
            self.with_store(|s| match answer {
                _ if !ctx.memory || ctx.keep_old => s.end_only(id),
                Some(a) => {
                    let (title, body) = sill_core::memory::split_title(&a);
                    if let Some(t) = title {
                        s.set_title(id, &t, "agent")?;
                    }
                    s.end(id, if body.is_empty() { &a } else { &body }, "agent")
                }
                None => s.end(id, &sill_core::memory::auto_summary(&ctx.facts, sill_core::store::now(), &self.home), "auto"),
            })
            .ok();
        }
        if let Some(r) = self.runners.lock().unwrap().get(id) {
            r.notify(&sill_core::wire::Event::SessionEnded { deleted: delete, reason: reason.to_string() });
        }
        self.retire_runner(id);
        self.ending.lock().unwrap().remove(id);
        let hub = self.clone();
        let id = id.to_string();
        let _ = std::thread::Builder::new().name(format!("sill-end-{id}")).spawn(move || {
            if delete {
                // the trash can's path: jail, state, rows — a ghost's scrubbed, its log removed
                if let Err(e) = hub.delete_session(&id) {
                    eprintln!("sill daemon: deleting session {id}: {e:#}");
                }
                return;
            }
            if let Err(e) = sill_core::session::stop(&id) {
                eprintln!("sill daemon: stopping session {id}: {e:#}");
            }
            refresh_memory();
        });
    }

    /// Start a stored session's jail from its row — pins, policy, desktop grants, the agent's binds
    /// — or find it running (what `shelf/session.rs` built in-process before the daemon).
    fn start_session(&self, id: &str) -> Result<Started> {
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let (s, pins) = {
            let store = self.store.lock().unwrap();
            let Some(s) = store.sessions()?.into_iter().find(|x| x.id == id) else { bail!("no session {id}") };
            let pins = store.pins(id)?;
            (s, pins)
        };
        let registry = sill_core::agents::load(&self.home);
        let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
        let uid = rustix::process::getuid().as_raw();
        let sdir = sill_core::session::state_dir(uid, id)?;
        let policy = Policy::load(&s.policy, &self.home)?;
        let binds = sill_core::session::binds_for(agent, &policy, &sdir, &self.home, &pins, s.ghost)?;
        let cfg = sill_core::config::load();
        let (x11, dbus) = sill_core::config::desktop_grants(&s.desktop, &cfg);
        let opts = StartOptions {
            session: id.to_string(),
            policy: s.policy.clone(),
            cwd: self.home.clone(),
            state_dir: Some(sdir),
            unsafe_state: false,
            share_net: false,
            env: vec![],
            shim_bin: None,
            agent: Some(s.agent.clone()),
            binds,
            x11,
            dbus,
            ghost: s.ghost,
        };
        let started = sill_core::session::ensure_started(&opts, &self.home, &self.exe).context("starting the jail")?;
        // the pinned repositories' HEAD when this open period began (kept across restarts)
        if let Err(e) = self.with_store(|st| sill_core::history::note_pin_starts(st, &s)) {
            eprintln!("sill daemon: session {id}: noting the pins' HEAD: {e:#}");
        }
        let fresh = started.child.is_some();
        if let Some(child) = started.child {
            self.supervise(id, child);
        }
        Ok(Started { socket: started.socket, fresh })
    }

    /// Wait on a child we started; when it ends, note it and drop it from the table.
    fn supervise(&self, id: &str, mut child: std::process::Child) {
        let pid = child.id();
        self.children.lock().unwrap().insert(id.to_string(), pid);
        self.started_at.lock().unwrap().insert(id.to_string(), sill_core::store::now());
        eprintln!("sill daemon: session {id} started (launch pid {pid})");
        let id = id.to_string();
        let children = self.children.clone();
        let _ = std::thread::Builder::new().name(format!("sill-wait-{id}")).spawn(move || {
            let st = child.wait();
            eprintln!("sill daemon: session {id} ended ({})", st.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()));
            forget(&children, &id, pid);
        });
    }

    /// Sessions that were running before this daemon started (a restart, docs/daemon.md §3.1):
    /// their `launch.pid` is alive — watch each with a pidfd; dead ones get their files swept.
    fn adopt_running(&self) {
        let Ok(dir) = sill_proto::runtime_dir() else { return };
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let Some(id) = e.file_name().to_str().map(str::to_string) else { continue };
            if !sill_core::session::valid_id(&id) || !e.path().is_dir() {
                continue;
            }
            let pid_file = e.path().join("launch.pid");
            let pid = std::fs::read_to_string(&pid_file).ok().and_then(|t| t.trim().parse::<u32>().ok());
            match pid {
                Some(pid) if sill_core::launcher::pid_file_alive(&pid_file) => {
                    self.children.lock().unwrap().insert(id.clone(), pid);
                    self.started_at.lock().unwrap().insert(id.clone(), sill_core::store::now());
                    eprintln!("sill daemon: adopted session {id} (launch pid {pid})");
                    let children = self.children.clone();
                    let _ = std::thread::Builder::new().name(format!("sill-wait-{id}")).spawn(move || {
                        if let Err(e) = sill_core::launcher::wait_pid_gone(pid) {
                            eprintln!("sill daemon: watching session {id}: {e:#}");
                        }
                        eprintln!("sill daemon: adopted session {id} ended");
                        forget(&children, &id, pid);
                    });
                }
                Some(_) => {
                    // a jail that died with nobody watching: its sockets and pids are stale
                    for f in ["bwrap.pid", "launch.pid", "shim.sock", "proxy.sock"] {
                        let _ = std::fs::remove_file(e.path().join(f));
                    }
                }
                None => {}
            }
        }
    }
}

/// The shim still has an agent process: the relay stays up when the shim stops an idle agent, so
/// the runner alone cannot tell a parked agent from a live one, and a summary asked of a stopped
/// one would only come back as `Ended`.
fn agent_process_up(id: &str) -> bool {
    let Ok(sock) = sill_core::session::socket_path(id) else { return false };
    sill_core::shim::Control::connect(&sock).and_then(|mut c| c.status()).map(|s| s.running).unwrap_or(false)
}

/// What a pin on a ghost session gets (decision 8: a live folder is an unrecorded host write).
const GHOST_NO_PINS: &str = "a ghost session has no pins: a live folder would write the host unrecorded, and a ghost's writes all revert";

/// The jail's read-only memory snapshot follows the store (docs/memory.md §2.3).
fn refresh_memory() {
    if let Err(e) = sill_core::memory::refresh_snapshot() {
        eprintln!("sill daemon: memory snapshot: {e:#}");
    }
}

/// Drop a session from the table when the process that ended is still the one recorded (a quick
/// restart may already have put the next jail's pid there).
fn forget(children: &Mutex<HashMap<String, u32>>, id: &str, pid: u32) {
    let mut c = children.lock().unwrap();
    if c.get(id) == Some(&pid) {
        c.remove(id);
    }
}

fn find(store: &Store, key: &str) -> Result<sill_memory::SessionInfo, Error> {
    sill_memory::find_session(store.conn(), key).map_err(Error::failed)?.ok_or_else(|| Error::failed(format!("no session {key}")))
}

fn args<T: DeserializeOwned>(v: Value) -> Result<T, Error> {
    serde_json::from_value(if v.is_null() { json!({}) } else { v }).map_err(Error::invalid_params)
}

fn to_value<T: serde::Serialize>(t: T) -> Result<Value, Error> {
    serde_json::to_value(t).map_err(|e| Error::new(Error::INTERNAL, e.to_string()))
}
