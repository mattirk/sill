//! Host control activations (docs/permission-modes.md, Host control). An activation is the
//! person's explicit grant of named host operations to one running session, held here in
//! memory only: it is never saved, never restored by a reopen or a daemon recovery, and ends
//! when the person revokes it, when the last frontend displaying it goes, or when the session
//! stops. Every activation and scope change is a new generation; the bridge's actions carry the
//! generation they were made under and a stale one is refused. Activations and revocations are
//! recorded in the session's conversation as notices.
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
}

#[derive(Default)]
struct Table {
    active: HashMap<String, Activation>,
    /// The last generation of each session this daemon gave, active or not.
    generations: HashMap<String, u64>,
    ended: HashMap<String, api::Ended>,
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
    match t.active.get(session) {
        Some(a) => api::State {
            session_id: session.into(),
            active: true,
            generation: a.generation,
            operations: a.operations.iter().cloned().collect(),
            displaying: displaying(hub, session, a.activator).len() as u32,
            since: Some(a.since.to_string()),
            ended: None,
        },
        None => api::State {
            session_id: session.into(),
            generation: t.generations.get(session).copied().unwrap_or(0),
            ended: t.ended.get(session).cloned(),
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

/// For a session record: its active control, if any.
pub fn brief(session: &str) -> Option<api::Brief> {
    let t = table().lock().unwrap();
    t.active.get(session).map(|a| api::Brief { generation: a.generation, operations: a.operations.iter().cloned().collect() })
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
    if !conn.has_capability(CAPABILITY) {
        let mut error = Error::domain("unsupported", "Host control needs a frontend that displays it");
        error.data.as_mut().unwrap()["details"] = json!({"capability": CAPABILITY});
        return Err(error.into());
    }
    let mut operations = BTreeSet::new();
    for op in &a.operations {
        if !OPERATIONS.contains(&op.as_str()) {
            return Err(Error::invalid_params(format!("unknown host operation {op:?}")).into());
        }
        operations.insert(op.clone());
    }
    if operations.is_empty() {
        return Err(Error::invalid_params("Host control needs at least one operation").into());
    }
    let (row, pins) = super::admin::session_with_pins(hub, &a.session_id)?;
    if row.ghost {
        return Err(domain("permission_denied", "ghost sessions support Default only"));
    }
    if row.ended_at.is_some() {
        return Err(domain("invalid_state", "the session has ended"));
    }
    let restrictions = sill_core::session::next_build_restrictions(&row, &sill_core::changes::home()?, &pins, &[])?;
    if restrictions.policy.permissions.host_control == sill_core::permissions::Rule::Forbidden {
        return Err(domain("permission_denied", "Host control is forbidden by the policy or a repository fragment"));
    }
    if !sill_core::session::is_running(&a.session_id) {
        return Err(domain("invalid_state", "the session is not running; Host control needs its jail"));
    }
    let _gate = super::host_bridge::gate();
    let listener = super::host_bridge::listen(&a.session_id)?;
    let (generation, changed) = {
        let mut t = table().lock().unwrap();
        let generation = t.generations.get(&a.session_id).copied().unwrap_or(0) + 1;
        let changed = t.active.get(&a.session_id).map(|old| old.operations.clone());
        t.generations.insert(a.session_id.clone(), generation);
        t.ended.remove(&a.session_id);
        t.active.insert(a.session_id.clone(), Activation { generation, operations: operations.clone(), since: sill_core::store::now(), activator: conn.id });
        (generation, changed)
    };
    super::host_bridge::serve(hub.clone(), a.session_id.clone(), generation, listener);
    let text = match changed {
        Some(old) if old == operations => format!("Host control confirmed (generation {generation}): {}.", list(&operations)),
        Some(old) => format!("Host control scope changed (generation {generation}): {}; before: {}.", list(&operations), list(&old)),
        None => format!("Host control activated (generation {generation}): {}. Actions can run outside the jail.", list(&operations)),
    };
    eprintln!("sill daemon: session {}: {text}", a.session_id);
    note(hub, &a.session_id, text);
    drop(_gate);
    Ok(state(hub, &a.session_id))
}

/// End an activation for `reason`; a no-op without one.
pub fn revoke(hub: &Hub, session: &str, reason: &str) -> Result<api::State> {
    let _gate = super::host_bridge::gate();
    let ended = {
        let mut t = table().lock().unwrap();
        let ended = t.active.remove(session).is_some();
        if ended {
            t.ended.insert(session.into(), api::Ended { reason: reason.into(), at: sill_core::store::now().to_string() });
        }
        ended
    };
    if ended {
        super::host_bridge::cleanup(hub, session);
        eprintln!("sill daemon: session {session}: Host control revoked: {reason}");
        note(hub, session, format!("Host control revoked: {reason}."));
    }
    Ok(state_of(hub, &table().lock().unwrap(), session))
}

/// A connection went or detached from a session: activations nobody displays any more end.
pub fn displaying_changed(hub: &Hub) {
    let active: Vec<(String, u64)> = table().lock().unwrap().active.iter().map(|(s, a)| (s.clone(), a.activator)).collect();
    for (session, activator) in active {
        if displaying(hub, &session, activator).is_empty() {
            let _ = revoke(hub, &session, "the last frontend displaying it left");
        }
    }
}

/// The session stopped, ended or was deleted.
pub fn session_ended(hub: &Hub, session: &str, reason: &str) {
    if table().lock().unwrap().active.contains_key(session) {
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
        "_sill/host_control.revoke" => args(params).and_then(|a| {
            if !conn.has_capability(CAPABILITY) {
                let mut error = Error::domain("unsupported", "Host control needs a frontend that displays it");
                error.data.as_mut().unwrap()["details"] = json!({"capability": CAPABILITY});
                return Err(error.into());
            }
            super::admin::session_with_pins(hub, &a.session_id)?;
            revoke(hub, &a.session_id, "revoked by the person").map(|s| json!(s))
        }),
        "_sill/host_control.keep" => serde_json::from_value::<api::Keep>(params).map_err(anyhow::Error::from).and_then(|a| {
            if !conn.has_capability(CAPABILITY) { return Err(domain("unsupported", "Host control needs a displaying frontend")); }
            let _gate = super::host_bridge::gate();
            let id = a.name.strip_prefix("host:").ok_or_else(|| domain("invalid_params", "expected host:<id>"))?;
            hub.with_store(|s| {
                let p = s.host_processes(&a.session_id)?.into_iter().find(|p| p.id == id).ok_or_else(|| domain("not_found", "host process not found"))?;
                s.keep_host_process(&p.id, a.keep)?; Ok(())
            })?;
            note(hub, &a.session_id, format!("Host process {}: keep after revocation = {}", a.name, a.keep));
            Ok(json!({}))
        }),
        "_sill/host_control.state" => args(params).and_then(|a| {
            if !conn.has_capability("sill.permission_modes") {
                let mut error = Error::domain("unsupported", "Host control state needs a frontend that shows permission state (session records carry the active brief)");
                error.data.as_mut().unwrap()["details"] = json!({"capability": "sill.permission_modes"});
                return Err(error.into());
            }
            super::admin::session_with_pins(hub, &a.session_id)?;
            Ok(json!(state(hub, &a.session_id)))
        }),
        _ => return None,
    })
}

pub fn shutdown(hub: &Hub) {
    let sessions: Vec<_> = table().lock().unwrap().active.keys().cloned().collect();
    for session in sessions { let _ = revoke(hub, &session, "daemon shutdown"); }
}
