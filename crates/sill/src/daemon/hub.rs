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
use sill_proto::{DaemonStatus, Ending, Started, method};

use super::Conn;

/// A session row to create: `_sill/sessions.create`'s input with its pins as paths. `pins` are
/// copied as given; with `remember_pins` and none given, use the latest session's pins.
pub(crate) struct NewSession {
    pub agent: String,
    pub policy: String,
    pub pins: Vec<PathBuf>,
    pub memory_scope: String,
    /// The ended session this one follows (*New here*).
    pub follows: String,
    pub remember_pins: bool,
    /// A ghost session (docs/memory.md §3): no pins (refused when given), no memory written, the
    /// row and every trace deleted at its end.
    pub ghost: bool,
}
use super::runner::{Runner, SummaryDone, SummaryKind};

// the Changes page's methods: a child module, so it reaches the hub's own fields
#[path = "changes.rs"]
mod changes;
#[path = "changes_public.rs"]
mod changes_public;
#[path = "sessions.rs"]
mod sessions;
#[path = "pins.rs"]
mod pins;
#[path = "workspaces.rs"]
mod workspaces;
#[path = "crosscheck.rs"]
mod crosscheck;

/// What an end that waits for the agent's summary needs once the answer is in (the facts as they
/// were when the end was asked for; the row's summary is replaced only when memory is on).
#[derive(serde::Serialize, serde::Deserialize)]
struct EndCtx {
    facts: sill_core::memory::Facts,
    memory: bool,
    /// The old summary stays: memory is off, or a reopened session was closed without a turn.
    keep_old: bool,
    /// Why the end was asked for (`close`, `idle`, `quit`): the `SessionEnded` event carries it.
    reason: String,
}

pub struct Hub {
    pub(super) store: Mutex<Store>,
    pub(super) credentials: Mutex<sill_core::credentials::Sync>,
    pub(super) sfp: Mutex<super::sfp::Subscriptions>,
    started: i64,
    mode: &'static str,
    launcher: &'static Launcher,
    /// The copy of this binary that sessions are started from (`session::staged_exe`).
    exe: PathBuf,
    pub(super) home: PathBuf,
    /// Session id → pid of its `sill launch`, for every jail this daemon started or adopted; the
    /// wait threads take an entry out when its process ends.
    children: Arc<Mutex<HashMap<String, u32>>>,
    /// Session id → the conversation the daemon runs for it (docs/daemon.md §3.3).
    // Release this registry lock before locking runner state: prompt dispatch reads membership
    // while holding its own state lock, and reads peer status from the durable store.
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    /// Starting a conversation (jail, agent, ACP) is one at a time: two attaches of one session
    /// must not start two agents.
    starting: Mutex<()>,
    /// Sessions whose end waits for the agent's summary turn (`sessions.end`).
    ending: Mutex<HashMap<String, EndCtx>>,
    /// Every open frontend connection, for what is everyone's news (`changes.changed`).
    conns: Mutex<Vec<std::sync::Weak<Conn>>>,
    /// When each running jail was started or adopted by this daemon: the idle timer counts from
    /// the latest of this, the last prompt and the last turn's end, so a session just resumed is
    /// not ended a minute in.
    started_at: Mutex<HashMap<String, i64>>,
    /// When the minute sweep last finished, or the daemon started: the watchdog's proof that the
    /// sweep — idle ends, ghost reverts, the stray reaper, the nightly run — still runs. It holds
    /// no lock while it works, so a step that hangs was invisible to the lock probe.
    swept: Mutex<std::time::Instant>,
    /// Jails being rebuilt for a pin change, one at a time per session.
    rebuilds: Rebuilds,
    crosscheck_lock: Mutex<()>,
}

/// Rebuilds of a session's jail after its pins changed: at most one runs per session, and a pin
/// change while one runs asks for a single one more, which reads the pins as they are by then.
/// However many changes come, the last rebuild sees the last pins.
#[derive(Default)]
pub(super) struct Rebuilds(Mutex<HashMap<String, bool>>);

impl Rebuilds {
    /// A rebuild of `id` is wanted. True: the caller runs it; false: one runs and will go again.
    pub(super) fn begin(&self, id: &str) -> bool {
        let mut m = self.0.lock().unwrap();
        match m.get_mut(id) {
            Some(again) => {
                *again = true;
                false
            }
            None => {
                m.insert(id.into(), false);
                true
            }
        }
    }

    /// A rebuild of `id` ended. True: the pins changed while it ran, so run it once more.
    pub(super) fn again(&self, id: &str) -> bool {
        let mut m = self.0.lock().unwrap();
        if m.get(id) == Some(&true) {
            m.insert(id.into(), false);
            true
        } else {
            m.remove(id);
            false
        }
    }
}

/// Debug builds only: the next sweep hangs (tests/daemon.rs, the watchdog's sweep check).
#[cfg(debug_assertions)]
static STALL_SWEEP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Taken and released before `deadline`, polling: `std::sync::Mutex` has no timed lock.
pub(super) fn probe<T>(m: &Mutex<T>, deadline: std::time::Instant) -> bool {
    loop {
        match m.try_lock() {
            Ok(_guard) => return true,
            Err(std::sync::TryLockError::Poisoned(_)) => return true, // the panic hook already left
            Err(std::sync::TryLockError::WouldBlock) if std::time::Instant::now() >= deadline => return false,
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

impl Hub {
    pub fn new(mode: &'static str) -> Result<Hub> {
        let store = Store::open_default()?;
        sill_core::operations::Journal::new(store.conn())?.recover()?;
        sill_core::workspace::records::init(store.conn())?;
        crosscheck::init(store.conn())?;
        store.conn().execute_batch("CREATE TABLE IF NOT EXISTS daemon_ends (id TEXT PRIMARY KEY, context TEXT NOT NULL); CREATE TABLE IF NOT EXISTS daemon_runners (id TEXT PRIMARY KEY, state TEXT NOT NULL);
            CREATE TRIGGER IF NOT EXISTS daemon_state_cleanup AFTER DELETE ON sessions BEGIN
                DELETE FROM daemon_runners WHERE id=OLD.id;
                DELETE FROM daemon_ends WHERE id=OLD.id;
            END")?;
        let ending = {
            let mut stmt = store.conn().prepare("SELECT id,context FROM daemon_ends")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut ending = HashMap::new();
            for row in rows { let (id, ctx) = row?; ending.insert(id, serde_json::from_str(&ctx)?); }
            ending
        };
        let launcher = sill_core::launcher::current();
        eprintln!("sill daemon: {}", launcher.describe());
        let hub = Hub {
            store: Mutex::new(store),
            sfp: Mutex::new(Default::default()),
            started: sill_core::store::now(),
            mode,
            launcher,
            exe: sill_core::session::staged_exe()?,
            home: sill_core::changes::home()?,
            credentials: Mutex::default(),
            children: Arc::default(),
            runners: Mutex::new(HashMap::new()),
            starting: Mutex::new(()),
            ending: Mutex::new(ending),
            started_at: Mutex::new(HashMap::new()),
            conns: Mutex::new(Vec::new()),
            swept: Mutex::new(std::time::Instant::now()),
            rebuilds: Rebuilds::default(),
            crosscheck_lock: Mutex::new(()),
        };
        hub.adopt_running();
        hub.sweep_dead_ghosts();
        hub.sweep_orphan_runtime();
        Ok(hub)
    }

    /// Called outside store/runner locks; only credential files cross the ghost boundary.
    pub(super) fn sync_credentials(&self) -> Result<()> {
        self.sync_credentials_for(None)
    }

    pub(super) fn sync_credentials_for(&self, session: Option<&str>) -> Result<()> {
        let mut sync = self.credentials.lock().unwrap();
        let rows = self.with_store(|s| s.sessions())?;
        let target = session.and_then(|id| rows.iter().find(|s| s.id == id)).map(|s| s.agent.as_str());
        let mut groups: Vec<(sill_core::credentials::Rules, PathBuf, Vec<PathBuf>)> = Vec::new();
        for row in rows.iter().filter(|r| r.ghost && (session.is_none() || target == Some(r.agent.as_str()))) {
            // Missing launch evidence is never reconstructed from today's definitions.
            let Ok(plan) = sill_core::session::recorded_filesystem(&row.id) else { continue };
            let profile = plan.profile.to_path()?;
            if !profile.is_dir() { continue; }
            let base = plan.normal_profile.to_path()?;
            if let Some((_, _, ghosts)) = groups.iter_mut().find(|(rules, normal, _)| *rules == plan.credentials && *normal == base) {
                ghosts.push(profile);
            } else { groups.push((plan.credentials, base, vec![profile])); }
        }
        let mut first_error = None;
        for (rules, base, ghosts) in groups {
            if let Err(e) = sync.reconcile(&rules, &base, &ghosts) {
                if first_error.is_none() { first_error = Some(e); }
            }
        }
        match first_error { Some(e) => Err(e), None => Ok(()) }
    }

    pub fn session_count(&self) -> usize {
        self.store.lock().unwrap().sessions().map(|s| s.len()).unwrap_or(0)
    }

    /// The watchdog's probe (docs/daemon.md §3.7): every lock of the hub and of each runner is
    /// taken and released within `patience`, or the one that was not is named — a deadlock, or
    /// a request that has held a lock for far longer than any legitimate work. The order is the
    /// one the requests use (hub locks before a runner's), so the probe itself cannot deadlock.
    /// Then the minute sweep has finished within `sweep_patience`: a sweep stuck in one step (a
    /// `systemctl` that never returns, a shim that does not answer) stops every later step and
    /// holds no lock, so only its heartbeat shows it. What is wrong, as one line.
    pub fn health(&self, patience: std::time::Duration, sweep_patience: std::time::Duration) -> std::result::Result<(), String> {
        let deadline = std::time::Instant::now() + patience;
        let held = |lock: &str| format!("the {lock} lock has been held for {} s", patience.as_secs());
        probe(&self.conns, deadline).then_some(()).ok_or_else(|| held("conns"))?;
        probe(&self.children, deadline).then_some(()).ok_or_else(|| held("children"))?;
        probe(&self.ending, deadline).then_some(()).ok_or_else(|| held("ending"))?;
        probe(&self.started_at, deadline).then_some(()).ok_or_else(|| held("started_at"))?;
        probe(&self.runners, deadline).then_some(()).ok_or_else(|| held("runners"))?;
        let runners: Vec<Arc<Runner>> = self.runners.lock().unwrap().values().cloned().collect();
        for r in runners {
            r.probe(deadline).then_some(()).ok_or_else(|| held("a runner's state"))?;
        }
        probe(&self.store, deadline).then_some(()).ok_or_else(|| held("store"))?;
        // held for a whole jail start: last, with what patience is left
        probe(&self.starting, deadline).then_some(()).ok_or_else(|| held("starting"))?;
        let since = self.swept.lock().unwrap().elapsed();
        if since > sweep_patience {
            return Err(format!("the minute sweep has not finished for {} s", since.as_secs()));
        }
        Ok(())
    }

    /// Membership only: callers already hold the store and may hold their own runner lock.
    /// Never lock another runner here; its durable state supplies the status at dispatch.
    pub(super) fn running_session_ids(&self) -> Vec<String> {
        self.runners.lock().unwrap().keys().cloned().collect()
    }

    /// The minute sweep finished (its heartbeat, read by `health`).
    pub fn swept(&self) {
        *self.swept.lock().unwrap() = std::time::Instant::now();
    }

    /// Debug builds only: how tests/daemon.rs makes the watchdog find a stuck sweep.
    #[cfg(debug_assertions)]
    pub fn stall_sweep(&self) {
        STALL_SWEEP.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Debug builds only: how tests/daemon.rs makes the watchdog find a stuck lock.
    #[cfg(debug_assertions)]
    pub fn hold_runners_forever(&self) {
        let _held = self.runners.lock().unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    /// The store, for one call (the runners write the transcript through it).
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        f(&self.store.lock().unwrap())
    }

    /// The session answers permission requests itself (its auto-allow, read at the moment asked).
    pub fn auto_allow(&self, id: &str) -> bool {
        self.store.lock().unwrap().sessions().ok().and_then(|v| v.into_iter().find(|s| s.id == id)).is_some_and(|s| s.auto_allow)
    }

    /// Every open frontend connection.
    pub(super) fn connections(&self) -> Vec<Arc<Conn>> {
        self.conns.lock().unwrap().iter().filter_map(|w| w.upgrade()).collect()
    }

    pub fn connected(&self, conn: &Arc<Conn>) {
        let mut c = self.conns.lock().unwrap();
        c.retain(|w| w.strong_count() > 0);
        c.push(Arc::downgrade(conn));
    }

    /// `changes.changed` to every frontend: a sweep reverted, a keep was made (by any client), a
    /// session went or a turn ended — whatever shows the overlays rescans.
    pub fn changes_changed(&self) {
        let req = sill_proto::rpc::Request::new(None, "_sill/changes.changed", json!({}));
        let Ok(line) = serde_json::to_string(&req) else { return };
        let conns: Vec<Arc<Conn>> = self.conns.lock().unwrap().iter().filter_map(|w| w.upgrade()).collect();
        for c in conns {
            c.send(line.clone());
        }
    }

    /// A connection closed: it is subscribed nowhere any more.
    pub fn disconnect(&self, conn: u64) {
        super::memory_api::disconnect(conn);
        let public_ids=self.sfp.lock().unwrap().sessions_for(conn);
        self.sfp.lock().unwrap().disconnect(conn);
        self.conns.lock().unwrap().retain(|w| w.upgrade().is_some_and(|c| c.id != conn));
        for id in public_ids {
            self.presence(&id);
        }
        // a Host control activation nobody displays any more ends (docs/permission-modes.md)
        super::host_control::displaying_changed(self);
    }

    /// Tell the session's shim how many frontends are attached (docs/daemon.md §3.6): the agent
    /// stream is the daemon's and never drops while it lives, so the shim's agent-idle rule needs
    /// this to know nobody is watching. On a thread: the shim may be slow, the caller is a request.
    pub(super) fn presence(&self, id: &str) {
        let n = self.sfp.lock().unwrap().count(id);
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
    /// closing its tab would (`sessions.end`, reason `idle`; 0 = never). Sessions with an
    /// attached frontend are exempt, including open background tabs. Then memory's nightly run.
    pub fn sweep(self: &Arc<Self>) {
        #[cfg(debug_assertions)]
        while STALL_SWEEP.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        super::memory_api::sweep();
        self.sfp.lock().unwrap().expire();
        super::streams::reap();
        super::terminals::reap();
        super::host_control::sweep(self);
        self.reap_strays();
        let now = sill_core::store::now();
        let mut ids: Vec<String> = self.children.lock().unwrap().keys().cloned().collect();
        ids.extend(self.runners.lock().unwrap().keys().cloned());
        ids.sort();
        ids.dedup();
        let sessions = self.with_store(|s| s.sessions()).unwrap_or_default();
        for id in ids {
            // An open tab is still in use even when the person is only reading it.
            if self.sfp.lock().unwrap().count(&id) > 0 {
                continue;
            }
            let Some(s) = sessions.iter().find(|x| x.id == id) else { continue };
            if s.ended_at.is_some() || self.ending.lock().unwrap().contains_key(&id) || self.persisted_end(&id).ok().flatten().is_some() {
                continue;
            }
            let runner = self.runners.lock().unwrap().get(&id).cloned();
            let runner = runner.filter(|r| r.alive());
            if runner.as_ref().is_some_and(|r| r.busy()) {
                continue;
            }
            // no runner but a relay that answers: a conversation not yet recovered, whose turn
            // state is unknown — not idle. No runner and no relay: a parked agent, idle since the
            // last prompt, as before the relay existed.
            if runner.is_none() && self.relay_answers(&id) {
                continue;
            }
            let policy = match sill_core::session::policy_snapshot(&id) {
                Ok(Some(snapshot)) => snapshot.policy,
                _ => {
                    let Ok(mut policy) = Policy::load(&s.policy, &self.home) else { continue };
                    if s.ghost { policy.ghostly(&self.home); }
                    policy
                }
            };
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
            // a session deleted for good takes a jail that still runs with it, before its state
            // goes from under it
            if sill_proto::runtime_dir().is_ok_and(|d| launcher_alive(&d.join(id), id))
                && let Err(e) = sill_core::session::stop(id)
            {
                eprintln!("sill daemon: memory nightly: stopping session {id}: {e:#}");
            }
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

    pub(super) fn runner(&self, id: &str) -> Result<Arc<Runner>, Error> {
        let runner = self.runners.lock().unwrap().get(id).cloned();
        match runner {
            Some(r) if r.alive() => Ok(r),
            _ => Err(Error::failed("the session is not running")),
        }
    }

    pub fn dispatch(self: &Arc<Self>, conn: &Arc<Conn>, m: &str, params: Value) -> Result<Value, Error> {
        if let Some(result) = self.crosscheck(conn, m, params.clone()) {
            return result.map_err(super::sfp::failure);
        }
        if let Some(result) = self.changes_public(m, params.clone()) { return result.map_err(super::sfp::failure); }
        if let Some(result) = super::agent_admin::handle(self, m, params.clone()) { return result.map_err(super::sfp::failure); }
        if let Some(result) = super::providers::handle(m, params.clone()) { return result.map_err(super::sfp::failure); }
        if let Some(result) = super::host_control::handle(self, conn, m, params.clone()) {
            return result.map_err(|e| {
                if let Some(e) = e.downcast_ref::<Error>() { return e.clone(); }
                if e.downcast_ref::<sill_core::sfp::DomainError>().is_some() { return super::sfp::failure(e); }
                Error::domain("operation_failed", format!("{e:#}"))
            });
        }
        if let Some(result) = self.workspaces(conn, m, params.clone()) {
            return result.map_err(|e| {
                if let Some(e) = e.downcast_ref::<Error>() { return e.clone(); }
                if e.downcast_ref::<sill_core::sfp::DomainError>().is_some() { return super::sfp::failure(e); }
                Error::domain("operation_failed", format!("{e:#}"))
            });
        }
        if let Some(result) = super::admin::handle(self, conn.id, m, params.clone()) {
            return result.map_err(|e| {
                if let Some(e) = e.downcast_ref::<Error>() { return e.clone(); }
                if e.downcast_ref::<sill_core::sfp::DomainError>().is_some() { return super::sfp::failure(e); }
                Error::domain("operation_failed", format!("{e:#}"))
            });
        }
        match m {
            method::DAEMON_STATUS => to_value(self.status()?),
            method::DAEMON_SHUTDOWN => Ok(json!({})),
            method::DAEMON_SCHEMA => Ok(sill_proto::schema::document()),
            _ => Err(Error::method_not_found(m)),
        }
    }

    fn status(&self) -> Result<DaemonStatus, Error> {
        let sessions = self.store.lock().unwrap().sessions().map_err(Error::failed)?;
        Ok(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: sill_proto::initialize::PROTOCOL_ID.into(),
            pid: std::process::id(),
            started: sill_core::sfp::timestamp(self.started),
            mode: self.mode.to_string(),
            launcher: self.launcher.describe(),
            limits: self.launcher.has_limits(),
            journal: self.launcher.journal,
            socket: sill_proto::sessions::HostPath::from_path(&sill_proto::socket_path().map_err(Error::failed)?),
            running: sessions.iter().filter(|s| s.ended_at.is_none() && sill_core::session::is_running(&s.id)).count(),
            sessions: sessions.len(),
        })
    }

    /// Reconnect the ACP owner even when nobody has a window open. The relay retains the SDK's
    /// live prompt future and reply channels across the main daemon's restart.
    /// After `READY=1`, on a thread (docs/daemon.md §3.9 item 3): reconnect the conversations
    /// whose relay outlived the last daemon and still answers, and finish the ends the last daemon
    /// had recorded but not applied (`daemon_ends`: sill's summary when the relay is gone). A jail
    /// without a live relay and no end pending — its agent parked by the shim, or never started —
    /// gets its runner on the first attach or prompt, as before the relay existed: a daemon
    /// restart wakes no agent.
    pub fn recover(self: &Arc<Self>) {
        // interrupted workspace operations settle from what is on disk before anything else runs
        self.workspaces_recover();
        self.crosscheck_recover();
        // first what must not run at all, so no stray's relay is recovered below (§3.11)
        self.reap_strays();
        let Ok(dir) = sill_proto::runtime_dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut ids: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        ids.extend(self.ending.lock().unwrap().keys().cloned());
        ids.sort();
        ids.dedup();
        for id in ids {
            if !sill_core::session::valid_id(&id) {
                continue;
            }
            let socket = dir.join(&id).join("relay.sock");
            let relay_answers = socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_ok();
            let end_pending = self.ending.lock().unwrap().contains_key(&id);
            // a socket nobody answers is a relay that died: swept when the session next starts
            if !relay_answers && !end_pending {
                continue;
            }
            if self.runners.lock().unwrap().contains_key(&id) {
                continue; // a frontend attached meanwhile
            }
            match self.ensure_runner(&id, false) {
                Ok(_) => eprintln!("sill daemon: recovered session {id}'s conversation from its relay"),
                Err(e) => eprintln!("sill daemon: recovering session {id}: {e:#}; its turn state remains unknown"),
            }
        }
    }

    pub(super) fn ensure_runner(self: &Arc<Self>, id: &str, fresh: bool) -> Result<Arc<Runner>> {
        if fresh {
            let existing = self.runners.lock().unwrap().get(id).cloned();
            if let Some(r) = existing { r.retire(self)?; }
            else if sill_proto::runtime_dir()?.join(id).join("relay.sock").exists() {
                if let Ok(r) = self.ensure_runner(id, false) { r.retire(self)?; }
            }
        }
        let _one_at_a_time = self.starting.lock().unwrap();
        let running = self.runners.lock().unwrap().get(id).cloned();
        let running = running.filter(|r| r.alive());
        if !fresh && let Some(r) = running { return Ok(r); }
        if fresh { self.retire_runner(id); }
        let dir = sill_proto::runtime_dir()?.join(id);
        let socket = dir.join("relay.sock");
        let mut snapshot = self.with_store(|s| Runner::load(id, s))?;
        if fresh {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while socket.exists() {
                if std::os::unix::net::UnixStream::connect(&socket).is_err() { break; }
                if std::time::Instant::now() >= deadline { bail!("previous ACP relay has not stopped; refusing a second connection"); }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        let existing = if fresh { None } else {
            match super::relay::connect(&socket, Runner::cursor(&snapshot)) {
                Ok(connection) => Some(connection),
                // it answers but will not cooperate (another build's relay after a deploy, a
                // cursor it lost): replaced, and the conversation starts again (§3.9 item 1)
                Err(e) if e.downcast_ref::<super::relay::Refused>().is_some() => {
                    eprintln!("sill daemon: session {id}: the ACP relay refused recovery ({e:#}); replacing it");
                    super::relay::evict(&dir, id)?;
                    None
                }
                Err(e) if std::os::unix::net::UnixStream::connect(&socket).is_ok() => return Err(e.context("existing ACP relay did not accept recovery; refusing to replace it")),
                Err(_) => None,
            }
        };
        let (relay, events) = match existing {
            Some(connection) => connection,
            None => {
                // Losing the worker is a different stream epoch: the SDK future that was writing
                // an ending summary is gone. Finish the persisted intent mechanically instead of
                // silently dropping it or asking a second, unrelated summary turn.
                if let Some(ctx) = self.persisted_end(id)? {
                    let reason = ctx.reason.clone();
                    self.finish_end(id, Some((ctx, None)), false, &reason);
                    bail!("session {id} ended with sill's summary after its ACP relay was lost");
                }
                if self.with_store(|store| Ok(store.sessions()?.into_iter().any(|session| session.id == id && session.ended_at.is_some())))? {
                    bail!("session {id} has ended");
                }
                let started = self.start_session(id)?;
                let (s, agent) = self.with_store(|store| {
                    let s = store.sessions()?.into_iter().find(|s| s.id == id).context("session missing")?;
                    let agent = sill_core::agents::load(&self.home)?.get(&s.agent).cloned().context("unknown agent")?;
                    agent.ready(&self.home)?;
                    Ok((s, agent))
                })?;
                let mut control = sill_core::shim::Control::connect(&started.socket)?;
                // Without a usable relay, a surviving agent may still hold the
                // failed SDK connection. Restart it before opening a new stream.
                if control.status()?.running { control.kill(3000)?; }
                if !control.status()?.running {
                    // model providers first (docs/model-providers.md): what the person set for the
                    // agent itself wins over what sill derives
                    let mut env = sill_core::providers::agent_env(&self.home, &agent.id)?;
                    env.extend(agent.env.clone()); env.extend(agent.sandbox_off_env.clone());
                    control.spawn(&agent.command, &env, Some(&self.home))?;
                }
                // A new relay is a new stream epoch. Its cursor starts at zero; old transient
                // snapshots cannot describe its initialization, but pending end intent stays.
                self.with_store(|s| { s.conn().execute("DELETE FROM daemon_runners WHERE id=?1", [id])?; Ok(()) })?;
                snapshot = serde_json::Value::Null;
                std::fs::create_dir_all(&dir)?;
                let _ = std::fs::remove_file(&socket);
                let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("relay.log"))?;
                let mut cmd = std::process::Command::new(&self.exe);
                cmd.args(["daemon", "relay", "--session", id, "--socket"]).arg(&socket)
                    .arg("--agent-socket").arg(&started.socket).arg("--cwd").arg(&self.home)
                    .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log)
                    .env_remove("SILL_SESSION").env_remove("NOTIFY_SOCKET").env_remove("LISTEN_PID").env_remove("LISTEN_FDS");
                if agent.experimental_acp_v2 { cmd.arg("--experimental-acp-v2"); }
                if (agent.supports_load || agent.experimental_acp_v2) && let Some(resume) = s.acp_session_id { cmd.arg("--resume").arg(resume); }
                use std::os::unix::process::CommandExt;
                unsafe { cmd.pre_exec(|| { rustix::process::setsid().map_err(std::io::Error::from)?; Ok(()) }); }
                let mut child = cmd.spawn()?;
                std::thread::spawn(move || { let _ = child.wait(); });
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    match super::relay::connect(&socket, 0) {
                        Ok(connection) => break connection,
                        Err(e) if std::time::Instant::now() >= deadline => return Err(e.context("starting ACP relay")),
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                    }
                }
            }
        };
        let runner = Runner::new(id, relay, snapshot)?;
        self.runners.lock().unwrap().insert(id.into(), runner.clone());
        let hub = self.clone(); let r = runner.clone();
        std::thread::Builder::new().name(format!("sill-conv-{id}")).spawn(move || r.run(hub, events))?;
        self.presence(id);
        Ok(runner)
    }

    fn remember_end(&self, id: &str, ctx: EndCtx) -> Result<()> {
        self.with_store(|s| { s.conn().execute("INSERT INTO daemon_ends(id,context) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET context=excluded.context", rusqlite::params![id, serde_json::to_string(&ctx)?])?; Ok(()) })?;
        self.ending.lock().unwrap().insert(id.into(), ctx);
        Ok(())
    }

    fn persisted_end(&self, id: &str) -> Result<Option<EndCtx>> {
        use rusqlite::OptionalExtension;
        self.with_store(|store| {
            let context: Option<String> = store.conn().query_row("SELECT context FROM daemon_ends WHERE id=?1", [id], |row| row.get(0)).optional()?;
            context.map(|context| serde_json::from_str(&context).map_err(anyhow::Error::from)).transpose()
        })
    }

    fn create_session_row(store: &Store, n: NewSession) -> Result<String> {
        crosscheck::init(store.conn())?;
        let s = store.create_with(&n.agent, &n.policy, n.ghost)?;
        let pins = if n.ghost {
            vec![]
        } else if n.pins.is_empty() && n.remember_pins {
            // Sessions are newest first; saved shortcuts never enter this list.
            store.sessions()?.into_iter().filter(|x| x.id != s.id && !store.conn().query_row("SELECT EXISTS(SELECT 1 FROM crosscheck_workers WHERE session_id=?1)", [&x.id], |r| r.get::<_, bool>(0)).unwrap_or(true)).find_map(|x| store.pins(&x.id).ok().filter(|p| !p.is_empty())).unwrap_or_default()
        } else {
            n.pins
        };
        for p in &pins {
            store.pin(&s.id, p)?;
        }
        if !n.memory_scope.is_empty() {
            store.set_memory_scope(&s.id, &n.memory_scope)?;
        }
        if !n.follows.is_empty() {
            store.set_follows(&s.id, &n.follows)?;
        }
        Ok(s.id)
    }

    /// The trash can: the conversation and jail go, then the session's state on the host and its
    /// row (its pins with it).
    pub(super) fn delete_session(&self, id: &str) -> Result<()> {
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        super::terminals::forget_session(id);
        super::host_control::session_ended(self, id, "the session was deleted");
        let ghost = self.is_ghost(id);
        let had_runner = self.runners.lock().unwrap().contains_key(id);
        let relay_dir = sill_proto::runtime_dir()?.join(id);
        let relay_socket = relay_dir.join("relay.sock");
        // Nothing below returns before the jail's stop: a deletion that gave up half-way used to
        // leave the session's jail running for good, adopted by every later daemon
        // (docs/daemon.md §3.11). A relay that could not be stopped only keeps the runtime dir,
        // state and row — after the jail is gone.
        let mut relay = if had_runner {
            Ok(())
        } else {
            self.with_store(|store| Runner::load(id, store)).and_then(|snapshot| super::relay::stop_unowned(&relay_socket, id, Runner::cursor(&snapshot)))
        };
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
        if had_runner {
            // the retired runner's Quit ends the relay; one that does not leave goes the hard way
            relay = super::relay::wait_stopped(&relay_socket).or_else(|e| {
                eprintln!("sill daemon: session {id}: {e:#}; evicting the ACP relay");
                super::relay::evict(&relay_dir, id)
            });
        }
        self.ending.lock().unwrap().remove(id);
        if let Err(e) = sill_core::session::stop(id) {
            eprintln!("sill daemon: stopping session {id} for deletion: {e:#}");
        }
        if let Err(e) = relay
            && self.relay_answers(id)
        {
            // the jail is stopped; what stays is the relay's journal and the session's files, which
            // must not be scrubbed under a worker that still answers. With its agent gone the
            // relay leaves by itself (relay.rs `ORPHAN_GRACE`); the deletion can be asked again.
            return Err(e.context(format!("session {id}'s jail is stopped, but its ACP relay still answers; its files and row stay until it is gone")));
        }
        if let Err(e) = self.sync_credentials_for(Some(id)) {
            eprintln!("sill daemon: credentials before deleting {id}: {e:#}");
        }
        let uid = rustix::process::getuid().as_raw();
        if let Err(e) = sill_core::session::remove_state(uid, id) {
            eprintln!("sill daemon: removing session {id}'s state: {e:#}");
        }
        sill_core::session::forget_policy_snapshots(id)?;
        self.with_store(|s| {
            // a ghost's rows are overwritten as they go and the file is rewritten without them,
            // its WAL emptied: the depth docs/memory.md §3.4 promises, no more
            if ghost {
                s.conn().execute_batch("PRAGMA secure_delete = ON")?;
            }
            s.conn().execute("DELETE FROM daemon_runners WHERE id=?1", [id])?;
            s.conn().execute("DELETE FROM daemon_ends WHERE id=?1", [id])?;
            let r = s.delete(id);
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
        self.changes_changed();
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
            let runner = self.runners.lock().unwrap().get(&s.id).cloned();
            let runner = runner.filter(|r| r.alive());
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
            self.changes_changed();
            if n > 0 {
                if let Err(error) = self.sfp_lifecycle(&s.id, None, Some(format!("ghost: {n} path(s) reverted, an hour after their last write"))) {
                    eprintln!("sill daemon: publishing ghost revert notice: {error:#}");
                }
            }
        }
    }

    /// A session's writes as the Changes page lists them: its upper trees against its policy (a
    /// ghost's tightened).
    fn session_changes(&self, s: &sill_core::store::Session, uid: u32, user: &str) -> Result<Vec<sill_core::ledger::Change>> {
        let sdir = sill_core::session::state_dir_path(uid, &s.id)?;
        if !sdir.is_dir() { return Ok(vec![]); }
        let launch = sill_core::session::policy_snapshot(&s.id)?.context("launch not recorded; rebuild the session")?;
        let filesystem = launch.filesystem.context("filesystem plan not recorded; rebuild the session")?;
        let policy = launch.policy;
        let plan = sill_core::changes::session_plan(policy.clone(), sdir, &self.home, uid, user, &filesystem)?;
        sill_core::changes::scan(&policy, &plan, None)
    }

    /// A relay for the session answers on its socket.
    fn relay_answers(&self, id: &str) -> bool {
        sill_proto::runtime_dir().map(|d| d.join(id).join("relay.sock")).is_ok_and(|s| s.exists() && std::os::unix::net::UnixStream::connect(&s).is_ok())
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
            if launcher_alive(&e.path(), &id) || self.children.lock().unwrap().contains_key(&id) {
                continue;
            }
            if std::fs::remove_dir_all(e.path()).is_ok() {
                eprintln!("sill daemon: removed the runtime dir of deleted session {id}");
            }
        }
    }

    /// Jails and relays that must not be running (docs/daemon.md §3.11): their session is gone
    /// from the store, or ended more than [`STRAY_GRACE_SECS`] ago — an end or a deletion that
    /// stopped half-way, a daemon that left before its stop thread was done. Each is stopped the
    /// way a deletion stops one: its runner retired, an unowned relay asked to quit (evicted when
    /// it will not), the jail stopped; the session's state and row are left as they are. Before
    /// this, nothing ever looked again: such a jail was adopted by every later daemon. Run on the
    /// recovery thread after `READY=1`, then by the minute sweep.
    pub(super) fn reap_strays(&self) {
        let Ok(dir) = sill_proto::runtime_dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        // a store that cannot be read says nothing about which sessions exist: never a reason to
        // stop one
        let sessions = match self.with_store(|s| s.sessions()) {
            Ok(sessions) => sessions,
            Err(e) => {
                eprintln!("sill daemon: looking for stray jails: {e:#}");
                return;
            }
        };
        let now = sill_core::store::now();
        for e in entries.flatten() {
            let Some(id) = e.file_name().to_str().map(str::to_string) else { continue };
            if !sill_core::session::valid_id(&id) || !e.path().is_dir() {
                continue;
            }
            let why = match sessions.iter().find(|s| s.id == id) {
                None => "is not in the store".to_string(),
                Some(s) => match s.ended_at {
                    Some(ended) if now - ended > STRAY_GRACE_SECS => format!("ended {} min ago", (now - ended) / 60),
                    _ => continue,
                },
            };
            let jail = launcher_alive(&e.path(), &id) || self.children.lock().unwrap().contains_key(&id);
            let runner = self.runners.lock().unwrap().contains_key(&id);
            let relay = runner || self.relay_answers(&id);
            if !jail && !relay {
                continue;
            }
            let what = match (jail, relay) {
                (true, true) => "jail and ACP relay are",
                (true, false) => "jail is",
                _ => "ACP relay is",
            };
            eprintln!("sill daemon: session {id} {why}, but its {what} still running: stopping it");
            if relay {
                let socket = e.path().join("relay.sock");
                let result = if runner {
                    // the retired runner's Quit ends the relay; one that does not leave is evicted
                    self.retire_runner(&id);
                    super::relay::wait_stopped(&socket).or_else(|_| super::relay::evict(&e.path(), &id))
                } else {
                    let cursor = self.with_store(|store| Runner::load(&id, store)).map(|snapshot| Runner::cursor(&snapshot)).unwrap_or(0);
                    super::relay::stop_unowned(&socket, &id, cursor)
                };
                if let Err(err) = result {
                    eprintln!("sill daemon: stopping the ACP relay of session {id}: {err:#}");
                }
            }
            if jail && let Err(err) = sill_core::session::stop(&id) {
                eprintln!("sill daemon: stopping the jail of session {id}: {err:#}");
            }
        }
    }

    /// The session's conversation is over as far as the daemon is concerned: no more events out.
    /// Stop a session's jail and its agent; the conversation stays and starts again when used.
    pub(super) fn stop_session(&self, id: &str) -> Result<()> {
        // Recovery/attachment must not adopt the old relay during teardown.
        let _one_at_a_time = self.starting.lock().unwrap();
        self.retire_runner(id);
        let dir = sill_proto::runtime_dir()?.join(id);
        // Quit is asynchronous. A successful jail stop must also make the old
        // agent connection unavailable before a subsequent start can reuse it.
        super::relay::wait_stopped(&dir.join("relay.sock"))
            .or_else(|_| super::relay::evict(&dir, id))?;
        sill_core::session::stop(id)
    }

    /// A session's pins changed (`None`: the global pins, every session's): each running jail
    /// they reach is stopped and started again at once, so its mounts and workspaces match the
    /// pins (docs/pins.md, docs/parallel-coding.md §12). Everything in the jail stops — a turn in
    /// progress as Stop does, `sill-proc` processes, Host control — and the conversation goes on
    /// at the next prompt; a notice says what stopped. A session whose jail is not running needs
    /// nothing: its next start reads the pins.
    pub(super) fn pins_changed(self: &Arc<Self>, session: Option<&str>) {
        let ids: Vec<String> = match session {
            Some(id) => vec![id.to_string()],
            None => self.children.lock().unwrap().keys().cloned().collect(),
        };
        for id in ids {
            if self.is_ghost(&id) || !sill_core::session::is_running(&id) || !self.rebuilds.begin(&id) {
                continue;
            }
            let hub = self.clone();
            let spawned = std::thread::Builder::new().name(format!("sill-rebuild-{id}")).spawn({
                let id = id.clone();
                move || loop {
                    hub.rebuild(&id);
                    if !hub.rebuilds.again(&id) {
                        break;
                    }
                }
            });
            if let Err(e) = spawned {
                eprintln!("sill daemon: session {id}: rebuilding for the pin change: {e:#}");
                self.rebuilds.again(&id);
            }
        }
    }

    fn rebuild(self: &Arc<Self>, id: &str) {
        let open = self.with_store(|s| Ok(s.session_exact(id)?.is_some_and(|s| s.ended_at.is_none()))).unwrap_or(false);
        if !open || !sill_core::session::is_running(id) {
            return;
        }
        let runner = self.runners.lock().unwrap().get(id).cloned();
        let busy = runner.is_some_and(|r| r.alive() && r.busy());
        let procs: Vec<String> = sill_core::shim::runtime_dir(id)
            .and_then(|d| sill_core::shim::Control::connect(&d.join("shim.sock")))
            .and_then(|mut c| c.proc_list())
            .map(|ps| ps.into_iter().filter(|p| p.running()).map(|p| p.name).collect())
            .unwrap_or_default();
        super::host_control::session_ended(self, id, "the jail was rebuilt for a pin change");
        if let Err(e) = self.stop_session(id) {
            eprintln!("sill daemon: session {id}: stopping the jail for the pin change: {e:#}");
            return;
        }
        let started = self.ensure_runner(id, false);
        let mut said = vec!["The jail was rebuilt for the pin change.".to_string()];
        if busy {
            said.push("The turn in progress was stopped.".into());
        }
        if !procs.is_empty() {
            said.push(format!("Stopped: {}.", procs.join(", ")));
        }
        if let Err(e) = &started {
            eprintln!("sill daemon: session {id}: starting the jail after the pin change: {e:#}");
            said.push(format!("It did not start again: {e:#}. It starts at the next prompt."));
        }
        super::host_control::note(self, id, said.join(" "));
        // a workspace unpinned is a result now, or gone: the shelf and Changes look again
        self.workspaces_settled(id);
        self.changes_changed();
    }

    /// Stop each running session of `agent` as `sessions.stop` does; their ids.
    pub(super) fn stop_sessions_of(&self, agent: &str) -> Vec<String> {
        let rows = self.store.lock().unwrap().sessions().unwrap_or_default();
        let runners: Vec<_> = self.runners.lock().unwrap().values().cloned().collect();
        let running: Vec<String> = runners.into_iter().filter(|r| r.alive()).map(|r| r.id.clone()).collect();
        let mut stopped = Vec::new();
        for id in running {
            if !rows.iter().any(|s| s.id == id && s.agent == agent) {
                continue;
            }
            match self.stop_session(&id) {
                Ok(()) => stopped.push(id),
                Err(e) => eprintln!("sill daemon: stopping session {id}: {e:#}"),
            }
        }
        stopped
    }

    /// The copies of `agent`'s home in this daemon's ghost sessions (a ghost's agent starts from a
    /// copy of the sill-owned home, its login included).
    pub(super) fn ghost_homes_of(&self, agent: &str) -> Vec<std::path::PathBuf> {
        let rows = self.store.lock().unwrap().sessions().unwrap_or_default();
        sill_core::session::existing_ghost_homes(&rows, agent)
    }

    fn retire_runner(&self, id: &str) {
        let runner = self.runners.lock().unwrap().remove(id);
        if let Some(runner) = runner {
            if let Err(error) = runner.retire(self) {
                eprintln!("sill daemon: retiring session {id}: {error:#}");
            }
        } else if let Err(error) = self.with_store(|store| Runner::mark_terminal(store, id)) {
            eprintln!("sill daemon: saving terminal state of session {id}: {error:#}");
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
        if let Err(e) = self.sync_credentials_for(Some(id)) {
            eprintln!("sill daemon: credentials before ending {id}: {e:#}");
        }
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let runner = self.runners.lock().unwrap().get(id).cloned();
        let runner = runner.filter(|r| r.alive());
        if let Some(r) = &runner {
            match r.summarizing() {
                Some(SummaryKind::End) => return Ok(Ending { pending: true, deleted: false }),
                Some(SummaryKind::Checkpoint) => {
                    let mut ctx = self.end_ctx(id)?;
                    ctx.reason = reason.to_string();
                    if r.adopt_checkpoint(self, &serde_json::to_string(&ctx)?)? {
                        // The SQL intent is committed with the runner's new summary kind. Do
                        // not insert the map afterwards: completion could already have removed
                        // it, leaving a stale entry that prevents future idle sweeps.
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
        ctx.keep_old = ctx.keep_old && !runner.as_ref().is_some_and(|r| r.had_turn());
        let keep_old = ctx.keep_old;
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
            self.remember_end(id, ctx)?;
            match r.ask_summary(self, &prompt, "Summarizing session..", SummaryKind::End) {
                Ok(()) => {
                    eprintln!("sill daemon: session {id} ends ({reason}): the agent writes its summary");
                    return Ok(Ending { pending: true, deleted: false });
                }
                Err(e) => eprintln!("sill daemon: session {id}: the agent could not be asked for a summary ({e:#})"),
            }
        }
        eprintln!("sill daemon: session {id} ends ({reason})");
        let mut ctx = self.end_ctx(id)?;
        ctx.keep_old = keep_old;
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
        r.ask_summary(self, &prompt, "Summarizing session..", SummaryKind::Checkpoint)
    }

    /// A summary turn is over (the runner's lock is not held): the end completes, or the
    /// checkpoint is stored and announced.
    pub fn summary_done(self: &Arc<Self>, id: &str, done: SummaryDone) {
        let answer = done.text.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
        match done.kind {
            SummaryKind::End => {
                let ctx = self.ending.lock().unwrap().remove(id).or_else(|| self.persisted_end(id).ok().flatten());
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
                if let Err(e)=self.sfp_lifecycle(id,None,None){eprintln!("sill daemon: publishing summary: {e:#}");}
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
        if let Err(e)=self.sfp_lifecycle(id,Some((delete,reason)),None){eprintln!("sill daemon: publishing session end: {e:#}");}
        // a deletion retires the runner itself and then waits for its relay to leave; retired
        // here, the relay was always halfway out when the deletion connected to it as an unowned
        // one — a reset connection, and before docs/daemon.md §3.11 a jail left running
        if !delete {
            self.retire_runner(id);
        }
        self.ending.lock().unwrap().remove(id);
        // The terminal runner cursor survives until session deletion/retention. The relay may
        // still be delivering Ended when this daemon stops; deleting its cursor now strands that
        // worker with an already-acknowledged stream and nothing to reconnect from.
        self.with_store(|s| { s.conn().execute("DELETE FROM daemon_ends WHERE id=?1", [id])?; Ok(()) }).ok();
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

    /// Start a stored session's jail from its row — pins, policy, the agent's binds
    /// — or find it running (what the frontend built in-process before the daemon).
    fn start_session(&self, id: &str) -> Result<Started> {
        self.sync_credentials_for(Some(id))?;
        if !sill_core::session::valid_id(id) {
            bail!("not a session id");
        }
        let (s, pins, workspace) = {
            let store = self.store.lock().unwrap();
            let Some(s) = store.sessions()?.into_iter().find(|x| x.id == id) else { bail!("no session {id}") };
            let pins = store.jail_pins(id)?; // explicit session pins only
            let workspace = store.workspace_mask(id)?;
            (s, pins, workspace)
        };
        // Reuse a live jail before computing a new launch plan. Recovery must not
        // require the state mount or installed policy just to reconnect its shim.
        if sill_core::session::is_running(id) {
            return Ok(Started { socket: sill_core::shim::runtime_dir(id)?.join("shim.sock"), fresh: false });
        }
        // `SILL_LAUNCHER=attach` (the daemon's own tests): a session's jail is the test's to
        // provide, a scripted shim on the session's socket. Nothing is launched: a session the
        // daemon made itself (a Crosscheck worker) waits here for its shim to answer.
        if std::env::var("SILL_LAUNCHER").is_ok_and(|v| v == "attach") {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !sill_core::session::is_running(id) {
                if std::time::Instant::now() >= deadline { bail!("SILL_LAUNCHER=attach: nothing answered on session {id}'s shim socket"); }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return Ok(Started { socket: sill_core::shim::runtime_dir(id)?.join("shim.sock"), fresh: false });
        }
        // a workspace session's workspaces, made or kept, before its jail (docs/parallel-coding.md §12)
        let workspace = if s.ghost {
            workspace
        } else {
            self.workspaces_for_start(id).context("making the session's workspaces")?;
            self.with_store(|store| store.workspace_mask(id))?
        };
        let registry = sill_core::agents::load(&self.home)?;
        let Some(agent) = registry.get(&s.agent) else { bail!("unknown agent {}", s.agent) };
        // Diagnose unavailable capabilities before preparing host state or launch bindings.
        // ensure_started resolves and validates again at the actual enforcement boundary.
        let resolved = sill_core::session::next_build(&s, &self.home, &pins, &workspace)?;
        resolved.policy.permissions.validate_launch(&resolved.policy.network)?;
        let uid = rustix::process::getuid().as_raw();
        let sdir = sill_core::session::state_dir(uid, id)?;
        agent.ready(&self.home)?;
        let binds = vec![];
        let opts = StartOptions {
            session: id.to_string(),
            policy: s.policy.clone(),
            pins: if s.ghost { vec![] } else { pins.clone() },
            cwd: self.home.clone(),
            state_dir: Some(sdir),
            unsafe_state: false,
            share_net: false,
            env: vec![],
            shim_bin: None,
            agent: Some(s.agent.clone()),
            binds,
            ghost: s.ghost,
            workspace: if s.ghost { vec![] } else { workspace },
        };
        let started = sill_core::session::ensure_started(&opts, &self.home, &self.exe).context("starting the jail")?;
        // the pinned repositories' HEAD when this open period began (kept across restarts)
        if !self.crosscheck_worker(id) && let Err(e) = self.with_store(|st| sill_core::history::note_pin_starts(st, &s)) {
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

    pub fn reconnect_runner(self: &Arc<Self>, id: &str) {
        let hub = self.clone(); let id = id.to_string();
        std::thread::spawn(move || {
            let mut wait = std::time::Duration::from_millis(100);
            loop {
                std::thread::sleep(wait);
                let open = hub.with_store(|s| Ok(s.sessions()?.into_iter().any(|s| s.id == id && s.ended_at.is_none()))).unwrap_or(false);
                if !open { return; }
                match hub.ensure_runner(&id, false) {
                    Ok(_) => return,
                    Err(e) => eprintln!("sill daemon: reconnecting session {id}: {e:#}"),
                }
                wait = (wait * 2).min(std::time::Duration::from_secs(10));
            }
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
                // only the session's own launcher: a stale file's pid may be anybody's by now
                Some(pid) if sill_core::launcher::is_session_process(pid, &id, sill_core::launcher::SessionProcess::Launcher) => {
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

/// How long after its end a session's jail may still be running before the minute sweep calls it a
/// stray (`Hub::reap_strays`): the end's own stop runs on a thread of its own and takes seconds.
const STRAY_GRACE_SECS: i64 = 120;

/// The session's own launcher is alive (its `launch.pid`, checked to be that process).
fn launcher_alive(runtime: &std::path::Path, id: &str) -> bool {
    sill_core::launcher::session_pid(&runtime.join("launch.pid"), id, sill_core::launcher::SessionProcess::Launcher).is_some()
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

fn args<T: DeserializeOwned>(v: Value) -> Result<T, Error> {
    serde_json::from_value(if v.is_null() { json!({}) } else { v }).map_err(Error::invalid_params)
}

fn to_value<T: serde::Serialize>(t: T) -> Result<Value, Error> {
    serde_json::to_value(t).map_err(|e| Error::new(Error::INTERNAL, e.to_string()))
}

#[cfg(test)]
mod rebuild_tests {
    use super::Rebuilds;

    #[test]
    fn one_rebuild_at_a_time_and_one_more_for_changes_during_it() {
        let r = Rebuilds::default();
        assert!(r.begin("s1"), "the first change starts a rebuild");
        assert!(!r.begin("s1"), "a change while it runs does not start a second");
        assert!(!r.begin("s1"), "nor does a third");
        assert!(r.begin("s2"), "another session's is its own");
        assert!(r.again("s1"), "the changes during it ask for exactly one more");
        assert!(!r.again("s1"), "and then it is done");
        assert!(!r.again("s2"));
        assert!(r.begin("s1"), "a later change starts afresh");
        assert!(!r.again("s1"));
    }
}
