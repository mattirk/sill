//! Memory queries (docs/memory.md §2.3), the same on the host and in the jail: the host runs them
//! on `sill.db`, the jail's `sill-memory` on the read-only snapshot the shelf writes
//! ([`write_snapshot`]) and the jail builder binds at [`JAIL_DIR`]. Both databases have the tables
//! the queries read under the same names — `sessions`, `session_pins`, `memories`, `entries`, and the
//! searchable text with its index ([`TEXT_SCHEMA`]) — the snapshot only a subset of their columns.
//!
//! Search is SQLite FTS5 over the searchable text: titles, summaries, notes and the person's prompts
//! (decision 14: not agent text — `transcript --grep` reads that linearly). Recall is by folder:
//! [`recall_pick`] chooses which ended sessions the next agent is told about.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};

/// Where the jail sees the snapshot's directory (read-only). A directory, not the file: the shelf
/// replaces the file by rename, and a bind of the directory shows the new one to running sessions.
pub const JAIL_DIR: &str = "/.sill/memory";
/// The snapshot's file name, in its directory on both sides.
pub const SNAPSHOT_FILE: &str = "memory.db";

/// The searchable text — one row per session title, summary, note and prompt of the person — and
/// its FTS5 index (external content: the index stores no second copy). `kind` is `title`,
/// `summary`, `note` or `prompt`; `ref` tells rows of one kind apart (an entry's seq, a note's ts).
pub const TEXT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_text (
    id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, ref INTEGER NOT NULL DEFAULT 0, text TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS memory_text_session ON memory_text (session_id, kind, ref);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5 (
    text, content = 'memory_text', content_rowid = 'id', tokenize = 'unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS memory_text_ai AFTER INSERT ON memory_text BEGIN
    INSERT INTO memory_fts (rowid, text) VALUES (new.id, new.text); END;
CREATE TRIGGER IF NOT EXISTS memory_text_ad AFTER DELETE ON memory_text BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, text) VALUES ('delete', old.id, old.text); END;
";

/// What keeps [`TEXT_SCHEMA`] in step with `sill.db`'s own tables: triggers, so every path that
/// writes a prompt, a summary, a note or a title (or deletes a session) updates the index without
/// knowing it exists.
pub const STORE_TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS memory_prompt_ai AFTER INSERT ON entries WHEN new.kind = 'user' BEGIN
    INSERT INTO memory_text (session_id, kind, ref, text) VALUES (new.session_id, 'prompt', new.seq, new.text); END;
CREATE TRIGGER IF NOT EXISTS memory_prompt_ad AFTER DELETE ON entries WHEN old.kind = 'user' BEGIN
    DELETE FROM memory_text WHERE session_id = old.session_id AND kind = 'prompt' AND ref = old.seq; END;
CREATE TRIGGER IF NOT EXISTS memory_title_ai AFTER INSERT ON sessions BEGIN
    INSERT INTO memory_text (session_id, kind, ref, text) VALUES (new.id, 'title', 0, new.title); END;
CREATE TRIGGER IF NOT EXISTS memory_title_au AFTER UPDATE OF title ON sessions BEGIN
    UPDATE memory_text SET text = new.title WHERE session_id = new.id AND kind = 'title'; END;
CREATE TRIGGER IF NOT EXISTS memory_summary_au AFTER UPDATE OF summary ON sessions WHEN new.summary IS NOT old.summary BEGIN
    DELETE FROM memory_text WHERE session_id = new.id AND kind = 'summary';
    INSERT INTO memory_text (session_id, kind, ref, text) SELECT new.id, 'summary', 0, new.summary WHERE new.summary <> ''; END;
CREATE TRIGGER IF NOT EXISTS memory_note_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memory_text (session_id, kind, ref, text) VALUES (new.session_id, new.kind, new.ts, new.text); END;
CREATE TRIGGER IF NOT EXISTS memory_note_ad AFTER DELETE ON memories BEGIN
    DELETE FROM memory_text WHERE session_id = old.session_id AND kind = old.kind AND ref = old.ts AND text = old.text; END;
CREATE TRIGGER IF NOT EXISTS memory_session_ad AFTER DELETE ON sessions BEGIN
    DELETE FROM memory_text WHERE session_id = old.id; END;
";

/// An UPDATE OF title on memory_text needs the index told too: FTS5 external content does not
/// follow updates of its content table by itself.
const TEXT_UPDATE_TRIGGER: &str = "
CREATE TRIGGER IF NOT EXISTS memory_text_au AFTER UPDATE ON memory_text BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO memory_fts (rowid, text) VALUES (new.id, new.text); END;
";

/// Set the searchable text up in `sill.db` (idempotent). The first time, it is filled from what
/// the store already holds, so sessions from before memory are searchable too.
pub fn install(conn: &Connection) -> Result<()> {
    let existed: bool = conn.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'memory_text'", [], |r| r.get::<_, i64>(0)).map(|n| n > 0)?;
    conn.execute_batch(TEXT_SCHEMA)?;
    conn.execute_batch(TEXT_UPDATE_TRIGGER)?;
    conn.execute_batch(STORE_TRIGGERS)?;
    if !existed {
        conn.execute_batch(
            "INSERT INTO memory_text (session_id, kind, ref, text) SELECT id, 'title', 0, title FROM sessions;
             INSERT INTO memory_text (session_id, kind, ref, text) SELECT id, 'summary', 0, summary FROM sessions WHERE summary <> '';
             INSERT INTO memory_text (session_id, kind, ref, text) SELECT session_id, kind, ts, text FROM memories;
             INSERT INTO memory_text (session_id, kind, ref, text) SELECT session_id, 'prompt', seq, text FROM entries WHERE kind = 'user';",
        )?;
    }
    Ok(())
}

/// Write the jail's snapshot of `src` (an open `sill.db`) into `dir`: a new file, renamed over the
/// old one, so a reader never sees half of it. Sessions' metadata, pins, summaries, notes, the
/// person's prompts and the agents' text and tool titles (for `transcript`; not tool details or
/// diffs), the searchable text and its index; `kept` adds what each session kept to the host
/// (session id, path, what), which lives in journals outside the db.
pub fn write_snapshot(src: &Path, dir: &Path, kept: &[(String, PathBuf, String)]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = dir.join(format!(".{SNAPSHOT_FILE}.{}.{nanos}", std::process::id()));
    let build = || -> Result<()> {
        let snap = Connection::open(&tmp)?;
        snap.execute_batch(
            "PRAGMA journal_mode = DELETE;
             CREATE TABLE sessions (id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL, created_at INTEGER NOT NULL,
                 last_used INTEGER NOT NULL, ended_at INTEGER, summary TEXT NOT NULL, summary_kind TEXT NOT NULL);
             CREATE TABLE session_pins (session_id TEXT NOT NULL, path TEXT NOT NULL);
             CREATE TABLE memories (session_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);
             CREATE TABLE entries (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq));
             CREATE TABLE kept (session_id TEXT NOT NULL, path TEXT NOT NULL, what TEXT NOT NULL);",
        )?;
        snap.execute_batch(TEXT_SCHEMA)?;
        snap.execute("ATTACH DATABASE ?1 AS src", params![format!("file:{}?mode=ro", src.display())]).or_else(|_| snap.execute("ATTACH DATABASE ?1 AS src", params![src.to_string_lossy()]))?;
        let tx = snap.unchecked_transaction()?;
        tx.execute_batch(
            "INSERT INTO sessions SELECT id, agent, title, created_at, last_used, ended_at, summary, summary_kind FROM src.sessions;
             INSERT INTO session_pins SELECT session_id, path FROM src.session_pins;
             INSERT INTO memories SELECT session_id, ts, kind, text FROM src.memories;
             INSERT INTO entries SELECT session_id, seq, ts, kind, text FROM src.entries WHERE kind IN ('user', 'agent', 'tool');
             INSERT INTO memory_text (session_id, kind, ref, text) SELECT session_id, kind, ref, text FROM src.memory_text;",
        )?;
        {
            let mut st = tx.prepare("INSERT INTO kept VALUES (?1, ?2, ?3)")?;
            for (id, path, what) in kept {
                st.execute(params![id, path.to_string_lossy(), what])?;
            }
        }
        tx.commit()?;
        snap.execute("DETACH DATABASE src", [])?;
        Ok(())
    };
    let r = build();
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let out = dir.join(SNAPSHOT_FILE);
    {
        use std::os::unix::fs::PermissionsExt; // owner only, like sill.db (security.md S21)
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &out).with_context(|| format!("rename to {}", out.display()))?;
    Ok(out)
}

/// The snapshot, opened read-only and immutable (the directory is read-only in the jail: no
/// journal, no lock files); None when there is none.
pub fn open_snapshot(dir: &Path) -> Result<Option<Connection>> {
    let path = dir.join(SNAPSHOT_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let uri = format!("file:{}?immutable=1", path.display());
    Ok(Some(Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?))
}

// --- sessions -------------------------------------------------------------------------------------

/// A session as memory sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub created_at: i64,
    pub last_used: i64,
    pub ended_at: Option<i64>,
    pub summary: String,
    pub summary_kind: String,
    pub pins: Vec<PathBuf>,
}

const SESSION_COLS: &str = "id, agent, title, created_at, last_used, ended_at, summary, summary_kind";

fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        id: r.get(0)?,
        agent: r.get(1)?,
        title: r.get(2)?,
        created_at: r.get(3)?,
        last_used: r.get(4)?,
        ended_at: r.get(5)?,
        summary: r.get(6)?,
        summary_kind: r.get(7)?,
        pins: Vec::new(),
    })
}

fn pins_of(conn: &Connection, id: &str) -> Result<Vec<PathBuf>> {
    let mut st = conn.prepare_cached("SELECT path FROM session_pins WHERE session_id = ?1 ORDER BY path")?;
    let rows = st.query_map(params![id], |r| r.get::<_, String>(0).map(PathBuf::from))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Every session with its pins, the most recently used first.
pub fn sessions(conn: &Connection) -> Result<Vec<SessionInfo>> {
    let mut st = conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY last_used DESC"))?;
    let mut out: Vec<SessionInfo> = st.query_map([], session_row)?.collect::<std::result::Result<_, _>>()?;
    for s in &mut out {
        s.pins = pins_of(conn, &s.id)?;
    }
    Ok(out)
}

/// A session by id, by a unique id prefix (four characters or more), or by its exact title.
pub fn find_session(conn: &Connection, key: &str) -> Result<Option<SessionInfo>> {
    let key = key.trim();
    let one = |sql: &str, arg: &str| -> Result<Vec<SessionInfo>> {
        let mut st = conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE {sql} ORDER BY last_used DESC LIMIT 2"))?;
        Ok(st.query_map(params![arg], session_row)?.collect::<std::result::Result<_, _>>()?)
    };
    let mut found = one("id = ?1", key)?;
    if found.is_empty() && key.len() >= 4 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        found = one("substr(id, 1, length(?1)) = ?1", key)?;
        if found.len() > 1 {
            bail!("{key} is the start of more than one session id");
        }
    }
    if found.is_empty() {
        found = one("title = ?1", key)?;
    }
    Ok(match found.into_iter().next() {
        Some(mut s) => {
            s.pins = pins_of(conn, &s.id)?;
            Some(s)
        }
        None => None,
    })
}

/// What a snapshot says a session kept (the host reads the journals instead).
pub fn snapshot_kept(conn: &Connection, id: &str) -> Vec<(PathBuf, String)> {
    let Ok(mut st) = conn.prepare("SELECT path, what FROM kept WHERE session_id = ?1") else { return Vec::new() };
    st.query_map(params![id], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, String>(1)?)))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// (ts, kind, text) of a session's notes, oldest first.
pub fn notes(conn: &Connection, id: &str) -> Result<Vec<(i64, String, String)>> {
    let mut st = conn.prepare("SELECT ts, kind, text FROM memories WHERE session_id = ?1 ORDER BY ts")?;
    let rows = st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn count_kind(conn: &Connection, id: &str, kind: &str) -> i64 {
    conn.query_row("SELECT count(*) FROM entries WHERE session_id = ?1 AND kind = ?2", params![id, kind], |r| r.get(0)).unwrap_or(0)
}

// --- time and paths -------------------------------------------------------------------------------

/// `YYYY-MM-DD` of a Unix time, UTC (no timezone data here; a day is what memory needs).
pub fn date(ts: i64) -> String {
    let (y, m, d) = civil(ts);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM` of a Unix time, UTC.
pub fn datetime(ts: i64) -> String {
    let s = ts.rem_euclid(86_400);
    format!("{} {:02}:{:02}", date(ts), s / 3600, s / 60 % 60)
}

fn civil(ts: i64) -> (i64, i64, i64) {
    // civil-from-days (Howard Hinnant), days since 1970-01-01
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A length of time the way a list says it: `5 min`, `2 h`, `3 d`.
pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 90 * 60 {
        format!("{} min", (secs + 59) / 60)
    } else if secs < 48 * 3600 {
        format!("{} h", (secs + 1800) / 3600)
    } else {
        format!("{} d", (secs + 43_200) / 86_400)
    }
}

/// `30d`, `12h`, `2w`, `90m` (plain digits are days) → seconds.
pub fn parse_age(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().ok()?;
    Some(n * match unit {
        "" | "d" => 86_400,
        "h" => 3600,
        "m" | "min" => 60,
        "w" => 7 * 86_400,
        "y" => 365 * 86_400,
        _ => return None,
    })
}

/// `~` for the home directory.
pub fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) if !home.as_os_str().is_empty() => format!("~/{}", rel.display()),
        _ => p.display().to_string(),
    }
}

fn untilde(s: &str, home: &Path) -> PathBuf {
    match s.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(s),
    }
}

fn normal(p: &Path) -> Vec<Component<'_>> {
    p.components().filter(|c| !matches!(c, Component::CurDir)).collect()
}

/// How far apart two folders are when one holds the other: 0 the same, 1 a parent or a child, …;
/// None when neither holds the other.
pub fn folder_distance(a: &Path, b: &Path) -> Option<usize> {
    let (a, b) = (normal(a), normal(b));
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    (long[..short.len()] == short[..]).then(|| long.len() - short.len())
}

/// The nearest of a session's pins to any of `folders`.
fn nearest(pins: &[PathBuf], folders: &[PathBuf]) -> Option<usize> {
    pins.iter().flat_map(|p| folders.iter().filter_map(move |f| folder_distance(p, f))).min()
}

// --- recall ---------------------------------------------------------------------------------------

/// Which earlier sessions a session's agent is told about (docs/memory.md §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Sessions in the same folders, their parents and children — nearest first, then newest.
    Here,
    /// The newest anywhere.
    Everywhere,
    Off,
}

impl Scope {
    /// The stored value (`here`, `everywhere`, `off`; anything else is the default), resolved
    /// against the session's pins: pinned → here, unpinned → everywhere (the global session).
    pub fn resolve(stored: &str, has_pins: bool) -> Scope {
        match stored {
            "here" => Scope::Here,
            "everywhere" => Scope::Everywhere,
            "off" => Scope::Off,
            _ if has_pins => Scope::Here,
            _ => Scope::Everywhere,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Scope::Here => "here",
            Scope::Everywhere => "everywhere",
            Scope::Off => "off",
        }
    }
}

/// The ended sessions to recall for session `me` with `pins`, at most `n`: `first` (the session a
/// *New here* came from) leads when it is ended; never `me` itself.
pub fn recall_pick<'a>(all: &'a [SessionInfo], me: &str, pins: &[PathBuf], scope: Scope, n: usize, first: Option<&str>) -> Vec<&'a SessionInfo> {
    if scope == Scope::Off || n == 0 {
        return Vec::new();
    }
    let mut pool: Vec<(usize, &SessionInfo)> = all
        .iter()
        .filter(|s| s.id != me && s.ended_at.is_some() && !s.summary.trim().is_empty())
        .filter_map(|s| match scope {
            Scope::Here if pins.is_empty() => None,
            Scope::Here => nearest(&s.pins, pins).map(|d| (d, s)),
            _ => Some((0, s)),
        })
        .collect();
    pool.sort_by(|(da, a), (db, b)| {
        let lead = |s: &SessionInfo| Some(s.id.as_str()) != first; // false sorts first
        (lead(a), da, std::cmp::Reverse(a.ended_at)).cmp(&(lead(b), db, std::cmp::Reverse(b.ended_at)))
    });
    if let Some(f) = first
        && !pool.iter().any(|(_, s)| s.id == f)
        && let Some(s) = all.iter().find(|s| s.id == f && s.ended_at.is_some() && !s.summary.trim().is_empty())
    {
        pool.insert(0, (0, s));
    }
    pool.into_iter().take(n).map(|(_, s)| s).collect()
}

/// The most of one summary a recall block quotes.
const RECALL_CHARS: usize = 1500;

/// The recall block for the standing instructions; `kept_count` says how many files a session kept.
/// Empty when there is nothing to say and the snapshot is not there to search either.
pub fn recall_block(picked: &[&SessionInfo], scope: Scope, searchable: bool, home: &Path, kept_count: impl Fn(&str) -> usize) -> String {
    let tools = "`sill-memory search WORDS [--folder PATH] [--since 30d]` finds more, `sill-memory show ID` prints a whole summary, `sill-memory transcript ID [--grep WORDS]` the conversation";
    if picked.is_empty() {
        return if searchable { format!("[Earlier sessions on this machine are remembered by sill (not your own memory): {tools}.]") } else { String::new() };
    }
    let mut out = String::from(match scope {
        Scope::Here => "[Memory — earlier sessions in these folders (sill remembers sessions on this machine; this is not your own memory):\n",
        _ => "[Memory — the latest sessions on this machine (sill remembers sessions on this machine; this is not your own memory):\n",
    });
    for s in picked {
        let ended = s.ended_at.unwrap_or(s.last_used);
        let place = if s.pins.is_empty() { String::new() } else { format!(" in {}", s.pins.iter().map(|p| tilde(p, home)).collect::<Vec<_>>().join(", ")) };
        let kept = match kept_count(&s.id) {
            0 => String::new(),
            1 => ", kept 1 file".to_string(),
            n => format!(", kept {n} files"),
        };
        out.push_str(&format!("- {}, {}, {}{place}{kept} — “{}” ({}):\n", date(ended), duration(ended - s.created_at), s.agent, s.title, s.id));
        // 15 lines and 1500 characters at most: agents write paragraphs as often as lines
        let mut budget = RECALL_CHARS;
        for l in s.summary.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).take(15) {
            if budget == 0 {
                break;
            }
            let n = l.chars().count();
            if n <= budget {
                out.push_str(&format!("  {l}\n"));
                budget -= n;
            } else {
                let cut: String = l.chars().take(budget).collect();
                out.push_str(&format!("  {cut}… (`sill-memory show {}` has the rest)\n", s.id));
                budget = 0;
            }
        }
    }
    if searchable {
        out.push_str(&format!("{tools}.]"));
    } else {
        out.pop();
        out.push(']');
    }
    out
}

// --- search ---------------------------------------------------------------------------------------

/// A search as typed: words, plus `folder:PATH`, `agent:NAME`, `since:30d` anywhere among them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub words: Vec<String>,
    pub folder: Option<PathBuf>,
    pub agent: Option<String>,
    /// Seconds back from now.
    pub since: Option<i64>,
}

impl Query {
    pub fn parse(text: &str, home: &Path) -> Query {
        let mut q = Query::default();
        for w in text.split_whitespace() {
            if let Some(f) = w.strip_prefix("folder:").filter(|f| !f.is_empty()) {
                q.folder = Some(untilde(f, home));
            } else if let Some(a) = w.strip_prefix("agent:").filter(|a| !a.is_empty()) {
                q.agent = Some(a.to_string());
            } else if let Some(s) = w.strip_prefix("since:").and_then(parse_age) {
                q.since = Some(s);
            } else {
                q.words.push(w.to_string());
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.folder.is_none() && self.agent.is_none() && self.since.is_none()
    }

    /// The words as an FTS5 expression: every word must occur, each as a prefix, quoted so that
    /// nothing the person types is FTS syntax.
    fn fts(&self) -> Option<String> {
        let terms: Vec<String> = self
            .words
            .iter()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
            .collect();
        (!terms.is_empty()).then(|| terms.join(" "))
    }

    fn admits(&self, s: &SessionInfo, now: i64) -> bool {
        if let Some(a) = &self.agent
            && !s.agent.eq_ignore_ascii_case(a)
        {
            return false;
        }
        if let Some(f) = &self.folder
            && nearest(&s.pins, std::slice::from_ref(f)).is_none()
        {
            return false;
        }
        if let Some(back) = self.since
            && s.ended_at.unwrap_or(s.last_used) < now - back
        {
            return false;
        }
        true
    }
}

/// One session found, with where the words matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub session: SessionInfo,
    /// What matched: `title`, `summary`, `note`, `prompt`; empty when only filters were given.
    pub kind: String,
    /// The matched text around the words, the words in [brackets]; empty when only filters.
    pub snippet: String,
}

/// Sessions matching `q`, best match first (FTS5 rank); with no words, the filters alone over every
/// session, most recently used first. At most `limit`.
pub fn search(conn: &Connection, q: &Query, now: i64, limit: usize) -> Result<Vec<Hit>> {
    let all = sessions(conn)?;
    let by_id = |id: &str| all.iter().find(|s| s.id == id);
    let mut out: Vec<Hit> = Vec::new();
    match q.fts() {
        None => {
            for s in all.iter().filter(|s| q.admits(s, now)).take(limit) {
                out.push(Hit { session: s.clone(), kind: String::new(), snippet: String::new() });
            }
        }
        Some(expr) => {
            let mut st = conn.prepare(
                "SELECT t.session_id, t.kind, snippet(memory_fts, 0, '[', ']', '…', 12) FROM memory_fts JOIN memory_text t ON t.id = memory_fts.rowid
                 WHERE memory_fts MATCH ?1 ORDER BY rank LIMIT 2000",
            )?;
            let rows = st.query_map(params![expr], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            for row in rows {
                let (id, kind, snippet) = row?;
                if out.len() >= limit {
                    break;
                }
                if out.iter().any(|h| h.session.id == id) {
                    continue;
                }
                if let Some(s) = by_id(&id).filter(|s| q.admits(s, now)) {
                    out.push(Hit { session: s.clone(), kind, snippet: snippet.replace('\n', " ") });
                }
            }
        }
    }
    Ok(out)
}

/// The line a list shows for a summary.
pub fn first_line(summary: &str) -> &str {
    summary.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// A summary for a tooltip: whole paragraphs from the top while they fit in `max` characters
/// (always the first, cut at a word if it alone is too long), then "…" when anything was left
/// out. Whole summaries run to a dozen paragraphs; a tooltip that long covers the menu.
pub fn brief(summary: &str, max: usize) -> String {
    let paras: Vec<&str> = summary.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).collect();
    let mut out = String::new();
    for (i, p) in paras.iter().enumerate() {
        if i > 0 && out.chars().count() + p.chars().count() + 2 > max {
            out.push_str("\n…");
            return out;
        }
        if i == 0 && p.chars().count() > max {
            let cut: String = p.chars().take(max).collect();
            let at = cut.rfind(char::is_whitespace).unwrap_or(cut.len());
            out.push_str(cut[..at].trim_end());
            out.push('…');
            return out;
        }
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(p);
    }
    out
}

// --- show and transcript --------------------------------------------------------------------------

/// A session's record as text: the facts, the summary, the notes.
pub fn show(conn: &Connection, s: &SessionInfo, kept: &[(PathBuf, String)], home: &Path) -> Result<String> {
    let mut out = format!(
        "{}  “{}”  {}  started {}{}\n",
        s.id,
        s.title,
        s.agent,
        datetime(s.created_at),
        match s.ended_at {
            Some(t) => format!(", ended {} ({})", datetime(t), duration(t - s.created_at)),
            None => ", still open".to_string(),
        }
    );
    if !s.pins.is_empty() {
        out.push_str(&format!("pinned: {}\n", s.pins.iter().map(|p| tilde(p, home)).collect::<Vec<_>>().join(", ")));
    }
    if !kept.is_empty() {
        out.push_str(&format!("kept to the host ({}): {}\n", kept.len(), kept.iter().map(|(p, w)| format!("{w} {}", tilde(p, home))).collect::<Vec<_>>().join(", ")));
    }
    out.push_str(&format!("{} prompts, {} tool calls (times are UTC)\n", count_kind(conn, &s.id, "user"), count_kind(conn, &s.id, "tool")));
    if s.summary.trim().is_empty() {
        out.push_str("\nno summary yet\n");
    } else {
        out.push_str(&format!("\nsummary ({}):\n{}\n", if s.summary_kind == "agent" { "by the agent" } else { "by sill, from the facts" }, s.summary.trim_end()));
    }
    for (ts, kind, text) in notes(conn, &s.id)? {
        out.push_str(&format!("\n{kind} {}:\n{}\n", date(ts), text.trim_end()));
    }
    Ok(out)
}

/// A session's conversation, oldest first: the person's and the agent's text with times (UTC), a
/// tool call as one line. With `grep`, only the messages holding every one of its words (any case).
pub fn transcript(conn: &Connection, s: &SessionInfo, grep: Option<&str>) -> Result<String> {
    let words: Vec<String> = grep.map(|g| g.split_whitespace().map(str::to_lowercase).collect()).unwrap_or_default();
    let mut st = conn.prepare("SELECT seq, ts, kind, text FROM entries WHERE session_id = ?1 AND kind IN ('user', 'agent', 'tool') ORDER BY seq")?;
    let rows = st.query_map(params![s.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    let mut out = format!("{}  “{}”  {}, times UTC\n", s.id, s.title, s.agent);
    let mut shown = 0;
    for row in rows {
        let (seq, ts, kind, text) = row?;
        if !words.is_empty() {
            let lower = text.to_lowercase();
            if !words.iter().all(|w| lower.contains(w.as_str())) {
                continue;
            }
        }
        shown += 1;
        let at = datetime(ts);
        let at = if words.is_empty() { at[11..].to_string() } else { format!("{at} #{seq}") };
        match kind.as_str() {
            "tool" => out.push_str(&format!("{at}   tool: {}\n", text.lines().next().unwrap_or(""))),
            _ => {
                let who = if kind == "user" { "person" } else { "agent" };
                let mut lines = text.trim().lines();
                out.push_str(&format!("{at} {who}: {}\n", lines.next().unwrap_or("")));
                for l in lines {
                    out.push_str(&format!("    {l}\n"));
                }
            }
        }
    }
    if shown == 0 {
        out.push_str(if words.is_empty() { "(no messages)\n" } else { "(no message holds all of those words)\n" });
    }
    Ok(out)
}

/// One session as a Markdown file (docs/memory.md §2.5 Export): the facts, the summary, the notes,
/// then the conversation as it stands (compacted or not) — the person's data in a form that
/// outlives sill.
pub fn export_markdown(conn: &Connection, s: &SessionInfo, kept: &[(PathBuf, String)], home: &Path) -> Result<String> {
    let mut out = format!("# {}\n\n", s.title);
    out.push_str(&format!("- session `{}`, agent {}\n- started {}", s.id, s.agent, datetime(s.created_at)));
    match s.ended_at {
        Some(t) => out.push_str(&format!(", ended {} ({})\n", datetime(t), duration(t - s.created_at))),
        None => out.push_str(", still open\n"),
    }
    if !s.pins.is_empty() {
        out.push_str(&format!("- pinned: {}\n", s.pins.iter().map(|p| format!("`{}`", tilde(p, home))).collect::<Vec<_>>().join(", ")));
    }
    if !kept.is_empty() {
        out.push_str(&format!("- kept to the host: {}\n", kept.iter().map(|(p, w)| format!("{w} `{}`", tilde(p, home))).collect::<Vec<_>>().join(", ")));
    }
    out.push_str(&format!("- {} prompts, {} tool calls; times UTC\n", count_kind(conn, &s.id, "user"), count_kind(conn, &s.id, "tool")));
    if !s.summary.trim().is_empty() {
        out.push_str(&format!("\n## Summary ({})\n\n{}\n", if s.summary_kind == "agent" { "by the agent" } else { "by sill, from the facts" }, s.summary.trim_end()));
    }
    let notes = notes(conn, &s.id)?;
    if !notes.is_empty() {
        out.push_str("\n## Notes\n");
        for (ts, kind, text) in notes {
            out.push_str(&format!("\n**{kind}, {}**\n\n{}\n", date(ts), text.trim_end()));
        }
    }
    out.push_str("\n## Conversation\n");
    let mut st = conn.prepare("SELECT ts, kind, text FROM entries WHERE session_id = ?1 AND kind IN ('user', 'agent', 'tool') ORDER BY seq")?;
    let rows = st.query_map(params![s.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (ts, kind, text) = row?;
        let at = &datetime(ts)[11..];
        match kind.as_str() {
            "tool" => out.push_str(&format!("\n*{at} tool: {}*\n", text.lines().next().unwrap_or(""))),
            "user" => out.push_str(&format!("\n### {at} person\n\n{}\n", text.trim())),
            _ => out.push_str(&format!("\n### {at} agent\n\n{}\n", text.trim())),
        }
    }
    Ok(out)
}

/// A file name for a session's export: date, id, the title's words.
pub fn export_name(s: &SessionInfo) -> String {
    let slug: String = s.title.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let slug = slug.split('-').filter(|w| !w.is_empty()).take(6).collect::<Vec<_>>().join("-");
    format!("{}-{}-{}.md", date(s.ended_at.unwrap_or(s.last_used)), &s.id[..s.id.len().min(8)], slug)
}

/// One search hit as two lines: the session, then what matched.
pub fn hit_lines(h: &Hit, home: &Path) -> String {
    let s = &h.session;
    let when = match s.ended_at {
        Some(t) => date(t),
        None => format!("{} open", date(s.last_used)),
    };
    let place = if s.pins.is_empty() { String::new() } else { format!("  {}", s.pins.iter().map(|p| tilde(p, home)).collect::<Vec<_>>().join(", ")) };
    let mut out = format!("{}  {when}  {}  “{}”{place}\n", s.id, s.agent, s.title);
    let detail = if !h.snippet.is_empty() && h.kind != "title" { format!("{}: {}", h.kind, h.snippet) } else { first_line(&s.summary).to_string() };
    if !detail.is_empty() {
        out.push_str(&format!("    {detail}\n"));
    }
    out
}

// --- the jail's command ---------------------------------------------------------------------------

pub const USAGE: &str = "usage: sill-memory search WORDS… [--folder PATH] [--agent NAME] [--since 30d]
       sill-memory show ID
       sill-memory transcript ID [--grep WORDS]

Sessions on this machine as sill remembers them: when, where, with which agent, a summary written
when each ended. ID is a session id (or its first characters) or its exact title. Search matches
titles, summaries, notes and the person's prompts; `transcript --grep` reads the agents' text too.";

/// `sill-memory …` over an open memory database: the output, or an error worth exit status 1.
pub fn cli(args: &[String], conn: &Connection, home: &Path, now: i64, kept: impl Fn(&str) -> Vec<(PathBuf, String)>) -> Result<String> {
    let flag = |args: &mut Vec<String>, name: &str| -> Result<Option<String>> {
        match args.iter().position(|a| a == name) {
            Some(i) if i + 1 < args.len() => {
                let v = args.remove(i + 1);
                args.remove(i);
                Ok(Some(v))
            }
            Some(_) => bail!("{name} expects a value"),
            None => Ok(None),
        }
    };
    let mut args: Vec<String> = args.to_vec();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    let session = |args: &[String]| -> Result<SessionInfo> {
        let key = args.join(" ");
        if key.trim().is_empty() {
            bail!("which session? {}", USAGE.lines().nth(1).unwrap_or("").trim());
        }
        find_session(conn, &key)?.with_context(|| format!("no session {key}"))
    };
    match cmd.as_str() {
        "search" => {
            let mut text = Vec::new();
            for (f, key) in [("--folder", "folder:"), ("--agent", "agent:"), ("--since", "since:")] {
                if let Some(v) = flag(&mut args, f)? {
                    if key == "since:" && parse_age(&v).is_none() {
                        bail!("--since expects 30d, 12h, 2w …");
                    }
                    text.push(format!("{key}{v}"));
                }
            }
            text.extend(args);
            let q = Query::parse(&text.join(" "), home);
            if q.is_empty() {
                bail!("search for what? {}", USAGE.lines().next().unwrap_or(""));
            }
            let hits = search(conn, &q, now, 30)?;
            if hits.is_empty() {
                return Ok("nothing found\n".into());
            }
            Ok(hits.iter().map(|h| hit_lines(h, home)).collect())
        }
        "show" => {
            let s = session(&args)?;
            show(conn, &s, &kept(&s.id), home)
        }
        "transcript" => {
            let grep = flag(&mut args, "--grep")?;
            let s = session(&args)?;
            transcript(conn, &s, grep.as_deref())
        }
        "" | "help" | "--help" | "-h" => Ok(format!("{USAGE}\n")),
        other => bail!("unknown command {other}\n{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_keeps_whole_paragraphs() {
        let s = "First paragraph here.\n\nSecond one, a bit longer than the first.\n\nThird.";
        assert_eq!(brief(s, 1000), s);
        assert_eq!(brief(s, 70), "First paragraph here.\n\nSecond one, a bit longer than the first.\n…");
        assert_eq!(brief(s, 30), "First paragraph here.\n…");
        // the first paragraph alone too long: cut at a word
        assert_eq!(brief("one two three four five", 12), "one two…");
        assert_eq!(brief("  \n\n", 12), "");
    }

    fn store_like() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL, created_at INTEGER NOT NULL,
                 last_used INTEGER NOT NULL, acp_session_id TEXT, ended_at INTEGER, summary TEXT NOT NULL DEFAULT '', summary_kind TEXT NOT NULL DEFAULT '');
             CREATE TABLE session_pins (session_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY (session_id, path));
             CREATE TABLE entries (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
                 extra TEXT NOT NULL DEFAULT '', PRIMARY KEY (session_id, seq));
             CREATE TABLE memories (session_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);
             INSERT INTO sessions (id, agent, title, created_at, last_used) VALUES ('old00001', 'claude', 'before memory', 100, 100);
             INSERT INTO entries VALUES ('old00001', 1, 100, 'user', 'the légacy prompt', '');",
        )
        .unwrap();
        install(&c).unwrap();
        install(&c).unwrap(); // idempotent: no second backfill
        c
    }

    fn session(c: &Connection, id: &str, title: &str, pins: &[&str], ended: Option<(i64, &str)>) {
        c.execute("INSERT INTO sessions (id, agent, title, created_at, last_used) VALUES (?1, 'claude', ?2, 1000, 1000)", params![id, title]).unwrap();
        for p in pins {
            c.execute("INSERT INTO session_pins VALUES (?1, ?2)", params![id, p]).unwrap();
        }
        if let Some((t, summary)) = ended {
            c.execute("UPDATE sessions SET ended_at = ?2, summary = ?3, summary_kind = 'agent', last_used = ?2 WHERE id = ?1", params![id, t, summary]).unwrap();
        }
    }

    #[test]
    fn the_index_follows_the_store() {
        let c = store_like();
        let home = Path::new("/home/me");
        let q = |t: &str| search(&c, &Query::parse(t, home), 10_000, 10).unwrap().into_iter().map(|h| (h.session.id, h.kind)).collect::<Vec<_>>();
        // backfilled at install, diacritics folded, prefixes match
        assert_eq!(q("legacy"), [("old00001".to_string(), "prompt".to_string())]);
        assert_eq!(q("leg"), [("old00001".to_string(), "prompt".to_string())]);
        session(&c, "aaaa1111", "proxy allow button", &["/home/me/GIT/sill"], None);
        assert_eq!(q("allow"), [("aaaa1111".to_string(), "title".to_string())]);
        c.execute("UPDATE sessions SET title = 'renamed thing' WHERE id = 'aaaa1111'", []).unwrap();
        assert!(q("allow").is_empty());
        assert_eq!(q("renamed").len(), 1);
        c.execute("INSERT INTO entries VALUES ('aaaa1111', 1, 1000, 'user', 'why does the proxy refuse github', '')", []).unwrap();
        c.execute("INSERT INTO entries VALUES ('aaaa1111', 2, 1001, 'agent', 'agent text is not indexed: zebra', '')", []).unwrap();
        assert_eq!(q("proxy github"), [("aaaa1111".to_string(), "prompt".to_string())]);
        assert!(q("zebra").is_empty());
        c.execute("UPDATE sessions SET summary = 'Fixed the CONNECT parser in proxy.rs', ended_at = 2000 WHERE id = 'aaaa1111'", []).unwrap();
        assert_eq!(q("connect parser")[0].1, "summary");
        c.execute("UPDATE sessions SET summary = 'replaced' WHERE id = 'aaaa1111'", []).unwrap();
        assert!(q("connect").is_empty());
        c.execute("INSERT INTO memories VALUES ('aaaa1111', 3000, 'note', 'remember the walrus')", []).unwrap();
        assert_eq!(q("walrus")[0].1, "note");
        // nothing the person types is FTS syntax
        assert!(search(&c, &Query::parse("\"unbalanced AND (", home), 0, 10).is_ok());
        assert!(search(&c, &Query::parse("NEAR( * \" -", home), 0, 10).is_ok());
        // filters, with words and alone
        assert_eq!(q("folder:~/GIT").len(), 1);
        assert_eq!(q("folder:~/GIT/sill/crates proxy").len(), 1);
        assert!(q("folder:~/elsewhere proxy").is_empty());
        assert!(q("agent:codex proxy").is_empty());
        assert_eq!(q("since:150m").len(), 1); // ended at 2000, now 10 000: the old one (100) is out
        assert!(q("since:1h").is_empty());
        // deleting the session takes its text out of the index
        c.execute("DELETE FROM entries WHERE session_id = 'aaaa1111'", []).unwrap();
        c.execute("DELETE FROM memories WHERE session_id = 'aaaa1111'", []).unwrap();
        c.execute("DELETE FROM sessions WHERE id = 'aaaa1111'", []).unwrap();
        assert!(q("walrus").is_empty() && q("renamed").is_empty() && q("github").is_empty());
        assert_eq!(c.query_row("SELECT count(*) FROM memory_text WHERE session_id = 'aaaa1111'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn recall_is_by_folder_nearest_then_newest() {
        let s = |id: &str, pins: &[&str], ended: Option<i64>| SessionInfo {
            id: id.into(),
            agent: "claude".into(),
            title: id.into(),
            created_at: 0,
            ended_at: ended,
            summary: format!("summary of {id}"),
            summary_kind: "agent".into(),
            pins: pins.iter().map(PathBuf::from).collect(),
            ..Default::default()
        };
        let all = vec![
            s("me", &["/w/sill"], None),
            s("child-old", &["/w/sill/crates/core"], Some(10)),
            s("same", &["/w/sill"], Some(20)),
            s("parent", &["/w"], Some(30)),
            s("other", &["/elsewhere"], Some(40)),
            s("open", &["/w/sill"], None),
            s("child-new", &["/w/sill/docs"], Some(50)),
        ];
        let ids = |v: Vec<&SessionInfo>| v.into_iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        let me_pins = [PathBuf::from("/w/sill")];
        assert_eq!(ids(recall_pick(&all, "me", &me_pins, Scope::Here, 3, None)), ["same", "child-new", "parent"]);
        assert_eq!(ids(recall_pick(&all, "me", &me_pins, Scope::Here, 9, None)), ["same", "child-new", "parent", "child-old"]);
        assert_eq!(ids(recall_pick(&all, "me", &me_pins, Scope::Everywhere, 2, None)), ["child-new", "other"]);
        assert!(recall_pick(&all, "me", &me_pins, Scope::Off, 3, None).is_empty());
        assert!(recall_pick(&all, "me", &[], Scope::Here, 3, None).is_empty());
        // New here: the session it came from leads, even from outside the folders
        assert_eq!(ids(recall_pick(&all, "me", &me_pins, Scope::Here, 2, Some("other"))), ["other", "same"]);
        assert_eq!(ids(recall_pick(&all, "me", &me_pins, Scope::Here, 2, Some("parent"))), ["parent", "same"]);
        assert_eq!(Scope::resolve("", true), Scope::Here);
        assert_eq!(Scope::resolve("", false), Scope::Everywhere);
        assert_eq!(Scope::resolve("off", true), Scope::Off);
        let home = Path::new("/w");
        let picked = recall_pick(&all, "me", &me_pins, Scope::Here, 1, None);
        let b = recall_block(&picked, Scope::Here, true, home, |id| if id == "same" { 7 } else { 0 });
        assert!(b.starts_with("[Memory — earlier sessions in these folders"), "{b}");
        assert!(b.contains("\n- 1970-01-01, 1 min, claude in ~/sill, kept 7 files — “same” (same):\n  summary of same\n`sill-memory search"), "{b}");
        assert!(b.ends_with("the conversation.]"), "{b}");
        let mut long = s("long", &["/w/sill"], Some(60));
        long.summary = "x".repeat(2000);
        let lb = recall_block(&[&long], Scope::Here, false, home, |_| 0);
        assert!(lb.contains(&format!("  {}… (`sill-memory show long` has the rest)]", "x".repeat(1500))) && lb.ends_with(']'), "{lb}");
        let none = recall_block(&[], Scope::Here, true, home, |_| 0);
        assert!(none.starts_with("[Earlier sessions on this machine are remembered by sill"), "{none}");
        assert_eq!(recall_block(&[], Scope::Here, false, home, |_| 0), "");
    }

    #[test]
    fn snapshot_round_trip_and_the_jail_command() {
        let dir = std::env::temp_dir().join(format!("sill-memory-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sill.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            drop(c);
        }
        let src = Connection::open(&db).unwrap();
        src.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL, created_at INTEGER NOT NULL,
                 last_used INTEGER NOT NULL, acp_session_id TEXT, ended_at INTEGER, summary TEXT NOT NULL DEFAULT '', summary_kind TEXT NOT NULL DEFAULT '');
             CREATE TABLE session_pins (session_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY (session_id, path));
             CREATE TABLE entries (session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
                 extra TEXT NOT NULL DEFAULT '', detail TEXT NOT NULL DEFAULT '', PRIMARY KEY (session_id, seq));
             CREATE TABLE memories (session_id TEXT NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL);",
        )
        .unwrap();
        install(&src).unwrap();
        session(&src, "bbbb2222", "tray icon", &["/home/me/GIT/sill"], Some((1_793_560_000, "Added the tray icon in shelf/tray.rs.\nNo appindicator.")));
        src.execute("INSERT INTO entries (session_id, seq, ts, kind, text, detail) VALUES ('bbbb2222', 1, 1793545200, 'user', 'add a tray icon', '')", []).unwrap();
        src.execute("INSERT INTO entries (session_id, seq, ts, kind, text, detail) VALUES ('bbbb2222', 2, 1793545260, 'tool', 'Edit tray.rs', 'SECRET DETAIL')", []).unwrap();
        src.execute("INSERT INTO entries (session_id, seq, ts, kind, text, detail) VALUES ('bbbb2222', 3, 1793545320, 'agent', 'Done: StatusNotifierItem\nover D-Bus', '')", []).unwrap();
        // written while the shelf holds the db open in WAL mode
        let snapdir = dir.join("memory");
        write_snapshot(&db, &snapdir, &[("bbbb2222".into(), PathBuf::from("/home/me/.config/x"), "added".into())]).unwrap();
        let names: Vec<_> = std::fs::read_dir(&snapdir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [SNAPSHOT_FILE], "no temporary left behind");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(snapdir.join(SNAPSHOT_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        let snap = open_snapshot(&snapdir).unwrap().unwrap();
        let home = Path::new("/home/me");
        let run = |a: &[&str]| cli(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &snap, home, 1_793_500_000, |id| snapshot_kept(&snap, id));
        let found = run(&["search", "tray"]).unwrap();
        assert!(found.starts_with("bbbb2222  2026-11-01  claude  “tray icon”  ~/GIT/sill\n"), "{found}");
        let why = run(&["search", "appindicator", "--folder", "~/GIT"]).unwrap();
        assert!(why.contains("\n    summary: ") && why.contains("[appindicator]"), "{why}");
        assert_eq!(run(&["search", "tray", "--folder", "/tmp"]).unwrap(), "nothing found\n");
        let shown = run(&["show", "bbbb"]).unwrap();
        assert!(shown.contains("kept to the host (1): added ~/.config/x"), "{shown}");
        assert!(shown.contains("1 prompts, 1 tool calls"), "{shown}");
        assert!(shown.contains("summary (by the agent):\nAdded the tray icon"), "{shown}");
        assert_eq!(run(&["show", "tray icon"]).unwrap(), shown);
        let t = run(&["transcript", "bbbb2222"]).unwrap();
        assert!(t.contains("\n15:00 person: add a tray icon\n15:01   tool: Edit tray.rs\n15:02 agent: Done: StatusNotifierItem\n    over D-Bus\n"), "{t}");
        assert!(!t.contains("SECRET"));
        let g = run(&["transcript", "bbbb2222", "--grep", "d-bus"]).unwrap();
        assert!(g.contains("2026-11-01 15:02 #3 agent:") && !g.contains("person:"), "{g}");
        assert!(run(&["show", "nope"]).is_err());
        assert!(run(&["search"]).is_err());
        assert!(run(&[]).unwrap().starts_with("usage:"));
        // the snapshot cannot be written through
        assert!(snap.execute("DELETE FROM sessions", []).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_and_folders() {
        assert_eq!(date(1_793_491_200), "2026-11-01");
        assert_eq!(datetime(1_793_491_200 + 3600 + 120), "2026-11-01 01:02");
        assert_eq!(duration(30), "1 min");
        assert_eq!(duration(2 * 3600), "2 h");
        assert_eq!(duration(5 * 86_400), "5 d");
        assert_eq!(parse_age("30d"), Some(30 * 86_400));
        assert_eq!(parse_age("2w"), Some(14 * 86_400));
        assert_eq!(parse_age("x"), None);
        assert_eq!(folder_distance(Path::new("/a/b"), Path::new("/a/b")), Some(0));
        assert_eq!(folder_distance(Path::new("/a"), Path::new("/a/b/c")), Some(2));
        assert_eq!(folder_distance(Path::new("/a/bc"), Path::new("/a/b")), None);
        assert_eq!(tilde(Path::new("/home/me"), Path::new("/home/me")), "~");
        assert_eq!(tilde(Path::new("/home/me/x"), Path::new("/home/me")), "~/x");
        let q = Query::parse("proxy folder:~/GIT agent:claude since:2w allow", Path::new("/home/me"));
        assert_eq!(q.words, ["proxy", "allow"]);
        assert_eq!(q.folder, Some(PathBuf::from("/home/me/GIT")));
        assert_eq!(q.fts().unwrap(), "\"proxy\"* \"allow\"*");
        assert_eq!(Query::parse("a\"b", Path::new("/")).fts().unwrap(), "\"a\"\"b\"*");
    }
}
