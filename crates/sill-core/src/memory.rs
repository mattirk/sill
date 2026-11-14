//! Memory (docs/memory.md): what sill keeps of a session once it ends — M0, "remember". The summary
//! is the agent's own last turn when it can be asked ([`summary_prompt`] is what it is asked);
//! otherwise sill writes one from the facts it has ([`auto_summary`]). Both end up in
//! `sessions.summary`, with `summary_kind` saying which. M1, "recall and search": the queries are
//! `sill_memory`'s (the same on the host and in the jail); this module adds what needs the store and
//! the host — the jail's snapshot ([`refresh_snapshot`]) and the recall block ([`recall`]).

use std::path::{Path, PathBuf};

use anyhow::Result;

pub use sill_memory::{Scope, brief, date, first_line, tilde};

use crate::store::{Session, Store};

/// What sill knows of a session without asking anyone.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    pub title: String,
    pub agent: String,
    pub created_at: i64,
    pub pins: Vec<PathBuf>,
    /// Host paths kept from this session, newest first, with what keep did (`added`, `updated`,
    /// `removed`, `symlink`, `directory`); undone keeps left out.
    pub kept: Vec<(PathBuf, String)>,
    pub prompts: i64,
    pub tool_calls: i64,
    pub first_prompt: Option<String>,
}

/// The facts from the store and the session's `kept/` journal.
pub fn collect(store: &Store, s: &Session, kept_dir: &Path) -> Facts {
    Facts {
        title: s.title.clone(),
        agent: s.agent.clone(),
        created_at: s.created_at,
        pins: store.pins(&s.id).unwrap_or_default(),
        kept: kept_in(kept_dir),
        prompts: store.count_kind(&s.id, "user").unwrap_or(0),
        tool_calls: store.count_kind(&s.id, "tool").unwrap_or(0),
        first_prompt: store.first_user_text(&s.id).ok().flatten(),
    }
}

/// A list for a sentence: the first `n`, then "and N more".
fn some(items: &[String], n: usize) -> String {
    if items.len() <= n {
        items.join(", ")
    } else {
        format!("{} and {} more", items[..n].join(", "), items.len() - n)
    }
}

/// The facts as one sentence each, shared by the prompt and the mechanical summary.
fn fact_lines(f: &Facts, home: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    if !f.pins.is_empty() {
        lines.push(format!("pinned folders: {}", f.pins.iter().map(|p| tilde(p, home)).collect::<Vec<_>>().join(", ")));
    }
    if !f.kept.is_empty() {
        let items: Vec<String> = f.kept.iter().map(|(p, what)| format!("{what} {}", tilde(p, home))).collect();
        lines.push(format!("kept to the host ({}): {}", f.kept.len(), some(&items, 8)));
    }
    lines.push(format!("{} prompt{}, {} tool call{}", f.prompts, if f.prompts == 1 { "" } else { "s" }, f.tool_calls, if f.tool_calls == 1 { "" } else { "s" }));
    lines
}

/// The ask shared by the end and a checkpoint: the summary, with the session's name on its first
/// line (title.rs: the summary names the session; [`split_title`] takes it off again).
const ASK: &str = "Write 5–15 lines for a future agent that opens these folders: what was asked, what was done and where (paths), decisions and why, what is unfinished, what not to try again. Plain text, no greeting, no questions — only that agent reads it. First line: `Title: ` and 2–5 words naming this session for a list of sessions (what it was about, not the folder).";

/// What the agent is asked as the session's last turn (docs/memory.md §2.2). The facts go with it
/// so the summary names real places, not "the file I edited".
pub fn summary_prompt(f: &Facts, home: &Path) -> String {
    format!("[sill: this session is ending. {ASK} Facts sill has: {}.]", fact_lines(f, home).join("; "))
}

/// *Summarize now* (docs/memory.md §2.5): the same ask as a checkpoint, the session going on.
pub fn checkpoint_prompt(f: &Facts, home: &Path) -> String {
    format!("[sill: a checkpoint, the session goes on afterwards. {ASK} Facts sill has: {}.]", fact_lines(f, home).join("; "))
}

/// The name the agent put on the summary's first line, and the summary without it: `Title: …`
/// (any case, with or without quotes or a trailing stop), capped at 40 characters. No such line,
/// or an empty one: no name, the text as it came.
pub fn split_title(answer: &str) -> (Option<String>, String) {
    let text = answer.trim();
    let Some((first, rest)) = text.split_once('\n').or(Some((text, ""))) else { unreachable!() };
    let first = first.trim().trim_start_matches(['*', '#', ' ']).trim_end_matches('*');
    let Some(rest_of_line) = first.get(..6).filter(|h| h.eq_ignore_ascii_case("title:")).map(|_| &first[6..]) else {
        return (None, text.to_string());
    };
    // markdown emphasis, quotes and a trailing stop go, in whatever order they were nested
    let mut title = rest_of_line;
    loop {
        let t = title.trim().trim_matches(|c: char| matches!(c, '"' | '“' | '”' | '\'' | '`' | '*')).trim_end_matches(['.', '!']);
        if t == title {
            break;
        }
        title = t;
    }
    if title.is_empty() {
        return (None, rest.trim().to_string());
    }
    let title = if title.chars().count() > 40 { title.chars().take(39).collect::<String>().trim_end().to_string() + "…" } else { title.to_string() };
    (Some(title), rest.trim().to_string())
}

/// The summary sill writes when the agent cannot be asked: the facts, marked as such. It leads with
/// what was asked — the line lists show next to the title and date, which they have already.
pub fn auto_summary(f: &Facts, ended_at: i64, home: &Path) -> String {
    let mut out = Vec::new();
    if let Some(p) = f.first_prompt.as_deref().map(|p| p.trim().replace('\n', " ")).filter(|p| !p.is_empty()) {
        let short: String = p.chars().take(200).collect();
        out.push(format!("Asked: “{short}{}”", if p.chars().count() > 200 { "…" } else { "" }));
    }
    out.push(format!("Session “{}” with {}, {}.", f.title, f.agent, date(if ended_at > 0 { ended_at } else { f.created_at })));
    for l in fact_lines(f, home) {
        let mut l = l;
        if let Some(c) = l.get(..1) {
            l.replace_range(..1, &c.to_uppercase());
        }
        out.push(format!("{l}."));
    }
    out.push("(written by sill from the facts; the agent could not be asked)".into());
    out.join("\n")
}

/// What a session kept to the host and did not undo, newest first: (path, what keep did).
pub fn kept_in(kept_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut kept: Vec<(PathBuf, String)> = crate::keep::list(kept_dir).into_iter().filter(|k| k.undone_at.is_none()).map(|k| (k.path, k.what)).collect();
    kept.reverse();
    kept
}

/// What session `id` kept, from its journal under the state dir.
pub fn kept_of(id: &str) -> Vec<(PathBuf, String)> {
    let uid = rustix::process::getuid().as_raw();
    crate::session::state_dir(uid, id).map(|d| kept_in(&d.join("kept"))).unwrap_or_default()
}

/// Where the jail's snapshot lives on the host: `$XDG_STATE_HOME/sill/memory/` (masked in the
/// jail like the rest of sill's state, and bound read-only at `sill_memory::JAIL_DIR`).
pub fn snapshot_dir() -> PathBuf {
    crate::store::default_path().ok().and_then(|p| p.parent().map(|d| d.join("memory"))).unwrap_or_else(|| PathBuf::from(".local/state/sill/memory"))
}

/// Write the jail's snapshot anew from `sill.db` (its own connection: callable off the GTK
/// thread), or — memory off — remove it. Called when a session ends or is deleted, and at start.
pub fn refresh_snapshot() -> Result<()> {
    // one at a time: two refreshes (a session's end, then a delete) must rename in the order they
    // read, or the deleted session comes back to the jail until the next one
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = snapshot_dir();
    if !crate::config::load().memory() {
        let _ = std::fs::remove_file(dir.join(sill_memory::SNAPSHOT_FILE));
        return Ok(());
    }
    let db = crate::store::default_path()?;
    let store = Store::open(&db)?; // schema validation and the index, before the snapshot reads them
    let mut kept = Vec::new();
    for s in store.sessions()? {
        for (path, what) in kept_of(&s.id) {
            kept.push((s.id.clone(), path, what));
        }
    }
    drop(store);
    std::fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    sill_memory::write_snapshot(&db, &dir, &kept)?;
    Ok(())
}

/// What one nightly run did (docs/memory.md §2.4).
#[derive(Debug, Default, PartialEq)]
pub struct Compaction {
    pub compacted: usize,
    pub trimmed: usize,
    pub deleted: usize,
}

/// Compaction and retention over the ended sessions, by the settings, as of `now`: level 1 after
/// `compact_after`, level 2 after `keep_full_transcripts`, deletion after `delete_after` (each
/// step off at 0); an ended session without a prompt goes at once (nothing happened in it — the
/// shelf deletes one as it closes, this catches those ended by an older shelf). Open sessions (no
/// `ended_at`) are never touched. `remove_state` takes a deleted session's state on the host (the
/// kept journal, its agent home) — the caller knows the uid.
pub fn compact_all(store: &Store, now: i64, remove_state: &dyn Fn(&str)) -> Result<Compaction> {
    let cfg = crate::config::load();
    let (after1, after2, delete) = (cfg.compact_after(), cfg.keep_full_transcripts(), cfg.delete_after());
    let mut done = Compaction::default();
    for s in store.sessions()? {
        let Some(ended) = s.ended_at else { continue };
        let age = now - ended;
        let empty = store.count_kind(&s.id, "user").unwrap_or(0) == 0;
        if empty || (delete > 0 && age > delete) {
            remove_state(&s.id);
            // what it did to the host folds into one line (SPEC §3.6 decision 4)
            sill_memory::history::fold(store.conn(), &s.id, now)?;
            store.delete(&s.id)?;
            done.deleted += 1;
            continue;
        }
        let level = if after2 > 0 && age > after2 { 2 } else if after1 > 0 && age > after1 { 1 } else { 0 };
        if level > s.compacted {
            store.compact(&s.id, level)?;
            if level == 2 { done.trimmed += 1 } else { done.compacted += 1 }
        }
    }
    Ok(done)
}

const NIGHTLY_KEY: &str = "memory_nightly";

/// The nightly run, if a day has passed since the last (own connection: callable off the GTK
/// thread): compaction and retention, a vacuum when anything went, the snapshot anew. None when it
/// was not due or memory is off.
pub fn nightly_if_due(remove_state: &dyn Fn(&str)) -> Result<Option<Compaction>> {
    if !crate::config::load().memory() {
        return Ok(None);
    }
    let store = Store::open(&crate::store::default_path()?)?;
    let now = crate::store::now();
    let last: i64 = store.meta(NIGHTLY_KEY).and_then(|v| v.parse().ok()).unwrap_or(0);
    if now - last < 86_400 {
        return Ok(None);
    }
    store.set_meta(NIGHTLY_KEY, &now.to_string())?;
    let done = compact_all(&store, now, remove_state)?;
    if done != Compaction::default() {
        store.vacuum()?;
    }
    drop(store);
    refresh_snapshot()?;
    Ok(Some(done))
}

/// Export (docs/memory.md §2.5): one Markdown file per session into `dir`; how many were written.
pub fn export_all(store: &Store, dir: &Path, home: &Path) -> Result<usize> {
    std::fs::create_dir_all(dir)?;
    let mut n = 0;
    for s in sill_memory::sessions(store.conn())? {
        let text = sill_memory::export_markdown(store.conn(), &s, &kept_of(&s.id), home)?;
        std::fs::write(dir.join(sill_memory::export_name(&s)), text)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod compaction_tests {
    use super::*;

    #[test]
    fn compaction_steps_and_retention() {
        let dir = std::env::temp_dir().join(format!("sill-memory-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("s.db")).unwrap();
        let s = store.create("claude", "default").unwrap();
        store.append(&s.id, "user", "fix the proxy", "").unwrap();
        store.append(&s.id, "thought", "hmm", "").unwrap();
        store.append(&s.id, "agent", "done, the proxy is fixed", "").unwrap();
        let seq = store.append_tool(&s.id, "Edit proxy.rs", "Completed", "Edit", Some("body"), Some("--- a\n+++ b\n")).unwrap();
        store.end(&s.id, "the proxy was fixed", "agent").unwrap();
        let ended = store.sessions().unwrap()[0].ended_at.unwrap();
        // level 1: thoughts and details go, the agent's text and the tool title stay
        store.compact(&s.id, 1).unwrap();
        let e = store.entries(&s.id).unwrap();
        assert!(e.iter().all(|x| x.kind != "thought"));
        let tool = e.iter().find(|x| x.seq == seq).unwrap();
        assert_eq!((tool.text.as_str(), tool.detail.as_str(), tool.diff.as_str()), ("Edit proxy.rs", "", ""));
        assert!(e.iter().any(|x| x.kind == "agent"));
        assert_eq!(store.sessions().unwrap()[0].compacted, 1);
        // level 2: the agent's text goes, prompts and summary remain and are still searchable
        store.compact(&s.id, 2).unwrap();
        let e = store.entries(&s.id).unwrap();
        assert!(e.iter().all(|x| x.kind != "agent"));
        assert!(e.iter().any(|x| x.kind == "user"));
        let home = Path::new("/home/x");
        let hits = sill_memory::search(store.conn(), &sill_memory::Query::parse("proxy", home), ended, 10).unwrap();
        assert_eq!(hits.len(), 1);
        // compact_all honours the settings' zero = off and never touches an open session
        let open = store.create("claude", "default").unwrap();
        store.append(&open.id, "thought", "still here", "").unwrap();
        let removed = std::cell::RefCell::new(Vec::new());
        let done = compact_all(&store, ended + 400 * 86_400, &|id| removed.borrow_mut().push(id.to_string())).unwrap();
        assert_eq!(done.deleted, 0, "delete_after is off by default");
        assert_eq!(store.entries(&open.id).unwrap().len(), 1);
        // an ended session nobody prompted is deleted whatever the settings; its state too
        let idle = store.create("claude", "default").unwrap();
        store.end(&idle.id, "Session with claude.\n0 prompts, 0 tool calls.", "auto").unwrap();
        let done = compact_all(&store, ended, &|id| removed.borrow_mut().push(id.to_string())).unwrap();
        assert_eq!(done.deleted, 1);
        assert_eq!(*removed.borrow(), vec![idle.id.clone()]);
        assert!(store.sessions().unwrap().iter().all(|x| x.id != idle.id));
        assert_eq!(store.sessions().unwrap().len(), 2, "the prompted and the open session stay");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The recall block for session `s` with `pins` (docs/memory.md §2.3): the ended sessions its scope
/// picks, `n` at most, with how to search the rest — empty with memory off.
pub fn recall(store: &Store, s: &Session, pins: &[PathBuf], home: &Path, n: usize) -> String {
    if !crate::config::load().memory() {
        return String::new();
    }
    let scope = Scope::resolve(&s.memory_scope, !pins.is_empty());
    if scope == Scope::Off || n == 0 {
        return String::new(); // no block at all, not even the pointer at sill-memory (§4)
    }
    let follows = Some(s.follows.as_str()).filter(|f| !f.is_empty());
    let candidates = sill_memory::recall_candidates(store.conn(), &s.id, pins, scope, n, follows).unwrap_or_default();
    let picked = candidates.iter().collect::<Vec<_>>();
    let searchable = snapshot_dir().join(sill_memory::SNAPSHOT_FILE).is_file();
    sill_memory::recall_block(&picked, scope, searchable, home, |id| kept_of(id).len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_summaries() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_793_491_200), "2026-11-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        let home = Path::new("/home/me");
        let f = Facts {
            title: "proxy allow button".into(),
            agent: "claude".into(),
            created_at: 1_793_491_200,
            pins: vec![home.join("GIT/sill")],
            kept: vec![(home.join("GIT/sill/src/a.rs"), "updated".into()), (PathBuf::from("/etc/x"), "added".into())],
            prompts: 1,
            tool_calls: 12,
            first_prompt: Some("make the allow button\nsafer".into()),
        };
        let p = summary_prompt(&f, home);
        assert!(p.starts_with("[sill: this session is ending."), "{p}");
        assert!(p.contains("First line: `Title: `"), "{p}");
        assert!(checkpoint_prompt(&f, home).starts_with("[sill: a checkpoint"));
        assert!(p.contains("pinned folders: ~/GIT/sill; kept to the host (2): updated ~/GIT/sill/src/a.rs, added /etc/x; 1 prompt, 12 tool calls.]"), "{p}");
        let a = auto_summary(&f, 1_793_491_200 + 3600, home);
        assert_eq!(first_line(&a), "Asked: “make the allow button safer”");
        assert!(a.contains("\nSession “proxy allow button” with claude, 2026-11-01.\nPinned folders: ~/GIT/sill.\nKept to the host (2): updated ~/GIT/sill/src/a.rs, added /etc/x.\n1 prompt, 12 tool calls.\n(written by sill"), "{a}");
        let silent = auto_summary(&Facts { first_prompt: None, ..f.clone() }, 0, home);
        assert_eq!(first_line(&silent), "Session “proxy allow button” with claude, 2026-11-01.");
        assert_eq!(first_line("\n\n  hello \nworld"), "hello");
    }

    #[test]
    fn the_summary_names_the_session() {
        assert_eq!(split_title("Title: proxy allow button\nAsked: make it safer.\nDone."), (Some("proxy allow button".into()), "Asked: make it safer.\nDone.".into()));
        assert_eq!(split_title("**Title:** “Settings page sections.”\n\nbody"), (Some("Settings page sections".into()), "body".into()));
        assert_eq!(split_title("TITLE: x"), (Some("x".into()), "".into()));
        assert_eq!(split_title("Title:\nbody"), (None, "body".into()));
        assert_eq!(split_title("Asked: things\nTitle: late"), (None, "Asked: things\nTitle: late".into()));
        let long = format!("Title: {}\nb", "word ".repeat(12));
        let (t, _) = split_title(&long);
        assert_eq!(t.as_deref().map(|t| t.chars().count()), Some(40));
        assert!(t.unwrap().ends_with('…'));
    }
}
