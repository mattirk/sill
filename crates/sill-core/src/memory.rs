//! Memory (docs/memory.md): what sill keeps of a session once it ends — M0, "remember". The summary
//! is the agent's own last turn when it can be asked ([`summary_prompt`] is what it is asked);
//! otherwise sill writes one from the facts it has ([`auto_summary`]). Both end up in
//! `sessions.summary`, with `summary_kind` saying which. M1, "recall and search": the queries are
//! `sill_memory`'s (the same on the host and in the jail); this module adds what needs the store and
//! the host — the jail's snapshot ([`refresh_snapshot`]) and the recall block ([`recall`]).

use std::path::{Path, PathBuf};

use anyhow::Result;

pub use sill_memory::{Scope, date, first_line, tilde};

use crate::store::{Session, Store};

/// What sill knows of a session without asking anyone.
#[derive(Debug, Clone, Default)]
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

/// What the agent is asked as the session's last turn (docs/memory.md §2.2). The facts go with it
/// so the summary names real places, not "the file I edited".
pub fn summary_prompt(f: &Facts, home: &Path) -> String {
    format!(
        "[sill: this session is ending. Write 5–15 lines for a future agent that opens these folders: what was asked, what was done and where (paths), decisions and why, what is unfinished, what not to try again. Plain text, no greeting, no questions — only that agent reads it. Facts sill has: {}.]",
        fact_lines(f, home).join("; ")
    )
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
    let dir = snapshot_dir();
    if !crate::config::load().memory() {
        let _ = std::fs::remove_file(dir.join(sill_memory::SNAPSHOT_FILE));
        return Ok(());
    }
    let db = crate::store::default_path()?;
    let store = Store::open(&db)?; // migrations and the index, before the snapshot reads them
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

/// The recall block for session `s` with `pins` (docs/memory.md §2.3): the ended sessions its scope
/// picks, `n` at most, with how to search the rest — empty with memory off.
pub fn recall(store: &Store, s: &Session, pins: &[PathBuf], home: &Path, n: usize) -> String {
    if !crate::config::load().memory() {
        return String::new();
    }
    let scope = Scope::resolve(&s.memory_scope, !pins.is_empty());
    let all = sill_memory::sessions(store.conn()).unwrap_or_default();
    let follows = Some(s.follows.as_str()).filter(|f| !f.is_empty());
    let picked = sill_memory::recall_pick(&all, &s.id, pins, scope, n, follows);
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
        assert!(p.contains("pinned folders: ~/GIT/sill; kept to the host (2): updated ~/GIT/sill/src/a.rs, added /etc/x; 1 prompt, 12 tool calls.]"), "{p}");
        let a = auto_summary(&f, 1_793_491_200 + 3600, home);
        assert_eq!(first_line(&a), "Asked: “make the allow button safer”");
        assert!(a.contains("\nSession “proxy allow button” with claude, 2026-11-01.\nPinned folders: ~/GIT/sill.\nKept to the host (2): updated ~/GIT/sill/src/a.rs, added /etc/x.\n1 prompt, 12 tool calls.\n(written by sill"), "{a}");
        let silent = auto_summary(&Facts { first_prompt: None, ..f.clone() }, 0, home);
        assert_eq!(first_line(&silent), "Session “proxy allow button” with claude, 2026-11-01.");
        assert_eq!(first_line("\n\n  hello \nworld"), "hello");
    }
}
