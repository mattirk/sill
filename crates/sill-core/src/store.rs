//! Session store (SPEC §3.2, §4.9): sessions and their transcripts in SQLite at
//! `$XDG_STATE_HOME/sill/sill.db`. A session is an isolated conversation with one agent; its jail is
//! the unit `sill-<id>` and its upper trees live under `/var/lib/sill/<uid>/sessions/<id>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

// UTF-8 paths are TEXT; only paths that cannot be represented as UTF-8 use SQLite BLOBs, and
// reads take either.
pub(crate) fn sql_path(path: &Path) -> rusqlite::types::Value {
    use std::os::unix::ffi::OsStrExt;
    match path.to_str() {
        Some(text) => rusqlite::types::Value::Text(text.into()),
        None => rusqlite::types::Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}
pub(crate) fn read_sql_path(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    use rusqlite::types::ValueRef;
    match row.get_ref(column)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into()),
        value => Err(rusqlite::Error::InvalidColumnType(column, "path".into(), value.data_type())),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

pub use sill_proto::wire::AgentSettings;

/// One stored transcript entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub call_id: String,
    pub parent: String,
    /// For `user`, from a frontend's view of the journal (the store keeps names in `extra`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::acp::UserAttachment>,
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

/// A host process the Host control bridge launched for a session.
#[derive(Debug, Clone, PartialEq)]
pub struct HostProcess {
    pub id: String,
    pub session_id: String,
    pub generation: u64,
    pub pid: u32,
    /// The process's start time in clock ticks since boot (`/proc/<pid>/stat`), its identity
    /// together with the pid.
    pub start_ticks: u64,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub started: i64,
    pub ended: Option<i64>,
    pub exit_code: Option<i64>,
    pub kept: bool,
    /// running, exited, stopped, lost (a survivor a later daemon could not find).
    pub state: String,
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
    /// A read-only snapshot reader, without schema setup or writer-side maintenance.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn=Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA query_only=ON; BEGIN")?;
        Ok(Self {conn})
    }
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
        // another writer (a CLI) holding a write: wait a little instead of failing at once (a
        // failure here is the service's crash loop, SPEC §4.11 #17)
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL,
                 created_at INTEGER NOT NULL, last_used INTEGER NOT NULL,
                 acp_session_id TEXT, policy TEXT NOT NULL DEFAULT 'default',
                 auto_allow INTEGER NOT NULL DEFAULT 0, agent_settings TEXT NOT NULL DEFAULT '{}',
                 ended_at INTEGER, summary TEXT NOT NULL DEFAULT '',
                 summary_kind TEXT NOT NULL DEFAULT '', memory_scope TEXT NOT NULL DEFAULT '',
                 follows TEXT NOT NULL DEFAULT '', compacted INTEGER NOT NULL DEFAULT 0,
                 title_kind TEXT NOT NULL DEFAULT '', ghost INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS sessions_creation_order ON sessions(created_at DESC,id DESC);
             CREATE TABLE IF NOT EXISTS session_pins (
                 session_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY (session_id, path));
             CREATE TABLE IF NOT EXISTS session_network_hosts (
                 session_id TEXT NOT NULL, host TEXT NOT NULL, PRIMARY KEY (session_id, host));
             CREATE TABLE IF NOT EXISTS session_permissions (
                 session_id TEXT NOT NULL, capability TEXT NOT NULL, enabled INTEGER NOT NULL,
                 PRIMARY KEY (session_id, capability));
             CREATE TABLE IF NOT EXISTS global_pins (path TEXT PRIMARY KEY, added_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS host_processes (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL, generation INTEGER NOT NULL,
                 pid INTEGER NOT NULL, start_ticks INTEGER NOT NULL, argv TEXT NOT NULL, cwd TEXT NOT NULL,
                 started INTEGER NOT NULL, ended INTEGER, exit_code INTEGER, kept INTEGER NOT NULL DEFAULT 0,
                 state TEXT NOT NULL DEFAULT 'running');
             CREATE TABLE IF NOT EXISTS entries (
                 session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL,
                 kind TEXT NOT NULL, text TEXT NOT NULL, extra TEXT NOT NULL DEFAULT '',
                 tool_kind TEXT NOT NULL DEFAULT '', detail TEXT NOT NULL DEFAULT '',
                 diff TEXT NOT NULL DEFAULT '', call_id TEXT NOT NULL DEFAULT '', parent TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (session_id, seq));
             CREATE TABLE IF NOT EXISTS agent_settings (agent TEXT PRIMARY KEY, settings TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS memories (
                 session_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);",
        )?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
        sill_memory::install(&conn).context("memory index")?;
        // the ledger across time (SPEC §3.6) and the pinned repositories' HEAD at a session's start
        sill_memory::history::install(&conn).context("history")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pin_heads (session_id TEXT NOT NULL, repo TEXT NOT NULL, head TEXT NOT NULL, ts INTEGER NOT NULL,
                 PRIMARY KEY (session_id, repo))",
        )?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS session_names (session_id TEXT PRIMARY KEY, name TEXT NOT NULL)")?;
        sill_memory::install_names(&conn).context("memory name index")?;
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

    pub fn set_auto_allow(&self, id: &str, on: bool) -> Result<()> {
        self.conn.execute("UPDATE sessions SET auto_allow = ?2 WHERE id = ?1", params![id, on as i32])?;
        Ok(())
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path()?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        self.session_query("ORDER BY last_used DESC", rusqlite::params![])
    }

    /// Stable keyset pagination. Rows are live at each request; creation keys never change.
    pub fn sessions_page(&self, after: Option<(i64, &str)>, limit: usize) -> Result<Vec<Session>> {
        let (time,id)=after.unwrap_or((i64::MAX,""));
        self.session_query("WHERE created_at < ?1 OR (created_at = ?1 AND id < ?2) ORDER BY created_at DESC, id DESC LIMIT ?3",
            params![time,id,limit as i64])
    }
    pub fn session_exact(&self, id:&str) -> Result<Option<Session>> {
        Ok(self.session_query("WHERE id = ?1", params![id])?.pop())
    }
    fn session_query(&self, suffix:&str, params: impl rusqlite::Params) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT id, agent, title, created_at, last_used, acp_session_id, policy, auto_allow, agent_settings, ended_at, summary, summary_kind, memory_scope, follows, compacted, title_kind, ghost FROM sessions {suffix}"
        ))?;
        let rows = st.query_map(params, |r| {
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
                ended_at: r.get(9)?,
                summary: r.get(10)?,
                summary_kind: r.get(11)?,
                memory_scope: r.get(12)?,
                follows: r.get(13)?,
                compacted: r.get(14)?,
                title_kind: r.get(15)?,
                ghost: r.get::<_, i32>(16)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn create(&self, agent: &str, policy: &str) -> Result<Session> {
        self.create_with(agent, policy, false)
    }

    /// A new session; `ghost` is fixed from here on (docs/memory.md §3.1).
    pub fn create_with(&self, agent: &str, policy: &str, ghost: bool) -> Result<Session> {
        // Serialize allocation with inserts, including other connections to this store.
        let tx = if self.conn.is_autocommit() {
            Some(rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?)
        } else {
            None // The caller's transaction already covers allocation and both inserts.
        };
        let used = self.conn.prepare("SELECT name FROM session_names")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        let s = Session {
            id: new_id(),
            agent: agent.to_string(),
            title: crate::title::unused_name(&used),
            title_kind: String::new(),
            created_at: now(),
            last_used: now(),
            acp_session_id: None,
            policy: policy.to_string(),
            auto_allow: false,
            agent_settings: self.agent_settings(agent), // the agent type's default
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
        self.conn.execute("INSERT INTO session_names (session_id, name) VALUES (?1, ?2)", params![s.id, s.title])?;
        if let Some(tx) = tx { tx.commit()?; }
        Ok(s)
    }

    /// The session name, initially generated and editable by the person.
    pub fn session_name(&self, id: &str) -> Option<String> {
        self.conn.query_row("SELECT name FROM session_names WHERE session_id = ?1", [id], |r| r.get(0)).ok()
    }

    /// Set the session's name; never replace the summary title. The name is stored and searched
    /// on its own, so summaries do not repeat it.
    pub fn set_name(&self, id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty() && name.chars().count() <= 80 && !name.chars().any(char::is_control), "name must contain 1–80 characters without control characters");
        self.conn.execute("INSERT INTO session_names (session_id, name) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET name = excluded.name", params![id, name])?;
        Ok(())
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

    /// A small key/value the daemon keeps about itself (when the nightly memory run last went).
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
        self.conn.execute("DELETE FROM session_network_hosts WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM session_permissions WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM pin_heads WHERE session_id = ?1", params![id])?;
        // its history rows stay (SPEC §3.6): what it did to the host outlives it
        self.conn.execute("DELETE FROM session_names WHERE session_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Human approvals belong to a session, never the agent's endpoint defaults.
    /// A host process the Host control bridge launched (docs/permission-modes.md): recorded with
    /// its session, generation and identity (pid and start ticks) so a revocation stops it and a
    /// later daemon can tell a survivor from a reused pid.
    pub fn add_host_process(&self, p: &HostProcess) -> Result<()> {
        self.conn.execute(
            "INSERT INTO host_processes (id, session_id, generation, pid, start_ticks, argv, cwd, started, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running')",
            params![p.id, p.session_id, p.generation as i64, p.pid as i64, p.start_ticks as i64,
                    serde_json::to_string(&p.argv)?, p.cwd.to_string_lossy(), p.started],
        )?;
        Ok(())
    }

    /// How a launched host process ended: exited with a code, stopped, or lost (not found again).
    pub fn end_host_process(&self, id: &str, state: &str, exit_code: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE host_processes SET ended = ?2, exit_code = ?3, state = ?4 WHERE id = ?1 AND ended IS NULL",
            params![id, now(), exit_code, state],
        )?;
        Ok(())
    }

    pub fn keep_host_process(&self, id: &str, kept: bool) -> Result<bool> {
        Ok(self.conn.execute("UPDATE host_processes SET kept = ?2 WHERE id = ?1", params![id, kept])? == 1)
    }

    pub fn host_processes(&self, session_id: &str) -> Result<Vec<HostProcess>> {
        let mut st = self.conn.prepare(
            "SELECT id, session_id, generation, pid, start_ticks, argv, cwd, started, ended, exit_code, kept, state
             FROM host_processes WHERE session_id = ?1 ORDER BY started, id")?;
        let rows = st.query_map(params![session_id], |r| {
            let argv: String = r.get(5)?;
            let cwd: String = r.get(6)?;
            Ok(HostProcess {
                id: r.get(0)?, session_id: r.get(1)?, generation: r.get::<_, i64>(2)? as u64,
                pid: r.get::<_, i64>(3)? as u32, start_ticks: r.get::<_, i64>(4)? as u64,
                argv: serde_json::from_str(&argv).unwrap_or_default(), cwd: PathBuf::from(cwd),
                started: r.get(7)?, ended: r.get(8)?, exit_code: r.get(9)?, kept: r.get(10)?, state: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn network_hosts(&self, session_id: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT host FROM session_network_hosts WHERE session_id = ?1 ORDER BY host")?;
        let rows = statement.query_map(params![session_id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn allow_network_host(&self, session_id: &str, host: &str) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        anyhow::ensure!(self.session_exact(session_id)?.is_some(), "session not found");
        anyhow::ensure!(!host.is_empty() && host.len() <= 1024 && !host.chars().any(char::is_control), "invalid host grant");
        self.conn.execute("INSERT OR IGNORE INTO session_network_hosts (session_id, host) VALUES (?1, ?2)", params![session_id, host])?;
        let count: i64 = self.conn.query_row("SELECT count(*) FROM session_network_hosts WHERE session_id = ?1", params![session_id], |row| row.get(0))?;
        anyhow::ensure!(count <= 128, "at most 128 session host grants");
        transaction.commit()?;
        Ok(())
    }

    /// Remembered capability choices: a preference for this session's next builds, never authority.
    pub fn permission_choices(&self, session_id: &str) -> Result<std::collections::BTreeMap<String, bool>> {
        let mut statement = self.conn.prepare("SELECT capability, enabled FROM session_permissions WHERE session_id = ?1")?;
        let rows = statement.query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `None` returns the capability to its policy default. Host control is an activation, not a choice.
    pub fn set_permission_choice(&self, session_id: &str, capability: &str, enabled: Option<bool>) -> Result<()> {
        anyhow::ensure!(crate::permissions::Permissions::default().rule(capability).is_some(), "unknown capability {capability:?}");
        anyhow::ensure!(capability != "host_control", "Host control is never a saved session choice");
        let transaction = self.conn.unchecked_transaction()?;
        anyhow::ensure!(self.session_exact(session_id)?.is_some(), "session not found");
        match enabled {
            Some(enabled) => self.conn.execute(
                "INSERT INTO session_permissions (session_id, capability, enabled) VALUES (?1, ?2, ?3)
                 ON CONFLICT (session_id, capability) DO UPDATE SET enabled = excluded.enabled",
                params![session_id, capability, enabled])?,
            None => self.conn.execute("DELETE FROM session_permissions WHERE session_id = ?1 AND capability = ?2", params![session_id, capability])?,
        };
        transaction.commit()?;
        Ok(())
    }

    /// Restore defaults: remove added capability and host grants; policy and pins stay selected.
    pub fn restore_default_permissions(&self, session_id: &str) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        anyhow::ensure!(self.session_exact(session_id)?.is_some(), "session not found");
        self.conn.execute("DELETE FROM session_permissions WHERE session_id = ?1", params![session_id])?;
        self.conn.execute("DELETE FROM session_network_hosts WHERE session_id = ?1", params![session_id])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pins(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let mut st = self.conn.prepare("SELECT path FROM session_pins WHERE session_id = ?1 ORDER BY path")?;
        let rows = st.query_map(params![session_id], |r| read_sql_path(r, 0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn pin(&self, session_id: &str, path: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_pins (session_id, path) VALUES (?1, ?2)",
            params![session_id, sql_path(path)],
        )?;
        Ok(())
    }

    /// The user's global pins (docs/pins.md): bound in every non-ghost jail, never a chip.
    pub fn global_pins(&self) -> Result<Vec<PathBuf>> {
        let mut st = self.conn.prepare("SELECT path FROM global_pins ORDER BY path")?;
        let rows = st.query_map([], |r| read_sql_path(r, 0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn global_pin(&self, path: &Path) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO global_pins (path, added_at) VALUES (?1, ?2)", params![sql_path(path), now()])?;
        Ok(())
    }

    pub fn global_unpin(&self, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM global_pins WHERE path = ?1", params![sql_path(path)])?;
        Ok(())
    }

    /// What a session's jail binds: its own pins, then the global ones it does not already hold (a
    /// session pin inside a global folder adds nothing, the global folder covers it). The session's
    /// own pins alone are its focus (chips, instructions, memory scope).
    pub fn jail_pins(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let mut out = self.pins(session_id)?;
        for g in self.global_pins()? {
            if !out.contains(&g) {
                out.push(g);
            }
        }
        // a workspace session never gets its source, nor a pin inside it (docs/parallel-coding.md §5);
        // an ancestor pin stays, the jail masks the source within it
        if let Some(source) = crate::workspace::records::session_source(&self.conn, session_id)? {
            out.retain(|p| !p.starts_with(&source));
        }
        Ok(out)
    }

    /// A workspace session's source checkout and object store, for its jail (§4–5).
    pub fn workspace_mask(&self, session_id: &str) -> Result<Option<crate::agent_storage::WorkspaceMask>> {
        Ok(crate::workspace::records::session_source(&self.conn, session_id)?.map(|source| crate::agent_storage::WorkspaceMask {
            objects: sill_proto::sessions::HostPath::from_path(&source.join(".git/objects")),
            source: sill_proto::sessions::HostPath::from_path(&source),
        }))
    }

    pub fn unpin(&self, session_id: &str, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM session_pins WHERE session_id = ?1 AND path = ?2", params![session_id, sql_path(path)])?;
        Ok(())
    }

    pub fn append(&self, session_id: &str, kind: &str, text: &str, extra: &str) -> Result<i64> {
        // the next seq and the insert in one statement: two writers (the daemon and a CLI) can't
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

    /// Keep adapter identities so a reopened conversation can rebuild delegation trees.
    pub fn set_tool_identity(&self, session_id: &str, seq: i64, call_id: &str, parent: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE entries SET call_id = ?3, parent = ?4 WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq, call_id, parent.unwrap_or("")])?;
        Ok(())
    }

    pub fn append_subagent_text(&self, session_id: &str, parent: &str, text: &str) -> Result<()> {
        let seq = self.append(session_id, "subagent", text, parent)?;
        self.set_tool_identity(session_id, seq, "", Some(parent))
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
        let mut st = self.conn.prepare("SELECT seq, ts, kind, text, extra, tool_kind, detail, diff, call_id, parent FROM entries WHERE session_id = ?1 ORDER BY seq")?;
        let rows = st.query_map(params![session_id], Self::entry_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The last `limit` entries before `before` (all of them when `None`), oldest first: the
    /// conversation is rendered from the end and earlier chunks on demand (SPEC §3.1 history).
    pub fn entries_before(&self, session_id: &str, before: Option<i64>, limit: usize) -> Result<Vec<Entry>> {
        let mut st = self.conn.prepare(
            "SELECT seq, ts, kind, text, extra, tool_kind, detail, diff, call_id, parent FROM entries WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![session_id, before.unwrap_or(i64::MAX), limit as i64], Self::entry_row)?;
        let mut v = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        v.reverse();
        // A page must include the delegations its children refer to. Otherwise the UI would
        // drop their text or render their calls as root steps at a history boundary.
        loop {
            let Some(first) = v.first().map(|e| e.seq) else { break };
            let mut start = first;
            for e in &v {
                if !e.parent.is_empty() {
                    let parent_seq: Option<i64> = self.conn.query_row(
                        "SELECT MIN(seq) FROM entries WHERE session_id = ?1 AND call_id = ?2",
                        params![session_id, e.parent], |r| r.get(0))?;
                    if let Some(seq) = parent_seq { start = start.min(seq); }
                }
            }
            if start == first { break; }
            let mut st = self.conn.prepare("SELECT seq, ts, kind, text, extra, tool_kind, detail, diff, call_id, parent FROM entries WHERE session_id = ?1 AND seq >= ?2 AND seq < ?3 ORDER BY seq")?;
            let mut prefix = st.query_map(params![session_id, start, first], Self::entry_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
            prefix.append(&mut v);
            v = prefix;
        }
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
            call_id: r.get(8)?,
            parent: r.get(9)?,
            attachments: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_preserve_non_utf8_bytes_and_text_rows_across_reopen() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("sill-store-byte-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("store.db");
        let exact = PathBuf::from(std::ffi::OsString::from_vec(b"/pin/\xff".to_vec()));
        let display_collision = PathBuf::from("/pin/\u{fffd}");
        {
            let store = Store::open(&file).unwrap();
            store.pin("s1", &exact).unwrap();
            store.pin("s1", &display_collision).unwrap();
            store.pin("s1", &exact).unwrap();
            store.global_pin(&exact).unwrap();
            store.global_pin(&display_collision).unwrap();
        }
        let store = Store::open(&file).unwrap();
        let pins = store.pins("s1").unwrap();
        assert_eq!(pins.len(), 2);
        assert!(pins.contains(&exact));
        assert!(pins.contains(&display_collision));
        assert_eq!(store.conn().query_row("SELECT typeof(path) FROM session_pins WHERE path = ?1", ["/pin/\u{fffd}"], |r| r.get::<_, String>(0)).unwrap(), "text");
        store.unpin("s1", &exact).unwrap();
        store.global_unpin(&exact).unwrap();
        assert_eq!(store.pins("s1").unwrap(), vec![display_collision.clone()]);
        assert_eq!(store.global_pins().unwrap(), vec![display_collision]);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_pins_join_a_sessions_jail_but_not_its_focus() {
        let dir = std::env::temp_dir().join(format!("sill-store-gpins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("s.db")).unwrap();
        let (g, own) = (Path::new("/tmp/g"), Path::new("/tmp/own"));
        store.global_pin(g).unwrap();
        store.global_pin(g).unwrap(); // once
        store.pin("s1", own).unwrap();
        assert_eq!(store.pins("s1").unwrap(), vec![own.to_path_buf()]);
        assert_eq!(store.jail_pins("s1").unwrap(), vec![own.to_path_buf(), g.to_path_buf()]);
        // pinned in the session too: once in the jail list, in the session's place
        store.pin("s1", g).unwrap();
        assert_eq!(store.jail_pins("s1").unwrap(), vec![g.to_path_buf(), own.to_path_buf()]);
        store.global_unpin(g).unwrap();
        assert!(store.global_pins().unwrap().is_empty());
        assert_eq!(store.jail_pins("s1").unwrap(), vec![g.to_path_buf(), own.to_path_buf()]); // the session pin stays
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delegation_survives_reopen_and_history_boundary() {
        let dir = std::env::temp_dir().join(format!("sill-delegation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.db");
        {
            let store = Store::open(&path).unwrap();
            let seq = store.append_tool("s", "Research", "InProgress", "Delegate", None, None).unwrap();
            store.set_tool_identity("s", seq, "task-1", None).unwrap();
            let seq = store.append_tool("s", "Read", "Completed", "Read", None, None).unwrap();
            store.set_tool_identity("s", seq, "read-1", Some("task-1")).unwrap();
            store.append_subagent_text("s", "task-1", "Found it").unwrap();
        }
        let store = Store::open(&path).unwrap();
        let entries = store.entries_before("s", None, 1).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].call_id, "task-1");
        assert_eq!(entries[1].parent, "task-1");
        assert_eq!((entries[2].kind.as_str(), entries[2].parent.as_str(), entries[2].text.as_str()), ("subagent", "task-1", "Found it"));
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn generated_names_avoid_stored_names_inside_and_outside_transactions() {
        let dir = std::env::temp_dir().join(format!("sill-name-allocation-{}", new_id()));
        let store = Store::open(&dir.join("store.db")).unwrap();
        let tx = store.conn().unchecked_transaction().unwrap();
        {
            let mut insert = store.conn().prepare("INSERT INTO session_names VALUES (?1, ?2)").unwrap();
            for index in 0..60_000 {
                insert.execute(params![format!("reserved-{index}"), crate::title::base_name(index)]).unwrap();
            }
        }
        let first = store.create("test", "default").unwrap();
        assert!(first.title.ends_with("-2"));
        assert_eq!(store.session_name(&first.id).as_deref(), Some(first.title.as_str()));
        tx.commit().unwrap();
        let second = store.create("test", "default").unwrap();
        assert_ne!(first.title, second.title);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn original_name_survives_renaming_and_reopen_without_duplicate_signatures() {
        let dir = std::env::temp_dir().join(format!("sill-original-name-{}", std::process::id()));
        let path = dir.join("store.db");
        let store = Store::open(&path).unwrap();
        let session = store.create("test", "default").unwrap();
        store.set_title(&session.id, "A new title", "person").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.session_name(&session.id).as_deref(), Some(session.title.as_str()));
        store.set_summary(&session.id, "Work completed", "agent").unwrap();
        assert_eq!(store.session_exact(&session.id).unwrap().unwrap().summary, "Work completed", "the name is not repeated in the summary");
        store.set_name(&session.id, "human choice").unwrap();
        let renamed = store.session_exact(&session.id).unwrap().unwrap();
        assert_eq!(renamed.title, "A new title");
        assert_eq!(renamed.summary, "Work completed");
        // the name stays findable through memory search instead
        let found: String = store.conn().query_row("SELECT t.session_id FROM memory_fts JOIN memory_text t ON t.id = memory_fts.rowid WHERE memory_fts MATCH 'choice' AND t.kind = 'name'", [], |r| r.get(0)).unwrap();
        assert_eq!(found, session.id);
        assert_eq!(store.session_name(&session.id).as_deref(), Some("human choice"));
        for invalid in ["", "  ", "two\nlines", "bad\tname"] { assert!(store.set_name(&session.id, invalid).is_err()); }
        assert!(store.set_name(&session.id, &"a".repeat(81)).is_err());
        store.delete(&session.id).unwrap();
        assert_eq!(store.session_name(&session.id), None);
        let _ = std::fs::remove_dir_all(dir);
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
        assert_eq!(e.summary, "fixed the proxy\nin proxy.rs");
        assert_eq!(e.summary_kind, "agent");
        // reopened: open again, the summary stays until the next end replaces it
        store.reopen(&s.id).unwrap();
        let r = get(&s.id);
        assert!(r.ended_at.is_none());
        assert_eq!(r.summary, e.summary);
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
    #[test]
    fn host_grants_are_bounded_persistent_and_session_owned() {
        let dir = std::env::temp_dir().join(format!("sill-host-grants-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        let store = Store::open(&path).unwrap();
        let first = store.create("codex", "default").unwrap();
        let second = store.create("codex", "default").unwrap();
        for n in 0..128 { store.allow_network_host(&first.id, &format!("h{n}.example.com")).unwrap(); }
        store.allow_network_host(&first.id, "h0.example.com").unwrap();
        assert!(store.allow_network_host(&first.id, "overflow.example.com").is_err());
        assert!(store.allow_network_host("missing", "example.com").is_err());
        assert!(store.network_hosts(&second.id).unwrap().is_empty());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.network_hosts(&first.id).unwrap().len(), 128);
        assert!(!store.network_hosts(&first.id).unwrap().contains(&"overflow.example.com".into()));
        store.delete(&first.id).unwrap();
        assert!(store.network_hosts(&first.id).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn permission_choices_persist_per_session_and_restore_defaults_keeps_pins() {
        let dir = std::env::temp_dir().join(format!("sill-permission-choices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        let store = Store::open(&path).unwrap();
        let first = store.create("codex", "custom").unwrap();
        let second = store.create("codex", "custom").unwrap();
        store.set_permission_choice(&first.id, "public_web", Some(true)).unwrap();
        store.set_permission_choice(&first.id, "test_desktop", Some(false)).unwrap();
        store.set_permission_choice(&first.id, "public_web", Some(false)).unwrap();
        assert!(store.set_permission_choice(&first.id, "host_control", Some(true)).is_err());
        assert!(store.set_permission_choice(&first.id, "unknown", Some(true)).is_err());
        assert!(store.set_permission_choice("missing", "gpu", Some(true)).is_err());
        assert!(store.permission_choices(&second.id).unwrap().is_empty());
        store.allow_network_host(&first.id, "example.com").unwrap();
        store.pin(&first.id, Path::new("/tmp/project")).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let remembered = store.permission_choices(&first.id).unwrap();
        assert_eq!(remembered.get("public_web"), Some(&false));
        assert_eq!(remembered.get("test_desktop"), Some(&false));
        store.set_permission_choice(&first.id, "test_desktop", None).unwrap();
        assert_eq!(store.permission_choices(&first.id).unwrap().len(), 1);
        store.restore_default_permissions(&first.id).unwrap();
        assert!(store.permission_choices(&first.id).unwrap().is_empty());
        assert!(store.network_hosts(&first.id).unwrap().is_empty());
        assert_eq!(store.pins(&first.id).unwrap(), [PathBuf::from("/tmp/project")]);
        assert_eq!(store.session_exact(&first.id).unwrap().unwrap().policy, "custom");
        store.set_permission_choice(&first.id, "gpu", Some(false)).unwrap();
        store.delete(&first.id).unwrap();
        assert!(store.permission_choices(&first.id).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

}
