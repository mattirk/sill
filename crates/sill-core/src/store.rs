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
    /// Answer the agent's permission requests with "allow once" automatically.
    pub auto_allow: bool,
    /// The agent controls as last set by the person: mode id and config option values, re-applied
    /// whenever the ACP session is (re)started.
    pub agent_settings: AgentSettings,
    /// Desktop access override: "" (the global default), "none", "x11", "x11+dbus" (SPEC §4.2).
    pub desktop: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentSettings {
    pub mode: Option<String>,
    /// option id → value id (selects) or "true"/"false" (booleans)
    #[serde(default)]
    pub config: std::collections::BTreeMap<String, String>,
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
    /// For `tool`: the ACP kind (`Edit`, `Execute`, …), the body text, the edit as a unified diff.
    pub tool_kind: String,
    pub detail: String,
    pub diff: String,
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
        // additive migrations
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN auto_allow INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN agent_settings TEXT NOT NULL DEFAULT '{}'", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN desktop TEXT NOT NULL DEFAULT ''", []);
        // tool calls keep what they did (2026-10-31): kind, body, the edit's diff
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN tool_kind TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN detail TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN diff TEXT NOT NULL DEFAULT ''", []);
        Ok(Self { conn })
    }

    pub fn set_agent_settings(&self, id: &str, s: &AgentSettings) -> Result<()> {
        self.conn.execute("UPDATE sessions SET agent_settings = ?2 WHERE id = ?1", params![id, serde_json::to_string(s)?])?;
        Ok(())
    }

    pub fn set_policy(&self, id: &str, policy: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET policy = ?2 WHERE id = ?1", params![id, policy])?;
        Ok(())
    }

    pub fn set_desktop(&self, id: &str, desktop: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET desktop = ?2 WHERE id = ?1", params![id, desktop])?;
        Ok(())
    }

    pub fn set_auto_allow(&self, id: &str, on: bool) -> Result<()> {
        self.conn.execute("UPDATE sessions SET auto_allow = ?2 WHERE id = ?1", params![id, on as i32])?;
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path()?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT id, agent, title, created_at, last_used, acp_session_id, policy, auto_allow, agent_settings, desktop FROM sessions ORDER BY last_used DESC",
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
                auto_allow: r.get::<_, i32>(7)? != 0,
                agent_settings: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                desktop: r.get(9)?,
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
            auto_allow: false,
            agent_settings: AgentSettings::default(),
            desktop: String::new(),
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

    /// Drop pins whose session no longer exists (older deletes didn't clear them); how many went.
    pub fn prune_pins(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM session_pins WHERE session_id NOT IN (SELECT id FROM sessions)", [])?)
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

    /// A `tool` entry: title, status, ACP kind, body text and the edit's diff.
    pub fn append_tool(&self, session_id: &str, title: &str, status: &str, kind: &str, detail: Option<&str>, diff: Option<&str>) -> Result<i64> {
        let seq = self.append(session_id, "tool", title, status)?;
        self.conn.execute(
            "UPDATE entries SET tool_kind = ?3, detail = ?4, diff = ?5 WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq, kind, detail.unwrap_or(""), diff.unwrap_or("")],
        )?;
        Ok(seq)
    }

    /// Update a stored `tool` entry (tool-call updates arrive after the call): title and status
    /// replace, a detail (output) is added under what is there, a diff replaces the diff.
    pub fn update_tool(&self, session_id: &str, seq: i64, title: Option<&str>, status: Option<&str>, detail: Option<&str>, diff: Option<&str>) -> Result<()> {
        if let Some(t) = title {
            self.conn.execute("UPDATE entries SET text = ?3 WHERE session_id = ?1 AND seq = ?2", params![session_id, seq, t])?;
        }
        if let Some(st) = status {
            self.conn.execute("UPDATE entries SET extra = ?3 WHERE session_id = ?1 AND seq = ?2", params![session_id, seq, st])?;
        }
        if let Some(d) = detail.filter(|d| !d.trim().is_empty()) {
            self.conn.execute(
                "UPDATE entries SET detail = CASE WHEN detail = '' THEN ?3 ELSE detail || char(10) || char(10) || ?3 END WHERE session_id = ?1 AND seq = ?2",
                params![session_id, seq, d],
            )?;
        }
        if let Some(d) = diff.filter(|d| !d.trim().is_empty()) {
            self.conn.execute("UPDATE entries SET diff = ?3 WHERE session_id = ?1 AND seq = ?2", params![session_id, seq, d])?;
        }
        Ok(())
    }

    pub fn count_kind(&self, session_id: &str, kind: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE session_id = ?1 AND kind = ?2",
            params![session_id, kind],
            |r| r.get(0),
        )?)
    }

    pub fn entries(&self, session_id: &str) -> Result<Vec<Entry>> {
        let mut st = self.conn.prepare("SELECT seq, ts, kind, text, extra, tool_kind, detail, diff FROM entries WHERE session_id = ?1 ORDER BY seq")?;
        let rows = st.query_map(params![session_id], |r| {
            Ok(Entry {
                seq: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                text: r.get(3)?,
                extra: r.get(4)?,
                tool_kind: r.get(5)?,
                detail: r.get(6)?,
                diff: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_updates_reach_the_stored_entry() {
        let dir = std::env::temp_dir().join(format!("sill-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("s.db")).unwrap();
        let seq = store.append_tool("s1", "Read", "Pending", "Read", Some("file_path: src/main.rs"), None).unwrap();
        store.update_tool("s1", seq, None, Some("InProgress"), None, None).unwrap();
        store.update_tool("s1", seq, Some("Read src/main.rs"), Some("Completed"), Some("fn main() {}"), Some("--- a\n+++ b\n")).unwrap();
        let e = store.entries("s1").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].text.as_str(), e[0].extra.as_str()), ("Read src/main.rs", "Completed"));
        assert_eq!(e[0].tool_kind, "Read");
        assert_eq!(e[0].detail, "file_path: src/main.rs\n\nfn main() {}"); // output under the input
        assert_eq!(e[0].diff, "--- a\n+++ b\n");
        // an unknown seq is a harmless no-op
        store.update_tool("s1", 99, Some("x"), None, None, None).unwrap();
        assert_eq!(store.entries("s1").unwrap()[0].text, "Read src/main.rs");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
