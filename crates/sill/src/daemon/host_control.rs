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
            ..Default::default()
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

fn session_row(hub: &Hub, session: &str) -> Result<sill_core::store::Session> {
    hub.with_store(|store| store.session_exact(session))?.ok_or_else(|| domain("not_found", "session not found"))
}

/// What stops an activation of a stored session whatever its state: its policy's ban.
fn forbidden(row: &sill_core::store::Session) -> Result<Option<String>> {
    let home = sill_core::changes::home()?;
    let base = sill_core::session::session_base_policy(&row.policy, &home, row.ghost, Some(&row.agent))?;
    Ok((base.permissions.host_control == sill_core::permissions::Rule::Forbidden).then(|| "Host control is forbidden by the session's policy".to_string()))
}

/// The state with the session policy's ban, for an answer to a frontend.
pub fn full_state(hub: &Hub, session: &str) -> Result<api::State> {
    let row = session_row(hub, session)?;
    let mut s = state(hub, session);
    s.forbidden = forbidden(&row)?;
    Ok(s)
}

/// For a session record: its active control, if any.
pub fn brief(session: &str) -> Option<api::Brief> {
    let t = table().lock().unwrap();
    t.active.get(session).map(|a| api::Brief { generation: a.generation, operations: a.operations.iter().cloned().collect() })
}

/// The named operations, known and at least one.
fn operations_of(named: &[String]) -> Result<BTreeSet<String>> {
    let mut operations = BTreeSet::new();
    for op in named {
        if !OPERATIONS.contains(&op.as_str()) {
            return Err(Error::invalid_params(format!("unknown host operation {op:?}")).into());
        }
        // the Mac's operations only where there is a Mac: a daemon in Sill.app's VM
        if api::MAC_OPERATIONS.contains(&op.as_str()) && !super::host_mac::attached() {
            return Err(Error::invalid_params(format!("{op} needs Sill.app on a Mac: this daemon does not run in its VM")).into());
        }
        operations.insert(op.clone());
    }
    if operations.is_empty() {
        return Err(Error::invalid_params("Host control needs at least one operation").into());
    }
    // the agent aims the Mac's input in pictures it has taken: control comes with viewing
    if operations.contains("mac_input") && !operations.contains("mac_view") {
        return Err(Error::invalid_params("mac_input needs mac_view: input is aimed in a picture of the Mac").into());
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

/// A notice in the session's conversation, published to its frontends.
pub(super) fn note(hub: &Hub, session: &str, text: String) {
    let result = hub.with_store(|store| {
        let journal = sill_core::sfp::Journal::new(store)?;
        let tx = store.conn().unchecked_transaction()?;
        let record = journal.ensure(session, &super::sfp::id())?;
        let mut block = sill_proto::sfp::Content::text(text);
        block.extra.insert("severity".into(), json!("warning"));
        let events = journal.append(
            session,
            None,
            &[sill_proto::sfp::Event::ItemCreated {
                item: sill_proto::sfp::Item {
                    item_id: super::sfp::id(),
                    created_seq: String::new(),
                    turn_id: None,
                    submission_id: None,
                    agent_generation_id: record.agent_generation_id,
                    kind: "notice".into(),
                    content: vec![block],
                    tool: None,
                },
            }],
        )?;
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
    let row = session_row(hub, &a.session_id)?;
    if row.ghost {
        return Err(domain("permission_denied", "ghost sessions support Default only"));
    }
    if row.ended_at.is_some() {
        return Err(domain("invalid_state", "the session has ended"));
    }
    if let Some(forbidden) = forbidden(&row)? {
        return Err(domain("permission_denied", forbidden));
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
    full_state(hub, &a.session_id)
}

/// End an activation for `reason`; a no-op without one.
/// The reason recorded (state, log) when the last frontend displaying an activation left.
const LAST_DISPLAYING_LEFT: &str = "the last frontend displaying it left";

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
        // the ordinary end (the window that showed it is gone) needs no explanation in the conversation
        let why = if reason == LAST_DISPLAYING_LEFT { String::new() } else { format!(": {reason}") };
        note(hub, session, format!("Host control revoked{why}."));
    }
    Ok(state_of(hub, &table().lock().unwrap(), session))
}

/// A connection went or detached from a session: activations nobody displays any more end.
pub fn displaying_changed(hub: &Hub) {
    let active: Vec<(String, u64)> = table().lock().unwrap().active.iter().map(|(s, a)| (s.clone(), a.activator)).collect();
    for (session, activator) in active {
        if displaying(hub, &session, activator).is_empty() {
            let _ = revoke(hub, &session, LAST_DISPLAYING_LEFT);
        }
    }
}

/// The session stopped, ended or was deleted.
pub fn session_ended(hub: &Hub, session: &str, reason: &str) {
    let active = table().lock().unwrap().active.contains_key(session);
    if active {
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
    let args = |v: serde_json::Value| {
        serde_json::from_value::<sill_proto::sessions::SessionKey>(v).map_err(|e| anyhow::Error::from(Error::invalid_params(e.to_string())))
    };
    Some(match method {
        "_sill/host_control.activate" => serde_json::from_value::<api::Activate>(params)
            .map_err(|e| anyhow::Error::from(Error::invalid_params(e.to_string())))
            .and_then(|a| activate(hub, conn, a))
            .map(|s| json!(s)),
        "_sill/host_control.revoke" => args(params).and_then(|a| {
            displaying_capability(conn)?;
            session_row(hub, &a.session_id)?;
            revoke(hub, &a.session_id, "revoked by the person")?;
            full_state(hub, &a.session_id).map(|s| json!(s))
        }),
        "_sill/host_control.keep" => serde_json::from_value::<api::Keep>(params).map_err(anyhow::Error::from).and_then(|a| {
            if !conn.has_capability(CAPABILITY) {
                return Err(domain("unsupported", "Host control needs a displaying frontend"));
            }
            let _gate = super::host_bridge::gate();
            let id = a.name.strip_prefix("host:").ok_or_else(|| domain("invalid_params", "expected host:<id>"))?;
            hub.with_store(|s| {
                let p = s.host_processes(&a.session_id)?.into_iter().find(|p| p.id == id).ok_or_else(|| domain("not_found", "host process not found"))?;
                s.keep_host_process(&p.id, a.keep)?;
                Ok(())
            })?;
            super::host_bridge::publish(hub, &a.session_id);
            note(hub, &a.session_id, format!("Host process {}: keep after revocation = {}", a.name, a.keep));
            Ok(json!({}))
        }),
        "_sill/host_control.state" => args(params).and_then(|a| {
            if !conn.has_capability("sill.permission_modes") {
                let mut error =
                    Error::domain("unsupported", "Host control state needs a frontend that shows permission state (session records carry the active brief)");
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
    for session in sessions {
        let _ = revoke(hub, &session, "daemon shutdown");
    }
    // A revocation already under way (a frontend that closed just before) has taken its
    // activation out of the table, so it is not in the list above, and holds the gate until its
    // host processes are stopped: wait for it, or the exit that follows cuts it off and leaves
    // them running. The wait comes last: one that began after an earlier wait would be missed.
    drop(super::host_bridge::gate());
}

#[cfg(test)]
mod mac_forward_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn delayed_mac_reply_does_not_block_revocation_or_other_sessions() {
        // A channel stands in for a Mac that has accepted a request but not replied.
        // Exercise both revocation and scope changes while the reply is outstanding.
        for replacement in [None, Some(2)] {
            let session = format!("test-mac-forward-{replacement:?}");
            {
                let _gate = super::super::host_bridge::gate();
                table()
                    .lock()
                    .unwrap()
                    .active
                    .insert(session.clone(), Activation { generation: 1, operations: BTreeSet::from(["mac_view".into()]), since: 0, activator: 0 });
            }
            let (started_tx, started_rx) = mpsc::channel();
            let (reply_tx, reply_rx) = mpsc::channel();
            let worker_session = session.clone();
            let worker = std::thread::spawn(move || {
                let held = super::super::host_bridge::gate();
                super::super::host_bridge::forward_mac(held, &worker_session, 1, || {
                    started_tx.send(()).unwrap();
                    reply_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .map(|_| ())
                .map_err(|e| e.to_string())
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let (changed_tx, changed_rx) = mpsc::channel();
            let change_session = session.clone();
            let change = std::thread::spawn(move || {
                // This is the same gate and table used by revoke/activate and every
                // other session's bridge. It must be available before the reply.
                let _gate = super::super::host_bridge::gate();
                let mut t = table().lock().unwrap();
                let mut old = t.active.remove(&change_session).unwrap();
                if let Some(generation) = replacement {
                    old.generation = generation;
                    t.active.insert(change_session, old);
                }
                changed_tx.send(()).unwrap();
            });
            let changed = changed_rx.recv_timeout(Duration::from_secs(2));
            reply_tx.send(()).unwrap();
            change.join().unwrap();
            let result = worker.join().unwrap();
            table().lock().unwrap().active.remove(&session);
            assert!(changed.is_ok(), "lifecycle gate held during Mac IO");
            assert!(result.unwrap_err().contains("do not replay automatically"));
        }
    }
}
