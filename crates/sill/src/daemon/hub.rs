//! The daemon's state and its methods: one store, one launcher probe, a dispatch by method name.
//! Transport-free on purpose — `serve` hands it a method and a value and writes back what it says.

use std::sync::Mutex;

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sill_core::store::Store;
use sill_proto::rpc::Error;
use sill_proto::{DaemonStatus, Key, MemorySearch, MemorySession, Session, Text, Transcript, method};

pub struct Hub {
    store: Mutex<Store>,
    started: i64,
    mode: &'static str,
    launcher: String,
    home: std::path::PathBuf,
}

impl Hub {
    pub fn new(mode: &'static str) -> Result<Hub> {
        let store = Store::open_default()?;
        let launcher = sill_core::launcher::current().describe();
        Ok(Hub { store: Mutex::new(store), started: sill_core::store::now(), mode, launcher, home: sill_core::changes::home()? })
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
            launcher: self.launcher.clone(),
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
                // an ended session has no jail by definition; the probe is a process per session
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
