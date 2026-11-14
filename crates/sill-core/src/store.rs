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
    /// Who named it last (title.rs): "" for the two-word name, "agent" when its summary named it
    /// (at a checkpoint or the end), "person" when renamed by hand.
    pub title_kind: String,
    pub created_at: i64,
    pub last_used: i64,
    /// The agent's own session id (ACP), once known; used for `session/load`.
    pub acp_session_id: Option<String>,
    pub policy: String,
    /// Answer the agent's permission requests with "allow once" automatically (per session, SPEC §3.2).
    pub auto_allow: bool,
    /// The agent controls as last set in this session, re-applied whenever its ACP session is
    /// (re)started; a new session starts from its agent type's default (SPEC §3.2).
    pub agent_settings: AgentSettings,
    /// Desktop access override: "" (the global default), "none", "x11", "x11+dbus" (SPEC §4.2).
    pub desktop: String,
    /// When the session ended — its tab closed (docs/memory.md §2.1); None while it is open.
    pub ended_at: Option<i64>,
    /// What came of it, written at the end: by its agent (`summary_kind` "agent") or by sill from
    /// the facts ("auto"); "person" once edited in the session page; empty until the first end
    /// or checkpoint.
    pub summary: String,
    pub summary_kind: String,
    /// Which earlier sessions its agent is told about (docs/memory.md §2.3): "here", "everywhere",
    /// "off"; "" = the default from the pins (`sill_memory::Scope::resolve`).
    pub memory_scope: String,
    /// The ended session this one was opened from with *New here*: its summary leads the recall.
    pub follows: String,
    /// Compaction (docs/memory.md §2.4): 0 = the whole transcript; 1 = thoughts, tool details and
    /// diffs dropped; 2 = the agent's text too — summary, notes, prompts and tool titles remain.
    pub compacted: i64,
    /// A ghost session (docs/memory.md §3): chosen at creation, never changed; it leaves no row.
    pub ghost: bool,
}

/// The agent controls as set by the person (SPEC §3.2): mode id and config option values. Kept twice:
/// per session (`sessions.agent_settings`, what that session runs with) and per agent type
/// (`agent_settings`, the default a new session starts from — every change in any session updates it).
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
        // the person's state is 0700/0600 whatever the umask says (security.md S21): the dir here,
        // the database after SQLite made it — the -wal/-shm files copy the main file's mode
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        for suffix in ["", "-wal", "-shm"] {
            let f = path.with_file_name(format!("{}{suffix}", path.file_name().unwrap_or_default().to_string_lossy()));
            if f.exists() {
                let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
            }
        }
        // another shelf or a CLI holding a write: wait a little instead of failing at once (a
        // failure here is the service's crash loop, SPEC §4.11 #17)
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
                 PRIMARY KEY (session_id, seq));
             CREATE TABLE IF NOT EXISTS agent_settings (agent TEXT PRIMARY KEY, settings TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS memories (
                 session_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);",
        )?;
        // additive migrations
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN auto_allow INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN agent_settings TEXT NOT NULL DEFAULT '{}'", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN desktop TEXT NOT NULL DEFAULT ''", []);
        // tool calls keep what they did (2026-10-31): kind, body, the edit's diff
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN tool_kind TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN detail TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE entries ADD COLUMN diff TEXT NOT NULL DEFAULT ''", []);
        // memory M0 (2026-11-01): when a session ended and what came of it
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN ended_at INTEGER", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN summary TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN summary_kind TEXT NOT NULL DEFAULT ''", []);
        // memory M1: recall scope, New here, and the searchable text kept by triggers
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN memory_scope TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN follows TEXT NOT NULL DEFAULT ''", []);
        // memory M2: compaction level per session, and when the nightly run last went
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0", []);
        // the summary names the session (title.rs): who named it last
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN title_kind TEXT NOT NULL DEFAULT ''", []);
        // ghost sessions (docs/memory.md §3): fixed at creation, deleted at the end
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN ghost INTEGER NOT NULL DEFAULT 0", []);
        conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
        sill_memory::install(&conn).context("memory index")?;
        // the ledger across time (SPEC §3.6) and the pinned repositories' HEAD at a session's start
        sill_memory::history::install(&conn).context("history")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pin_heads (session_id TEXT NOT NULL, repo TEXT NOT NULL, head TEXT NOT NULL, ts INTEGER NOT NULL,
                 PRIMARY KEY (session_id, repo))",
        )?;
        Ok(Self { conn })
    }

    pub fn set_session_settings(&self, id: &str, s: &AgentSettings) -> Result<()> {
        self.conn.execute("UPDATE sessions SET agent_settings = ?2 WHERE id = ?1", params![id, serde_json::to_string(s)?])?;
        Ok(())
    }

    /// The default controls for an agent type: what a new session starts from (empty until chosen).
    pub fn agent_settings(&self, agent: &str) -> AgentSettings {
        self.conn
            .query_row("SELECT settings FROM agent_settings WHERE agent = ?1", params![agent], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn set_agent_settings(&self, agent: &str, s: &AgentSettings) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_settings (agent, settings) VALUES (?1, ?2) ON CONFLICT(agent) DO UPDATE SET settings = excluded.settings",
            params![agent, serde_json::to_string(s)?],
        )?;
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
            "SELECT id, agent, title, created_at, last_used, acp_session_id, policy, auto_allow, agent_settings, desktop, ended_at, summary, summary_kind, memory_scope, follows, compacted, title_kind, ghost FROM sessions ORDER BY last_used DESC",
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
                ended_at: r.get(10)?,
                summary: r.get(11)?,
                summary_kind: r.get(12)?,
                memory_scope: r.get(13)?,
                follows: r.get(14)?,
                compacted: r.get(15)?,
                title_kind: r.get(16)?,
                ghost: r.get::<_, i32>(17)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn create(&self, agent: &str, policy: &str) -> Result<Session> {
        self.create_with(agent, policy, false)
    }

    /// A new session; `ghost` is fixed from here on (docs/memory.md §3.1).
    pub fn create_with(&self, agent: &str, policy: &str, ghost: bool) -> Result<Session> {
        let s = Session {
            id: new_id(),
            agent: agent.to_string(),
            title: crate::title::random_name(),
            title_kind: String::new(),
            created_at: now(),
            last_used: now(),
            acp_session_id: None,
            policy: policy.to_string(),
            auto_allow: false,
            agent_settings: self.agent_settings(agent), // the agent type's default
            desktop: String::new(),
            ended_at: None,
            summary: String::new(),
            summary_kind: String::new(),
            memory_scope: String::new(),
            follows: String::new(),
            compacted: 0,
            ghost,
        };
        self.conn.execute(
            "INSERT INTO sessions (id, agent, title, created_at, last_used, acp_session_id, policy, agent_settings, ghost) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8)",
            params![s.id, s.agent, s.title, s.created_at, s.last_used, s.policy, serde_json::to_string(&s.agent_settings)?, ghost as i32],
        )?;
        Ok(s)
    }

    pub fn set_acp_session_id(&self, id: &str, acp: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET acp_session_id = ?2 WHERE id = ?1", params![id, acp])?;
        Ok(())
    }

    /// Name the session; `kind` says who did (`Session::title_kind`). The last name wins, whoever
    /// gave it: a summary renames a session the person named.
    pub fn set_title(&self, id: &str, title: &str, kind: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET title = ?2, title_kind = ?3 WHERE id = ?1", params![id, title, kind])?;
        Ok(())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET last_used = ?2 WHERE id = ?1", params![id, now()])?;
        Ok(())
    }

    /// The session ended: when, and what came of it (docs/memory.md §2.2); `kind` is "agent" or "auto".
    pub fn end(&self, id: &str, summary: &str, kind: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET ended_at = ?2, summary = ?3, summary_kind = ?4 WHERE id = ?1", params![id, now(), summary, kind])?;
        Ok(())
    }

    /// The summary alone, the session open or not (docs/memory.md §2.5): the person's edit
    /// (`kind` "person") or a mid-session checkpoint by the agent ("agent"); the end replaces it.
    pub fn set_summary(&self, id: &str, summary: &str, kind: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET summary = ?2, summary_kind = ?3 WHERE id = ?1", params![id, summary, kind])?;
        Ok(())
    }

    /// The session ended; its summary stays as it is (memory off, or a reopened session closed
    /// without a turn keeps the summary it had).
    pub fn end_only(&self, id: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET ended_at = ?2 WHERE id = ?1", params![id, now()])?;
        Ok(())
    }

    /// Compaction (docs/memory.md §2.4). Level 1: the agent's thoughts go, tool calls keep title
    /// and status and lose detail and diff — the transcript still reads. Level 2: the agent's text
    /// goes too; summary, notes, the person's prompts and tool titles remain, which is what recall
    /// and search use. A lower level than the session has changes nothing.
    pub fn compact(&self, id: &str, level: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if level >= 1 {
            tx.execute("DELETE FROM entries WHERE session_id = ?1 AND kind = 'thought'", params![id])?;
            tx.execute("UPDATE entries SET detail = '', diff = '' WHERE session_id = ?1 AND kind = 'tool' AND (detail <> '' OR diff <> '')", params![id])?;
        }
        if level >= 2 {
            tx.execute("DELETE FROM entries WHERE session_id = ?1 AND kind = 'agent'", params![id])?;
        }
        tx.execute("UPDATE sessions SET compacted = MAX(compacted, ?2) WHERE id = ?1", params![id, level.min(2)])?;
        tx.commit()?;
        Ok(())
    }

    /// Give the space of deleted rows back to the filesystem (after a compaction run).
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// A small key/value the shelf keeps about itself (when the nightly memory run last went).
    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).ok()
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, value])?;
        Ok(())
    }

    /// The session's recall scope (docs/memory.md §2.3); "" = the default from its pins.
    pub fn set_memory_scope(&self, id: &str, scope: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET memory_scope = ?2 WHERE id = ?1", params![id, scope])?;
        Ok(())
    }

    /// The session was opened from `from` with *New here*: that one's summary leads its recall.
    pub fn set_follows(&self, id: &str, from: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET follows = ?2 WHERE id = ?1", params![id, from])?;
        Ok(())
    }

    /// The connection, for the memory queries (`sill_memory`), which read the same tables on the
    /// host as in the jail's snapshot.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// An ended session opened again: open until it ends next; the summary stays until replaced.
    pub fn reopen(&self, id: &str) -> Result<()> {
        self.conn.execute("UPDATE sessions SET ended_at = NULL WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// The first thing the person asked in a session, if anything.
    pub fn first_user_text(&self, id: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT text FROM entries WHERE session_id = ?1 AND kind = 'user' ORDER BY seq LIMIT 1")?;
        let mut rows = st.query(params![id])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    /// A memory entry on a session: a `note` the person wrote (docs/memory.md §2.2).
    pub fn add_memory(&self, id: &str, kind: &str, text: &str) -> Result<()> {
        self.conn.execute("INSERT INTO memories (session_id, ts, kind, text) VALUES (?1, ?2, ?3, ?4)", params![id, now(), kind, text])?;
        Ok(())
    }

    /// (ts, kind, text) of a session's memory entries, oldest first.
    pub fn memories(&self, id: &str) -> Result<Vec<(i64, String, String)>> {
        let mut st = self.conn.prepare("SELECT ts, kind, text FROM memories WHERE session_id = ?1 ORDER BY ts, rowid")?;
        let rows = st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// One memory entry gone, by its stamp and text (the page's × on a note).
    pub fn remove_memory(&self, id: &str, ts: i64, text: &str) -> Result<()> {
        self.conn.execute("DELETE FROM memories WHERE session_id = ?1 AND ts = ?2 AND text = ?3", params![id, ts, text])?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM memories WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM entries WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM session_pins WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM pin_heads WHERE session_id = ?1", params![id])?;
        // its history rows stay (SPEC §3.6): what it did to the host outlives it
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
        // the next seq and the insert in one statement: two writers (a shelf and the CLI) can't
        // pick the same number
        let seq: i64 = self.conn.query_row(
            "INSERT INTO entries (session_id, seq, ts, kind, text, extra)
             SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4, ?5 FROM entries WHERE session_id = ?1
             RETURNING seq",
            params![session_id, now(), kind, text, extra],
            |r| r.get(0),
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
        let rows = st.query_map(params![session_id], Self::entry_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The last `limit` entries before `before` (all of them when `None`), oldest first: the
    /// conversation is rendered from the end and earlier chunks on demand (SPEC §3.1 history).
    pub fn entries_before(&self, session_id: &str, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        let mut st = self.conn.prepare(
            "SELECT seq, ts, kind, text, extra, tool_kind, detail, diff FROM entries WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![session_id, before.unwrap_or(i64::MAX), limit as i64], Self::entry_row)?;
        let mut v = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        v.reverse();
        Ok(v)
    }

    pub fn entry_count(&self, session_id: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row("SELECT count(*) FROM entries WHERE session_id = ?1", params![session_id], |r| r.get(0))?;
        Ok(n as usize)
    }

    fn entry_row(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
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
        // the tail and the chunks before it, oldest first
        for i in 0..5 {
            store.append("s1", "user", &format!("m{i}"), "").unwrap();
        }
        assert_eq!(store.entry_count("s1").unwrap(), 6);
        let tail = store.entries_before("s1", None, 2).unwrap();
        assert_eq!(tail.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["m3", "m4"]);
        let earlier = store.entries_before("s1", Some(tail[0].seq), 2).unwrap();
        assert_eq!(earlier.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["m1", "m2"]);
        assert_eq!(store.entries_before("s1", Some(earlier[0].seq), 10).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_defaults_seed_new_sessions_which_then_keep_their_own() {
        let dir = std::env::temp_dir().join(format!("sill-store-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.db");
        let store = Store::open(&path).unwrap();
        assert!(store.agent_settings("claude").mode.is_none());
        let mut s = AgentSettings { mode: Some("auto".into()), ..Default::default() };
        s.config.insert("model".into(), "sonnet".into());
        store.set_agent_settings("claude", &s).unwrap();
        s.config.insert("model".into(), "gpt".into());
        store.set_agent_settings("codex", &s).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.agent_settings("claude").mode.as_deref(), Some("auto"));
        assert_eq!(store.agent_settings("claude").config.get("model").map(String::as_str), Some("sonnet"));
        assert_eq!(store.agent_settings("codex").config.get("model").map(String::as_str), Some("gpt"));
        // a new session starts from its agent's default and then keeps its own choices
        let fresh = store.create("claude", "default").unwrap();
        assert_eq!(fresh.agent_settings.config.get("model").map(String::as_str), Some("sonnet"));
        let mut own = fresh.agent_settings.clone();
        own.config.insert("effort".into(), "high".into());
        store.set_session_settings(&fresh.id, &own).unwrap();
        let back = store.sessions().unwrap().into_iter().find(|s| s.id == fresh.id).unwrap();
        assert_eq!(back.agent_settings.config.get("effort").map(String::as_str), Some("high"));
        assert!(store.create("gemini", "default").unwrap().agent_settings.config.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_ends_reopens_and_its_memory_goes_with_it() {
        let dir = std::env::temp_dir().join(format!("sill-store-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("s.db")).unwrap();
        let s = store.create("claude", "default").unwrap();
        let get = |id: &str| store.sessions().unwrap().into_iter().find(|s| s.id == id).unwrap();
        assert!(get(&s.id).ended_at.is_none());
        assert_eq!(store.first_user_text(&s.id).unwrap(), None);
        store.append(&s.id, "agent", "hello", "").unwrap();
        store.append(&s.id, "user", "fix the proxy", "").unwrap();
        store.append(&s.id, "user", "and the tests", "").unwrap();
        assert_eq!(store.first_user_text(&s.id).unwrap().as_deref(), Some("fix the proxy"));
        store.end(&s.id, "fixed the proxy\nin proxy.rs", "agent").unwrap();
        let e = get(&s.id);
        assert!(e.ended_at.is_some());
        assert_eq!((e.summary.as_str(), e.summary_kind.as_str()), ("fixed the proxy\nin proxy.rs", "agent"));
        // reopened: open again, the summary stays until the next end replaces it
        store.reopen(&s.id).unwrap();
        let r = get(&s.id);
        assert!(r.ended_at.is_none());
        assert_eq!(r.summary, "fixed the proxy\nin proxy.rs");
        // named by the person, then by the agent's summary: the last name wins
        store.set_title(&s.id, "my name", "person").unwrap();
        assert_eq!((get(&s.id).title.as_str(), get(&s.id).title_kind.as_str()), ("my name", "person"));
        store.set_title(&s.id, "proxy fix", "agent").unwrap();
        assert_eq!((get(&s.id).title.as_str(), get(&s.id).title_kind.as_str()), ("proxy fix", "agent"));
        store.add_memory(&s.id, "note", "the person's note").unwrap();
        assert_eq!(store.memories(&s.id).unwrap().iter().map(|m| m.2.as_str()).collect::<Vec<_>>(), ["the person's note"]);
        store.delete(&s.id).unwrap();
        assert!(store.memories(&s.id).unwrap().is_empty());
        assert!(store.sessions().unwrap().iter().all(|x| x.id != s.id));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
