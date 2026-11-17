//! Public lifecycle boundary; no backend structs are serialized here.
use super::*;
use crate::daemon::sfp::failure;
use rusqlite::OptionalExtension;
use sill_proto::sessions as api;

fn record(store: &Store, s: sill_core::store::Session) -> Result<api::Session> {
    // a session never attached has no journal state yet: no time worked
    let (active_ms, active_since_ms) = store
        .conn()
        .query_row("SELECT json_extract(state,'$.active_ms'),json_extract(state,'$.active_since_ms') FROM sfp_sessions WHERE session_id=?1", [&s.id], |r| {
            Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64, r.get::<_, Option<i64>>(1)?.map(|v| v as u64)))
        })
        .unwrap_or((0, None));
    Ok(api::Session {
        active_ms,
        active_since_ms,
        running: s.ended_at.is_none() && sill_core::session::is_running(&s.id),
        policy_changed: s.ended_at.is_none()
            && sill_core::changes::home().is_ok_and(|home| {
                let pins = if s.ghost { vec![] } else { store.jail_pins(&s.id).unwrap_or_default() };
                let Ok(mut registry) = sill_core::agents::load(&home) else { return true };
                let domains = registry.remove(&s.agent).map(|a| a.domains).unwrap_or_default();
                let current = sill_core::session::recorded_filesystem(&s.id);
                let next = (|| -> Result<sill_core::session::Resolution> {
                    let resolution = sill_core::session::next_build_base(&s, &home, &pins)?;
                    let resolution = sill_core::session::apply_grants(resolution, &store.permission_choices(&s.id)?, store.network_hosts(&s.id)?);
                    let workspace = store.workspace_mask(&s.id)?;
                    sill_core::session::freeze_filesystem(
                        resolution,
                        &s.agent,
                        &home,
                        &sill_core::session::state_dir_path(rustix::process::getuid().as_raw(), &s.id)?,
                        &pins,
                        s.ghost,
                        &workspace,
                    )
                })();
                match (current, next) {
                    (Ok(current), Ok(next)) if next.filesystem.as_ref().is_some_and(|next| current.same_mounts(next)) => {}
                    _ => return true,
                }
                match sill_core::session::session_base_policy(&s.policy, &home, s.ghost, Some(&s.agent)) {
                    Ok(base) => sill_core::session::jail_outdated_with_grants(&s.id, &base, &pins, store, &domains),
                    Err(_) => true,
                }
            }),
        name: store.session_name(&s.id),
        host_control: crate::daemon::host_control::brief(&s.id),
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
pub(super) fn create(store: &Store, mut a: api::Create) -> Result<api::SessionKey> {
    if a.create_id.is_empty() || a.create_id.len() > 128 || a.create_id.chars().any(char::is_control) {
        return Err(Error::invalid_params("create_id must contain 1–128 bytes without control characters").into());
    }
    if a.agent.is_empty() || a.agent.len() > 128 || a.policy.is_empty() || a.policy.len() > 128 || a.pins.len() > 128 {
        return Err(Error::invalid_params("agent and policy are required (at most 128 bytes); at most 128 pins").into());
    }
    if a.ghost && !a.pins.is_empty() {
        return Err(Error::invalid_params(GHOST_NO_PINS).into());
    }
    let pins = a.pins.iter().map(|p| p.to_path().map_err(Error::invalid_params)).collect::<Result<Vec<_>, _>>()?;
    for p in &mut a.pins {
        p.display.clear();
    }
    let input = sill_core::sfp::canonical_fingerprint(&serde_json::to_value(&a)?);
    store.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS sfp_creates (create_id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, session_id TEXT);
        CREATE TRIGGER IF NOT EXISTS sfp_create_deleted AFTER DELETE ON sessions BEGIN
          UPDATE sfp_creates SET session_id=NULL WHERE session_id=OLD.id;
        END;",
    )?;
    let tx = store.conn().unchecked_transaction()?;
    let key = sill_core::sfp::create_fingerprint(&a.create_id);
    let previous: Option<(String, Option<String>)> =
        store.conn().query_row("SELECT fingerprint,session_id FROM sfp_creates WHERE create_id=?1", [&key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((old, session_id)) = previous {
        if input != old {
            return Err(Error::domain("id_conflict", "create_id was already used with different input").into());
        }
        return Ok(api::SessionKey { session_id: session_id.ok_or_else(|| Error::domain("gone", "the created session has been deleted"))? });
    }
    let count: i64 = store.conn().query_row("SELECT count(*) FROM sfp_creates", [], |r| r.get(0))?;
    if count >= 65536 {
        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Capacity, None, "create retry ledger is full").into());
    }
    if !a.follows.is_empty() {
        exact(store, &a.follows)?;
    }
    let workspaces = !a.ghost && a.workspaces.unwrap_or_else(|| sill_core::config::load().automatic_workspaces());
    let session_id = Hub::create_session_row(
        store,
        NewSession { agent: a.agent, policy: a.policy, pins, memory_scope: a.memory_scope, follows: a.follows, remember_pins: a.remember_pins, ghost: a.ghost },
    )?;
    // a session's mode is fixed when it is made (docs/parallel-coding.md §12)
    sill_core::workspace::records::set_mode(store.conn(), &session_id, workspaces)?;
    store.conn().execute("INSERT INTO sfp_creates(create_id,fingerprint,session_id) VALUES (?1,?2,?3)", rusqlite::params![key, input, session_id])?;
    tx.commit()?;
    Ok(api::SessionKey { session_id })
}
/// Fill a public page across internal workers. The cursor still names the last visible
/// row, so hidden rows neither shorten a page nor cause later sessions to disappear.
fn public_page(store: &Store, after: Option<(i64, &str)>, limit: usize) -> Result<Vec<sill_core::store::Session>> {
    let hidden = {
        let mut statement = store.conn().prepare("SELECT session_id FROM crosscheck_workers")?;
        statement.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<std::collections::HashSet<_>, _>>()?
    };
    let mut cursor = after.map(|(time, id)| (time, id.to_string()));
    let mut visible = Vec::new();
    while visible.len() < limit {
        let rows = store.sessions_page(cursor.as_ref().map(|(time, id)| (*time, id.as_str())), 128)?;
        let exhausted = rows.len() < 128;
        if let Some(last) = rows.last() {
            cursor = Some((last.created_at, last.id.clone()));
        }
        visible.extend(rows.into_iter().filter(|s| !hidden.contains(&s.id)).take(limit - visible.len()));
        if exhausted {
            break;
        }
    }
    Ok(visible)
}

impl Hub {
    pub(in crate::daemon) fn public_sessions(self: &Arc<Self>, method: &str, params: Value) -> Option<Result<Value, Error>> {
        if let Some(result) = self.public_pins(method, params.clone()) {
            return Some(result);
        }
        let result = || -> Result<Value> {
            match method {
                "_sill/sessions.list" => {
                    let a: api::List = args(params)?;
                    let limit = a.limit.unwrap_or(64) as usize;
                    if !(1..=128).contains(&limit) {
                        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(128), "limit must be 1–128").into());
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
                        let mut rows = public_page(store, after, limit + 1)?;
                        let more = rows.len() > limit;
                        rows.truncate(limit);
                        let next = more.then(|| {
                            let s = rows.last().unwrap();
                            format!("{}:{}", s.created_at, s.id)
                        });
                        let sessions = rows.into_iter().map(|s| record(store, s)).collect::<Result<Vec<_>>>()?;
                        let value = json!(api::Page { sessions, next });
                        if serde_json::to_vec(&value)?.len() > 4 * 1024 * 1024 {
                            return Err(sill_proto::rpc::Error::limit(
                                sill_proto::rpc::LimitScope::Result,
                                Some(4194304),
                                "session page exceeds 4 MiB; request fewer records",
                            )
                            .into());
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
                        return Err(sill_proto::rpc::Error::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "session update exceeds 1 MiB").into());
                    }
                    let refresh = matches!(&a.change, api::Change::Name { .. } | api::Change::Title { .. } | api::Change::Summary { .. });
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
                            api::Change::Name { text } => {
                                let text = text.trim();
                                if text.is_empty() || text.chars().count() > 80 || text.chars().any(char::is_control) {
                                    return Err(Error::invalid_params("name must contain 1–80 characters without control characters").into());
                                }
                                store.set_name(&a.session_id, text)?;
                            }
                            api::Change::Title { text, kind } => store.set_title(&a.session_id, &text, &kind)?,
                            api::Change::Summary { text, kind } => store.set_summary(&a.session_id, &text, &kind)?,
                            api::Change::AutoAllow { enabled } => store.set_auto_allow(&a.session_id, enabled)?,
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
                    let e = if row.ended_at.is_some() { Ending { pending: false, deleted: false } } else { self.end_session(&a.session_id, &a.reason)? };
                    Ok(json!(api::Ending { pending: e.pending, deleted: e.deleted }))
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
                    if row.ended_at.is_some() {
                        return Err(Error::domain("invalid_state", "reopen the session before resetting its agent").into());
                    }
                    self.ensure_runner(&a.session_id, true)?;
                    Ok(json!({}))
                }
                "_sill/sessions.log" => {
                    let a: api::Log = args(params)?;
                    self.with_store(|s| exact(s, &a.session_id))?;
                    let n = a.lines.unwrap_or(100);
                    if n == 0 || n > 20000 {
                        return Err(Error::invalid_params("lines must be 1–20000").into());
                    }
                    let text = sill_core::session::read_log(&sill_core::session::log_source(&a.session_id), n as usize);
                    if serde_json::to_vec(&text)?.len() > 4 * 1024 * 1024 {
                        return Err(sill_proto::rpc::Error::limit(
                            sill_proto::rpc::LimitScope::Result,
                            Some(4194304),
                            "log exceeds 4 MiB; request fewer lines",
                        )
                        .into());
                    }
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
                            self.ensure_runner(&a.session_id, false)?;
                            Ok(json!(api::Started { session_id: a.session_id, fresh: s.fresh }))
                        }
                        "_sill/sessions.stop" => {
                            crate::daemon::host_control::session_ended(self, &a.session_id, "the session stopped");
                            self.stop_session(&a.session_id)?;
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
    fn public_pages_fill_across_worker_rows_without_losing_the_cursor() {
        let dir = std::env::temp_dir().join(format!("sill-review-pages-{}", crate::daemon::sfp::id()));
        let store = Store::open(&dir.join("store.db")).unwrap();
        store.conn().execute_batch("CREATE TABLE crosscheck_workers(session_id TEXT PRIMARY KEY,run_id TEXT NOT NULL)").unwrap();
        let mut ordinary = Vec::new();
        for i in 0..135 {
            let session = store.create("test", "default").unwrap();
            store.conn().execute("UPDATE sessions SET created_at=?1 WHERE id=?2", rusqlite::params![i, session.id]).unwrap();
            if i >= 7 {
                store.conn().execute("INSERT INTO crosscheck_workers VALUES (?1,'run')", [&session.id]).unwrap();
            } else {
                ordinary.push(session.id);
            }
        }
        ordinary.reverse();
        let first = public_page(&store, None, 4).unwrap();
        assert_eq!(first.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), ordinary[..4]);
        let cursor = first.last().unwrap();
        let second = public_page(&store, Some((cursor.created_at, &cursor.id)), 4).unwrap();
        assert_eq!(second.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), ordinary[4..]);
        assert!(public_page(&store, Some((0, &ordinary[6])), 4).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remember_pins_inherits_session_pins_only_and_preserves_retries() {
        let dir = std::env::temp_dir().join(format!("sill-remember-pins-{}", crate::daemon::sfp::id()));
        let store = Store::open(&dir.join("store.db")).unwrap();
        store.global_pin(std::path::Path::new("/shortcut")).unwrap();
        let input =
            |id: &str| api::Create { create_id: id.into(), agent: "test".into(), policy: "default".into(), workspaces: Some(false), ..Default::default() };
        let source = create(&store, input("source")).unwrap();
        store.pin(&source.session_id, std::path::Path::new("/remembered")).unwrap();
        let empty = create(&store, input("off")).unwrap();
        assert!(store.pins(&empty.session_id).unwrap().is_empty());
        let mut remembered = input("on");
        remembered.remember_pins = true;
        let inherited = create(&store, remembered.clone()).unwrap();
        assert_eq!(store.pins(&inherited.session_id).unwrap(), vec![std::path::PathBuf::from("/remembered")]);
        let mut explicit = input("explicit");
        explicit.remember_pins = true;
        explicit.pins = vec![api::HostPath::from_path(std::path::Path::new("/explicit"))];
        let own = create(&store, explicit).unwrap();
        assert_eq!(store.pins(&own.session_id).unwrap(), vec![std::path::PathBuf::from("/explicit")]);
        assert_eq!(create(&store, remembered).unwrap().session_id, inherited.session_id);
        let mut ghost = input("ghost");
        ghost.remember_pins = true;
        ghost.ghost = true;
        let ghost = create(&store, ghost).unwrap();
        assert!(store.pins(&ghost.session_id).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn create_fingerprint_survives_new_optional_fields() {
        // A later sfp/1 build may add a create option; an old client leaves it out and an
        // identical retry must still match the receipt written before the field existed.
        let input = api::Create { create_id: "id".into(), agent: "test".into(), policy: "default".into(), ..Default::default() };
        let today = sill_core::sfp::canonical_fingerprint(&serde_json::to_value(&input).unwrap());
        let mut later = serde_json::to_value(&input).unwrap();
        later["worktree"] = serde_json::json!(false);
        later["kind"] = serde_json::json!("");
        later["lineage"] = serde_json::json!({"parent": null, "grants": []});
        assert_eq!(sill_core::sfp::canonical_fingerprint(&later), today);
        later["worktree"] = serde_json::json!(true);
        assert_ne!(sill_core::sfp::canonical_fingerprint(&later), today, "a set field is input");
        let mut reordered = serde_json::Map::new();
        for (k, v) in serde_json::to_value(&input).unwrap().as_object().unwrap().iter().rev() {
            reordered.insert(k.clone(), v.clone());
        }
        assert_eq!(sill_core::sfp::canonical_fingerprint(&serde_json::Value::Object(reordered)), today);
    }

    #[test]
    fn full_create_ledger_preserves_retries_conflicts_and_tombstones_after_reopen() {
        let dir = std::env::temp_dir().join(format!("sill-create-capacity-{}", crate::daemon::sfp::id()));
        let path = dir.join("store.db");
        let store = Store::open(&path).unwrap();
        let input = api::Create { create_id: "durable".into(), agent: "test".into(), policy: "default".into(), ..Default::default() };
        let first = create(&store, input.clone()).unwrap();
        // Fill the remaining slots with tombstones, without creating 65k sessions.
        store
            .conn()
            .execute_batch(
                "WITH RECURSIVE n(x) AS (
            VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<65535
        ) INSERT INTO sfp_creates SELECT 'fixture-' || x, 'fixture', NULL FROM n;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let code = |result: Result<api::SessionKey>| {
            let error = result.err().expect("request must fail");
            failure(error).data.unwrap()["code"].as_str().unwrap().to_owned()
        };
        let mut fresh = input.clone();
        fresh.create_id = "new-id".into();
        let error = failure(create(&store, fresh.clone()).err().unwrap());
        assert_eq!(error.data.as_ref().unwrap()["code"], "limit_exceeded");
        assert_eq!(error.data.unwrap()["details"]["scope"], "capacity");
        assert_eq!(create(&store, input.clone()).unwrap().session_id, first.session_id);
        let mut conflict = input.clone();
        conflict.agent = "different".into();
        assert_eq!(code(create(&store, conflict.clone())), "id_conflict");
        assert_eq!(store.sessions().unwrap().len(), 1);
        store.delete(&first.session_id).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(code(create(&store, input)), "gone");
        assert_eq!(code(create(&store, conflict)), "id_conflict");
        assert_eq!(code(create(&store, fresh)), "limit_exceeded");
        assert!(store.sessions().unwrap().is_empty());
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_creates", [], |r| r.get::<_, i64>(0)).unwrap(), 65536);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

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
        store.conn().execute_batch("CREATE TRIGGER reject_pin BEFORE INSERT ON session_pins BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        let mut failed = input.clone();
        failed.create_id = "rolled-back".into();
        assert!(create(&store, failed).is_err());
        assert_eq!(store.sessions().unwrap().len(), 1);
        assert_eq!(store.conn().query_row("SELECT count(*) FROM sfp_creates", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
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
