//! Storage administration stays outside jails. Cleanup is serialized with jail startup
//! only for the inactivity check and rename, never for the potentially slow removal.
use super::Hub;
use anyhow::{Result, ensure};
use serde_json::Value;
use sill_proto::method;
use sill_proto::storage as api;

fn root() -> std::path::PathBuf {
    std::path::Path::new(sill_core::jail::STATE_FS).join(rustix::process::getuid().as_raw().to_string()).join("sessions")
}
fn active(hub: &Hub, id: &str) -> Result<bool> {
    if hub.children.lock().unwrap().contains_key(id) || sill_core::session::is_running(id) {
        return Ok(true);
    }
    let rt = sill_core::shim::runtime_dir(id)?;
    use sill_core::launcher::{SessionProcess, session_pid};
    Ok(session_pid(&rt.join("launch.pid"), id, SessionProcess::Launcher).is_some() || session_pid(&rt.join("bwrap.pid"), id, SessionProcess::Bwrap).is_some())
}
fn caches(hub: &Hub) -> Result<api::Caches> {
    let sessions = hub.with_store(|s| s.sessions())?;
    let root = root();
    let mut caches = Vec::new();
    for session in sessions {
        let measured = sill_core::storage::cache_bytes(&root, &session.id);
        if matches!(measured, Ok(0)) {
            continue;
        }
        let (bytes, problem) = match measured {
            Ok(bytes) => (bytes, None),
            Err(e) => (0, Some(format!("Could not measure this cache: {e:#}"))),
        };
        let active = active(hub, &session.id)?;
        caches.push(api::BuildCache {
            path: sill_proto::sessions::HostPath::from_path(&root.join(&session.id).join(sill_core::storage::BUILD_PARENT).join("target")),
            session_id: session.id,
            title: session.title,
            bytes,
            active,
            problem,
        });
    }
    caches.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    Ok(api::Caches { caches })
}
fn clear(hub: &Hub, id: &str) -> Result<()> {
    // Avoid simultaneous clears of the same detached cache. No session is stopped.
    static CLEARING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _clearing = CLEARING.try_lock().map_err(|_| anyhow::anyhow!("Another cache cleanup is under way."))?;
    let detached = {
        let _starting = hub.starting.lock().unwrap();
        ensure!(sill_core::session::valid_id(id), "invalid session id");
        ensure!(hub.with_store(|s| Ok(s.sessions()?.iter().any(|s| s.id == id)))?, "Unknown session");
        ensure!(!active(hub, id)?, "This session is running. Its cache was not cleared.");
        sill_core::storage::detach(&root(), id)?
    };
    if let Some(parent) = detached {
        sill_core::storage::remove_detached(&parent)?;
    }
    Ok(())
}

/// The requests answered here, each named once (`HANDLERS` in hub.rs holds every such list).
pub(super) const METHODS: &[&str] = &[method::STORAGE_SPACE, method::STORAGE_CACHES, method::STORAGE_CLEAR_BUILD_CACHE];
pub(super) fn handle(hub: &Hub, conn: &super::Conn, method: &str, params: Value) -> Option<Result<Value>> {
    if !METHODS.contains(&method) {
        return None;
    }
    if !conn.has_capability("sill.storage") {
        return Some(Err(sill_proto::rpc::Error::needs_capability("sill.storage", "Storage administration needs sill.storage").into()));
    }
    Some((|| match method {
        method::STORAGE_SPACE => {
            let _: sill_proto::Empty = serde_json::from_value(params).map_err(sill_proto::rpc::Error::invalid_params)?;
            Ok(serde_json::to_value(api::Space {
                home: sill_core::storage::space(&hub.home)?,
                state: sill_core::storage::space(std::path::Path::new(sill_core::jail::STATE_FS))?,
            })?)
        }
        method::STORAGE_CACHES => {
            let _: sill_proto::Empty = serde_json::from_value(params).map_err(sill_proto::rpc::Error::invalid_params)?;
            Ok(serde_json::to_value(caches(hub)?)?)
        }
        method::STORAGE_CLEAR_BUILD_CACHE => {
            let args: api::ClearBuildCache = serde_json::from_value(params).map_err(sill_proto::rpc::Error::invalid_params)?;
            clear(hub, &args.session_id)?;
            Ok(serde_json::json!({}))
        }
        _ => unreachable!(),
    })())
}
