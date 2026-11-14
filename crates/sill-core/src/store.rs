//! Session store (SPEC §3.2, §4.9): sessions and their transcripts in SQLite at
//! `$XDG_STATE_HOME/sill/sill.db`. A session is an isolated conversation with one agent; its jail is
//! the unit `sill-<id>` and its upper trees live under `/var/lib/sill/<uid>/sessions/<id>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub created_at: i64,
    pub last_used: i64,
    /// The agent's own session id (ACP), once known; used for `session/load`.
    pub acp_session_id: Option<String>,
    pub policy: String,
}

/// One stored transcript entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub seq: i64,
    pub ts: i64,
    /// `user` | `agent` | `thought` | `tool` | `note`
    pub kind: String,
    pub text: String,
    /// For `tool`: the status; otherwise empty.
    pub extra: String,
}

pub struct Store {
    conn: Connection,
}

pub fn default_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    Ok(base.join("sill").join("sill.db"))
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn new_id() -> String {
    use rand::RngExt;
    let n: u64 = rand::rng().random();
    format!("{:08x}", (n & 0xffff_ffff) as u32)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL,
                 created_at INTEGER NOT NULL, last_used INTEGER NOT NULL,
                 acp_session_id TEXT, policy TEXT NOT NULL DEFAULT 'default');
             CREATE TABLE IF NOT EXISTS session_pins (
                 session_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY (session_id, path));
             CREATE TABLE IF NOT EXISTS entries (
                 session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL,
                 kind TEXT NOT NULL, text TEXT NOT NULL, extra TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (session_id, seq));",
        )?;
        Ok(Self { conn })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path()?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT id, agent, title, created_at, last_used, acp_session_id, policy FROM sessions ORDER BY last_used DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                agent: r.get(1)?,
                title: r.get(2)?,
                created_at: r.get(3)?,
                last_used: r.get(4)?,
                acp_session_id: r.get(5)?,
                policy: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn create(&self, agent: &str, policy: &str) -> Result<Session> {
        let s = Session {
            id: new_id(),
            agent: agent.to_string(),
            title: crate::title::random_name(),
            created_at: now(),
            last_used: now(),
            acp_session_id: None,
            policy: policy.to_string(),
        };
        self.conn.execute(
            "INSERT INTO sessions (id, agent, title, created_at, last_used, acp_session_id, policy) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
            params![s.id, s.agent, s.title, s.created_at, s.last_used, s.policy],
        )?;
        Ok(s)
    }

    pub fn set_acp_session_id(&self, id: &str, acp: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET acp_session_id = ?2 WHERE id = ?1", params![id, acp])?;
        Ok(())
    }

    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET title = ?2 WHERE id = ?1", params![id, title])?;
        Ok(())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET last_used = ?2 WHERE id = ?1", params![id, now()])?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM entries WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM session_pins WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn pins(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let mut st = self.conn.prepare("SELECT path FROM session_pins WHERE session_id = ?1 ORDER BY path")?;
        let rows = st.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).map(PathBuf::from).collect())
    }

    pub fn pin(&self, session_id: &str, path: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_pins (session_id, path) VALUES (?1, ?2)",
            params![session_id, path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn unpin(&self, session_id: &str, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM session_pins WHERE session_id = ?1 AND path = ?2", params![session_id, path.to_string_lossy()])?;
        Ok(())
    }

    pub fn append(&self, session_id: &str, kind: &str, text: &str, extra: &str) -> Result<i64> {
        let seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM entries WHERE session_id = ?1", params![session_id], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO entries (session_id, seq, ts, kind, text, extra) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, seq, now(), kind, text, extra],
        )?;
        Ok(seq)
    }

    pub fn entries(&self, session_id: &str) -> Result<Vec<Entry>> {
        let mut st = self.conn.prepare("SELECT seq, ts, kind, text, extra FROM entries WHERE session_id = ?1 ORDER BY seq")?;
        let rows = st.query_map(params![session_id], |r| {
            Ok(Entry { seq: r.get(0)?, ts: r.get(1)?, kind: r.get(2)?, text: r.get(3)?, extra: r.get(4)? })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
