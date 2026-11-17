//! Memory records exposed independently of storage structs and CLI formatting helpers.
use super::Hub;
use anyhow::Result;
use serde_json::{Value, json};
use sill_proto::method;
use sill_proto::{memory as api, rpc::Error};
fn invalid(s: impl ToString) -> anyhow::Error {
    Error::invalid_params(s.to_string()).into()
}
fn domain(c: &'static str, s: &str) -> anyhow::Error {
    sill_core::sfp::DomainError { code: c, message: s.into(), details: None }.into()
}
fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(invalid)
}
fn limit(n: Option<u16>) -> Result<usize> {
    let n = n.unwrap_or(64) as usize;
    if !(1..=128).contains(&n) {
        return Err(invalid("limit must be 1–128"));
    }
    Ok(n)
}
fn exact(s: &sill_core::store::Store, id: &str) -> Result<()> {
    if !sill_core::session::valid_id(id) {
        return Err(invalid("invalid session_id"));
    }
    if !s.session_exact(id)?.is_some_and(|s| !s.ghost) {
        return Err(domain("not_found", "memory session not found"));
    }
    Ok(())
}
fn bounded(v: impl serde::Serialize) -> Result<Value> {
    let v = json!(v);
    if serde_json::to_vec(&v)?.len() > 4 * 1024 * 1024 {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Result, Some(4194304), "memory page exceeds 4 MiB"));
    }
    Ok(v)
}

/// The requests answered here, each named once (`HANDLERS` in hub.rs holds every such list).
pub(super) const METHODS: &[&str] = &[
    method::MEMORY_TEXT_CLOSE,
    method::MEMORY_COMPACT,
    method::MEMORY_FORGET,
    method::MEMORY_LIST,
    method::MEMORY_TEXT,
    method::MEMORY_SEARCH,
    method::MEMORY_NOTES_LIST,
    method::MEMORY_NOTES_ADD,
    method::MEMORY_NOTES_REMOVE,
    method::MEMORY_REFRESH,
];
pub fn handle(hub: &Hub, conn: u64, m: &str, p: Value) -> Option<Result<Value>> {
    if !METHODS.contains(&m) {
        return None;
    }
    Some((|| match m {
        method::MEMORY_COMPACT => {
            let _: sill_proto::Empty = args(p)?;
            let uid = rustix::process::getuid().as_raw();
            let done = hub.with_store(|s| {
                let d = sill_core::memory::compact_all(s, sill_core::store::now(), &|id| {
                    let _ = sill_core::session::remove_state(uid, id);
                })?;
                s.vacuum()?;
                Ok(d)
            })?;
            let _ = sill_core::memory::refresh_snapshot();
            bounded(api::Compacted { compacted: done.compacted.to_string(), trimmed: done.trimmed.to_string(), deleted: done.deleted.to_string() })
        }
        method::MEMORY_FORGET => {
            let _: sill_proto::Empty = args(p)?;
            let ids = hub.with_store(|s| Ok(s.sessions()?.into_iter().filter(|s| s.ended_at.is_some()).map(|s| s.id).collect::<Vec<_>>()))?;
            for id in &ids {
                hub.delete_session(id)?;
            }
            bounded(api::Forgotten { deleted: ids.len().to_string() })
        }
        method::MEMORY_LIST => {
            let a: api::List = args(p)?;
            let n = limit(a.limit)?;
            let store = sill_core::store::Store::open_read_only(&sill_core::store::default_path()?)?;
            let mut q=store.conn().prepare("SELECT id,agent,title,ended_at,CASE WHEN instr(summary,char(10))>0 THEN substr(summary,1,instr(summary,char(10))-1) ELSE summary END FROM sessions WHERE ghost=0 AND ended_at IS NOT NULL AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")?;
            let rows = q.query_map(rusqlite::params![a.after, (n + 1) as i64], |r| {
                Ok(api::Session { session_id: r.get(0)?, agent: r.get(1)?, title: r.get(2)?, ended_at: r.get::<_, i64>(3)?.to_string(), summary: r.get(4)? })
            })?;
            let mut sessions = Vec::new();
            let mut bytes = 0;
            let mut more = false;
            for row in rows {
                let row = row?;
                let size = serde_json::to_vec(&row)?.len();
                if size > 4 * 1024 * 1024 {
                    return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4194304), "memory metadata record exceeds 4 MiB"));
                }
                if sessions.len() == n || bytes + size > 4 * 1024 * 1024 {
                    more = true;
                    break;
                }
                bytes += size;
                sessions.push(row);
            }
            let next = if more { sessions.last().map(|s| s.session_id.clone()) } else { None };
            bounded(api::Page { sessions, next })
        }
        method::MEMORY_TEXT => bounded(text_page(hub, conn, args(p)?)?),
        method::MEMORY_TEXT_CLOSE => {
            let a: api::TextClose = args(p)?;
            let mut leases = leases().lock().unwrap();
            if leases.get(&a.snapshot_id).is_some_and(|l| l.conn == conn) {
                leases.remove(&a.snapshot_id);
            }
            Ok(json!({}))
        }
        method::MEMORY_SEARCH => {
            let a: api::Search = args(p)?;
            let n = limit(a.limit)?;
            let start = a.after.unwrap_or(0) as usize;
            if start > 500 || a.query.len() > 4096 {
                return Err(invalid("search cursor exceeds 500 or query exceeds 4096 bytes"));
            }
            let q = sill_memory::Query::parse(&a.query, &hub.home);
            if q.is_empty() {
                return Err(invalid("empty search"));
            }
            let store = sill_core::store::Store::open_read_only(&sill_core::store::default_path()?)?;
            let mut hits = Vec::new();
            let mut accepted = 0;
            let mut bytes = 0;
            let mut more = false;
            sill_memory::search_visit(store.conn(), &q, sill_core::store::now(), 500, |h| {
                let ghost: bool = store.conn().query_row("SELECT ghost FROM sessions WHERE id=?1", [&h.session.id], |r| r.get(0))?;
                if ghost {
                    return Ok(true);
                }
                if accepted < start {
                    accepted += 1;
                    return Ok(true);
                }
                let hit = api::Hit { text: sill_memory::hit_lines(&h, &hub.home), session_id: h.session.id };
                let size = serde_json::to_vec(&hit)?.len();
                if size > 4 * 1024 * 1024 {
                    return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4194304), "memory hit exceeds 4 MiB"));
                }
                if hits.len() == n || bytes + size > 4 * 1024 * 1024 {
                    more = true;
                    return Ok(false);
                }
                bytes += size;
                hits.push(hit);
                accepted += 1;
                Ok(true)
            })?;
            let next = more.then_some(accepted as u16);
            bounded(api::Hits { hits, next })
        }
        method::MEMORY_NOTES_LIST => {
            let a: api::NotesList = args(p)?;
            let n = limit(a.limit)?;
            let after = a.after.as_deref().unwrap_or("0").parse::<i64>().map_err(invalid)?;
            let store = sill_core::store::Store::open_read_only(&sill_core::store::default_path()?)?;
            exact(&store, &a.session_id)?;
            let mut q = store
                .conn()
                .prepare("SELECT rowid,ts,kind,text,length(CAST(text AS BLOB)) FROM memories WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3")?;
            let mut rows = q.query(rusqlite::params![a.session_id, after, (n + 1) as i64])?;
            let mut notes = Vec::new();
            let mut bytes = 0;
            let mut more = false;
            while let Some(r) = rows.next()? {
                if r.get::<_, i64>(4)? > 4 * 1024 * 1024 {
                    return Err(sill_core::sfp::DomainError::limit(
                        sill_proto::rpc::LimitScope::Record,
                        Some(4194304),
                        "memory note exceeds 4 MiB; use memory.text instead",
                    ));
                }
                let note = api::Note { cursor: r.get::<_, i64>(0)?.to_string(), timestamp: r.get::<_, i64>(1)?.to_string(), kind: r.get(2)?, text: r.get(3)? };
                let size = serde_json::to_vec(&note)?.len();
                if size > 4 * 1024 * 1024 {
                    return Err(sill_core::sfp::DomainError::limit(
                        sill_proto::rpc::LimitScope::Record,
                        Some(4194304),
                        "encoded memory note exceeds 4 MiB; use memory.text instead",
                    ));
                }
                if notes.len() == n || bytes + size > 4 * 1024 * 1024 {
                    more = true;
                    break;
                }
                bytes += size;
                notes.push(note);
            }
            let next = if more { notes.last().map(|n| n.cursor.clone()) } else { None };
            bounded(api::Notes { notes, next })
        }
        method::MEMORY_NOTES_ADD => {
            let a: api::AddNote = args(p)?;
            if a.text.len() > 1024 * 1024 {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "note exceeds 1 MiB"));
            }
            hub.with_store(|s| {
                exact(s, &a.session_id)?;
                s.add_memory(&a.session_id, "note", &a.text)
            })?;
            let _ = sill_core::memory::refresh_snapshot();
            Ok(json!({}))
        }
        method::MEMORY_NOTES_REMOVE => {
            let a: api::RemoveNote = args(p)?;
            let ts = a.timestamp.parse::<i64>().map_err(invalid)?;
            hub.with_store(|s| {
                exact(s, &a.session_id)?;
                s.remove_memory(&a.session_id, ts, &a.text)
            })?;
            let _ = sill_core::memory::refresh_snapshot();
            Ok(json!({}))
        }
        method::MEMORY_REFRESH => {
            let _: sill_proto::Empty = args(p)?;
            sill_core::memory::refresh_snapshot()?;
            Ok(json!({}))
        }
        _ => unreachable!(),
    })())
}

// Completed exports are immutable anonymous disk files. Building uses a separate read-only
// SQLite transaction, so long exports neither hold Hub's writer mutex nor rebuild on each page.
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
const TEXT_TTL: Duration = Duration::from_secs(60);
const MAX_TEXT_BYTES: usize = 256 * 1024 * 1024;
const MAX_TEXT_TOTAL: usize = 512 * 1024 * 1024;
struct TextLease {
    conn: u64,
    session: String,
    identity: String,
    file: Option<File>,
    bytes: usize,
    fingerprint: String,
    expires: Instant,
    deadline: Instant,
}
fn leases() -> &'static Mutex<HashMap<String, TextLease>> {
    static LEASES: OnceLock<Mutex<HashMap<String, TextLease>>> = OnceLock::new();
    LEASES.get_or_init(Default::default)
}
pub(super) fn sweep() {
    leases().lock().unwrap().retain(|_, l| l.expires > Instant::now());
}
pub(super) fn disconnect(conn: u64) {
    leases().lock().unwrap().retain(|_, l| l.conn != conn);
}
struct Reservation(String);
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut leases = leases().lock().unwrap();
        if leases.get(&self.0).is_some_and(|l| l.file.is_none()) {
            leases.remove(&self.0);
        }
    }
}
struct TextWriter {
    file: File,
    bytes: usize,
    failed: Option<anyhow::Error>,
    deadline: Instant,
}
impl std::fmt::Write for TextWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.failed.is_some() {
            return Err(std::fmt::Error);
        }
        if self.bytes.saturating_add(s.len()) > MAX_TEXT_BYTES {
            self.failed = Some(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(268435456), "memory snapshot exceeds 256 MiB"));
            return Err(std::fmt::Error);
        }
        if Instant::now() > self.deadline {
            self.failed =
                Some(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, None, "memory snapshot exceeded its 30-second build budget"));
            return Err(std::fmt::Error);
        }
        if let Err(e) = self.file.write_all(s.as_bytes()) {
            self.failed = Some(e.into());
            return Err(std::fmt::Error);
        }
        self.bytes += s.len();
        Ok(())
    }
}
fn create_text(hub: &Hub, conn: u64, a: &api::TextRead, identity: &str) -> Result<String> {
    let id = format!("{}{}{}{}", sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id());
    {
        let mut leases = leases().lock().unwrap();
        leases.retain(|_, l| l.expires > Instant::now());
        if leases.len() >= 32
            || leases.values().filter(|l| l.conn == conn).count() >= 4
            || leases.values().map(|l| l.bytes).sum::<usize>().saturating_add(MAX_TEXT_BYTES) > MAX_TEXT_TOTAL
        {
            return Err(sill_core::sfp::DomainError::limit(
                sill_proto::rpc::LimitScope::Capacity,
                None,
                "memory snapshot lease or disk budget exhausted; close old snapshots",
            ));
        }
        leases.insert(
            id.clone(),
            TextLease {
                conn,
                session: a.session_id.clone(),
                identity: identity.into(),
                file: None,
                bytes: MAX_TEXT_BYTES,
                fingerprint: String::new(),
                expires: Instant::now() + TEXT_TTL,
                deadline: Instant::now() + Duration::from_secs(1800),
            },
        );
    }
    let _reservation = Reservation(id.clone());
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        let path = sill_proto::runtime_dir()?.join(format!(".memory-{id}"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        // Unlink immediately: crash, disconnect, expiry and all errors cannot leave exports behind.
        std::fs::remove_file(&path)?;
        file
    };
    let mut writer = TextWriter { file, bytes: 0, failed: None, deadline: Instant::now() + Duration::from_secs(30) };
    let store = sill_core::store::Store::open_read_only(&sill_core::store::default_path()?)?;
    let ghost: Option<bool> = store.conn().query_row("SELECT ghost FROM sessions WHERE id=?1", [&a.session_id], |r| r.get(0)).optional()?;
    if ghost != Some(false) {
        return Err(domain("not_found", "memory session not found"));
    }
    // A pathological individual row must not allocate unbounded memory in formatters.
    let largest:i64=store.conn().query_row("SELECT max(n) FROM (SELECT coalesce(max(length(CAST(text AS BLOB))),0) n FROM entries WHERE session_id=?1 UNION ALL SELECT coalesce(max(length(CAST(text AS BLOB))),0) FROM memories WHERE session_id=?1 UNION ALL SELECT length(CAST(summary AS BLOB)) FROM sessions WHERE id=?1)",[&a.session_id],|r|r.get(0))?;
    if largest > 64 * 1024 * 1024 {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(67108864), "memory record exceeds 64 MiB"));
    }
    let info = sill_memory::find_session(store.conn(), &a.session_id)?.ok_or_else(|| domain("not_found", "session not found"))?;
    let rendered = match a.format {
        api::Format::Show => sill_memory::show_to(store.conn(), &info, &sill_core::memory::kept_of(&a.session_id), &hub.home, &mut writer),
        api::Format::Transcript => sill_memory::transcript_to(store.conn(), &info, a.grep.as_deref(), &mut writer),
        api::Format::Export => sill_memory::export_markdown_to(store.conn(), &info, &sill_core::memory::kept_of(&a.session_id), &hub.home, &mut writer),
        api::Format::Recall => {
            let row = store.session_exact(&a.session_id)?.ok_or_else(|| domain("not_found", "session not found"))?;
            let text = sill_core::memory::recall(&store, &row, &info.pins, &hub.home, sill_core::config::load().memory_recall());
            std::fmt::Write::write_str(&mut writer, &text).map_err(Into::into)
        }
    };
    if let Some(e) = writer.failed.take() {
        return Err(e);
    }
    rendered?;
    drop(store); // Release the SQLite read snapshot before serving pages.
    writer.file.seek(SeekFrom::Start(0))?;
    let fingerprint = sill_core::keep::sha256_reader(&mut writer.file)?;
    let mut leases = leases().lock().unwrap();
    let lease = leases.get_mut(&id).ok_or_else(|| domain("snapshot_expired", "connection closed while building memory snapshot"))?;
    lease.file = Some(writer.file);
    lease.bytes = writer.bytes;
    lease.fingerprint = fingerprint;
    lease.expires = Instant::now() + TEXT_TTL;
    Ok(id)
}
fn text_page(hub: &Hub, conn: u64, a: api::TextRead) -> Result<api::TextPage> {
    if !sill_core::session::valid_id(&a.session_id) {
        return Err(invalid("invalid session_id"));
    }
    if a.grep.as_ref().is_some_and(|s| s.len() > 4096) {
        return Err(invalid("grep exceeds 4096 bytes"));
    }
    let n = a.length.unwrap_or(1024 * 1024) as usize;
    if n == 0 || n > 1024 * 1024 {
        return Err(invalid("length must be 1–1048576"));
    }
    let offset = a.offset.as_deref().unwrap_or("0").parse::<usize>().map_err(invalid)?;
    let identity = sill_core::sfp::create_fingerprint(&serde_json::to_string(&(&a.session_id, &a.format, &a.grep))?);
    let id = match &a.snapshot_id {
        Some(id) => id.clone(),
        None => {
            if offset != 0 || a.fingerprint.is_some() {
                return Err(domain("invalid_cursor", "start a new memory snapshot at offset zero without a fingerprint"));
            }
            create_text(hub, conn, &a, &identity)?
        }
    };
    let mut leases = leases().lock().unwrap();
    leases.retain(|_, l| l.expires > Instant::now());
    let lease =
        leases.get_mut(&id).filter(|l| l.conn == conn).ok_or_else(|| domain("snapshot_expired", "memory snapshot expired or belongs to another connection"))?;
    if lease.session != a.session_id
        || lease.identity != identity
        || offset > lease.bytes
        || a.fingerprint.as_ref().is_some_and(|f| f != &lease.fingerprint)
        || (offset > 0 && a.fingerprint.is_none())
    {
        return Err(domain("invalid_cursor", "memory snapshot parameters or fingerprint do not match"));
    }
    let file = lease.file.as_mut().ok_or_else(|| domain("busy", "memory snapshot is still building"))?;
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut bytes = vec![0; n.min(lease.bytes - offset)];
    file.read_exact(&mut bytes)?;
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        Err(e) if e.error_len().is_none() => bytes.truncate(e.valid_up_to()),
        Err(_) => return Err(domain("invalid_cursor", "offset is not a UTF-8 boundary")),
    }
    if bytes.is_empty() && offset < lease.bytes {
        return Err(invalid("length is too small for next UTF-8 character"));
    }
    let end = offset + bytes.len();
    lease.expires = (Instant::now() + TEXT_TTL).min(lease.deadline);
    Ok(api::TextPage {
        snapshot_id: id,
        text: String::from_utf8(bytes)?,
        next: (end < lease.bytes).then(|| end.to_string()),
        fingerprint: lease.fingerprint.clone(),
    })
}
use rusqlite::OptionalExtension;

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn disconnect_and_expiry_release_only_their_leases() {
        let first = format!("test-{}", sill_core::store::new_id());
        let other = format!("test-{}", sill_core::store::new_id());
        let owner = u64::MAX - 1;
        let foreign = u64::MAX;
        let lease = |conn, expired| TextLease {
            conn,
            session: "fixture".into(),
            identity: "fixture".into(),
            file: None,
            bytes: 0,
            fingerprint: String::new(),
            expires: if expired { Instant::now() - Duration::from_secs(1) } else { Instant::now() + TEXT_TTL },
            deadline: Instant::now() + Duration::from_secs(1800),
        };
        {
            let mut l = leases().lock().unwrap();
            l.insert(first.clone(), lease(owner, false));
            l.insert(other.clone(), lease(foreign, false));
        }
        disconnect(owner);
        {
            let mut l = leases().lock().unwrap();
            assert!(!l.contains_key(&first));
            assert!(l.contains_key(&other));
            l.get_mut(&other).unwrap().expires = Instant::now() - Duration::from_secs(1);
        }
        sweep();
        assert!(!leases().lock().unwrap().contains_key(&other));
    }
}
