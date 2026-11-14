//! Public lifecycle boundary; no backend structs are serialized here.
use super::*;
use crate::daemon::sfp::failure;
use rusqlite::OptionalExtension;
use sill_proto::sessions as api;

fn record(store: &Store, s: sill_core::store::Session) -> Result<api::Session> {
    Ok(api::Session {
        running: s.ended_at.is_none() && sill_core::session::is_running(&s.id),
        pins: store.pins(&s.id)?.iter().map(|p| api::HostPath::from_path(p)).collect(),
        session_id: s.id,
        agent: s.agent,
        title: s.title,
        title_kind: s.title_kind,
        created_at: s.created_at.to_string(),
        last_used: s.last_used.to_string(),
        ended_at: s.ended_at.map(|t| t.to_string()),
        policy: s.policy,
        auto_allow: s.auto_allow,
        desktop: s.desktop,
        memory_scope: s.memory_scope,
        summary: s.summary,
        summary_kind: s.summary_kind,
        ghost: s.ghost,
        settings: s.agent_settings.into(),
        follows: (!s.follows.is_empty()).then_some(s.follows),
        compacted: s.compacted.to_string(),
    })
}
fn exact(store: &Store, id: &str) -> Result<sill_core::store::Session> {
    store.session_exact(id)?.ok_or_else(|| Error::domain("not_found", "session not found").into())
}
fn create(store: &Store, mut a: api::Create) -> Result<api::SessionKey> {
    if a.create_id.is_empty() || a.create_id.len() > 128 || a.create_id.chars().any(char::is_control) {
        return Err(Error::invalid_params("create_id must contain 1–128 bytes without control characters").into());
    }
    if a.agent.is_empty() || a.agent.len() > 128 || a.policy.is_empty() || a.policy.len() > 128 || a.pins.len() > 128 {
        return Err(Error::invalid_params("agent and policy are required (at most 128 bytes); at most 128 pins").into());
    }
    if a.ghost && !a.pins.is_empty() {
        return Err(Error::invalid_params(GHOST_NO_PINS).into());
    }
    let pins = a
        .pins
        .iter()
        .map(|p| p.to_path().map_err(Error::invalid_params))
        .collect::<Result<Vec<_>, _>>()?;
    for p in &mut a.pins {
        p.display.clear();
    }
    let input = sill_core::sfp::create_fingerprint(&serde_json::to_string(&a)?);
    store.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS sfp_creates (create_id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, session_id TEXT);
        CREATE TRIGGER IF NOT EXISTS sfp_create_deleted AFTER DELETE ON sessions BEGIN
          UPDATE sfp_creates SET session_id=NULL WHERE session_id=OLD.id;
        END;",
    )?;
    let tx = store.conn().unchecked_transaction()?;
    let key = sill_core::sfp::create_fingerprint(&a.create_id);
    let previous: Option<(String, Option<String>)> = store
        .conn()
        .query_row("SELECT fingerprint,session_id FROM sfp_creates WHERE create_id=?1", [&key], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    if let Some((old, session_id)) = previous {
        if input != old {
            return Err(Error::domain("id_conflict", "create_id was already used with different input").into());
        }
        return Ok(api::SessionKey {
            session_id: session_id.ok_or_else(|| Error::domain("gone", "the created session has been deleted"))?,
        });
    }
    let count: i64 = store.conn().query_row("SELECT count(*) FROM sfp_creates", [], |r| r.get(0))?;
    if count >= 65536 {
        return Err(Error::domain("limit_exceeded", "create retry ledger is full").into());
    }
    if !a.follows.is_empty() {
        exact(store, &a.follows)?;
    }
    let session_id = Hub::create_session_row(
        store,
        NewSession {
            agent: a.agent,
            policy: a.policy,
            pins,
            desktop: a.desktop,
            memory_scope: a.memory_scope,
            follows: a.follows,
            remember_pins: a.remember_pins,
            ghost: a.ghost,
        },
    )?;
    store.conn().execute(
        "INSERT INTO sfp_creates(create_id,fingerprint,session_id) VALUES (?1,?2,?3)",
        rusqlite::params![key, input, session_id],
    )?;
    tx.commit()?;
    Ok(api::SessionKey { session_id })
}
impl Hub {
    pub(in crate::daemon) fn public_sessions(self: &Arc<Self>, method: &str, params: Value) -> Option<Result<Value, Error>> {
        if let Some(result) = self.public_pins(method, params.clone()) { return Some(result); }
        let result = || -> Result<Value> {
            match method {
                "_sill/sessions.list" => {
                    let a: api::List = args(params)?;
                    let limit = a.limit.unwrap_or(64) as usize;
                    if !(1..=128).contains(&limit) {
                        return Err(Error::domain("limit_exceeded", "limit must be 1–128").into());
                    }
                    let after = a
                        .after
                        .as_deref()
                        .map(|s| -> Result<(i64, &str)> {
                            let (time, id) = s.split_once(':').ok_or_else(|| Error::domain("invalid_cursor", "invalid list cursor"))?;
                            let time = time.parse::<i64>().map_err(|_| Error::domain("invalid_cursor", "invalid list cursor"))?;
                            if !sill_core::session::valid_id(id) {
                                return Err(Error::domain("invalid_cursor", "invalid list cursor").into());
                            }
                            Ok((time, id))
                        })
                        .transpose()?;
                    self.with_store(|store| {
                        let mut rows = store.sessions_page(after, limit + 1)?;
                        let more = rows.len() > limit;
                        rows.truncate(limit);
                        let next = more.then(|| {
                            let s = rows.last().unwrap();
                            format!("{}:{}", s.created_at, s.id)
                        });
                        let sessions = rows.into_iter().map(|s| record(store, s)).collect::<Result<Vec<_>>>()?;
                        let value = json!(api::Page { sessions, next });
                        if serde_json::to_vec(&value)?.len() > 4 * 1024 * 1024 {
                            return Err(Error::domain("limit_exceeded", "session page exceeds 4 MiB; request fewer records").into());
                        }
                        Ok(value)
                    })
                }
                "_sill/sessions.get" => {
                    let a: api::SessionKey = args(params)?;
                    self.with_store(|s| Ok(json!(record(s, exact(s, &a.session_id)?)?)))
                }
                "_sill/sessions.create" => {
                    let a: api::Create = args(params)?;
                    self.with_store(|s| Ok(json!(create(s, a)?)))
                }
                "_sill/sessions.update" => {
                    let a: api::Update = args(params)?;
                    if serde_json::to_vec(&a.change)?.len() > 1024 * 1024 {
                        return Err(Error::domain("limit_exceeded", "session update exceeds 1 MiB").into());
                    }
                    let refresh = matches!(&a.change, api::Change::Title { .. } | api::Change::Summary { .. });
                    self.with_store(|store| {
                        let row = exact(store, &a.session_id)?;
                        if row.ghost && matches!(&a.change, api::Change::Summary { .. }) {
                            return Err(Error::domain("invalid_state", "a ghost has no summary").into());
                        }
                        let journal = sill_core::sfp::Journal::new(store)?;
                        let tx = store.conn().unchecked_transaction()?;
                        let mut session = journal.ensure(&a.session_id, &crate::daemon::sfp::id())?;
                        if session.deleted {
                            return Err(Error::domain("gone", "session is being deleted").into());
                        }
                        match a.change {
                            api::Change::Title { text, kind } => store.set_title(&a.session_id, &text, &kind)?,
                            api::Change::Summary { text, kind } => store.set_summary(&a.session_id, &text, &kind)?,
                            api::Change::AutoAllow { enabled } => store.set_auto_allow(&a.session_id, enabled)?,
                            api::Change::Desktop { value } => store.set_desktop(&a.session_id, &value)?,
                            api::Change::MemoryScope { value } => store.set_memory_scope(&a.session_id, &value)?,
                            api::Change::Policy { value } => store.set_policy(&a.session_id, &value)?,
                            api::Change::Settings { settings } => store.set_session_settings(&a.session_id, &settings.into())?,
                            api::Change::Reopen => store.reopen(&a.session_id)?,
                        }
                        let row = exact(store, &a.session_id)?;
                        session.title = Some(row.title);
                        session.summary = (!row.summary.is_empty()).then_some(row.summary);
                        if session.lifecycle == "ended" && row.ended_at.is_none() {
                            session.lifecycle = "open".into();
                            session.activity = "idle".into();
                            session.end_reason = None;
                            session.deleted = false;
                        }
                        let events = journal.append(&a.session_id, None, &[sill_proto::sfp::Event::SessionUpdated { session }])?;
                        tx.commit()?;
                        self.sfp_publish(&a.session_id, &events);
                        Ok(())
                    })?;
                    if refresh {
                        refresh_memory();
                    }
                    Ok(json!({}))
                }
                "_sill/sessions.end" => {
                    let a: api::End = args(params)?;
                    if !["close", "idle", "quit"].contains(&a.reason.as_str()) {
                        return Err(Error::invalid_params("reason must be close, idle or quit").into());
                    }
                    let row = self.with_store(|s| exact(s, &a.session_id))?;
                    let e = if row.ended_at.is_some() {
                        Ending {
                            pending: false,
                            deleted: false,
                        }
                    } else {
                        self.end_session(&a.session_id, &a.reason)?
                    };
                    Ok(json!(api::Ending {
                        pending: e.pending,
                        deleted: e.deleted
                    }))
                }
                "_sill/sessions.delete" => {
                    let a: api::SessionKey = args(params)?;
                    self.with_store(|s| exact(s, &a.session_id))?;
                    self.delete_session(&a.session_id)?;
                    Ok(json!({}))
                }
                "_sill/sessions.reset" => {
                    let a: api::SessionKey = args(params)?;
                    let row = self.with_store(|s| exact(s, &a.session_id))?;
                    if row.ended_at.is_some() { return Err(Error::domain("invalid_state", "reopen the session before resetting its agent").into()); }
                    self.ensure_runner(&a.session_id, true)?;
                    Ok(json!({}))
                }
                "_sill/sessions.log" => {
                    let a: api::Log = args(params)?;
                    self.with_store(|s| exact(s, &a.session_id))?;
                    let n = a.lines.unwrap_or(100);
                    if n == 0 || n > 20000 { return Err(Error::invalid_params("lines must be 1–20000").into()); }
                    let text = sill_core::session::read_log(&sill_core::session::log_source(&a.session_id), n as usize);
                    if serde_json::to_vec(&text)?.len()>4*1024*1024 { return Err(Error::domain("limit_exceeded", "log exceeds 4 MiB; request fewer lines").into()); }
                    Ok(json!(sill_proto::Text { text }))
                }
                "_sill/sessions.start" | "_sill/sessions.stop" | "_sill/sessions.checkpoint" => {
                    let a: api::SessionKey = args(params)?;
                    let row = self.with_store(|s| exact(s, &a.session_id))?;
                    if row.ended_at.is_some() {
                        return Err(Error::domain("invalid_state", "session has ended; reopen it explicitly").into());
                    }
                    match method {
                        "_sill/sessions.start" => {
                            let s = self.start_session(&a.session_id)?;
                            Ok(json!(api::Started {
                                session_id: a.session_id,
                                fresh: s.fresh
                            }))
                        }
                        "_sill/sessions.stop" => {
                            self.retire_runner(&a.session_id);
                            sill_core::session::stop(&a.session_id)?;
                            Ok(json!({}))
                        }
                        _ => {
                            self.checkpoint(&a.session_id)?;
                            Ok(json!({}))
                        }
                    }
                }
                _ => unreachable!(),
            }
        };
        if !["list", "get", "create", "update", "start", "stop", "end", "checkpoint", "delete", "reset", "log"]
            .iter()
            .any(|n| method == format!("_sill/sessions.{n}"))
        {
            return None;
        }
        Some(result().map_err(failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creation_is_atomic_and_paths_never_use_display_text() {
        let dir = std::env::temp_dir().join(format!("sill-create-test-{}", crate::daemon::sfp::id()));
        let store = Store::open(&dir.join("store.db")).unwrap();
        let input = api::Create {
            create_id: "key".into(),
            agent: "test".into(),
            policy: "default".into(),
            pins: vec![api::HostPath::from_path(std::path::Path::new("/exact"))],
            ..Default::default()
        };
        let first = create(&store, input.clone()).unwrap();
        let mut retry = input.clone();
        retry.pins[0].display = "/wrong".into();
        assert_eq!(create(&store, retry).unwrap().session_id, first.session_id);
        assert_eq!(store.pins(&first.session_id).unwrap(), vec![PathBuf::from("/exact")]);
        // Force failure after the row was inserted, before the retry receipt is committed.
        store
            .conn()
            .execute_batch("CREATE TRIGGER reject_pin BEFORE INSERT ON session_pins BEGIN SELECT RAISE(ABORT,'test failure'); END;")
            .unwrap();
        let mut failed = input.clone();
        failed.create_id = "rolled-back".into();
        assert!(create(&store, failed).is_err());
        assert_eq!(store.sessions().unwrap().len(), 1);
        assert_eq!(
            store.conn().query_row("SELECT count(*) FROM sfp_creates", [], |r| r.get::<_, i64>(0)).unwrap(),
            1
        );
        store.conn().execute_batch("DROP TRIGGER reject_pin").unwrap();
        let mut non_utf8 = input;
        non_utf8.create_id = "exact-bytes".into();
        non_utf8.pins[0].bytes_base64 = "L/8=".into();
        non_utf8.pins[0].display = "/not-the-path".into();
        let exact = non_utf8.pins[0].to_path().unwrap();
        let created = create(&store, non_utf8.clone()).unwrap();
        assert_eq!(store.pins(&created.session_id).unwrap(), vec![exact]);
        assert_eq!(create(&store, non_utf8).unwrap().session_id, created.session_id);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
