//! Host control activations (docs/permission-modes.md, Host control). An activation is the
//! person's explicit grant of named host operations to one running session, held here in
//! memory only: it is never saved, never restored by a reopen or a daemon recovery, and ends
//! when the person revokes it, when the last frontend displaying it goes, or when the session
//! stops. Every activation and scope change is a new generation; the bridge's actions carry the
//! generation they were made under and a stale one is refused. Activations and revocations are
//! recorded in the session's conversation as notices.
//!
//! Repository restrictions (docs/permission-modes.md, "Repository restrictions and Host
//! control"): host code is not bound by the jail, so operations that can bypass a pinned
//! repository's fragment need the person's separate exception, recorded first and consumed by
//! one activation. A base-policy ban has none.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde_json::json;
use sill_proto::host_control::{self as api, CAPABILITY, OPERATIONS};
use sill_proto::rpc::Error;

use super::{Conn, Hub};

struct Activation {
    generation: u64,
    operations: BTreeSet<String>,
    since: i64,
    /// The connection that activated: displaying for as long as it lives, attached or not.
    activator: u64,
    /// The repository exception this generation runs under.
    exception: Option<api::Exception>,
}

/// An exception recorded for the session's next activation, by the connection that may use it.
struct Pending {
    conn: u64,
    exception: api::Exception,
}

#[derive(Default)]
struct Table {
    active: HashMap<String, Activation>,
    /// The last generation of each session this daemon gave, active or not.
    generations: HashMap<String, u64>,
    ended: HashMap<String, api::Ended>,
    pending: HashMap<String, Pending>,
}

fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn domain(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    sill_core::sfp::DomainError { code, message: message.into(), details: None }.into()
}

/// Connections that display `session`'s Host control: the activator, and every connection with
/// the capability attached to the session.
fn displaying(hub: &Hub, session: &str, activator: u64) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for conn in hub.connections() {
        let attached = hub.sfp.lock().unwrap().sessions_for(conn.id).iter().any(|s| s == session);
        if conn.id == activator || (attached && conn.has_capability(CAPABILITY)) {
            out.insert(conn.id);
        }
    }
    out
}

fn state_of(hub: &Hub, t: &Table, session: &str) -> api::State {
    let pending = t.pending.get(session).map(|p| p.exception.clone());
    match t.active.get(session) {
        Some(a) => api::State {
            session_id: session.into(),
            active: true,
            generation: a.generation,
            operations: a.operations.iter().cloned().collect(),
            displaying: displaying(hub, session, a.activator).len() as u32,
            since: Some(a.since.to_string()),
            // one recorded for a scope change shows over the one in use
            exception: pending.or_else(|| a.exception.clone()),
            ..Default::default()
        },
        None => api::State {
            session_id: session.into(),
            generation: t.generations.get(session).copied().unwrap_or(0),
            ended: t.ended.get(session).cloned(),
            exception: pending,
            ..Default::default()
        },
    }
}

/// The state, after checking that an active session's jail is still there.
pub fn state(hub: &Hub, session: &str) -> api::State {
    if table().lock().unwrap().active.contains_key(session) && !sill_core::session::is_running(session) {
        let _ = revoke(hub, session, "the session's jail is gone");
    }
    state_of(hub, &table().lock().unwrap(), session)
}

/// What stops or conditions an activation of a stored session: the base policy's ban, and the
/// repository restrictions host operations can bypass (or why they could not be read).
fn restrictions(row: &sill_core::store::Session, pins: &[std::path::PathBuf]) -> Result<(Option<String>, Result<Vec<api::Conflict>, String>)> {
    let home = sill_core::changes::home()?;
    let base = sill_core::session::session_base_policy(&row.policy, &home, row.ghost, Some(&row.agent))?;
    let forbidden = (base.permissions.host_control == sill_core::permissions::Rule::Forbidden)
        .then(|| "Host control is forbidden by the session's policy; no repository exception can lift that".to_string());
    let conflicts = sill_core::session::next_build_restrictions(row, &home, pins, &[])
        .and_then(|resolution| sill_core::policy_fragments::host_conflicts(&resolution))
        .map(|found| {
            found
                .into_iter()
                .map(|c| api::Conflict {
                    source: sill_proto::sessions::HostPath::from_path(&c.path),
                    sha256: c.sha256,
                    restriction: c.restriction.into(),
                    detail: c.detail,
                    consequence: c.consequence.into(),
                    operations: c.operations.into_iter().map(String::from).collect(),
                })
                .collect()
        })
        .map_err(|e| format!("{e:#}"));
    Ok((forbidden, conflicts))
}

/// The state with the session's ban and repository restrictions, for an answer to a frontend.
pub fn full_state(hub: &Hub, session: &str) -> Result<api::State> {
    let (row, pins) = super::admin::session_with_pins(hub, session)?;
    let mut s = state(hub, session);
    let (forbidden, conflicts) = restrictions(&row, &pins)?;
    s.forbidden = forbidden;
    match conflicts {
        Ok(conflicts) => s.conflicts = conflicts,
        Err(error) => s.conflicts_error = Some(error),
    }
    Ok(s)
}

/// For a session record: its active control, if any.
pub fn brief(session: &str) -> Option<api::Brief> {
    let t = table().lock().unwrap();
    t.active.get(session).map(|a| api::Brief {
        generation: a.generation,
        operations: a.operations.iter().cloned().collect(),
        exception: a.exception.is_some(),
    })
}

/// A list of restrictions for a notice: where, what, and what host code can do despite it.
fn described(conflicts: &[api::Conflict]) -> String {
    conflicts
        .iter()
        .map(|c| format!("{}: {} ({})", c.source.display, c.detail, c.consequence.trim_end_matches('.')))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The named operations, known and at least one.
fn operations_of(named: &[String]) -> Result<BTreeSet<String>> {
    let mut operations = BTreeSet::new();
    for op in named {
        if !OPERATIONS.contains(&op.as_str()) {
            return Err(Error::invalid_params(format!("unknown host operation {op:?}")).into());
        }
        operations.insert(op.clone());
    }
    if operations.is_empty() {
        return Err(Error::invalid_params("Host control needs at least one operation").into());
    }
    Ok(operations)
}

fn displaying_capability(conn: &Conn) -> Result<()> {
    if !conn.has_capability(CAPABILITY) {
        let mut error = Error::domain("unsupported", "Host control needs a frontend that displays it");
        error.data.as_mut().unwrap()["details"] = json!({"capability": CAPABILITY});
        return Err(error.into());
    }
    Ok(())
}

/// Record the person's repository exception for the session's next activation.
pub fn record_exception(hub: &Hub, conn: &Conn, a: api::RecordException) -> Result<api::State> {
    displaying_capability(conn)?;
    let operations = operations_of(&a.operations)?;
    let (row, pins) = super::admin::session_with_pins(hub, &a.session_id)?;
    if row.ghost {
        return Err(domain("permission_denied", "ghost sessions support Default only"));
    }
    if row.ended_at.is_some() {
        return Err(domain("invalid_state", "the session has ended"));
    }
    let (forbidden, conflicts) = restrictions(&row, &pins)?;
    if let Some(forbidden) = forbidden {
        return Err(domain("permission_denied", forbidden));
    }
    let conflicts = conflicts.map_err(|e| domain("invalid_state", format!("the repository restrictions could not be read: {e}")))?;
    let scope: Vec<String> = operations.iter().cloned().collect();
    let needed: Vec<api::Conflict> = conflicts.into_iter().filter(|c| c.applies(&scope)).collect();
    if needed.is_empty() {
        return Err(domain("invalid_state", "these operations meet no repository restriction; no exception is needed"));
    }
    let wanted: BTreeSet<api::Acknowledged> = needed.iter().map(api::Conflict::key).collect();
    let given: BTreeSet<api::Acknowledged> = a.acknowledged.iter().cloned().collect();
    if wanted != given {
        return Err(domain("invalid_state", "the acknowledged restrictions are not the ones these operations meet now (a fragment changed?); read the state and review them again"));
    }
    let _gate = super::host_bridge::gate();
    let text = format!(
        "Repository exception recorded for the next Host control activation ({}): {}. The jail keeps these restrictions; host code is not bound by them.",
        list(&operations),
        described(&needed)
    );
    table().lock().unwrap().pending.insert(
        a.session_id.clone(),
        Pending {
            conn: conn.id,
            exception: api::Exception { operations: scope, conflicts: needed, at: sill_core::store::now().to_string(), generation: None },
        },
    );
    eprintln!("sill daemon: session {}: {text}", a.session_id);
    note(hub, &a.session_id, text);
    drop(_gate);
    full_state(hub, &a.session_id)
}

/// A notice in the session's conversation, published to its frontends.
pub(super) fn note(hub: &Hub, session: &str, text: String) {
    let result = hub.with_store(|store| {
        let journal = sill_core::sfp::Journal::new(store)?;
        let tx = store.conn().unchecked_transaction()?;
        let record = journal.ensure(session, &super::sfp::id())?;
        let mut block = sill_proto::sfp::Content::text(text);
        block.extra.insert("severity".into(), json!("warning"));
        let events = journal.append(session, None, &[sill_proto::sfp::Event::ItemCreated {
            item: sill_proto::sfp::Item {
                item_id: super::sfp::id(), created_seq: String::new(), turn_id: None, submission_id: None,
                agent_generation_id: record.agent_generation_id, kind: "notice".into(), content: vec![block], tool: None,
            },
        }])?;
        tx.commit()?;
        hub.sfp_publish(session, &events);
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("sill daemon: session {session}: recording a Host control notice: {e:#}");
    }
}

fn list(operations: &BTreeSet<String>) -> String {
    operations.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// Activate, or change the scope of an active control. The connection must display Host control.
pub fn activate(hub: &std::sync::Arc<Hub>, conn: &Conn, a: api::Activate) -> Result<api::State> {
    displaying_capability(conn)?;
    let operations = operations_of(&a.operations)?;
    let (row, pins) = super::admin::session_with_pins(hub, &a.session_id)?;
    if row.ghost {
        return Err(domain("permission_denied", "ghost sessions support Default only"));
    }
    if row.ended_at.is_some() {
        return Err(domain("invalid_state", "the session has ended"));
    }
    let (forbidden, conflicts) = restrictions(&row, &pins)?;
    if let Some(forbidden) = forbidden {
        return Err(domain("permission_denied", forbidden));
    }
    let conflicts = conflicts.map_err(|e| domain("invalid_state", format!("the repository restrictions could not be read: {e}")))?;
    if !sill_core::session::is_running(&a.session_id) {
        return Err(domain("invalid_state", "the session is not running; Host control needs its jail"));
    }
    let scope: Vec<String> = operations.iter().cloned().collect();
    let needed: Vec<api::Conflict> = conflicts.into_iter().filter(|c| c.applies(&scope)).collect();
    let _gate = super::host_bridge::gate();
    // the exception before the bridge: a refused scope change leaves the running one untouched
    let exception = if needed.is_empty() {
        None
    } else {
        let t = table().lock().unwrap();
        let covered = t.pending.get(&a.session_id).filter(|p| p.conn == conn.id).is_some_and(|p| {
            let acknowledged: BTreeSet<api::Acknowledged> = p.exception.conflicts.iter().map(api::Conflict::key).collect();
            needed.iter().all(|c| acknowledged.contains(&c.key())) && scope.iter().all(|op| p.exception.operations.contains(op))
        });
        if !covered {
            return Err(sill_core::sfp::DomainError {
                code: "exception_required",
                message: format!(
                    "these operations can bypass repository restrictions; record an exception for them first: {}",
                    described(&needed)
                ),
                details: Some(json!({"conflicts": needed})),
            }
            .into());
        }
        Some(api::Exception { operations: scope.clone(), conflicts: needed.clone(), at: sill_core::store::now().to_string(), generation: None })
    };
    let listener = super::host_bridge::listen(&a.session_id)?;
    let (generation, changed) = {
        let mut t = table().lock().unwrap();
        let generation = t.generations.get(&a.session_id).copied().unwrap_or(0) + 1;
        let changed = t.active.get(&a.session_id).map(|old| old.operations.clone());
        t.generations.insert(a.session_id.clone(), generation);
        t.ended.remove(&a.session_id);
        // consumed, or not needed: an exception serves one activation
        t.pending.remove(&a.session_id);
        let exception = exception.clone().map(|e| api::Exception { generation: Some(generation), ..e });
        t.active.insert(a.session_id.clone(), Activation { generation, operations: operations.clone(), since: sill_core::store::now(), activator: conn.id, exception });
        (generation, changed)
    };
    super::host_bridge::serve(hub.clone(), a.session_id.clone(), generation, listener);
    let mut text = match changed {
        Some(old) if old == operations => format!("Host control confirmed (generation {generation}): {}.", list(&operations)),
        Some(old) => format!("Host control scope changed (generation {generation}): {}; before: {}.", list(&operations), list(&old)),
        None => format!("Host control activated (generation {generation}): {}. Actions can run outside the jail.", list(&operations)),
    };
    if let Some(e) = &exception {
        text.push_str(&format!(" Under a repository exception for this generation only: {}.", described(&e.conflicts)));
    }
    eprintln!("sill daemon: session {}: {text}", a.session_id);
    note(hub, &a.session_id, text);
    drop(_gate);
    full_state(hub, &a.session_id)
}

/// End an activation for `reason`; a no-op without one.
/// The reason recorded (state, log) when the last frontend displaying an activation left.
const LAST_DISPLAYING_LEFT: &str = "the last frontend displaying it left";

pub fn revoke(hub: &Hub, session: &str, reason: &str) -> Result<api::State> {
    let _gate = super::host_bridge::gate();
    let (ended, under_exception, withdrawn) = {
        let mut t = table().lock().unwrap();
        // a recorded exception goes too: after revocation the conflict needs a fresh look
        let withdrawn = t.pending.remove(session).is_some();
        let ended = t.active.remove(session);
        if ended.is_some() {
            t.ended.insert(session.into(), api::Ended { reason: reason.into(), at: sill_core::store::now().to_string() });
        }
        (ended.is_some(), ended.is_some_and(|a| a.exception.is_some()), withdrawn)
    };
    if ended {
        super::host_bridge::cleanup(hub, session);
        eprintln!("sill daemon: session {session}: Host control revoked: {reason}");
        let exception = if under_exception { " Its repository exception ends with it." } else { "" };
        // the ordinary end (the window that showed it is gone) needs no explanation in the conversation
        let why = if reason == LAST_DISPLAYING_LEFT { String::new() } else { format!(": {reason}") };
        note(hub, session, format!("Host control revoked{why}.{exception}"));
    } else if withdrawn {
        note(hub, session, format!("Repository exception withdrawn before use: {reason}."));
    }
    Ok(state_of(hub, &table().lock().unwrap(), session))
}

/// A connection went or detached from a session: activations nobody displays any more end.
pub fn displaying_changed(hub: &Hub) {
    // an exception recorded by a connection that left is nobody's to use
    let live: BTreeSet<u64> = hub.connections().iter().map(|c| c.id).collect();
    let orphaned: Vec<String> = {
        let t = table().lock().unwrap();
        t.pending.iter().filter(|(_, p)| !live.contains(&p.conn)).map(|(s, _)| s.clone()).collect()
    };
    for session in orphaned {
        let removed = {
            let mut t = table().lock().unwrap();
            let gone = t.pending.get(&session).is_some_and(|p| !live.contains(&p.conn));
            if gone {
                t.pending.remove(&session);
            }
            gone
        };
        if removed {
            note(hub, &session, "Repository exception withdrawn before use: the frontend that recorded it left.".into());
        }
    }
    let active: Vec<(String, u64)> = table().lock().unwrap().active.iter().map(|(s, a)| (s.clone(), a.activator)).collect();
    for (session, activator) in active {
        if displaying(hub, &session, activator).is_empty() {
            let _ = revoke(hub, &session, LAST_DISPLAYING_LEFT);
        }
    }
}

/// The session stopped, ended or was deleted.
pub fn session_ended(hub: &Hub, session: &str, reason: &str) {
    let t = table().lock().unwrap();
    let involved = t.active.contains_key(session) || t.pending.contains_key(session);
    drop(t);
    if involved {
        let _ = revoke(hub, session, reason);
    }
}

/// The minute sweep: activations whose jail died, or whose frontends are gone, end.
pub fn sweep(hub: &Hub) {
    let sessions: Vec<String> = table().lock().unwrap().active.keys().cloned().collect();
    for session in sessions {
        if !sill_core::session::is_running(&session) {
            let _ = revoke(hub, &session, "the session's jail is gone");
        }
    }
    displaying_changed(hub);
}

pub fn handle(hub: &std::sync::Arc<Hub>, conn: &Conn, method: &str, params: serde_json::Value) -> Option<Result<serde_json::Value>> {
    let args = |v: serde_json::Value| serde_json::from_value::<sill_proto::sessions::SessionKey>(v).map_err(|e| anyhow::Error::from(Error::invalid_params(e.to_string())));
    Some(match method {
        "_sill/host_control.activate" => serde_json::from_value::<api::Activate>(params)
            .map_err(|e| anyhow::Error::from(Error::invalid_params(e.to_string())))
            .and_then(|a| activate(hub, conn, a))
            .map(|s| json!(s)),
        "_sill/host_control.exception" => serde_json::from_value::<api::RecordException>(params)
            .map_err(|e| anyhow::Error::from(Error::invalid_params(e.to_string())))
            .and_then(|a| record_exception(hub, conn, a))
            .map(|s| json!(s)),
        "_sill/host_control.revoke" => args(params).and_then(|a| {
            displaying_capability(conn)?;
            super::admin::session_with_pins(hub, &a.session_id)?;
            revoke(hub, &a.session_id, "revoked by the person")?;
            full_state(hub, &a.session_id).map(|s| json!(s))
        }),
        "_sill/host_control.keep" => serde_json::from_value::<api::Keep>(params).map_err(anyhow::Error::from).and_then(|a| {
            if !conn.has_capability(CAPABILITY) { return Err(domain("unsupported", "Host control needs a displaying frontend")); }
            let _gate = super::host_bridge::gate();
            let id = a.name.strip_prefix("host:").ok_or_else(|| domain("invalid_params", "expected host:<id>"))?;
            hub.with_store(|s| {
                let p = s.host_processes(&a.session_id)?.into_iter().find(|p| p.id == id).ok_or_else(|| domain("not_found", "host process not found"))?;
                s.keep_host_process(&p.id, a.keep)?; Ok(())
            })?;
            super::host_bridge::publish(hub, &a.session_id);
            note(hub, &a.session_id, format!("Host process {}: keep after revocation = {}", a.name, a.keep));
            Ok(json!({}))
        }),
        "_sill/host_control.state" => args(params).and_then(|a| {
            if !conn.has_capability("sill.permission_modes") {
                let mut error = Error::domain("unsupported", "Host control state needs a frontend that shows permission state (session records carry the active brief)");
                error.data.as_mut().unwrap()["details"] = json!({"capability": "sill.permission_modes"});
                return Err(error.into());
            }
            full_state(hub, &a.session_id).map(|s| json!(s))
        }),
        _ => return None,
    })
}

pub fn shutdown(hub: &Hub) {
    let sessions: Vec<_> = table().lock().unwrap().active.keys().cloned().collect();
    for session in sessions { let _ = revoke(hub, &session, "daemon shutdown"); }
}
