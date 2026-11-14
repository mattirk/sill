//! Public Changes contracts. The existing checked filesystem operations remain authoritative.
use super::Hub;
use anyhow::Result;
use serde_json::{Value, json};
use sill_proto::{changes as api, rpc::Error, sessions::HostPath};
fn invalid(e: impl std::fmt::Display) -> anyhow::Error {
    Error::invalid_params(e).into()
}
fn domain(code: &str, text: &str) -> anyhow::Error {
    Error::domain(code, text).into()
}
fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(invalid)
}
fn path(p: &HostPath) -> Result<std::path::PathBuf> {
    p.to_path().map_err(invalid)
}
fn batch(n: usize) -> Result<()> {
    if n == 0 || n > 128 {
        Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(128), "expected 1–128 items")))
    } else {
        Ok(())
    }
}
fn limit(n: Option<u16>) -> Result<usize> {
    let n = n.unwrap_or(64) as usize;
    if !(1..=128).contains(&n) {
        Err(invalid("limit must be 1–128"))
    } else {
        Ok(n)
    }
}
fn session(h: &Hub, id: &str) -> Result<()> {
    if !sill_core::session::valid_id(id) {
        return Err(invalid("invalid session_id"));
    }
    h.with_store(|s| {
        if s.sessions()?.iter().any(|s| s.id == id) {
            Ok(())
        } else {
            Err(domain("not_found", "session not found"))
        }
    })
}
fn kept_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || id.contains('/') || id.chars().any(char::is_control) {
        Err(invalid("invalid keep_id"))
    } else {
        Ok(())
    }
}
fn live(repo: &HostPath, p: &HostPath) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let repo = path(repo)?;
    let p = path(p)?;
    let rel = p.strip_prefix(&repo).map_err(invalid)?.to_path_buf();
    Ok((repo, rel))
}
fn bounded<T: serde::Serialize>(v: T) -> Result<Value> {
    let v = serde_json::to_value(v)?;
    if serde_json::to_vec(&v)?.len() > 4 * 1024 * 1024 {
        return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Result, Some(4194304), "result exceeds 4 MiB")));
    }
    Ok(v)
}
fn done(h: &Hub, d: Result<sill_core::wire::Done>) -> Result<Value> {
    h.changes_changed();
    bounded(api::Done::from(d?))
}
/// Listings being read page by page: a listing is computed once, at its first page, and the
/// later pages read that one (the cursor names it and the row to go on from) — the overlays are
/// not scanned again for each page, and no listing is a mix of two scans. Held until its last page
/// is read; at most [`listings::HELD`] at a time, the oldest going first, and a cursor of one
/// that went is `snapshot_expired`: list again.
mod listings {
    use super::api;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    pub const HELD: usize = 8;
    type Rows = Arc<Vec<api::Entry>>;
    static LISTINGS: Mutex<VecDeque<(String, Rows)>> = Mutex::new(VecDeque::new());

    pub fn token() -> anyhow::Result<String> {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// A cursor's listing and the row it goes on from.
    pub fn cursor(cursor: &str) -> Option<(String, usize)> {
        if cursor.len() > 256 {
            return None;
        }
        serde_json::from_str(cursor).ok()
    }

    pub fn get(token: &str) -> Option<Rows> {
        LISTINGS.lock().unwrap().iter().find(|(t, _)| t == token).map(|(_, rows)| rows.clone())
    }

    /// Keep `rows` for the pages still to come.
    pub fn hold(token: String, rows: Rows) {
        let mut held = LISTINGS.lock().unwrap();
        if held.iter().any(|(t, _)| *t == token) {
            return;
        }
        if held.len() == HELD {
            held.pop_front();
        }
        held.push_back((token, rows));
    }

    pub fn drop(token: &str) {
        LISTINGS.lock().unwrap().retain(|(t, _)| t != token);
    }

    /// The rows of one page from row `from`: at most `limit` of them and 3 MiB, and the row the
    /// next page goes on from.
    pub fn page(rows: &[api::Entry], from: usize, limit: usize) -> anyhow::Result<(Vec<api::Entry>, usize)> {
        const BYTES: usize = 3 * 1024 * 1024;
        let (mut entries, mut bytes, mut at) = (Vec::new(), 0, from.min(rows.len()));
        while let Some(row) = rows.get(at) {
            let size = serde_json::to_vec(row)?.len();
            if size > BYTES {
                return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Record, None, "changes entry too large")));
            }
            if entries.len() == limit || bytes + size > BYTES {
                break;
            }
            bytes += size;
            at += 1;
            entries.push(row.clone());
        }
        Ok((entries, at))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use sill_proto::sessions::HostPath;

        fn rows(n: usize) -> Rows {
            let path = |p: String| HostPath::from_path(std::path::Path::new(&p));
            Arc::new((0..n).map(|i| api::Entry::Live { repo: path("/r".into()), path: path(format!("/r/{i}")), status: "M".into() }).collect())
        }

        /// The pages of a listing are cut from the one listing, each going on where the last
        /// ended; a listing is held while pages remain, the oldest going when too many are.
        #[test]
        fn pages_read_the_one_listing_and_only_so_many_are_held() {
            let listing = rows(5);
            let (first, at) = page(&listing, 0, 2).unwrap();
            assert_eq!((first.len(), at), (2, 2));
            let (rest, at) = page(&listing, at, 128).unwrap();
            assert_eq!((rest.len(), at), (3, 5), "the end: nothing to go on from");
            let (none, at) = page(&listing, 99, 2).unwrap();
            assert_eq!((none.len(), at), (0, 5), "a row past the end is the end");
            assert_eq!(cursor("[\"ab\",3]"), Some(("ab".to_string(), 3)));
            assert_eq!(cursor("nonsense"), None);
            assert_eq!(cursor(&format!("[\"{}\",3]", "a".repeat(300))), None);
            let tokens: Vec<String> = (0..=HELD).map(|_| token().unwrap()).collect();
            assert_ne!(tokens[0], tokens[1]);
            for t in &tokens {
                hold(t.clone(), listing.clone());
                hold(t.clone(), listing.clone());
            }
            assert!(get(&tokens[0]).is_none(), "the oldest went for the newest");
            assert!(tokens[1..].iter().all(|t| get(t).is_some()));
            drop(&tokens[1]);
            assert!(get(&tokens[1]).is_none(), "read to its end: no longer held");
            for t in &tokens {
                drop(t);
            }
        }
    }
}

impl Hub {
    /// Every pending entry in the page's order: pinned repositories, each session's writes, what
    /// was kept; and the sessions that could not be read.
    fn changes_rows(&self) -> Result<(Vec<api::Entry>, Vec<api::Unreadable>)> {
        let list = self.changes_list()?;
        let unreadable = list.unreadable;
        let mut rows = Vec::new();
        for r in list.live {
            for (rel, status) in r.status {
                let p = HostPath::from_path(&r.repo.join(rel));
                let key = json!(["live", "", p.bytes_base64, ""]).to_string();
                rows.push((
                    key,
                    api::Entry::Live {
                        repo: HostPath::from_path(&r.repo),
                        path: p,
                        status,
                    },
                ));
            }
        }
        for s in list.sessions {
            for (agent_state, r) in s
                .changes
                .into_iter()
                .map(|r| (false, r))
                .chain(s.agent_state.into_iter().map(|r| (true, r)))
            {
                let p = HostPath::from_path(&r.change.path);
                let key = json!(["overlay", s.id, p.bytes_base64, ""]).to_string();
                rows.push((
                    key,
                    api::Entry::Overlay {
                        session_id: s.id.clone(),
                        title: s.title.clone(),
                        ghost: s.ghost,
                        agent_state,
                        path: p,
                        kind: match r.change.kind {
                            sill_core::ledger::Kind::Added => "added",
                            sill_core::ledger::Kind::Modified => "modified",
                            sill_core::ledger::Kind::Deleted => "deleted",
                            sill_core::ledger::Kind::TypeChanged => "type_changed",
                            sill_core::ledger::Kind::Replaced => "replaced",
                        }
                        .into(),
                        file_type: format!("{:?}", r.change.file_type).to_lowercase(),
                        size: r.change.size.to_string(),
                        conflict: r.change.conflict,
                        written: r.written.map(|v| v.to_string()),
                        repo: (!r.repo.as_os_str().is_empty())
                            .then(|| HostPath::from_path(&r.repo)),
                        hash: r.hash,
                        protected: r.protected,
                    },
                ));
            }
            for k in s.kept {
                let p = HostPath::from_path(&k.path);
                let key = json!(["kept", s.id, p.bytes_base64, k.id]).to_string();
                rows.push((
                    key,
                    api::Entry::Kept {
                        session_id: s.id.clone(),
                        title: s.title.clone(),
                        ghost: s.ghost,
                        keep_id: k.id,
                        timestamp: k.ts.to_string(),
                        path: p,
                        what: k.what,
                        before: k.before,
                        hash: k.hash,
                    },
                ));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((rows.into_iter().map(|(_, row)| row).collect(), unreadable))
    }

    pub(super) fn changes_public(&self, method: &str, params: Value) -> Option<Result<Value>> {
        if !matches!(
            method,
            "_sill/changes.list"
                | "_sill/changes.diff"
                | "_sill/changes.keep"
                | "_sill/changes.revert"
                | "_sill/changes.undo"
                | "_sill/changes.git-revert"
                | "_sill/changes.history"
        ) {
            return None;
        }
        Some((|| match method {
            "_sill/changes.list" => {
                let a: api::List = parse(params)?;
                let limit = limit(a.limit)?;
                let mut unreadable = vec![];
                let (token, rows, from) = match &a.after {
                    None => {
                        let (rows, unread) = self.changes_rows()?;
                        unreadable = unread;
                        (listings::token()?, std::sync::Arc::new(rows), 0)
                    }
                    Some(cursor) => {
                        let (token, from) = listings::cursor(cursor).ok_or_else(|| domain("invalid_cursor", "invalid changes cursor"))?;
                        let rows = listings::get(&token).ok_or_else(|| domain("snapshot_expired", "this listing is no longer held; list again"))?;
                        (token, rows, from)
                    }
                };
                let (entries, at) = listings::page(&rows, from, limit)?;
                let mut page = api::Page { entries, next: None, unreadable };
                if at < rows.len() {
                    page.next = Some(json!([token, at]).to_string());
                    listings::hold(token, rows);
                } else {
                    listings::drop(&token);
                }
                bounded(page)
            }
            "_sill/changes.diff" => {
                let a: api::Diff = parse(params)?;
                let target = match a.target {
                    api::Target::Change {
                        session_id,
                        path: p,
                    } => {
                        session(self, &session_id)?;
                        sill_core::wire::DiffOf::Change {
                            session: session_id,
                            path: path(&p)?,
                        }
                    }
                    api::Target::Kept {
                        session_id,
                        keep_id,
                    } => {
                        session(self, &session_id)?;
                        kept_id(&keep_id)?;
                        sill_core::wire::DiffOf::Kept {
                            session: session_id,
                            id: keep_id,
                        }
                    }
                    api::Target::Live { repo, path } => {
                        let (repo, rel) = live(&repo, &path)?;
                        sill_core::wire::DiffOf::Live { repo, rel }
                    }
                };
                let text = self.changes_diff(target)?;
                let fingerprint = sill_core::sfp::create_fingerprint(&text);
                let offset = a
                    .offset
                    .as_deref()
                    .unwrap_or("0")
                    .parse::<usize>()
                    .map_err(invalid)?;
                if offset > 0 && a.fingerprint.as_ref() != Some(&fingerprint) {
                    return Err(domain(
                        "invalid_cursor",
                        "diff changed; restart from offset zero",
                    ));
                }
                if offset > text.len() || !text.is_char_boundary(offset) {
                    return Err(domain("invalid_cursor", "invalid diff offset"));
                }
                let mut end = text.len().min(offset + 1024 * 1024);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                bounded(api::DiffPage {
                    text: text[offset..end].into(),
                    next_offset: (end < text.len()).then(|| end.to_string()),
                    fingerprint,
                })
            }
            "_sill/changes.keep" => {
                let a: api::Keep = parse(params)?;
                session(self, &a.session_id)?;
                batch(a.items.len())?;
                let items = a
                    .items
                    .into_iter()
                    .map(|i| {
                        if i.expected_hash.as_ref().is_some_and(|s| {
                            s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit())
                        }) {
                            return Err(invalid("invalid expected_hash"));
                        }
                        Ok((path(&i.path)?, i.expected_hash))
                    })
                    .collect::<Result<Vec<_>>>()?;
                done(self, self.changes_keep(&a.session_id, items))
            }
            "_sill/changes.revert" => {
                let a: api::Revert = parse(params)?;
                session(self, &a.session_id)?;
                batch(a.paths.len())?;
                let paths = a.paths.iter().map(path).collect::<Result<_>>()?;
                done(self, self.changes_revert(&a.session_id, paths))
            }
            "_sill/changes.undo" => {
                let a: api::Undo = parse(params)?;
                session(self, &a.session_id)?;
                batch(a.keep_ids.len())?;
                for id in &a.keep_ids {
                    kept_id(id)?;
                }
                done(self, self.changes_undo(&a.session_id, a.keep_ids, a.force))
            }
            "_sill/changes.git-revert" => {
                let a: api::GitRevert = parse(params)?;
                let (repo, rel) = live(&a.repo, &a.path)?;
                let result = self.changes_git_revert(&repo, &rel);
                self.changes_changed();
                bounded(sill_proto::Text { text: result? })
            }
            "_sill/changes.history" => {
                let a: api::History = parse(params)?;
                let limit = limit(a.limit)?;
                let before = a
                    .before
                    .map(|s| {
                        serde_json::from_str::<(i64, i64)>(&s)
                            .map_err(|_| domain("invalid_cursor", "invalid history cursor"))
                    })
                    .transpose()?;
                let kind = a
                    .kind
                    .map(|s| {
                        sill_core::history::Kind::parse(&s)
                            .ok_or_else(|| invalid("unknown history kind"))
                    })
                    .transpose()?;
                let f = sill_core::history::Filter {
                    before,
                    since: a.since.map(|s| s.parse().map_err(invalid)).transpose()?,
                    path: a.path.as_ref().map(path).transpose()?,
                    agent: a.agent,
                    kind,
                    session: a.session_id,
                    limit: limit + 1,
                };
                let rows = self.changes_history(f)?;
                let more = rows.len() > limit;
                let mut events = Vec::new();
                let mut next = None;
                let mut bytes = 0;
                for e in rows.into_iter().take(limit) {
                    let cursor = json!([e.ts, e.id]).to_string();
                    let entry = api::HistoryEvent {
                        event_id: e.id.to_string(),
                        timestamp: e.ts.to_string(),
                        kind: e.kind.id().into(),
                        session_id: e.session_id,
                        session_title: e.session_title,
                        agent: e.agent,
                        ghost: e.ghost,
                        path: (!e.path.as_os_str().is_empty())
                            .then(|| HostPath::from_path(&e.path)),
                        what: e.what,
                        hash_new: e.hash_new,
                        hash_old: e.hash_old,
                        count: e.count.to_string(),
                        paths: e.paths.iter().map(|p| HostPath::from_path(p)).collect(),
                        keep_id: e.kept_id,
                        head_start: e.head_start,
                        head_end: e.head_end,
                        commits: e.commits.map(|v| v.to_string()),
                        files: e.files.map(|v| v.to_string()),
                    };
                    let size = serde_json::to_vec(&entry)?.len();
                    if size > 3 * 1024 * 1024 {
                        return Err(anyhow::Error::from(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Record, None, "history entry too large")));
                    }
                    if bytes + size > 3 * 1024 * 1024 {
                        return bounded(api::HistoryPage { events, next });
                    }
                    bytes += size;
                    events.push(entry);
                    next = Some(cursor);
                }
                bounded(api::HistoryPage {
                    events,
                    next: if more { next } else { None },
                })
            }
            _ => unreachable!(),
        })())
    }
}
