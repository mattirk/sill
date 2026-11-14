//! The ledger across time (SPEC §3.6), the store side: who an event is by (a ghost anonymous), the
//! pinned repositories' `HEAD` at a session's start and its range at the end. The table and the
//! query are `sill_memory::history`, shared with the jail.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::params;
pub use sill_memory::history::{Event, Filter, Kind};

use crate::store::{Session, Store};

/// An event of `kind` on `path` by session `s` now: a ghost's without its id and title (SPEC §3.6
/// decision 1).
pub fn event(s: &Session, kind: Kind, path: &Path) -> Event {
    let mut e = Event::new(kind, path, crate::store::now());
    e.agent = s.agent.clone();
    e.ghost = s.ghost;
    if !s.ghost {
        e.session_id = Some(s.id.clone());
        e.session_title = s.title.clone();
    }
    e
}

pub fn record(store: &Store, e: &Event) -> Result<()> {
    sill_memory::history::record(store.conn(), e)
}

/// `HEAD` of a repository (through sill's safe git view), None without one.
pub fn head(repo: &Path) -> Option<String> {
    let git = crate::changes::Git::open(repo).ok()?;
    let out = git.cmd().args(["rev-parse", "--verify", "-q", "HEAD"]).output().ok()?;
    let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !h.is_empty()).then_some(h)
}

/// Commits and files between two commits: `git rev-list --count` and `git diff --name-only`.
fn range(repo: &Path, a: &str, b: &str) -> (i64, i64) {
    let Ok(git) = crate::changes::Git::open(repo) else { return (0, 0) };
    let count = |args: &[&str]| -> i64 {
        git.cmd().args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default().parse().unwrap_or(0)
    };
    let commits = count(&["rev-list", "--count", &format!("{a}..{b}")]);
    let files = git
        .cmd()
        .args(["diff", "--name-only", "--no-ext-diff", "--no-textconv", a, b])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.is_empty()).count() as i64)
        .unwrap_or(0);
    (commits, files)
}

/// A session's jail starts: each pinned git repository's `HEAD`, unless this open period already
/// has one (a restart for a policy change is the same period). A ghost has no pins.
pub fn note_pin_starts(store: &Store, s: &Session) -> Result<()> {
    if s.ghost {
        return Ok(());
    }
    for repo in store.jail_pins(&s.id)?.into_iter().filter(|p| p.join(".git").exists()) {
        if let Some(h) = head(&repo) {
            store.conn().execute(
                "INSERT OR IGNORE INTO pin_heads (session_id, repo, head, ts) VALUES (?1, ?2, ?3, ?4)",
                params![s.id, repo.to_string_lossy(), h, crate::store::now()],
            )?;
        }
    }
    Ok(())
}

/// A session ends (or is deleted): each pinned repository whose `HEAD` moved since the start gets a
/// `Pinned` row with the range; the start marks go, so a reopened session starts a new period.
pub fn close_pins(store: &Store, s: &Session) -> Result<usize> {
    let starts: Vec<(PathBuf, String)> = {
        let mut st = store.conn().prepare("SELECT repo, head FROM pin_heads WHERE session_id = ?1")?;
        st.query_map(params![s.id], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, String>(1)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut n = 0;
    for (repo, start) in starts {
        let Some(end) = head(&repo) else { continue };
        if end == start {
            continue;
        }
        let (commits, files) = range(&repo, &start, &end);
        let mut e = event(s, Kind::Pinned, &repo);
        e.head_start = Some(start);
        e.head_end = Some(end);
        e.commits = Some(commits);
        e.files = Some(files);
        record(store, &e)?;
        n += 1;
    }
    store.conn().execute("DELETE FROM pin_heads WHERE session_id = ?1", params![s.id])?;
    Ok(n)
}
