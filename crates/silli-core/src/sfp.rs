//! Durable SFP state and event journal. All writes use savepoints so they participate in a
//! runner's enclosing transaction. Publish returned events only after that transaction commits.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use silli_proto::sfp::*;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct DomainError {
    pub code: &'static str,
    pub message: String,
    pub details: Option<serde_json::Value>,
}
impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for DomainError {}
impl DomainError {
    /// `limit_exceeded` with its required details (`silli_proto::rpc::LimitDetails`).
    pub fn limit(scope: silli_proto::rpc::LimitScope, maximum: Option<u64>, message: impl Into<String>) -> anyhow::Error {
        let details = serde_json::to_value(silli_proto::rpc::LimitDetails { scope, maximum }).unwrap();
        DomainError { code: "limit_exceeded", message: message.into(), details: Some(details) }.into()
    }
}
fn error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    DomainError { code, message: message.into(), details: None }.into()
}

#[derive(Debug, Clone)]
pub struct Source {
    pub generation_id: String,
    pub seq: u64,
}

pub const RETAINED_EVENTS: i64 = 4096;
pub const RETAINED_EVENT_BYTES: i64 = 16 * 1024 * 1024;

pub struct Journal<'a> {
    store: &'a Store,
}
impl<'a> Journal<'a> {
    pub fn new(store: &'a Store) -> Result<Self> {
        store.conn().execute_batch(
            "CREATE TABLE IF NOT EXISTS sfp_sessions (
            session_id TEXT PRIMARY KEY, seq INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS sfp_records (
            session_id TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL,
            created_seq INTEGER NOT NULL, json TEXT NOT NULL, removed INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(session_id,kind,id));
          CREATE TABLE IF NOT EXISTS sfp_blocks (
            session_id TEXT NOT NULL, item_id TEXT NOT NULL, block_id TEXT NOT NULL,
            retired INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(session_id,item_id,block_id));
          CREATE TABLE IF NOT EXISTS active_hours (hour INTEGER PRIMARY KEY, ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS sfp_events (
            session_id TEXT NOT NULL, seq INTEGER NOT NULL, json TEXT NOT NULL, bytes INTEGER NOT NULL,
            PRIMARY KEY(session_id,seq));
          CREATE TABLE IF NOT EXISTS sfp_source_ranges (
            session_id TEXT NOT NULL, generation_id TEXT NOT NULL, first TEXT NOT NULL, last TEXT NOT NULL,
            PRIMARY KEY(session_id,generation_id,first));
          CREATE TABLE IF NOT EXISTS sfp_answers (
            session_id TEXT NOT NULL, answer_id TEXT NOT NULL, question_id TEXT NOT NULL, input TEXT NOT NULL,
            PRIMARY KEY(session_id,answer_id));
          CREATE TABLE IF NOT EXISTS sfp_submission_inputs (
            session_id TEXT NOT NULL, submission_id TEXT NOT NULL, input TEXT NOT NULL,
            PRIMARY KEY(session_id,submission_id));
          CREATE TRIGGER IF NOT EXISTS sfp_delete_session AFTER DELETE ON sessions BEGIN
            DELETE FROM sfp_sessions WHERE session_id=OLD.id;
            DELETE FROM sfp_records WHERE session_id=OLD.id;
            DELETE FROM sfp_blocks WHERE session_id=OLD.id;
            DELETE FROM sfp_events WHERE session_id=OLD.id;
            DELETE FROM sfp_answers WHERE session_id=OLD.id;
            DELETE FROM sfp_submission_inputs WHERE session_id=OLD.id;
          END;
          CREATE TRIGGER IF NOT EXISTS sfp_delete_source_ranges AFTER DELETE ON sessions BEGIN
            DELETE FROM sfp_source_ranges WHERE session_id=OLD.id;
          END;",
        )?;
        store.conn().execute_batch("CREATE INDEX IF NOT EXISTS sfp_event_sizes ON sfp_events(session_id,seq DESC,bytes)")?;
        Ok(Self { store })
    }

    fn atomic<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.store.conn().execute_batch("SAVEPOINT sfp_atomic")?;
        match f() {
            Ok(value) => {
                self.store.conn().execute_batch("RELEASE sfp_atomic")?;
                Ok(value)
            }
            Err(e) => {
                self.store.conn().execute_batch("ROLLBACK TO sfp_atomic; RELEASE sfp_atomic")?;
                Err(e)
            }
        }
    }

    fn exists(&self, id: &str) -> Result<()> {
        let exists: bool = self.store.conn().query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)", [id], |r| r.get(0))?;
        if !exists {
            return Err(error("not_found", "session does not exist"));
        }
        Ok(())
    }

    /// Initialize public state once. Adopting a relay must use its existing generation.
    /// A replacement generation must be made visible through a SessionUpdated event.
    pub fn ensure(&self, session_id: &str, generation_id: &str) -> Result<SessionState> {
        self.atomic(|| {
            self.exists(session_id)?;
            let row = self.store.sessions()?.into_iter().find(|s| s.id == session_id).ok_or_else(|| error("not_found", "session does not exist"))?;
            let state = SessionState {
                session_id: session_id.into(),
                agent_generation_id: generation_id.into(),
                lifecycle: if row.ended_at.is_some() { "ended" } else { "open" }.into(),
                title: Some(row.title),
                summary: (!row.summary.is_empty()).then_some(row.summary),
                end_reason: None,
                deleted: false,
                activity: "unknown".into(),
                turn_id: None,
                question_ids: vec![],
                background: None,
                capabilities: BTreeMap::from([("steering".into(), false)]),
                controls: None,
                authentication: None,
                usage: None,
                agent_title: None,
                commands: vec![],
                active_ms: 0,
                active_since_ms: None,
                summarizing: None,
            };
            self.store
                .conn()
                .execute("INSERT OR IGNORE INTO sfp_sessions(session_id,state) VALUES (?1,?2)", params![session_id, serde_json::to_string(&state)?])?;
            // The memory transcript (`entries`) stays memory's: it is never imported into
            // canonical history or used to infer source identity and replay continuity.
            self.session(session_id)
        })
    }

    /// A new worker without continuity invalidates old delivery evidence. Queued, certainly
    /// unsent input is retained in acceptance order but held until explicitly resumed.
    pub fn replace_generation(&self, session_id: &str, generation_id: &str) -> Result<Vec<StoredEvent>> {
        self.atomic(|| {
            let mut session = self.session(session_id)?;
            if session.agent_generation_id == generation_id {
                return Ok(vec![]);
            }
            session.agent_generation_id = generation_id.into();
            session.activity = "unknown".into();
            session.turn_id = None;
            session.question_ids.clear();
            session.background = None;
            // The replacement agent announces its own commands; the old ones may not exist there.
            session.commands.clear();
            // Retain the old controls only as stale presentation until the replacement agent
            // confirms its own schema and restored selections.
            if let Some(controls) = &mut session.controls {
                controls.status = "loading".into();
                controls.error = None;
            }
            let mut events = vec![Event::SessionUpdated { session }];
            let records = self.active_records(session_id)?;
            let mut interrupted = false;
            let mut held = false;
            for record in records {
                match record {
                    Record::Submission { mut submission } if submission.outcome == "pending" => {
                        if submission.delivery == "queued" && submission.insertion == "pending" {
                            held = true;
                            submission.held = true;
                            submission.hold_reason =
                                Some(Reason { code: "agent_replaced".into(), message: "Resume explicitly to deliver to the replacement agent".into() });
                        } else {
                            interrupted = true;
                            if submission.insertion == "pending" {
                                submission.insertion = "unknown".into();
                            }
                            submission.outcome = "unknown".into();
                            submission.reason = Some(Reason {
                                code: "agent_replaced".into(),
                                message: "Delivery or outcome could not be established before agent replacement".into(),
                            });
                        }
                        events.push(Event::SubmissionUpdated { submission });
                    }
                    Record::Question { mut question } => {
                        if question.state == "open" {
                            question.state = "cancelled".into();
                            question.reason = Some(Reason { code: "agent_replaced".into(), message: "Question belongs to the previous agent".into() });
                            events.push(Event::QuestionUpdated { question });
                        } else if question.state == "answered" && question.delivery.as_deref() == Some("pending") {
                            question.delivery = Some("unknown".into());
                            events.push(Event::QuestionUpdated { question });
                        }
                    }
                    Record::Turn { mut turn } if turn.outcome == "pending" => {
                        interrupted = true;
                        turn.activity = "unknown".into();
                        turn.outcome = "unknown".into();
                        turn.reason = Some(Reason { code: "agent_replaced".into(), message: "Agent replacement interrupted activity".into() });
                        events.push(Event::TurnUpdated { turn });
                    }
                    _ => {}
                }
            }
            let mut notice = if interrupted {
                "Agent restarted. Previous work may be incomplete and wasn’t resent.".to_string()
            } else {
                "Started a new agent conversation.".to_string()
            };
            if held {
                notice.push_str(" Queued messages are paused. Resume to send them.");
            }
            events.push(Event::ItemCreated {
                item: Item {
                    item_id: new_id(),
                    created_seq: String::new(),
                    turn_id: None,
                    submission_id: None,
                    agent_generation_id: generation_id.into(),
                    kind: "note".into(),
                    content: vec![Content::text(notice)],
                    tool: None,
                },
            });
            self.append(session_id, None, &events)
        })
    }

    pub fn session(&self, session_id: &str) -> Result<SessionState> {
        self.exists(session_id)?;
        let json: Option<String> = self.store.conn().query_row("SELECT state FROM sfp_sessions WHERE session_id=?1", [session_id], |r| r.get(0)).optional()?;
        serde_json::from_str(&json.ok_or_else(|| error("not_found", "session has no SFP state"))?).map_err(Into::into)
    }

    /// Add a completed working interval to the hours it covers. The hours outlive the session:
    /// they are what the agents worked, not what is still stored of it.
    fn record_active(&self, start: u64, end: u64) -> Result<()> {
        for (hour, ms) in hour_shares(start, end) {
            self.store.conn().execute(
                "INSERT INTO active_hours(hour,ms) VALUES(?1,?2) ON CONFLICT(hour) DO UPDATE SET ms=ms+excluded.ms",
                params![hour as i64, ms as i64],
            )?;
        }
        Ok(())
    }

    /// The time agents worked in each UTC hour from `since_ms` to `now_ms`, over all sessions and
    /// summed where several worked at once: (Unix hour, milliseconds), ascending, hours without
    /// work left out. Intervals still running count up to `now_ms`.
    pub fn active_hours(&self, since_ms: u64, now_ms: u64) -> Result<Vec<(u64, u64)>> {
        let mut hours = std::collections::BTreeMap::new();
        let conn = self.store.conn();
        let mut stored = conn.prepare("SELECT hour,ms FROM active_hours WHERE hour>=?1")?;
        for row in stored.query_map([(since_ms / HOUR_MS) as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (hour, ms) = row?;
            *hours.entry(hour as u64).or_insert(0u64) += ms as u64;
        }
        let mut running =
            conn.prepare("SELECT json_extract(state,'$.active_since_ms') FROM sfp_sessions WHERE json_extract(state,'$.active_since_ms') IS NOT NULL")?;
        for start in running.query_map([], |r| r.get::<_, i64>(0))? {
            for (hour, ms) in hour_shares((start? as u64).max(since_ms / HOUR_MS * HOUR_MS), now_ms) {
                *hours.entry(hour).or_insert(0) += ms;
            }
        }
        Ok(hours.into_iter().collect())
    }

    pub fn cursor(&self, session_id: &str) -> Result<Cursor> {
        self.exists(session_id)?;
        let seq: Option<i64> = self.store.conn().query_row("SELECT seq FROM sfp_sessions WHERE session_id=?1", [session_id], |r| r.get(0)).optional()?;
        Ok(Cursor { session_id: session_id.into(), seq: seq.ok_or_else(|| error("not_found", "session has no SFP state"))?.to_string() })
    }

    pub fn source_seen(&self, session_id: &str, source: &Source) -> Result<bool> {
        // Never infer that a missing lower sequence was seen merely from a high-water mark.
        let key = format!("{:020}", source.seq);
        Ok(self.store.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM sfp_source_ranges WHERE session_id=?1 AND generation_id=?2 AND first<=?3 AND last>=?3)",
            params![session_id, source.generation_id, key],
            |r| r.get(0),
        )?)
    }

    fn remember_source(&self, session_id: &str, source: &Source) -> Result<()> {
        let low = format!("{:020}", source.seq.saturating_sub(1));
        let high = format!("{:020}", source.seq.saturating_add(1));
        let mut stmt =
            self.store.conn().prepare("SELECT first,last FROM sfp_source_ranges WHERE session_id=?1 AND generation_id=?2 AND first<=?3 AND last>=?4")?;
        let ranges = stmt
            .query_map(params![session_id, source.generation_id, high, low], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut first = source.seq;
        let mut last = source.seq;
        for (a, b) in ranges {
            first = first.min(a.parse()?);
            last = last.max(b.parse()?);
        }
        self.store.conn().execute(
            "DELETE FROM sfp_source_ranges WHERE session_id=?1 AND generation_id=?2 AND first<=?3 AND last>=?4",
            params![session_id, source.generation_id, high, low],
        )?;
        self.store.conn().execute(
            "INSERT INTO sfp_source_ranges VALUES (?1,?2,?3,?4)",
            params![session_id, source.generation_id, format!("{first:020}"), format!("{last:020}")],
        )?;
        Ok(())
    }

    /// Deduplication and state changes share one transaction, including batches which emit no
    /// public events. Source sequence zero is a relay state replay, not a durable source identity.
    pub fn append(&self, session_id: &str, source: Option<Source>, events: &[Event]) -> Result<Vec<StoredEvent>> {
        self.atomic(|| {
            self.exists(session_id)?;
            if let Some(s) = &source {
                if s.seq == 0 {
                    return Err(error("invalid_state", "source sequence zero has no durable identity"));
                }
                if self.source_seen(session_id, s)? {
                    return Ok(vec![]);
                }
                let state = self.session(session_id)?;
                if state.agent_generation_id != s.generation_id {
                    return Err(error("invalid_state", "source belongs to a different agent generation"));
                }
            }
            let mut seq = sequence(&self.cursor(session_id)?.seq)?;
            let mut output = Vec::with_capacity(events.len());
            for original in events {
                seq = seq.checked_add(1).ok_or_else(|| DomainError::limit(silli_proto::rpc::LimitScope::Capacity, None, "session event sequence exhausted"))?;
                let mut event = original.clone();
                match &mut event {
                    Event::SessionUpdated { session } => {
                        let previous = self.session(session_id)?;
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                        if let Some((start, end)) = update_active_time(&previous, session, now) {
                            self.record_active(start, end)?;
                        }
                    }
                    Event::ItemCreated { item } if item.created_seq.is_empty() => item.created_seq = seq.to_string(),
                    Event::SubmissionUpdated { submission } if submission.accepted_seq.is_empty() => submission.accepted_seq = seq.to_string(),
                    _ => {}
                }
                self.apply(session_id, seq, &event)?;
                let json = serde_json::to_string(&event)?;
                self.store
                    .conn()
                    .execute("INSERT INTO sfp_events(session_id,seq,json,bytes) VALUES (?1,?2,?3,?4)", params![session_id, seq, json, json.len() as i64])?;
                output.push(StoredEvent { seq: seq.to_string(), event });
            }
            self.store.conn().execute("UPDATE sfp_sessions SET seq=?2 WHERE session_id=?1", params![session_id, seq])?;
            if let Some(s) = source {
                self.remember_source(session_id, &s)?;
            }
            // Keep a contiguous replay suffix bounded by both count and serialized bytes.
            // Canonical records and retry identities are deliberately independent of replay.
            self.store.conn().execute(
                "DELETE FROM sfp_events WHERE session_id=?1 AND seq IN (
                SELECT seq FROM (SELECT seq, ROW_NUMBER() OVER (ORDER BY seq DESC) AS n,
                    SUM(bytes) OVER (ORDER BY seq DESC) AS total_bytes
                    FROM sfp_events WHERE session_id=?1)
                WHERE n>?2 OR total_bytes>?3)",
                params![session_id, RETAINED_EVENTS, RETAINED_EVENT_BYTES],
            )?;
            Ok(output)
        })
    }

    fn put(&self, session_id: &str, kind: &str, id: &str, seq: i64, record: &Record) -> Result<()> {
        let json = serde_json::to_string(record)?;
        if json.len() > 64 * 1024 * 1024 {
            return Err(DomainError::limit(silli_proto::rpc::LimitScope::Record, Some(67108864), "record exceeds 64 MiB"));
        }
        self.store.conn().execute(
            "INSERT INTO sfp_records(session_id,kind,id,created_seq,json) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(session_id,kind,id) DO UPDATE SET json=excluded.json",
            params![session_id, kind, id, seq, json],
        )?;
        Ok(())
    }
    fn record(&self, session_id: &str, kind: &str, id: &str) -> Result<Record> {
        self.exists(session_id)?;
        let json: Option<String> = self
            .store
            .conn()
            .query_row("SELECT json FROM sfp_records WHERE session_id=?1 AND kind=?2 AND id=?3 AND removed=0", params![session_id, kind, id], |r| r.get(0))
            .optional()?;
        Ok(serde_json::from_str(&json.ok_or_else(|| error("not_found", format!("{kind} does not exist")))?)?)
    }
    pub fn item(&self, session_id: &str, id: &str) -> Result<Item> {
        match self.record(session_id, "item", id)? {
            Record::Item { item } => Ok(item),
            _ => unreachable!(),
        }
    }
    pub fn submission(&self, session_id: &str, id: &str) -> Result<Submission> {
        match self.record(session_id, "submission", id)? {
            Record::Submission { submission } => Ok(submission),
            _ => unreachable!(),
        }
    }
    pub fn question(&self, session_id: &str, id: &str) -> Result<Question> {
        match self.record(session_id, "question", id)? {
            Record::Question { question } => Ok(question),
            _ => unreachable!(),
        }
    }
    fn blocks(&self, session_id: &str, item_id: &str, old: &[Content], new: &[Content]) -> Result<()> {
        for block in new {
            let retired: Option<bool> = self
                .store
                .conn()
                .query_row(
                    "SELECT retired FROM sfp_blocks WHERE session_id=?1 AND item_id=?2 AND block_id=?3",
                    params![session_id, item_id, block.block_id],
                    |r| r.get(0),
                )
                .optional()?;
            if retired == Some(true) {
                return Err(error("invalid_state", "removed block identity cannot be reused"));
            }
            if old.iter().any(|b| b.block_id == block.block_id && b.kind != block.kind) {
                return Err(error("invalid_state", "block kind is immutable"));
            }
            self.store
                .conn()
                .execute("INSERT OR IGNORE INTO sfp_blocks(session_id,item_id,block_id) VALUES (?1,?2,?3)", params![session_id, item_id, block.block_id])?;
        }
        for block in old.iter().filter(|b| !new.iter().any(|n| n.block_id == b.block_id)) {
            self.store
                .conn()
                .execute("UPDATE sfp_blocks SET retired=1 WHERE session_id=?1 AND item_id=?2 AND block_id=?3", params![session_id, item_id, block.block_id])?;
        }
        Ok(())
    }
    fn apply(&self, session_id: &str, seq: i64, event: &Event) -> Result<()> {
        match event {
            Event::SessionUpdated { session } => {
                if session.session_id != session_id {
                    return Err(error("invalid_state", "foreign session state"));
                }
                self.store.conn().execute("UPDATE sfp_sessions SET state=?2 WHERE session_id=?1", params![session_id, serde_json::to_string(session)?])?;
            }
            Event::ItemCreated { item } => {
                if sequence(&item.created_seq)? != seq {
                    return Err(error("invalid_state", "item creation cursor differs from its event"));
                }
                validate_blocks(&item.content)?;
                let exists: bool = self.store.conn().query_row(
                    "SELECT EXISTS(SELECT 1 FROM sfp_records WHERE session_id=?1 AND kind='item' AND id=?2)",
                    params![session_id, item.item_id],
                    |r| r.get(0),
                )?;
                if exists {
                    return Err(error("invalid_state", "item identity cannot be reused"));
                }
                self.blocks(session_id, &item.item_id, &[], &item.content)?;
                self.put(session_id, "item", &item.item_id, seq, &Record::Item { item: item.clone() })?;
            }
            Event::ItemPatched { item_id, set } => {
                let mut item = self.item(session_id, item_id)?;
                if let Some(content) = &set.content {
                    validate_blocks(content)?;
                    self.blocks(session_id, item_id, &item.content, content)?;
                    item.content = content.clone();
                }
                if let Some(tool) = &set.tool {
                    if item.kind != "tool" {
                        return Err(error("invalid_state", "tool patch on another item kind"));
                    }
                    item.tool = Some(tool.clone());
                }
                self.put(session_id, "item", item_id, seq, &Record::Item { item })?;
            }
            Event::ItemTextAppended { item_id, block_id, text } => {
                let mut item = self.item(session_id, item_id)?;
                let block = item.content.iter_mut().find(|b| &b.block_id == block_id).ok_or_else(|| error("invalid_state", "append target does not exist"))?;
                if !matches!(block.kind.as_str(), "text" | "thought") {
                    return Err(error("invalid_state", "append target is not text"));
                }
                block.text.as_mut().ok_or_else(|| error("invalid_state", "text block has no text"))?.push_str(text);
                self.put(session_id, "item", item_id, seq, &Record::Item { item })?;
            }
            Event::ItemRemoved { item_id, .. } => {
                self.item(session_id, item_id)?;
                self.store.conn().execute("UPDATE sfp_records SET removed=1 WHERE session_id=?1 AND kind='item' AND id=?2", params![session_id, item_id])?;
            }
            Event::TurnUpdated { turn } => self.put(session_id, "turn", &turn.turn_id, seq, &Record::Turn { turn: turn.clone() })?,
            Event::SubmissionUpdated { submission } => {
                if submission.session_id != session_id {
                    return Err(error("invalid_state", "foreign submission"));
                }
                self.put(session_id, "submission", &submission.submission_id, seq, &Record::Submission { submission: submission.clone() })?;
            }
            Event::QuestionUpdated { question } => {
                self.put(session_id, "question", &question.question_id, seq, &Record::Question { question: question.clone() })?
            }
        }
        Ok(())
    }

    /// Only workflow state can affect dispatch/recovery. Historical content must never be
    /// loaded simply to select the next submission or retire an interrupted worker.
    pub fn active_records(&self, session_id: &str) -> Result<Vec<Record>> {
        self.exists(session_id)?;
        let mut stmt = self.store.conn().prepare(
            "SELECT json FROM sfp_records WHERE session_id=?1 AND removed=0 AND kind IN ('submission','turn','question') AND (
            (kind='submission' AND json_extract(json,'$.submission.outcome')='pending') OR
            (kind='turn' AND json_extract(json,'$.turn.outcome')='pending') OR
            (kind='question' AND (json_extract(json,'$.question.state')='open' OR json_extract(json,'$.question.delivery')='pending')))
            ORDER BY created_seq,kind,id",
        )?;
        let mut records = Vec::new();
        for row in stmt.query_map([session_id], |r| r.get::<_, String>(0))? {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(records)
    }

    /// Immutable materialization for a connection-bound snapshot lease. The caller retains these
    /// records until its lease expires; later writes cannot alter any page.
    pub fn snapshot(&self, session_id: &str) -> Result<(Cursor, Vec<Record>)> {
        self.atomic(|| {
            let at = self.cursor(session_id)?;
            let mut records = vec![Record::Session { session: self.session(session_id)? }];
            let mut stmt = self.store.conn().prepare("SELECT json FROM sfp_records WHERE session_id=?1 AND removed=0 ORDER BY created_seq,kind,id")?;
            for json in stmt.query_map([session_id], |r| r.get::<_, String>(0))? {
                records.push(serde_json::from_str(&json?)?);
            }
            Ok((at, records))
        })
    }

    /// Stream an immutable capture to a caller-owned spool while the store lock is held.
    /// Only one serialized record is resident at a time.
    pub fn visit_snapshot(&self, session_id: &str, mut visit: impl FnMut(String) -> Result<()>) -> Result<Cursor> {
        let at = self.cursor(session_id)?;
        visit(serde_json::to_string(&Record::Session { session: self.session(session_id)? })?)?;
        let mut stmt = self.store.conn().prepare("SELECT json FROM sfp_records WHERE session_id=?1 AND removed=0 ORDER BY created_seq,kind,id")?;
        for row in stmt.query_map([session_id], |r| r.get::<_, String>(0))? {
            visit(row?)?;
        }
        Ok(at)
    }

    pub fn replay(&self, session_id: &str, since: &Cursor, limit: usize) -> Result<Vec<StoredEvent>> {
        if since.session_id != session_id {
            return Err(error("invalid_cursor", "cursor belongs to another session"));
        }
        let seq = sequence(&since.seq)?;
        let last = sequence(&self.cursor(session_id)?.seq)?;
        if seq > last {
            return Err(error("invalid_cursor", "cursor is ahead of the daemon"));
        }
        let first: Option<i64> = self.store.conn().query_row("SELECT MIN(seq) FROM sfp_events WHERE session_id=?1", [session_id], |r| r.get(0))?;
        if seq < first.map(|n| n - 1).unwrap_or(last) {
            return Err(error("cursor_expired", "replay was compacted; obtain a replacement snapshot"));
        }
        let mut stmt = self.store.conn().prepare("SELECT seq,json FROM sfp_events WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let mut events = vec![];
        for row in stmt.query_map(params![session_id, seq, limit.min(65536) as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (seq, json) = row?;
            events.push(StoredEvent { seq: seq.to_string(), event: serde_json::from_str(&json)? });
        }
        Ok(events)
    }

    /// Return an existing decision for an identical retry, even if the agent is now busy or gone.
    pub fn accept(&self, input: &Submit, delivery: &str) -> Result<(Submission, Vec<StoredEvent>)> {
        self.atomic(|| {
            self.exists(&input.session_id)?;
            validate_id(&input.submission_id)?;
            if input.purpose.as_deref().is_some_and(|p| p != "commit" && p != silli_proto::crosscheck::FIX_PURPOSE) {
                return Err(error("invalid_params", "unknown submission purpose"));
            }
            if serde_json::to_string(&input.text)?.len() > 1024 * 1024 {
                return Err(DomainError::limit(silli_proto::rpc::LimitScope::Request, Some(1048576), "prompt text exceeds 1 MiB encoded limit"));
            }
            let fingerprint = serde_json::to_string(input)?;
            let previous: Option<String> = self
                .store
                .conn()
                .query_row(
                    "SELECT input FROM sfp_submission_inputs WHERE session_id=?1 AND submission_id=?2",
                    params![input.session_id, input.submission_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                if previous != fingerprint {
                    return Err(error("id_conflict", "submission ID was used for different input"));
                }
                return Ok((self.submission(&input.session_id, &input.submission_id)?, vec![]));
            }
            let ended: bool = self.store.conn().query_row("SELECT ended_at IS NOT NULL FROM sessions WHERE id=?1", [&input.session_id], |r| r.get(0))?;
            if ended {
                return Err(error("invalid_state", "session has ended"));
            }
            if !matches!(delivery, "direct" | "queued" | "steering") {
                return Err(error("invalid_state", "invalid delivery mode"));
            }
            let state = self.session(&input.session_id)?;
            let submission = Submission {
                submission_id: input.submission_id.clone(),
                session_id: input.session_id.clone(),
                accepted_seq: String::new(),
                agent_generation_id: state.agent_generation_id,
                text: input.text.clone(),
                attachments: input.attachments.clone(),
                acceptance: "accepted".into(),
                delivery: delivery.into(),
                held: false,
                hold_reason: None,
                insertion: "pending".into(),
                outcome: "pending".into(),
                reason: None,
                purpose: input.purpose.clone(),
            };
            // the prompt's conversation item comes when it is sent (`dispatched`): a queued prompt
            // belongs where the agent reads it, not where it was typed
            let events = self.append(&input.session_id, None, &[Event::SubmissionUpdated { submission }])?;
            self.store.conn().execute("INSERT INTO sfp_submission_inputs VALUES (?1,?2,?3)", params![input.session_id, input.submission_id, fingerprint])?;
            self.store.append(&input.session_id, "user", &input.text, "")?;
            self.store.touch(&input.session_id)?;
            Ok((self.submission(&input.session_id, &input.submission_id)?, events))
        })
    }

    /// `submission` goes to the agent now: its delivery as given, and its prompt as a user item
    /// at this point of the conversation.
    pub fn dispatched(&self, submission: &Submission) -> Result<Vec<StoredEvent>> {
        let session_id = &submission.session_id;
        let mut events = vec![Event::SubmissionUpdated { submission: submission.clone() }];
        events.push(Event::ItemCreated {
            item: Item {
                item_id: new_id(),
                created_seq: String::new(),
                turn_id: None,
                submission_id: Some(submission.submission_id.clone()),
                agent_generation_id: submission.agent_generation_id.clone(),
                kind: "user".into(),
                content: vec![Content::text(&submission.text)],
                tool: None,
            },
        });
        self.append(session_id, None, &events)
    }

    pub fn cancel(&self, key: &SubmissionKey) -> Result<(Submission, Vec<StoredEvent>)> {
        self.atomic(|| {
            let mut s = self.submission(&key.session_id, &key.submission_id)?;
            if s.outcome == "cancelled" {
                return Ok((s, vec![]));
            }
            if s.delivery != "queued" || s.insertion != "pending" || s.outcome != "pending" {
                return Err(error("invalid_state", "only an unsent queued submission can be cancelled"));
            }
            s.outcome = "cancelled".into();
            s.reason = Some(Reason { code: "cancelled_before_dispatch".into(), message: "Cancelled while queued".into() });
            let events = self.append(&key.session_id, None, &[Event::SubmissionUpdated { submission: s.clone() }])?;
            Ok((s, events))
        })
    }
    pub fn resume(&self, key: &SubmissionKey) -> Result<(Submission, Vec<StoredEvent>)> {
        self.atomic(|| {
            let mut s = self.submission(&key.session_id, &key.submission_id)?;
            if s.delivery != "queued" || s.insertion != "pending" || s.outcome != "pending" {
                return Err(error("invalid_state", "submission is not an unsent queued intent"));
            }
            if !s.held {
                return Ok((s, vec![]));
            }
            s.held = false;
            s.hold_reason = None;
            s.agent_generation_id = self.session(&key.session_id)?.agent_generation_id;
            let events = self.append(&key.session_id, None, &[Event::SubmissionUpdated { submission: s.clone() }])?;
            Ok((s, events))
        })
    }

    /// Atomically record the winning answer and its delivery intent. Relay delivery follows only
    /// after the enclosing transaction commits. Retries never reopen an answered question.
    pub fn answer(&self, input: &AnswerQuestion) -> Result<(Question, Vec<StoredEvent>)> {
        self.atomic(|| {
            self.exists(&input.session_id)?;
            validate_id(&input.answer_id)?;
            let fingerprint = serde_json::to_string(input)?;
            let prior: Option<(String, String)> = self
                .store
                .conn()
                .query_row("SELECT question_id,input FROM sfp_answers WHERE session_id=?1 AND answer_id=?2", params![input.session_id, input.answer_id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            if let Some((question_id, previous)) = prior {
                if previous != fingerprint {
                    return Err(error("id_conflict", "answer ID was used for different input"));
                }
                return Ok((self.question(&input.session_id, &question_id)?, vec![]));
            }
            let mut q = self.question(&input.session_id, &input.question_id)?;
            if q.state != "open" {
                return Err(DomainError {
                    code: "stale_answer",
                    message: "question is already resolved".into(),
                    details: Some(serde_json::json!({"question":q})),
                }
                .into());
            }
            if q.agent_generation_id != self.session(&input.session_id)?.agent_generation_id {
                return Err(error("stale_answer", "question belongs to an obsolete agent generation"));
            }
            validate_answer(&q, &input.answer)?;
            q.state = "answered".into();
            q.answer_id = Some(input.answer_id.clone());
            q.answer = Some(input.answer.clone());
            q.delivery = Some("pending".into());
            let events = self.append(&input.session_id, None, &[Event::QuestionUpdated { question: q.clone() }])?;
            self.store
                .conn()
                .execute("INSERT INTO sfp_answers VALUES (?1,?2,?3,?4)", params![input.session_id, input.answer_id, input.question_id, fingerprint])?;
            Ok((q, events))
        })
    }
}

pub fn sequence(value: &str) -> Result<i64> {
    if value.is_empty() || value.len() > 1 && value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error("invalid_cursor", "sequence must be a canonical unsigned decimal string"));
    }
    value.parse::<i64>().map_err(|_| error("invalid_cursor", "sequence exceeds the supported range"))
}
fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 {
        return Err(error("invalid_params", "ID must contain 1 to 256 bytes"));
    }
    Ok(())
}
fn validate_blocks(content: &[Content]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for block in content {
        validate_id(&block.block_id)?;
        if !ids.insert(&block.block_id) {
            return Err(error("invalid_state", "duplicate block ID"));
        }
    }
    Ok(())
}
fn validate_answer(q: &Question, answer: &Answer) -> Result<()> {
    match answer {
        Answer::Cancel => Ok(()),
        Answer::Permission { option_id } if q.kind == "permission" => {
            let option = q.options.iter().find(|o| &o.option_id == option_id).ok_or_else(|| error("invalid_params", "permission option does not exist"))?;
            if !matches!(option.kind.as_str(), "allow_once" | "allow_always" | "reject_once" | "reject_always") {
                return Err(error("unsupported", "unknown permission meaning cannot authorize an action"));
            }
            Ok(())
        }
        Answer::Form { values } if q.kind == "form" => {
            if values.keys().any(|key| !q.fields.iter().any(|f| &f.key == key)) {
                return Err(error("invalid_params", "unknown form field"));
            }
            // A field left empty is absent. Which fields are required is the agent's schema to
            // judge (silli keeps none of it): demanding all of them refused every form with an
            // optional note, and the session waited for good.
            for field in &q.fields {
                let Some(value) = values.get(&field.key) else { continue };
                let valid = match field.kind.as_str() {
                    "text" => value.is_string(),
                    "boolean" => value.is_boolean(),
                    "number" => value.is_number(),
                    "choice" => value.as_str().is_some_and(|v| field.choices.iter().any(|c| c.value == v)),
                    "choices" => value.as_array().is_some_and(|vs| vs.iter().all(|v| v.as_str().is_some_and(|v| field.choices.iter().any(|c| c.value == v)))),
                    _ => return Err(error("unsupported", "unknown form field kind")),
                };
                if !valid {
                    return Err(error("invalid_params", format!("invalid form field {}", field.key)));
                }
            }
            Ok(())
        }
        _ => Err(error("invalid_params", "answer does not match question kind")),
    }
}

/// Random opaque public identities, independent of ACP identifiers and transcript row numbers.
pub fn new_id() -> String {
    use rand::RngExt;
    format!("{:032x}", rand::rng().random::<u128>())
}

/// UTC RFC 3339 for timestamps in the supported civil range.
pub fn timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86400);
    let day_seconds = seconds.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", day_seconds / 3600, day_seconds / 60 % 60, day_seconds % 60)
}

/// Carry the durable total forward even when a producer holds an older state copy.
/// Returns the working interval this update completed, in Unix milliseconds.
fn update_active_time(previous: &SessionState, next: &mut SessionState, now: u64) -> Option<(u64, u64)> {
    next.active_ms = previous.active_ms;
    next.active_since_ms = previous.active_since_ms;
    let working = next.activity == "working" && next.lifecycle == "open";
    let mut completed = None;
    if (!working || previous.agent_generation_id != next.agent_generation_id)
        && let Some(start) = next.active_since_ms.take()
    {
        next.active_ms = next.active_ms.saturating_add(now.saturating_sub(start));
        completed = Some((start, now));
    }
    if working && next.active_since_ms.is_none() {
        next.active_since_ms = Some(now);
    }
    completed
}

const HOUR_MS: u64 = 3_600_000;

/// A working interval as its share of each UTC hour it covers: (Unix hour, milliseconds).
fn hour_shares(start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut shares = Vec::new();
    let mut at = start;
    while at < end {
        let next = ((at / HOUR_MS + 1) * HOUR_MS).min(end);
        shares.push((at / HOUR_MS, next - at));
        at = next;
    }
    shares
}

/// The retry identity of a request record that may gain optional fields within sfp/1.
///
/// Object members at their default (null, false, 0, "", [] or {}) are dropped, recursively,
/// and keys are sorted, so a field added later with a default that means "absent" leaves every
/// earlier fingerprint unchanged. Array elements keep their positions. The `sfp-request/1`
/// prefix versions this form; changing it would orphan every durable receipt.
pub fn canonical_fingerprint(value: &serde_json::Value) -> String {
    fn prune(value: &serde_json::Value) -> Option<serde_json::Value> {
        use serde_json::Value;
        match value {
            Value::Null | Value::Bool(false) => None,
            Value::Number(n) if n.as_f64() == Some(0.0) => None,
            Value::String(s) if s.is_empty() => None,
            Value::Array(a) if a.is_empty() => None,
            Value::Array(a) => Some(Value::Array(a.iter().map(|v| prune(v).unwrap_or(Value::Null)).collect())),
            Value::Object(o) => {
                let kept: std::collections::BTreeMap<_, _> = o.iter().filter_map(|(k, v)| Some((k.clone(), prune(v)?))).collect();
                (!kept.is_empty()).then(|| Value::Object(kept.into_iter().collect()))
            }
            other => Some(other.clone()),
        }
    }
    let canonical = prune(value).unwrap_or(serde_json::Value::Null);
    create_fingerprint(&format!("sfp-request/1\n{canonical}"))
}

/// A create retry retains only the input fingerprint, never pins or ghost configuration.
pub fn create_fingerprint(input: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(input.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    /// Codex's form (2026-11-03): a choice and an optional note. Left empty, the note is absent;
    /// the answer stands. A value that is there is still checked.
    #[test]
    fn a_form_answer_may_leave_a_field_out_but_not_give_a_wrong_value() {
        let field = |key: &str, kind: &str, choices: Vec<Choice>| FormField {
            key: key.into(),
            title: String::new(),
            description: String::new(),
            kind: kind.into(),
            choices,
        };
        let choice = |v: &str| Choice { value: v.into(), title: v.into(), description: String::new() };
        let q = Question {
            question_id: "q".into(),
            agent_generation_id: "g".into(),
            kind: "form".into(),
            title: "Codex needs your input".into(),
            description: None,
            subject: None,
            tool_item_id: None,
            options: vec![],
            fields: vec![
                field("permission_prompt", "choice", vec![choice("Test prompt"), choice("Specific action")]),
                field("permission_prompt_note", "text", vec![]),
            ],
            state: "open".into(),
            answer_id: None,
            answer: None,
            delivery: None,
            reason: None,
        };
        let form = |v: serde_json::Value| Answer::Form { values: serde_json::from_value(v).unwrap() };
        validate_answer(&q, &form(serde_json::json!({"permission_prompt":"Test prompt"}))).unwrap();
        validate_answer(&q, &form(serde_json::json!({"permission_prompt":"Test prompt","permission_prompt_note":"thanks"}))).unwrap();
        assert!(validate_answer(&q, &form(serde_json::json!({"permission_prompt":"something else"}))).is_err());
        assert!(validate_answer(&q, &form(serde_json::json!({"permission_prompt_note":7}))).is_err());
        assert!(validate_answer(&q, &form(serde_json::json!({"unknown":"x"}))).is_err());
    }

    use super::*;
    #[test]
    fn active_time_excludes_waits_and_survives_storage() {
        let f = Fixture::new();
        let j = f.journal();
        let mut previous = j.session(&f.id).unwrap();
        for (activity, now, total, since) in [
            ("working", 1000, 0, Some(1000)),
            ("working", 2000, 0, Some(1000)),
            ("waiting", 3500, 2500, None),
            ("idle", 9000, 2500, None),
            ("working", 12000, 2500, Some(12000)),
            ("idle", 14000, 4500, None),
        ] {
            let mut next = previous.clone();
            next.activity = activity.into();
            update_active_time(&previous, &mut next, now);
            assert_eq!((next.active_ms, next.active_since_ms), (total, since));
            previous = next;
        }
        // Store a known completed total, then pass a stale producer state through append.
        j.apply(&f.id, 0, &Event::SessionUpdated { session: previous }).unwrap();
        let mut stale = j.session(&f.id).unwrap();
        stale.active_ms = 0;
        j.append(&f.id, None, &[Event::SessionUpdated { session: stale }]).unwrap();
        let reopened = Store::open(&f.dir.join("s.db")).unwrap();
        assert_eq!(Journal::new(&reopened).unwrap().session(&f.id).unwrap().active_ms, 4500);
    }

    #[test]
    fn working_intervals_are_kept_by_hour_and_outlive_their_session() {
        const H: u64 = 3_600_000;
        assert_eq!(hour_shares(H - 1000, 2 * H + 500), [(0, 1000), (1, H), (2, 500)]);
        assert!(hour_shares(5, 5).is_empty());
        let f = Fixture::new();
        let j = f.journal();
        j.record_active(10 * H + 1000, 11 * H + 2000).unwrap();
        j.record_active(10 * H + 5000, 10 * H + 6000).unwrap();
        // a session working now counts up to the moment asked about
        let mut working = j.session(&f.id).unwrap();
        working.activity = "working".into();
        working.active_since_ms = Some(12 * H + 100);
        j.apply(&f.id, 0, &Event::SessionUpdated { session: working }).unwrap();
        assert_eq!(j.active_hours(0, 12 * H + 700).unwrap(), [(10, H), (11, 2000), (12, 600)]);
        assert_eq!(j.active_hours(11 * H + 10, 12 * H + 700).unwrap(), [(11, 2000), (12, 600)]);
        f.store.delete(&f.id).unwrap();
        assert_eq!(j.active_hours(0, 13 * H).unwrap(), [(10, H), (11, 2000)]);
    }

    #[test]
    fn active_time_survives_a_generation_change_with_stale_producer_totals() {
        let f = Fixture::new();
        let mut previous = f.journal().session(&f.id).unwrap();
        previous.active_ms = 9000;
        previous.activity = "working".into();
        previous.active_since_ms = Some(1000);
        let mut replacement = previous.clone();
        replacement.agent_generation_id = "replacement".into();
        replacement.active_ms = 0;
        replacement.active_since_ms = None;
        update_active_time(&previous, &mut replacement, 3000);
        assert_eq!((replacement.active_ms, replacement.active_since_ms), (11000, Some(3000)));
        let mut waiting = replacement.clone();
        waiting.activity = "waiting".into();
        update_active_time(&replacement, &mut waiting, 4000);
        assert_eq!((waiting.active_ms, waiting.active_since_ms), (12000, None));
        let mut resumed = waiting.clone();
        resumed.activity = "working".into();
        resumed.active_ms = 0;
        update_active_time(&waiting, &mut resumed, 7000);
        assert_eq!((resumed.active_ms, resumed.active_since_ms), (12000, Some(7000)));
    }

    struct Fixture {
        dir: std::path::PathBuf,
        store: Store,
        id: String,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("silli-sfp-{}", new_id()));
            let store = Store::open(&dir.join("s.db")).unwrap();
            let id = store.create("test", "default").unwrap().id;
            Journal::new(&store).unwrap().ensure(&id, "g1").unwrap();
            Self { dir, store, id }
        }
        fn journal(&self) -> Journal<'_> {
            Journal::new(&self.store).unwrap()
        }
        fn submit(&self, id: &str) -> Submit {
            Submit { session_id: self.id.clone(), submission_id: id.into(), text: "hello".into(), attachments: vec![], purpose: None }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn item(id: &str) -> Item {
        Item {
            item_id: id.into(),
            created_seq: String::new(),
            turn_id: None,
            submission_id: None,
            agent_generation_id: "g1".into(),
            kind: "agent".into(),
            content: vec![Content::text("hello")],
            tool: None,
        }
    }
    fn question(id: &str) -> Question {
        Question {
            question_id: id.into(),
            agent_generation_id: "g1".into(),
            kind: "permission".into(),
            title: "Run?".into(),
            description: None,
            subject: None,
            tool_item_id: None,
            options: vec![PermissionOption { option_id: "yes".into(), title: "Yes".into(), kind: "allow_once".into() }],
            fields: vec![],
            state: "open".into(),
            answer_id: None,
            answer: None,
            delivery: None,
            reason: None,
        }
    }
    fn code(e: anyhow::Error) -> String {
        e.downcast_ref::<DomainError>().unwrap().code.into()
    }

    #[test]
    fn source_receipts_coalesce_without_filling_unseen_gaps() {
        let f = Fixture::new();
        let j = f.journal();
        for seq in [1, 3, 4] {
            j.append(&f.id, Some(Source { generation_id: "g1".into(), seq }), &[]).unwrap();
        }
        assert!(!j.source_seen(&f.id, &Source { generation_id: "g1".into(), seq: 2 }).unwrap());
        let count = || f.store.conn().query_row("SELECT count(*) FROM sfp_source_ranges", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count(), 2);
        j.append(&f.id, Some(Source { generation_id: "g1".into(), seq: 2 }), &[]).unwrap();
        assert_eq!(count(), 1);
        for seq in 1..=4 {
            assert!(j.source_seen(&f.id, &Source { generation_id: "g1".into(), seq }).unwrap());
        }
        let tx = f.store.conn().unchecked_transaction().unwrap();
        j.append(&f.id, Some(Source { generation_id: "g1".into(), seq: 5 }), &[]).unwrap();
        tx.rollback().unwrap();
        assert!(!j.source_seen(&f.id, &Source { generation_id: "g1".into(), seq: 5 }).unwrap());
        f.store.delete(&f.id).unwrap();
        assert_eq!(count(), 0);
    }

    #[test]
    fn compaction_expires_cursors_but_preserves_state_and_retry_identity() {
        let f = Fixture::new();
        let j = f.journal();
        let input = f.submit("retained-retry");
        let accepted = j.accept(&input, "queued").unwrap().0;
        let events: Vec<_> = (0..RETAINED_EVENTS + 5).map(|n| Event::ItemCreated { item: item(&format!("i{n}")) }).collect();
        j.append(&f.id, Some(Source { generation_id: "g1".into(), seq: 1 }), &events).unwrap();
        let cursor = Cursor { session_id: f.id.clone(), seq: "0".into() };
        assert_eq!(code(j.replay(&f.id, &cursor, 10).unwrap_err()), "cursor_expired");
        let count: i64 = f.store.conn().query_row("SELECT COUNT(*) FROM sfp_events", [], |r| r.get(0)).unwrap();
        assert_eq!(count, RETAINED_EVENTS);
        assert_eq!(j.accept(&input, "queued").unwrap().0, accepted);
        assert!(j.source_seen(&f.id, &Source { generation_id: "g1".into(), seq: 1 }).unwrap());
        assert_eq!(j.item(&f.id, "i0").unwrap().content[0].text.as_deref(), Some("hello"));
        let last = j.cursor(&f.id).unwrap();
        assert!(j.replay(&f.id, &last, 10).unwrap().is_empty());
    }

    #[test]
    fn source_dedup_and_snapshot_survive_reopen() {
        let f = Fixture::new();
        let j = f.journal();
        let source = Source { generation_id: "g1".into(), seq: 1 };
        let events = j.append(&f.id, Some(source.clone()), &[Event::ItemCreated { item: item("i") }]).unwrap();
        assert_eq!(events[0].seq, "1");
        let original = j.snapshot(&f.id).unwrap();
        let reopened = Store::open(&f.dir.join("s.db")).unwrap();
        let other = Journal::new(&reopened).unwrap();
        assert!(
            other
                .append(&f.id, Some(source), &[Event::ItemTextAppended { item_id: "i".into(), block_id: "text".into(), text: "duplicate".into() }])
                .unwrap()
                .is_empty()
        );
        other
            .append(
                &f.id,
                Some(Source { generation_id: "g1".into(), seq: 2 }),
                &[Event::ItemTextAppended { item_id: "i".into(), block_id: "text".into(), text: " world".into() }],
            )
            .unwrap();
        assert_eq!(other.item(&f.id, "i").unwrap().content[0].text.as_deref(), Some("hello world"));
        assert_eq!(original.0.seq, "1");
        assert!(matches!(&original.1[1],Record::Item {item} if item.content[0].text.as_deref()==Some("hello")));
        assert_eq!(other.replay(&f.id, &Cursor { session_id: f.id.clone(), seq: "0".into() }, 10).unwrap().len(), 2);
    }
    #[test]
    fn enclosing_rollback_rolls_back_state_events_and_source_identity() {
        let f = Fixture::new();
        let j = f.journal();
        let source = Source { generation_id: "g1".into(), seq: 1 };
        {
            let tx = f.store.conn().unchecked_transaction().unwrap();
            j.append(&f.id, Some(source.clone()), &[Event::ItemCreated { item: item("i") }]).unwrap();
            tx.rollback().unwrap();
        }
        assert_eq!(j.cursor(&f.id).unwrap().seq, "0");
        assert!(!j.source_seen(&f.id, &source).unwrap());
        assert_eq!(j.append(&f.id, Some(source), &[Event::ItemCreated { item: item("i") }]).unwrap()[0].seq, "1");
        let err = j
            .append(
                &f.id,
                None,
                &[
                    Event::ItemTextAppended { item_id: "i".into(), block_id: "text".into(), text: "should roll back".into() },
                    Event::ItemTextAppended { item_id: "missing".into(), block_id: "text".into(), text: "".into() },
                ],
            )
            .unwrap_err();
        assert_eq!(code(err), "not_found");
        assert_eq!(j.cursor(&f.id).unwrap().seq, "1");
        assert_eq!(j.item(&f.id, "i").unwrap().content[0].text.as_deref(), Some("hello"));
    }
    /// A queued prompt shows where the agent receives it: its item is made at dispatch, after what
    /// came meanwhile.
    #[test]
    fn a_prompt_item_is_made_where_it_is_dispatched() {
        let f = Fixture::new();
        let j = f.journal();
        let (queued, events) = j.accept(&f.submit("q"), "queued").unwrap();
        assert!(!events.iter().any(|e| matches!(e.event, Event::ItemCreated { .. })));
        j.append(&f.id, None, &[Event::ItemCreated { item: item("meanwhile") }]).unwrap();
        let mut sent = queued;
        sent.delivery = "direct".into();
        j.dispatched(&sent).unwrap();
        let (_, records) = j.snapshot(&f.id).unwrap();
        let items: Vec<&Item> = records
            .iter()
            .filter_map(|r| match r {
                Record::Item { item } => Some(item),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "what came meanwhile, then the prompt");
        assert_eq!(items[0].item_id, "meanwhile");
        assert_eq!((items[1].kind.as_str(), items[1].submission_id.as_deref()), ("user", Some("q")));
        assert_eq!(j.submission(&f.id, "q").unwrap().delivery, "direct");
    }
    #[test]
    fn acceptance_is_durable_deduplicated_and_conflicting_input_is_rejected() {
        let f = Fixture::new();
        let j = f.journal();
        let input = f.submit("sub");
        let (first, events) = j.accept(&input, "queued").unwrap();
        assert_eq!(events.len(), 1, "the item comes at dispatch");
        let reopened = Store::open(&f.dir.join("s.db")).unwrap();
        let j = Journal::new(&reopened).unwrap();
        let (retry, events) = j.accept(&input, "direct").unwrap();
        assert_eq!(first, retry);
        assert!(events.is_empty());
        let mut different = input.clone();
        different.text.push('!');
        assert_eq!(code(j.accept(&different, "direct").unwrap_err()), "id_conflict");
        let key = SubmissionKey { session_id: f.id.clone(), submission_id: "sub".into() };
        assert_eq!(j.cancel(&key).unwrap().0.outcome, "cancelled");
        assert!(j.cancel(&key).unwrap().1.is_empty());
        assert_eq!(j.accept(&input, "direct").unwrap().0.outcome, "cancelled");
        f.store.end_only(&f.id).unwrap();
        assert_eq!(j.accept(&input, "direct").unwrap().0.outcome, "cancelled", "ending cannot erase a prior acceptance decision");
        let mut new = input.clone();
        new.submission_id = "new-intent".into();
        assert_eq!(code(j.accept(&new, "queued").unwrap_err()), "invalid_state");
    }
    #[test]
    fn first_answer_survives_retry_and_loser_cannot_overwrite_it() {
        let f = Fixture::new();
        let j = f.journal();
        j.append(&f.id, None, &[Event::QuestionUpdated { question: question("q") }]).unwrap();
        let input =
            AnswerQuestion { session_id: f.id.clone(), question_id: "q".into(), answer_id: "a".into(), answer: Answer::Permission { option_id: "yes".into() } };
        let (winner, _) = j.answer(&input).unwrap();
        assert_eq!(winner.delivery.as_deref(), Some("pending"));
        let reopened = Store::open(&f.dir.join("s.db")).unwrap();
        let j = Journal::new(&reopened).unwrap();
        assert!(j.answer(&input).unwrap().1.is_empty());
        let mut loser = input.clone();
        loser.answer_id = "other".into();
        loser.answer = Answer::Cancel;
        assert_eq!(code(j.answer(&loser).unwrap_err()), "stale_answer");
        loser.answer_id = "a".into();
        assert_eq!(code(j.answer(&loser).unwrap_err()), "id_conflict");
        loser = input;
        loser.question_id = "other-question".into();
        assert_eq!(code(j.answer(&loser).unwrap_err()), "id_conflict");
    }
    #[test]
    fn cursor_scope_and_canonical_encoding_are_validated() {
        let f = Fixture::new();
        let j = f.journal();
        for seq in ["01", "-1", "1", "18446744073709551615", ""] {
            assert_eq!(code(j.replay(&f.id, &Cursor { session_id: f.id.clone(), seq: seq.into() }, 10).unwrap_err()), "invalid_cursor");
        }
        assert_eq!(code(j.replay(&f.id, &Cursor { session_id: "other".into(), seq: "0".into() }, 10).unwrap_err()), "invalid_cursor");
        assert!(j.replay(&f.id, &j.cursor(&f.id).unwrap(), 10).unwrap().is_empty());
    }
    #[test]
    fn replacement_marks_retained_controls_as_stale_in_the_generation_event() {
        let f = Fixture::new();
        let j = f.journal();
        let mut session = j.session(&f.id).unwrap();
        session.controls =
            Some(silli_proto::sfp::SessionControls { status: "ready".into(), error: None, current_mode: Some("old".into()), modes: vec![], options: vec![] });
        j.append(&f.id, None, &[Event::SessionUpdated { session }]).unwrap();
        let events = j.replace_generation(&f.id, "g2").unwrap();
        let Event::SessionUpdated { session } = &events[0].event else { panic!("generation update") };
        assert_eq!(session.agent_generation_id, "g2");
        let controls = session.controls.as_ref().unwrap();
        assert_eq!(controls.status, "loading");
        assert_eq!(controls.current_mode.as_deref(), Some("old"));
    }

    #[test]
    fn agent_replacement_holds_unsent_work_and_never_resends_uncertain_work() {
        let f = Fixture::new();
        let j = f.journal();
        j.accept(&f.submit("sent"), "direct").unwrap();
        j.accept(&f.submit("queued"), "queued").unwrap();
        j.append(&f.id, None, &[Event::QuestionUpdated { question: question("q") }]).unwrap();
        j.replace_generation(&f.id, "g2").unwrap();
        let sent = j.submission(&f.id, "sent").unwrap();
        assert_eq!(sent.insertion, "unknown");
        assert_eq!(sent.outcome, "unknown");
        let queued = j.submission(&f.id, "queued").unwrap();
        assert!(queued.held);
        assert_eq!(queued.insertion, "pending");
        let key = SubmissionKey { session_id: f.id.clone(), submission_id: "queued".into() };
        let resumed = j.resume(&key).unwrap().0;
        assert!(!resumed.held);
        assert_eq!(resumed.agent_generation_id, "g2");
        assert_eq!(resumed.accepted_seq, queued.accepted_seq);
        assert_eq!(j.question(&f.id, "q").unwrap().state, "cancelled");
        assert_eq!(code(j.resume(&SubmissionKey { session_id: f.id.clone(), submission_id: "sent".into() }).unwrap_err()), "invalid_state");
    }
    #[test]
    fn replacement_notice_distinguishes_idle_queued_and_interrupted_work() {
        for (delivery, expected) in [
            (None, "Started a new agent conversation."),
            (Some("queued"), "Started a new agent conversation. Queued messages are paused. Resume to send them."),
            (Some("direct"), "Agent restarted. Previous work may be incomplete and wasn’t resent."),
        ] {
            let f = Fixture::new();
            let j = f.journal();
            if let Some(delivery) = delivery {
                j.accept(&f.submit("input"), delivery).unwrap();
            }
            j.replace_generation(&f.id, "g2").unwrap();
            let (_, records) = j.snapshot(&f.id).unwrap();
            assert!(records.iter().any(|record| matches!(record, Record::Item {item}
                if item.kind=="note" && item.content.iter().any(|block| block.text.as_deref()==Some(expected)))));
            assert!(j.replace_generation(&f.id, "g2").unwrap().is_empty());
        }
    }
    #[test]
    fn removed_block_and_item_identities_cannot_be_reused() {
        let f = Fixture::new();
        let j = f.journal();
        j.append(&f.id, None, &[Event::ItemCreated { item: item("i") }]).unwrap();
        j.append(&f.id, None, &[Event::ItemPatched { item_id: "i".into(), set: ItemPatch { content: Some(vec![]), tool: None } }]).unwrap();
        assert_eq!(
            code(
                j.append(
                    &f.id,
                    None,
                    &[Event::ItemPatched { item_id: "i".into(), set: ItemPatch { content: Some(vec![Content::text("reuse")]), tool: None } }]
                )
                .unwrap_err()
            ),
            "invalid_state"
        );
        j.append(&f.id, None, &[Event::ItemRemoved { item_id: "i".into(), reason: Reason { code: "test".into(), message: "".into() } }]).unwrap();
        assert_eq!(code(j.append(&f.id, None, &[Event::ItemCreated { item: item("i") }]).unwrap_err()), "invalid_state");
    }
    #[test]
    fn memory_transcripts_are_not_imported_and_deletion_erases_canonical_data() {
        let f = Fixture::new();
        let session = f.store.create("test", "default").unwrap();
        f.store.append(&session.id, "user", "old prompt", "").unwrap();
        let j = f.journal();
        j.ensure(&session.id, "g").unwrap();
        let first = j.snapshot(&session.id).unwrap();
        assert_eq!(first.1.len(), 1);
        assert_eq!(f.store.entries(&session.id).unwrap().len(), 1);
        j.ensure(&session.id, "new").unwrap();
        assert_eq!(j.snapshot(&session.id).unwrap(), first);
        j.append(&session.id, None, &[Event::ItemCreated { item: item("canonical") }]).unwrap();
        assert_eq!(j.snapshot(&session.id).unwrap().1.len(), 2);
        f.store.delete(&session.id).unwrap();
        assert_eq!(code(j.cursor(&session.id).unwrap_err()), "not_found");
        let count: i64 = f.store.conn().query_row("SELECT count(*) FROM sfp_records WHERE session_id=?1", [&session.id], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn timestamp_is_rfc3339() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(951827696), "2000-02-29T12:34:56Z");
        assert_eq!(timestamp(-1), "1969-12-31T23:59:59Z");
    }
}
