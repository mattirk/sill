//! The daemon's state and its methods: one store, the claimed launcher, the sessions it supervises,
//! a dispatch by method name. Transport-free on purpose — `serve` hands it a method and a value and
//! writes back what it says.

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
use sill_proto::{DaemonStatus, Key, LogRequest, MemorySearch, MemorySession, Session, Started, Text, Transcript, method};

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
        };
        hub.adopt_running();
        Ok(hub)
    }

    pub fn session_count(&self) -> usize {
        self.store.lock().unwrap().sessions().map(|s| s.len()).unwrap_or(0)
    }

    pub fn dispatch(&self, m: &str, params: Value) -> Result<Value, Error> {
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
                sill_core::session::stop(&k.key).map_err(|e| Error::failed(format!("{e:#}")))?;
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
                let mut ended: Vec<MemorySession> = sill_memory::sessions(store.conn())
                    .map_err(Error::failed)?
                    .into_iter()
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
                })
            })
            .collect()
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
        let binds = sill_core::session::binds_for(agent, &policy, &sdir, &self.home, &pins)?;
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
        };
        let started = sill_core::session::ensure_started(&opts, &self.home, &self.exe).context("starting the jail")?;
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
