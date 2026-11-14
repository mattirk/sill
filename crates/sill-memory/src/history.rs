//! The ledger across time (SPEC §3.6): one row per event on a host path — kept, undone,
//! reverted, auto-reverted — and per pinned repository's range of commits, a session's deletion,
//! the fold retention leaves. In `sill.db`; the jail's snapshot carries it read-only, so the same
//! query serves `sill changes history` on the host and `sill-memory history` in the jail.
//!
//! A ghost's keep is a row without its session (SPEC §3.6 decision 1); a ghost's reverts are none.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, kind TEXT NOT NULL,
    session_id TEXT, session_title TEXT NOT NULL DEFAULT '', agent TEXT NOT NULL DEFAULT '',
    ghost INTEGER NOT NULL DEFAULT 0,
    path TEXT NOT NULL DEFAULT '', what TEXT NOT NULL DEFAULT '',
    hash_new TEXT, hash_old TEXT,
    count INTEGER NOT NULL DEFAULT 1, paths TEXT NOT NULL DEFAULT '',
    kept_id TEXT,
    repo TEXT, head_start TEXT, head_end TEXT, commits INTEGER, files INTEGER);
CREATE INDEX IF NOT EXISTS history_ts ON history (ts);
CREATE INDEX IF NOT EXISTS history_session ON history (session_id);";

/// What happened. `Kept`, `Undone`, `Reverted` are the person's, path by path; `AutoReverted` is
/// one sweep of one session (`paths` behind it); `Pinned` a repository's commits during a session;
/// `Deleted` the session itself; `Folded` what retention left of a session's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Kept,
    Undone,
    Reverted,
    AutoReverted,
    Pinned,
    Deleted,
    Folded,
}

impl Kind {
    pub fn id(self) -> &'static str {
        match self {
            Kind::Kept => "kept",
            Kind::Undone => "undone",
            Kind::Reverted => "reverted",
            Kind::AutoReverted => "auto-reverted",
            Kind::Pinned => "pinned",
            Kind::Deleted => "deleted",
            Kind::Folded => "folded",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Kept, Kind::Undone, Kind::Reverted, Kind::AutoReverted, Kind::Pinned, Kind::Deleted, Kind::Folded].into_iter().find(|k| k.id() == s)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub id: i64,
    pub ts: i64,
    pub kind: Kind,
    /// None for a ghost's keep.
    pub session_id: Option<String>,
    pub session_title: String,
    pub agent: String,
    pub ghost: bool,
    /// The host path; for a sweep or a fold the longest common prefix of `paths`; for `Pinned` the
    /// repository.
    pub path: PathBuf,
    /// What keep did (`added` `updated` `removed` …), what undo did, a fold's tally.
    pub what: String,
    pub hash_new: Option<String>,
    pub hash_old: Option<String>,
    pub count: i64,
    pub paths: Vec<PathBuf>,
    /// The keep's id in the session's `kept/` journal: its diff while the session lives.
    pub kept_id: Option<String>,
    pub head_start: Option<String>,
    pub head_end: Option<String>,
    pub commits: Option<i64>,
    pub files: Option<i64>,
}

impl Event {
    /// An event of `kind` on `path` now, the rest empty.
    pub fn new(kind: Kind, path: &Path, ts: i64) -> Event {
        Event {
            id: 0,
            ts,
            kind,
            session_id: None,
            session_title: String::new(),
            agent: String::new(),
            ghost: false,
            path: path.to_path_buf(),
            what: String::new(),
            hash_new: None,
            hash_old: None,
            count: 1,
            paths: vec![],
            kept_id: None,
            head_start: None,
            head_end: None,
            commits: None,
            files: None,
        }
    }
}

pub fn install(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

pub fn record(conn: &Connection, e: &Event) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let mut paths = Vec::new();
    for path in &e.paths {
        anyhow::ensure!(!path.as_os_str().as_bytes().contains(&0), "history path contains NUL");
        paths.extend_from_slice(path.as_os_str().as_bytes());
        paths.push(0);
    }
    conn.execute(
        "INSERT INTO history (ts, kind, session_id, session_title, agent, ghost, path, what, hash_new, hash_old, count, paths, kept_id,
             repo, head_start, head_end, commits, files)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            e.ts,
            e.kind.id(),
            e.session_id,
            e.session_title,
            e.agent,
            e.ghost as i32,
            sql_path(&e.path),
            e.what,
            e.hash_new,
            e.hash_old,
            e.count,
            paths,
            e.kept_id,
            (e.kind == Kind::Pinned).then(|| sql_path(&e.path)),
            e.head_start,
            e.head_end,
            e.commits,
            e.files
        ],
    )?;
    Ok(())
}

/// What a history listing asks for; every field narrows it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Filter {
    /// Unix time; only events at or after it.
    pub since: Option<i64>,
    /// Exclusive newest-first keyset cursor (timestamp, event identity).
    #[serde(default)]
    pub before: Option<(i64, i64)>,
    /// Events on this path or below it (a sweep's or a fold's listed paths count too).
    pub path: Option<PathBuf>,
    pub agent: Option<String>,
    pub kind: Option<Kind>,
    pub session: Option<String>,
    /// At most this many, newest first (0 = 500).
    #[serde(default)]
    pub limit: usize,
}

pub(crate) fn sql_path(path: &Path) -> rusqlite::types::Value {
    use std::os::unix::ffi::OsStrExt;
    match path.to_str() {
        Some(text) => rusqlite::types::Value::Text(text.into()),
        None => rusqlite::types::Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec()).into()
}
pub(crate) fn read_path(r: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<PathBuf> {
    use rusqlite::types::ValueRef;
    match r.get_ref(column)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Ok(path_from_bytes(bytes)),
        value => Err(rusqlite::Error::InvalidColumnType(column, "path".into(), value.data_type())),
    }
}
fn read_paths(r: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Vec<PathBuf>> {
    use rusqlite::types::ValueRef;
    // NUL-delimited bytes: a NUL cannot occur in a Unix filename, but newlines and arbitrary
    // bytes can.
    match r.get_ref(column)? {
        ValueRef::Blob(bytes) => Ok(bytes.split(|b| *b == 0).filter(|b| !b.is_empty()).map(path_from_bytes).collect()),
        value => Err(rusqlite::Error::InvalidColumnType(column, "paths".into(), value.data_type())),
    }
}

pub fn query(conn: &Connection, f: &Filter) -> Result<Vec<Event>> {
    let mut st = conn.prepare(
        "SELECT id, ts, kind, session_id, session_title, agent, ghost, path, what, hash_new, hash_old, count, paths, kept_id,
                head_start, head_end, commits, files
         FROM history
         WHERE (?1 IS NULL OR ts >= ?1)
           AND (?2 IS NULL OR agent = ?2)
           AND (?3 IS NULL OR kind = ?3)
           AND (?4 IS NULL OR session_id = ?4)
           AND (?5 IS NULL OR ts < ?5 OR (ts = ?5 AND id < ?6))
         ORDER BY ts DESC, id DESC",
    )?;
    let limit = if f.limit == 0 { 500 } else { f.limit };
    let rows = st.query_map(params![f.since, f.agent, f.kind.map(|k| k.id()), f.session, f.before.map(|x|x.0), f.before.map(|x|x.1)], |r| {
        Ok(Event {
            id: r.get(0)?, ts: r.get(1)?,
            kind: Kind::parse(&r.get::<_, String>(2)?).unwrap_or(Kind::Folded),
            session_id: r.get(3)?, session_title: r.get(4)?, agent: r.get(5)?,
            ghost: r.get::<_, i32>(6)? != 0, path: read_path(r, 7)?,
            what: r.get(8)?, hash_new: r.get(9)?, hash_old: r.get(10)?, count: r.get(11)?,
            paths: read_paths(r, 12)?, kept_id: r.get(13)?, head_start: r.get(14)?,
            head_end: r.get(15)?, commits: r.get(16)?, files: r.get(17)?,
        })
    })?;
    // Match decoded paths component-wise, never a lossy string or a newline substring.
    // Iterate lazily so filtering a large ledger does not materialize it in memory.
    let mut out = Vec::new();
    for row in rows {
        let event = row?;
        if let Some(path) = &f.path {
            let matches = event.path.starts_with(path)
                || (event.kind == Kind::Pinned && path.starts_with(&event.path))
                || event.paths.iter().any(|p| p.starts_with(path));
            if !matches { continue; }
        }
        out.push(event);
        if out.len() == limit { break; }
    }
    Ok(out)
}

/// Retention (SPEC §3.6 decision 4): a session's rows become one `Folded` row — how many of each
/// kind, under which common prefix — and the rows go. Its `Pinned` ranges and its `Deleted` row
/// fold too. Nothing to fold: nothing happens.
pub fn fold(conn: &Connection, session_id: &str, now: i64) -> Result<()> {
    let rows = query(conn, &Filter { session: Some(session_id.into()), limit: 100_000, ..Default::default() })?;
    let rows: Vec<Event> = rows.into_iter().filter(|e| e.kind != Kind::Folded).collect();
    let Some(first) = rows.first() else { return Ok(()) };
    let mut tally: Vec<(Kind, i64)> = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    // oldest first: the tally reads in the order things happened
    for e in rows.iter().rev() {
        match tally.iter_mut().find(|(k, _)| *k == e.kind) {
            Some((_, n)) => *n += e.count,
            None => tally.push((e.kind, e.count)),
        }
        if e.kind != Kind::Deleted {
            paths.push(e.path.clone());
        }
    }
    let mut ev = Event::new(Kind::Folded, &common_prefix(&paths), now);
    ev.session_id = Some(session_id.into());
    ev.session_title = first.session_title.clone();
    ev.agent = first.agent.clone();
    ev.what = tally.iter().map(|(k, n)| format!("{n} {}", k.id())).collect::<Vec<_>>().join(", ");
    ev.count = rows.len() as i64;
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM history WHERE session_id = ?1 AND kind <> 'folded'", params![session_id])?;
    record(&tx, &ev)?;
    tx.commit()?;
    Ok(())
}

/// The longest common directory prefix of `paths` (`/` when they share none; empty for none).
pub fn common_prefix(paths: &[PathBuf]) -> PathBuf {
    let Some(first) = paths.first() else { return PathBuf::new() };
    let mut prefix: Vec<std::path::Component> = first.components().collect();
    for p in &paths[1..] {
        let n = prefix.iter().zip(p.components()).take_while(|(a, b)| *a == b).count();
        prefix.truncate(n);
    }
    // one path: its parent, so a sweep of one file reads "under ~/.cache/foo"
    if paths.len() == 1 && prefix.len() > 1 {
        prefix.pop();
    }
    prefix.iter().collect()
}

/// One event as a listing line: date and time, kind, path, then who.
pub fn line(e: &Event, home: &Path) -> String {
    let path = crate::tilde(&e.path, home);
    let detail = match e.kind {
        Kind::Kept | Kind::Undone => format!("{path} ({})", e.what),
        Kind::Reverted => path,
        Kind::AutoReverted => format!("{} path(s) under {path}", e.count),
        Kind::Pinned => format!(
            "{path}: {} commit(s), {} file(s), {}..{}",
            e.commits.unwrap_or(0),
            e.files.unwrap_or(0),
            short(e.head_start.as_deref()),
            short(e.head_end.as_deref())
        ),
        Kind::Deleted => "the session".to_string(),
        Kind::Folded => format!("{} under {path}", e.what),
    };
    let who = if e.ghost {
        format!("{} · a ghost session", e.agent)
    } else if e.session_title.is_empty() {
        e.agent.clone()
    } else {
        format!("{} · {}", e.agent, e.session_title)
    };
    format!("{}  {:<13} {detail}  — {who}\n", crate::datetime(e.ts), e.kind.id())
}

fn short(h: Option<&str>) -> &str {
    h.map(|h| &h[..h.len().min(8)]).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_paths_newlines_and_keyset_pages() {
        use std::os::unix::ffi::OsStringExt;
        let c = Connection::open_in_memory().unwrap();
        install(&c).unwrap();
        let exact = PathBuf::from(std::ffi::OsString::from_vec(b"/files/\xff\nname".to_vec()));
        let mut sweep = Event::new(Kind::AutoReverted, Path::new("/files"), 20);
        sweep.paths = vec![exact.clone(), "/files/two\nlines".into()];
        record(&c, &sweep).unwrap();
        record(&c, &Event::new(Kind::Kept, &exact, 20)).unwrap();
        let all = query(&c, &Filter::default()).unwrap();
        assert_eq!(all[0].path, exact);
        assert_eq!(all[1].paths, sweep.paths);
        let found = query(&c, &Filter { path: Some(exact.clone()), ..Default::default() }).unwrap();
        assert_eq!(found.len(), 2);
        assert!(query(&c, &Filter { path: Some("/files/\u{fffd}\nname".into()), ..Default::default() }).unwrap().is_empty());
        let first = query(&c, &Filter { limit: 1, ..Default::default() }).unwrap();
        let next = query(&c, &Filter { before: Some((first[0].ts, first[0].id)), limit: 1, ..Default::default() }).unwrap();
        assert_eq!(first[0].ts, next[0].ts);
        assert_ne!(first[0].id, next[0].id);
    }

    #[test]
    fn record_query_fold() {
        let c = Connection::open_in_memory().unwrap();
        install(&c).unwrap();
        let mk = |kind, p: &str, ts| {
            let mut e = Event::new(kind, Path::new(p), ts);
            e.session_id = Some("s1".into());
            e.session_title = "tidy".into();
            e.agent = "claude".into();
            e
        };
        record(&c, &mk(Kind::Kept, "/home/me/.config/a/b.toml", 10)).unwrap();
        let mut sweep = mk(Kind::AutoReverted, "/home/me/.cache/x", 20);
        sweep.count = 2;
        sweep.paths = vec!["/home/me/.cache/x/1".into(), "/home/me/.cache/x/2".into()];
        record(&c, &sweep).unwrap();
        let mut g = Event::new(Kind::Kept, Path::new("/home/me/notes.txt"), 30);
        g.ghost = true;
        g.agent = "claude".into();
        record(&c, &g).unwrap();
        let all = query(&c, &Filter::default()).unwrap();
        assert_eq!(all.iter().map(|e| e.ts).collect::<Vec<_>>(), [30, 20, 10], "newest first");
        assert!(all[0].session_id.is_none() && all[0].ghost);
        let under = |p: &str| query(&c, &Filter { path: Some(p.into()), ..Default::default() }).unwrap().len();
        assert_eq!(under("/home/me/.config"), 1);
        assert_eq!(under("/home/me/.cache/x/2"), 1, "a sweep's listed path");
        assert_eq!(under("/home/me/.cache/x/1/deeper"), 0, "a sweep matches by what it lists");
        assert_eq!(under("/home/me/.cache"), 1, "the listed paths are below the filter");
        let mut solo = mk(Kind::AutoReverted, "/home/me", 25);
        solo.paths = vec!["/home/me/one.txt".into()];
        record(&c, &solo).unwrap();
        assert_eq!(under("/home/me/.config"), 1, "a one-file sweep under ~ does not match ~/.config");
        c.execute("DELETE FROM history WHERE ts = 25", []).unwrap();
        assert_eq!(under("/home/me/.configx"), 0, "a prefix is a directory, not a string");
        assert_eq!(query(&c, &Filter { since: Some(15), ..Default::default() }).unwrap().len(), 2);
        assert_eq!(query(&c, &Filter { kind: Some(Kind::AutoReverted), ..Default::default() }).unwrap()[0].paths.len(), 2);
        fold(&c, "s1", 99).unwrap();
        let after = query(&c, &Filter::default()).unwrap();
        assert_eq!(after.len(), 2, "the ghost's row stays, s1's two fold into one");
        let f = after.iter().find(|e| e.kind == Kind::Folded).unwrap();
        assert_eq!(f.what, "1 kept, 2 auto-reverted");
        assert_eq!(f.path, PathBuf::from("/home/me"));
        assert!(line(f, Path::new("/home/me")).contains("1 kept, 2 auto-reverted under ~"));
        assert_eq!(common_prefix(&["/a/b/c".into()]), PathBuf::from("/a/b"));
    }
}
