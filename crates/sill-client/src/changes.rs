//! The Changes page's data (reference `07`, `shelf/changes.rs`): what sessions wrote outside their
//! pins — held in their overlays until the person keeps or reverts it — the pinned repositories'
//! own changes, which are live on the host, and changes kept recently, which can be undone. Groups,
//! requests and diffs, without a toolkit.
use serde_json::{Value, json};
use sill_proto::Client;
pub use sill_proto::changes::Entry;
use sill_proto::changes::{
    Diff, DiffPage, Done, GitRevert, History, HistoryPage, Keep, KeepItem, List, Page, Revert,
    Target, Undo,
};
use sill_proto::sessions::HostPath;

use crate::files::key;

/// A ghost's writes revert after this long without a keep (docs/memory.md §3.2).
pub const GHOST_MINUTES: u64 = 60;
/// Settings → auto-revert when it was never set: a day (sill-core's `AUTO_REVERT_DEFAULT`).
pub const AUTO_REVERT_DEFAULT: u64 = 1440;

/// The auto-revert minutes a settings record means: its value, or the default when unset.
pub fn auto_revert(settings: &sill_proto::admin::Settings) -> u64 {
    settings
        .auto_revert_minutes
        .map_or(AUTO_REVERT_DEFAULT, u64::from)
}

/// One request: the method and its parameters.
pub type Request = (&'static str, Value);

#[derive(Debug, Clone)]
pub enum Kind {
    /// A pinned repository's own changes: live on the host, as git sees them.
    Live { repo: HostPath },
    /// A session's writes outside its pins, in one repository (or none).
    Session {
        session_id: String,
        title: String,
        ghost: bool,
        repo: Option<HostPath>,
    },
    /// Changes kept from a session, which can be undone.
    Kept { session_id: String, title: String },
}

#[derive(Debug, Clone)]
pub struct Group {
    pub kind: Kind,
    pub entries: Vec<Entry>,
}

impl Group {
    /// The session whose overlay the group's changes are in.
    pub fn session(&self) -> Option<&str> {
        match &self.kind {
            Kind::Session { session_id, .. } | Kind::Kept { session_id, .. } => Some(session_id),
            Kind::Live { .. } => None,
        }
    }
}

/// Entries grouped as the page shows them, in the daemon's order: pinned repositories, then each
/// session's writes by repository, then what was kept.
pub fn groups(entries: &[Entry]) -> Vec<Group> {
    let mut groups: Vec<(String, Group)> = vec![];
    for entry in entries {
        let (name, kind) = match entry {
            Entry::Live { repo, .. } => (
                format!("live {}", key(repo)),
                Kind::Live { repo: repo.clone() },
            ),
            Entry::Overlay {
                session_id,
                title,
                ghost,
                repo,
                ..
            } => (
                format!("session {session_id} {}", repo.as_ref().map_or("", key)),
                Kind::Session {
                    session_id: session_id.clone(),
                    title: title.clone(),
                    ghost: *ghost,
                    repo: repo.clone(),
                },
            ),
            Entry::Kept {
                session_id, title, ..
            } => (
                format!("kept {session_id}"),
                Kind::Kept {
                    session_id: session_id.clone(),
                    title: title.clone(),
                },
            ),
        };
        match groups.iter_mut().find(|(n, _)| *n == name) {
            Some((_, group)) => group.entries.push(entry.clone()),
            None => groups.push((
                name,
                Group {
                    kind,
                    entries: vec![entry.clone()],
                },
            )),
        }
    }
    let rank = |g: &Group| match g.kind {
        Kind::Live { .. } => 0,
        Kind::Session { .. } => 1,
        Kind::Kept { .. } => 2,
    };
    let mut groups: Vec<Group> = groups.into_iter().map(|(_, g)| g).collect();
    groups.sort_by_key(rank);
    groups
}

/// A stable identity for an entry, for the page's open diffs and armed buttons.
pub fn entry_key(entry: &Entry) -> String {
    match entry {
        Entry::Live { repo, path, .. } => format!("live {} {}", key(repo), key(path)),
        Entry::Overlay {
            session_id, path, ..
        } => format!("overlay {session_id} {}", key(path)),
        Entry::Kept {
            session_id,
            keep_id,
            ..
        } => format!("kept {session_id} {keep_id}"),
    }
}

pub fn path(entry: &Entry) -> &HostPath {
    match entry {
        Entry::Live { path, .. } | Entry::Overlay { path, .. } | Entry::Kept { path, .. } => path,
    }
}

/// What the entry's diff is read from.
pub fn target(entry: &Entry) -> Target {
    match entry {
        Entry::Live { repo, path, .. } => Target::Live {
            repo: repo.clone(),
            path: path.clone(),
        },
        Entry::Overlay {
            session_id, path, ..
        } => Target::Change {
            session_id: session_id.clone(),
            path: path.clone(),
        },
        Entry::Kept {
            session_id,
            keep_id,
            ..
        } => Target::Kept {
            session_id: session_id.clone(),
            keep_id: keep_id.clone(),
        },
    }
}

/// Keep these overlay entries (copy them to the host), each only if it is still the version
/// shown.
pub fn keep(session_id: &str, entries: &[&Entry]) -> Request {
    let items = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Overlay { path, hash, .. } => Some(KeepItem {
                path: path.clone(),
                expected_hash: hash.clone(),
            }),
            _ => None,
        })
        .collect();
    let keep = Keep {
        session_id: session_id.into(),
        items,
    };
    ("_sill/changes.keep", json!(keep))
}

/// Drop these overlay writes.
pub fn revert(session_id: &str, entries: &[&Entry]) -> Request {
    let revert = Revert {
        session_id: session_id.into(),
        paths: entries.iter().map(|e| path(e).clone()).collect(),
    };
    ("_sill/changes.revert", json!(revert))
}

/// Undo a keep: the host file goes back to what it was before.
pub fn undo(session_id: &str, keep_id: &str) -> Request {
    let undo = Undo {
        session_id: session_id.into(),
        keep_ids: vec![keep_id.into()],
        force: false,
    };
    ("_sill/changes.undo", json!(undo))
}

/// `git checkout HEAD` for a pinned repository's file; an untracked file is removed.
pub fn git_revert(repo: &HostPath, path: &HostPath) -> Request {
    let revert = GitRevert {
        repo: repo.clone(),
        path: path.clone(),
    };
    ("_sill/changes.git-revert", json!(revert))
}

/// Can be kept: an overlay write the host has not changed meanwhile, of a path sill keeps.
pub fn keepable(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Overlay {
            conflict: false,
            protected: None,
            ..
        }
    )
}

/// Every pending entry, page by page.
pub fn list(client: &Client) -> anyhow::Result<Vec<Entry>> {
    let mut entries = vec![];
    let mut after = None;
    loop {
        let page: Page = client.call(
            "_sill/changes.list",
            List {
                after,
                limit: Some(128),
            },
        )?;
        entries.extend(page.entries);
        anyhow::ensure!(entries.len() <= 20_000, "too many changes to show");
        after = page.next;
        if after.is_none() {
            return Ok(entries);
        }
    }
}

/// History before `before` (newest first).
pub fn history(client: &Client, before: Option<String>) -> anyhow::Result<HistoryPage> {
    client.call(
        "_sill/changes.history",
        History {
            before,
            limit: Some(100),
            ..History::default()
        },
    )
}

/// An entry's whole diff, read page by page against one fingerprint: a file that changes
/// meanwhile is an error, never a mix of two versions.
pub fn diff(client: &Client, target: Target) -> anyhow::Result<String> {
    let mut text = String::new();
    let mut offset = None;
    let mut fingerprint: Option<String> = None;
    loop {
        let page: DiffPage = client.call(
            "_sill/changes.diff",
            Diff {
                target: target.clone(),
                offset: offset.clone(),
                fingerprint: fingerprint.clone(),
            },
        )?;
        if let Some(expected) = &fingerprint {
            anyhow::ensure!(
                *expected == page.fingerprint,
                "The file changed while reading its diff. Open it again."
            );
        }
        text.push_str(&page.text);
        anyhow::ensure!(
            text.len() <= crate::files::MAX_TEXT,
            "This diff exceeds the 2 MiB preview limit."
        );
        match page.next_offset {
            Some(next) => {
                offset = Some(next);
                fingerprint = Some(page.fingerprint);
            }
            None => return Ok(text),
        }
    }
}

/// What a keep, revert or undo did, in one line (the shelf's message under the page).
pub fn said(verb: &str, value: &Value) -> String {
    let Ok(done) = serde_json::from_value::<Done>(value.clone()) else {
        return format!("{verb}: done");
    };
    let mut line = match done.done.len() {
        0 => format!("nothing to {verb}"),
        1 => format!("{verb}: {}", crate::files::name(&done.done[0].path)),
        n => format!("{verb}: {n} files"),
    };
    if !done.failed.is_empty() {
        let why: Vec<String> = done
            .failed
            .iter()
            .map(|f| format!("{} ({})", crate::files::name(&f.path), f.message))
            .collect();
        line.push_str(&format!("; not done: {}", why.join(", ")));
    }
    line
}

/// When an overlay write reverts by itself, in seconds from `now`: a ghost's after an hour, other
/// writes outside a repository after Settings → auto-revert (0 is off).
pub fn reverts_in(entry: &Entry, auto_revert_minutes: u64, now: i64) -> Option<u64> {
    let Entry::Overlay {
        ghost,
        written,
        repo,
        conflict: false,
        ..
    } = entry
    else {
        return None;
    };
    let minutes = if *ghost {
        GHOST_MINUTES
    } else if repo.is_none() && auto_revert_minutes > 0 {
        auto_revert_minutes
    } else {
        return None;
    };
    let written: i64 = written.as_deref()?.parse().ok()?;
    let age = (now - written).max(0) as u64;
    Some((minutes * 60).saturating_sub(age))
}

/// `45 min`, `17 h`.
pub fn human(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    if minutes >= 120 {
        format!("{} h", minutes / 60)
    } else {
        format!("{minutes} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> HostPath {
        HostPath::from_path(std::path::Path::new(p))
    }

    fn overlay(session: &str, file: &str, ghost: bool, written: i64) -> Entry {
        serde_json::from_value(json!({
            "type":"overlay", "session_id": session, "title": format!("{session} title"),
            "ghost": ghost, "agent_state": false, "path": path(file), "kind":"added",
            "file_type":"file", "size":"3", "conflict": false, "written": written.to_string(),
            "repo": null, "hash":"h1", "protected": null
        }))
        .unwrap()
    }

    #[test]
    fn entries_group_by_repository_then_session_then_kept() {
        let entries = vec![
            overlay("s1", "/tmp/a", false, 0),
            serde_json::from_value(json!({"type":"live", "repo": path("/r"),
                "path": path("/r/x"), "status":"?"}))
            .unwrap(),
            overlay("s1", "/tmp/b", false, 0),
            serde_json::from_value(json!({"type":"kept", "session_id":"s1", "title":"t",
                "ghost": false, "keep_id":"k1", "timestamp":"1", "path": path("/tmp/c"),
                "what":"wrote", "before":"absent", "hash": null}))
            .unwrap(),
        ];
        let groups = groups(&entries);
        assert!(matches!(groups[0].kind, Kind::Live { .. }));
        assert!(matches!(&groups[1].kind, Kind::Session { session_id, .. } if session_id == "s1"));
        assert_eq!(groups[1].entries.len(), 2);
        assert!(matches!(groups[2].kind, Kind::Kept { .. }));
        let refs: Vec<&Entry> = groups[1].entries.iter().collect();
        let (method, params) = keep("s1", &refs);
        assert_eq!(method, "_sill/changes.keep");
        serde_json::from_value::<Keep>(params.clone()).unwrap();
        assert_eq!(params["items"][0]["expected_hash"], "h1");
        let (_, params) = undo("s1", "k1");
        assert_eq!(
            params,
            json!({"session_id":"s1", "keep_ids":["k1"], "force": false})
        );
        assert_eq!(
            git_revert(&path("/r"), &path("/r/x")).0,
            "_sill/changes.git-revert"
        );
    }

    #[test]
    fn writes_revert_by_themselves_on_a_ghost_or_outside_repositories() {
        let ghost = overlay("g", "/tmp/a", true, 1_000);
        assert_eq!(reverts_in(&ghost, 0, 1_000 + 600), Some(3_000));
        let plain = overlay("s", "/tmp/a", false, 1_000);
        assert_eq!(reverts_in(&plain, 0, 2_000), None, "auto-revert off");
        assert_eq!(reverts_in(&plain, 24 * 60, 1_000), Some(24 * 3_600));
        assert_eq!(human(3_000), "50 min");
        assert_eq!(human(17 * 3_600), "17 h");
        let done = json!({"done":[{"path": path("/tmp/a"), "message":""}],
                          "failed":[{"path": path("/tmp/b"), "message":"changed on the host"}]});
        assert_eq!(
            said("kept", &done),
            "kept: a; not done: b (changed on the host)"
        );
        let unset: sill_proto::admin::Settings =
            serde_json::from_value(json!({"theme": "dracula"})).unwrap();
        assert_eq!(
            auto_revert(&unset),
            AUTO_REVERT_DEFAULT,
            "unset is a day, not off"
        );
    }
}
