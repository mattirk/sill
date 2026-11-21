//! Storage administration outside jails. Session reclaim serializes with startup and reopen.
use super::Hub;
use anyhow::{Result, ensure};
use serde_json::Value;
use silli_proto::method;
use silli_proto::storage as api;

fn root() -> std::path::PathBuf {
    std::path::Path::new(silli_core::jail::STATE_FS).join(rustix::process::getuid().as_raw().to_string()).join("sessions")
}
fn active(hub: &Hub, id: &str) -> Result<bool> {
    if hub.children.lock().unwrap().contains_key(id) || silli_core::session::is_running(id) {
        return Ok(true);
    }
    let rt = silli_core::shim::runtime_dir(id)?;
    use silli_core::launcher::{SessionProcess, session_pid};
    Ok(session_pid(&rt.join("launch.pid"), id, SessionProcess::Launcher).is_some() || session_pid(&rt.join("bwrap.pid"), id, SessionProcess::Bwrap).is_some())
}
fn caches(hub: &Hub) -> Result<api::Caches> {
    let sessions = hub.with_store(|s| s.sessions())?;
    let root = root();
    let mut caches = Vec::new();
    for session in sessions {
        let measured = silli_core::storage::cache_bytes(&root, &session.id);
        if matches!(measured, Ok(0)) {
            continue;
        }
        let (bytes, problem) = match measured {
            Ok(bytes) => (bytes, None),
            Err(e) => (0, Some(format!("Could not measure this cache: {e:#}"))),
        };
        let active = active(hub, &session.id)?;
        caches.push(api::BuildCache {
            path: silli_proto::sessions::HostPath::from_path(&root.join(&session.id).join(silli_core::storage::BUILD_PARENT).join("target")),
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
        ensure!(silli_core::session::valid_id(id), "invalid session id");
        ensure!(hub.with_store(|s| Ok(s.sessions()?.iter().any(|s| s.id == id)))?, "Unknown session");
        ensure!(!active(hub, id)?, "This session is running. Its cache was not cleared.");
        silli_core::storage::detach(&root(), id)?
    };
    if let Some(parent) = detached {
        silli_core::storage::remove_detached(&parent)?;
    }
    Ok(())
}

fn reclaim_blocker(hub: &Hub, session: &silli_core::store::Session, workspaces: usize) -> Result<Option<String>> {
    if session.ended_at.is_none() || active(hub, &session.id)? {
        return Ok(Some("End this session before reclaiming its files.".into()));
    }
    if workspaces > 0 {
        return Ok(Some(format!("Review {workspaces} workspace(s) in Changes. Apply or discard their work first.")));
    }
    let pending = hub.session_pending(session, rustix::process::getuid().as_raw())?.into_iter().filter(|change| !change.ignored).count();
    Ok((pending > 0).then(|| format!("Review {pending} pending change(s) in Changes before reclaiming.")))
}

fn measure_directories(state: &std::path::Path, paths: &[std::path::PathBuf]) -> Result<Vec<(String, u64)>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // One invocation counts hard links only once across these directories.
    // Reviewed pins have their own kernel-owned overlay work directories.
    let staged_work = state.join("staged-pins/*/work");
    let output = std::process::Command::new("du").args(["--null", "-sx", "-B1", "--exclude"]).arg(staged_work).arg("--").args(paths).output()?;
    ensure!(output.status.success(), "Could not measure session files: {}", String::from_utf8_lossy(&output.stderr));
    let mut directories = Vec::new();
    for row in output.stdout.split(|byte| *byte == 0).filter(|row| !row.is_empty()) {
        let split = row.iter().position(|byte| *byte == b'\t').ok_or_else(|| anyhow::anyhow!("Missing directory size"))?;
        directories.push((String::from_utf8_lossy(&row[split + 1..]).into_owned(), std::str::from_utf8(&row[..split])?.parse()?));
    }
    Ok(directories)
}

fn session_usage(hub: &Hub, session: &silli_core::store::Session) -> Result<api::SessionUsage> {
    let mut paths = hub.with_store(|s| silli_core::workspace::records::storage_paths(s.conn(), &session.id))?;
    let workspaces = paths.len();
    let state = silli_core::session::state_dir_path(rustix::process::getuid().as_raw(), &session.id)?;
    if state.try_exists()? {
        for entry in std::fs::read_dir(&state)? {
            let entry = entry?;
            // Overlayfs owns its work directories. They contain kernel bookkeeping.
            if entry.file_name() != "work" {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    let directories = measure_directories(&state, &paths)?;
    let blocked = reclaim_blocker(hub, session, workspaces).unwrap_or_else(|e| Some(format!("{e:#}")));
    Ok(api::SessionUsage { session_id: session.id.clone(), title: session.title.clone(), directories, ended_at: session.ended_at, blocked })
}

fn usage(hub: &Hub) -> Result<Vec<api::SessionUsage>> {
    let mut rows = Vec::new();
    for session in hub.with_store(|s| s.sessions())? {
        match session_usage(hub, &session) {
            Ok(row) if !row.directories.is_empty() => rows.push(row),
            Ok(_) => (),
            Err(e) => rows.push(api::SessionUsage {
                session_id: session.id,
                title: session.title,
                directories: Vec::new(),
                ended_at: session.ended_at,
                blocked: Some(format!("{e:#}")),
            }),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.directories.iter().map(|(_, n)| n).sum::<u64>()));
    Ok(rows)
}

fn reclaim(hub: &Hub, args: api::Reclaim) -> Result<()> {
    let _starting = hub.starting.lock().unwrap();
    ensure!(silli_core::session::valid_id(&args.session_id), "Invalid session id");
    let session = hub.with_store(|s| s.sessions())?.into_iter().find(|s| s.id == args.session_id).ok_or_else(|| anyhow::anyhow!("Unknown session"))?;
    ensure!(session.ended_at == Some(args.ended_at), "Session changed since scan");
    if let Some(why) = session_usage(hub, &session)?.blocked {
        anyhow::bail!(why);
    }
    silli_core::session::remove_state(rustix::process::getuid().as_raw(), &session.id)
}

/// The requests answered here, each named once (`HANDLERS` in hub.rs holds every such list).
pub(super) const METHODS: &[&str] =
    &[method::STORAGE_SESSIONS, method::STORAGE_RECLAIM, method::STORAGE_SPACE, method::STORAGE_CACHES, method::STORAGE_CLEAR_BUILD_CACHE];
pub(super) fn handle(hub: &Hub, conn: &super::Conn, method: &str, params: Value) -> Option<Result<Value>> {
    if !METHODS.contains(&method) {
        return None;
    }
    if !conn.has_capability("silli.storage") {
        return Some(Err(silli_proto::rpc::Error::needs_capability("silli.storage", "Storage administration needs silli.storage").into()));
    }
    Some((|| match method {
        method::STORAGE_SESSIONS => {
            let _: silli_proto::Empty = serde_json::from_value(params).map_err(silli_proto::rpc::Error::invalid_params)?;
            Ok(serde_json::to_value(usage(hub)?)?)
        }
        method::STORAGE_RECLAIM => {
            reclaim(hub, serde_json::from_value(params).map_err(silli_proto::rpc::Error::invalid_params)?)?;
            Ok(serde_json::json!({}))
        }
        method::STORAGE_SPACE => {
            let _: silli_proto::Empty = serde_json::from_value(params).map_err(silli_proto::rpc::Error::invalid_params)?;
            Ok(serde_json::to_value(api::Space {
                home: silli_core::storage::space(&hub.home)?,
                state: silli_core::storage::space(std::path::Path::new(silli_core::jail::STATE_FS))?,
            })?)
        }
        method::STORAGE_CACHES => {
            let _: silli_proto::Empty = serde_json::from_value(params).map_err(silli_proto::rpc::Error::invalid_params)?;
            Ok(serde_json::to_value(caches(hub)?)?)
        }
        method::STORAGE_CLEAR_BUILD_CACHE => {
            let args: api::ClearBuildCache = serde_json::from_value(params).map_err(silli_proto::rpc::Error::invalid_params)?;
            clear(hub, &args.session_id)?;
            Ok(serde_json::json!({}))
        }
        _ => unreachable!(),
    })())
}

#[cfg(test)]
mod tests {
    use super::measure_directories;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn session_inventory_counts_arbitrary_outputs_once_and_does_not_follow_links() {
        let root = std::env::temp_dir().join(format!("silli-inventory-{}", crate::daemon::sfp::id()));
        let gradle = root.join("gradle output");
        let nuget = root.join("nuget\npackages");
        fs::create_dir_all(&gradle).unwrap();
        fs::create_dir(&nuget).unwrap();
        fs::write(gradle.join("library"), vec![1; 131072]).unwrap();
        fs::hard_link(gradle.join("library"), nuget.join("another-name")).unwrap();
        let outside = root.join("checkout");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("large"), vec![1; 1048576]).unwrap();
        symlink(&outside, gradle.join("checkout")).unwrap();

        let rows = measure_directories(&root, &[gradle, nuget.clone()]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, nuget.to_string_lossy());
        let total: u64 = rows.iter().map(|(_, bytes)| bytes).sum();
        assert!((131072..262144).contains(&total), "{rows:?}");
        fs::remove_dir_all(root).unwrap();
    }
}
