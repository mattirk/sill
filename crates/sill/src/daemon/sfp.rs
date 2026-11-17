//! SFP synchronization. Store lock orders snapshot capture, subscription and publication.
use super::{Conn, Hub};
use anyhow::Result;
use serde_json::{Value, json};
use sill_core::sfp::Journal;
use sill_proto::{
    rpc::{Error, Request, Response},
    sfp::*,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE_RECORDS: usize = 128;
const MAX_SNAPSHOTS: usize = 8;
const PAGE_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024 * 1024;
const REPLAY_LIMIT: usize = 4096;
#[derive(Default)]
pub(super) struct Subscriptions {
    active: HashMap<(u64, String), Subscription>,
    snapshots: HashMap<String, Snapshot>,
}
struct Subscription {
    id: String,
    conn: Arc<Conn>,
}
struct Snapshot {
    conn: u64,
    session: String,
    at: Cursor,
    spool: rusqlite::Connection,
    page_count: usize,
    bytes: usize,
    expires: Instant,
    deadline: Instant,
}
impl Subscriptions {
    pub fn expire(&mut self) {
        self.snapshots.retain(|_, s| s.expires > Instant::now());
    }
    pub fn sessions_for(&self, conn: u64) -> Vec<String> {
        self.active.keys().filter(|(c, _)| *c == conn).map(|(_, s)| s.clone()).collect()
    }
    pub fn count(&self, session: &str) -> usize {
        self.active.keys().filter(|(_, s)| s == session).count()
    }
    pub fn disconnect(&mut self, conn: u64) {
        self.active.retain(|(c, _), _| *c != conn);
        self.snapshots.retain(|_, s| s.conn != conn);
    }
    fn remove(&mut self, conn: u64, session: &str) {
        self.active.remove(&(conn, session.into()));
        self.snapshots.retain(|_, s| s.conn != conn || s.session != session);
    }
    fn publish(&mut self, session: &str, events: &[StoredEvent]) {
        self.active.retain(|(_, sid), sub| {
            if sid != session {
                return true;
            }
            for event in events {
                if !notify(&sub.conn, session, &sub.id, event) {
                    return false;
                }
            }
            true
        });
    }
}
fn notify(conn: &Conn, session: &str, sub: &str, event: &StoredEvent) -> bool {
    let params = SessionEvent { session_id: session.into(), subscription_id: sub.into(), seq: event.seq.clone(), event: event.event.clone() };
    conn.send(serde_json::to_string(&Request::new(None, "_sill/session.event", json!(params))).unwrap())
}
pub(super) fn id() -> String {
    // Multiple independently random components; opaque identity, unrelated to relay request IDs.
    format!("{}{}{}{}", sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id())
}
fn domain(code: &str, message: &str) -> Error {
    let mut e = Error::new(Error::FAILED, message);
    e.data = Some(json!({"code":code}));
    e
}
pub(super) fn failure(e: anyhow::Error) -> Error {
    if let Some(e) = e.downcast_ref::<sill_core::sfp::DomainError>() {
        let mut rpc = domain(e.code, &e.message);
        if let Some(details) = &e.details {
            rpc.data.as_mut().unwrap()["details"] = details.clone();
        }
        return rpc;
    }
    e.downcast_ref::<Error>().cloned().unwrap_or_else(|| domain("invalid_state", &format!("{e:#}")))
}
fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(Error::invalid_params)
}

impl Hub {
    pub(super) fn sfp_lifecycle(&self, session_id: &str, ended: Option<(bool, &str)>, note: Option<String>) -> Result<()> {
        if ended.is_some() {
            super::host_control::session_ended(self, session_id, "the session ended");
        }
        self.with_store(|store| {
            let journal = Journal::new(store)?;
            let row = store.sessions()?.into_iter().find(|s| s.id == session_id).ok_or_else(|| domain("not_found", "session not found"))?;
            let tx = store.conn().unchecked_transaction()?;
            let mut session = journal.ensure(session_id, &id())?;
            session.title = Some(row.title);
            session.summary = (!row.summary.is_empty()).then_some(row.summary);
            if let Some((deleted, reason)) = ended {
                session.lifecycle = "ended".into();
                session.activity = "idle".into();
                session.summarizing = None;
                session.end_reason = Some(reason.into());
                session.deleted = deleted;
            }
            let mut events = vec![];
            if ended.is_some() {
                let records = journal.active_records(session_id)?;
                for record in records {
                    match record {
                        Record::Submission { mut submission } if submission.outcome == "pending" => {
                            if submission.delivery == "queued" && submission.insertion == "pending" {
                                submission.outcome = "cancelled".into();
                            } else {
                                submission.outcome = "unknown".into();
                                if submission.insertion == "pending" {
                                    submission.insertion = "unknown".into();
                                }
                            }
                            submission.reason = Some(Reason { code: "session_ended".into(), message: "Session ended".into() });
                            events.push(Event::SubmissionUpdated { submission });
                        }
                        Record::Turn { mut turn } if turn.outcome == "pending" => {
                            turn.activity = "unknown".into();
                            turn.outcome = "unknown".into();
                            turn.reason = Some(Reason { code: "session_ended".into(), message: "Session ended without a final turn result".into() });
                            events.push(Event::TurnUpdated { turn });
                        }
                        Record::Question { mut question } if question.state == "open" || question.delivery.as_deref() == Some("pending") => {
                            if question.state == "open" {
                                question.state = "cancelled".into();
                            } else {
                                question.delivery = Some("unknown".into());
                            }
                            question.reason = Some(Reason { code: "session_ended".into(), message: "Session ended".into() });
                            events.push(Event::QuestionUpdated { question });
                        }
                        _ => {}
                    }
                }
                session.question_ids.clear();
                session.turn_id = None;
            }
            if let Some(text) = note {
                events.push(Event::ItemCreated {
                    item: Item {
                        item_id: id(),
                        created_seq: String::new(),
                        turn_id: None,
                        submission_id: None,
                        agent_generation_id: session.agent_generation_id.clone(),
                        kind: "note".into(),
                        content: vec![Content::text(text)],
                        tool: None,
                    },
                });
            }
            events.push(Event::SessionUpdated { session });
            let committed = journal.append(session_id, None, &events)?;
            tx.commit()?;
            self.sfp_publish(session_id, &committed);
            Ok(())
        })
    }
    /// Caller holds the store lock and has committed before publishing.
    pub(super) fn sfp_publish(&self, session: &str, events: &[StoredEvent]) {
        self.sfp.lock().unwrap().publish(session, events);
    }
    pub(super) fn sfp_request(self: &Arc<Self>, conn: &Arc<Conn>, request_id: u64, method: &str, params: Value) -> Result<bool> {
        if let Err(error) = super::admin::frontend_gate(self, conn, method, &params) {
            conn.reply(&Response::err(Some(request_id), error));
            return Ok(true);
        }
        if let Some(result) = self.public_sessions(method, params.clone()) {
            conn.reply(&match result {
                Ok(value) => Response::ok(request_id, value),
                Err(error) => Response::err(Some(request_id), error),
            });
            return Ok(true);
        }
        if let Some(result) = super::host::handle(self, method, params.clone()) {
            conn.reply(&match result {
                Ok(value) => Response::ok(request_id, value),
                Err(error) => Response::err(Some(request_id), failure(error)),
            });
            return Ok(true);
        }
        if let Some(result) = super::terminals::handle(method, params.clone()) {
            conn.reply(&match result {
                Ok(value) => Response::ok(request_id, value),
                Err(error) => Response::err(Some(request_id), failure(error)),
            });
            return Ok(true);
        }
        if method == "_sill/sessions.attach" {
            let result = (|| -> Result<(), Error> {
                let a: Attach = args(params)?;
                if let Some(since) = &a.since {
                    if since.session_id != a.session_id {
                        return Err(domain("invalid_cursor", "cursor belongs to another session"));
                    }
                    sill_core::sfp::sequence(&since.seq).map_err(failure)?;
                }
                // Resolve exact identity before any start; SFP mutations never use a title/prefix.
                self.with_store(|s| {
                    if !s.sessions()?.iter().any(|s| s.id == a.session_id) {
                        return Err(domain("not_found", "session not found").into());
                    }
                    if let Some(since) = &a.since {
                        let journal = Journal::new(s)?;
                        let last = match journal.cursor(&a.session_id) {
                            Ok(cursor) => sill_core::sfp::sequence(&cursor.seq)?,
                            Err(e) if e.downcast_ref::<sill_core::sfp::DomainError>().is_some_and(|e| e.code == "not_found") => 0,
                            Err(e) => return Err(e),
                        };
                        if sill_core::sfp::sequence(&since.seq)? > last {
                            return Err(domain("invalid_cursor", "cursor is ahead of the daemon").into());
                        }
                    }
                    Ok(())
                })
                .map_err(failure)?;
                let runner = self.ensure_runner(&a.session_id, false).map_err(failure)?;
                let store = self.store.lock().unwrap();
                runner.sfp_initialize(&store, self).map_err(failure)?;
                let journal = Journal::new(&store).map_err(failure)?;
                let through = journal.cursor(&a.session_id).map_err(failure)?;
                let replay = a
                    .since
                    .as_ref()
                    .map(|since| {
                        if since.session_id != a.session_id {
                            return Err(domain("invalid_cursor", "cursor belongs to another session").into());
                        }
                        match journal.replay(&a.session_id, since, REPLAY_LIMIT + 1) {
                            Ok(events) => Ok(Some(events)),
                            Err(e) if e.downcast_ref::<sill_core::sfp::DomainError>().is_some_and(|e| e.code == "cursor_expired") => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                    .transpose()
                    .map_err(failure)?
                    .flatten();
                let sub_id = id();
                let mut subscriptions = self.sfp.lock().unwrap();
                subscriptions.snapshots.retain(|_, s| s.expires > Instant::now());
                let (reply, tail) = if let (Some(after), Some(events)) = (&a.since, replay) {
                    if events.len() <= REPLAY_LIMIT
                        && events.iter().all(|e| serde_json::to_vec(e).is_ok_and(|b| b.len() < sill_proto::initialize::MAX_MESSAGE_BYTES - 4096))
                    {
                        (Attached::Replay { subscription_id: sub_id.clone(), after: after.clone(), through }, events)
                    } else {
                        (snapshot(&journal, conn.id, &a.session_id, &sub_id, &mut subscriptions)?, vec![])
                    }
                } else {
                    (snapshot(&journal, conn.id, &a.session_id, &sub_id, &mut subscriptions)?, vec![])
                };
                // Replacement invalidates old leases, except the newly allocated one.
                let new_token = match &reply {
                    Attached::Snapshot { snapshot, .. } => Some(snapshot.token.clone()),
                    _ => None,
                };
                subscriptions.snapshots.retain(|token, s| s.conn != conn.id || s.session != a.session_id || Some(token) == new_token.as_ref());
                subscriptions.active.insert((conn.id, a.session_id.clone()), Subscription { id: sub_id.clone(), conn: conn.clone() });
                conn.reply(&Response::ok(request_id, json!(reply)));
                for event in tail {
                    if !notify(conn, &a.session_id, &sub_id, &event) {
                        break;
                    }
                }
                drop(subscriptions);
                drop(store);
                self.presence(&a.session_id);
                Ok(())
            })();
            if let Err(e) = result {
                conn.reply(&Response::err(Some(request_id), e));
            }
            return Ok(true);
        }
        let result: Result<Value, Error> = match method {
            "_sill/sessions.snapshot" => (|| {
                let a: SnapshotRequest = args(params)?;
                let mut all = self.sfp.lock().unwrap();
                all.snapshots.retain(|_, s| s.expires > Instant::now());
                let s =
                    all.snapshots.get_mut(&a.token).filter(|s| s.conn == conn.id).ok_or_else(|| domain("snapshot_expired", "snapshot lease is unavailable"))?;
                let page = match a.page {
                    Some(p) => p.parse::<usize>().map_err(|_| Error::invalid_params("invalid page"))?,
                    None => 0,
                };
                let mut result = s.page(page)?;
                let remaining = s.deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(60));
                s.expires = Instant::now() + remaining;
                result.expires_at = Some(sill_core::sfp::timestamp(sill_core::store::now() + remaining.as_secs() as i64));
                Ok(json!(result))
            })(),
            "_sill/sessions.detach" => (|| {
                let a: Detach = args(params)?;
                let mut all = self.sfp.lock().unwrap();
                if all.active.get(&(conn.id, a.session_id.clone())).is_some_and(|s| s.id == a.subscription_id) {
                    all.remove(conn.id, &a.session_id);
                }
                drop(all);
                self.presence(&a.session_id);
                super::host_control::displaying_changed(self);
                Ok(json!({}))
            })(),
            "_sill/uploads.create" => (|| {
                let a: UploadCreate = args(params)?;
                self.with_store(|s| super::uploads::create(s, a)).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/uploads.get" => (|| {
                let a: UploadGet = args(params)?;
                self.with_store(|s| super::uploads::get(s, a)).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/sessions.control" => (|| {
                let a: SessionControl = args(params)?;
                self.runner_for_sfp(&a.session_id)?.sfp_control(self, a.control).map_err(failure)?;
                Ok(json!({}))
            })(),
            "_sill/sessions.cancel" => (|| {
                let a: CancelTurn = args(params)?;
                self.runner_for_sfp(&a.session_id)?.sfp_cancel_turn(self, &a.turn_id).map_err(failure)?;
                Ok(json!({"requested":true}))
            })(),
            "_sill/sessions.submit" => (|| {
                let a: Submit = args(params)?;
                let previous = self
                    .with_store(|store| {
                        let journal = Journal::new(store)?;
                        match journal.submission(&a.session_id, &a.submission_id) {
                            // compare what was accepted: inline attachments as the uploads they became
                            Ok(_) => {
                                let tx = store.conn().unchecked_transaction()?;
                                let (submission, _) = journal.accept(&super::runner::inline_as_uploads(store, a.clone())?, "queued")?;
                                tx.commit()?;
                                Ok(Some(submission))
                            }
                            Err(e) if e.downcast_ref::<sill_core::sfp::DomainError>().is_some_and(|e| e.code == "not_found") => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                    .map_err(failure)?;
                if let Some(previous) = previous {
                    return Ok(json!(previous));
                }
                self.ensure_runner(&a.session_id, false).map_err(failure)?.sfp_submit(self, a).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/submissions.get" => (|| {
                let a: SubmissionKey = args(params)?;
                self.with_store(|s| Journal::new(s)?.submission(&a.session_id, &a.submission_id)).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/submissions.cancel" | "_sill/submissions.resume" => (|| {
                let a: SubmissionKey = args(params)?;
                self.runner_for_sfp(&a.session_id)?.sfp_queue_action(self, &a, method.ends_with("resume")).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/submissions.send_now" => (|| {
                let a: SubmissionKey = args(params)?;
                self.runner_for_sfp(&a.session_id)?.sfp_send_now(self, &a).map(|s| json!(s)).map_err(failure)
            })(),
            "_sill/questions.answer" => (|| {
                // the answer is open in the schema (questions carry it in results); the request is strict
                if let Some(answer) = params.get("answer") {
                    sill_proto::rpc::strict::<sill_proto::sfp::Answer>(answer.clone())?;
                }
                let a: AnswerQuestion = args(params)?;
                self.runner_for_sfp(&a.session_id)?.sfp_answer(self, a).map(|s| json!(s)).map_err(failure)
            })(),
            _ => return Ok(false),
        };
        conn.reply(&match result {
            Ok(v) => Response::ok(request_id, v),
            Err(e) => Response::err(Some(request_id), e),
        });
        Ok(true)
    }
    fn runner_for_sfp(&self, id: &str) -> Result<Arc<super::runner::Runner>, Error> {
        self.runner(id)
    }
}
impl Snapshot {
    fn page(&self, index: usize) -> Result<SnapshotPage, Error> {
        if index >= self.page_count {
            return Err(Error::invalid_params("invalid page"));
        }
        let json: String = self.spool.query_row("SELECT json FROM pages WHERE id=?1", [index as i64], |r| r.get(0)).map_err(Error::failed)?;
        let mut page: SnapshotPage = serde_json::from_str(&json).map_err(Error::failed)?;
        page.at = self.at.clone();
        page.next_page = (index + 1 < self.page_count).then(|| (index + 1).to_string());
        Ok(page)
    }
}
fn snapshot(journal: &Journal<'_>, conn: u64, session: &str, sub: &str, all: &mut Subscriptions) -> Result<Attached, Error> {
    if all.snapshots.values().filter(|s| s.conn == conn && s.session != session).count() >= MAX_SNAPSHOTS {
        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, None, "too many snapshots"));
    }
    if all.snapshots.values().filter(|s| s.conn != conn || s.session != session).count() >= 64 {
        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, Some(64), "too many active snapshots"));
    }
    // SQLite's empty filename creates a private, automatically deleted temporary database.
    // Bound its cache; unlike the former Vec this does not retain the full history in RAM.
    let spool = rusqlite::Connection::open("").map_err(Error::failed)?;
    spool.execute_batch("PRAGMA cache_size=-2048; CREATE TABLE pages(id INTEGER PRIMARY KEY,json TEXT NOT NULL); BEGIN").map_err(Error::failed)?;
    let at = journal.cursor(session).map_err(failure)?;
    let mut page_count = 0usize;
    let mut bytes = 0usize;
    // Keep the previous lease usable until replacement succeeds, and reserve its disk use
    // throughout capture. Excluding it would allow a replacement to exceed the aggregate cap.
    let existing_bytes: usize = all.snapshots.values().map(|s| s.bytes).sum();
    let mut save = |records: Vec<Record>, fragment: Option<RecordFragment>| -> anyhow::Result<()> {
        let page = SnapshotPage { at: at.clone(), records, fragment, next_page: None, expires_at: None };
        let json = serde_json::to_string(&page)?;
        bytes = bytes.saturating_add(json.len());
        if bytes > MAX_SNAPSHOT_BYTES || existing_bytes.saturating_add(bytes) > 2 * MAX_SNAPSHOT_BYTES {
            return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, None, "snapshot disk quota exceeded").into());
        }
        spool.execute("INSERT INTO pages VALUES (?1,?2)", rusqlite::params![page_count as i64, json])?;
        page_count += 1;
        Ok(())
    };
    let mut records = Vec::new();
    let mut page_bytes = 0usize;
    let mut record_index = 0usize;
    journal
        .visit_snapshot(session, |json| {
            if json.len() > 64 * 1024 * 1024 {
                return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Record, Some(67108864), "record exceeds 64 MiB").into());
            }
            if json.len() > PAGE_BYTES {
                if !records.is_empty() {
                    save(std::mem::take(&mut records), None)?;
                    page_bytes = 0;
                }
                let mut offset = 0;
                while offset < json.len() {
                    let mut end = (offset + 256 * 1024).min(json.len());
                    while !json.is_char_boundary(end) {
                        end -= 1;
                    }
                    save(
                        vec![],
                        Some(RecordFragment {
                            record: record_index.to_string(),
                            offset: offset.to_string(),
                            json: json[offset..end].into(),
                            last: end == json.len(),
                        }),
                    )?;
                    offset = end;
                }
            } else {
                if records.len() >= PAGE_RECORDS || page_bytes + json.len() > PAGE_BYTES {
                    save(std::mem::take(&mut records), None)?;
                    page_bytes = 0;
                }
                page_bytes += json.len();
                records.push(serde_json::from_str(&json)?);
            }
            record_index += 1;
            Ok(())
        })
        .map_err(failure)?;
    if !records.is_empty() {
        save(records, None).map_err(failure)?;
    }
    drop(save);
    spool.execute_batch("COMMIT").map_err(Error::failed)?;
    let token = id();
    let expires_at = sill_core::sfp::timestamp(sill_core::store::now() + 60);
    all.snapshots.insert(
        token.clone(),
        Snapshot {
            conn,
            session: session.into(),
            at: at.clone(),
            spool,
            page_count,
            bytes,
            expires: Instant::now() + Duration::from_secs(60),
            deadline: Instant::now() + Duration::from_secs(1800),
        },
    );
    Ok(Attached::Snapshot { subscription_id: sub.into(), snapshot: SnapshotLease { token, at, expires_at } })
}

#[cfg(test)]
#[path = "sfp_tests.rs"]
mod tests;
