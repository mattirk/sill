//! Public host data methods. No core structs are serialized across this boundary.
use super::Hub;
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sill_proto::{admin as api, rpc::Error, sessions::HostPath};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

fn invalid(text: impl ToString) -> anyhow::Error {
    Error::invalid_params(text.to_string()).into()
}
fn domain(code: &'static str, text: impl ToString) -> anyhow::Error {
    sill_core::sfp::DomainError { code, message: text.to_string(), details: None }.into()
}
fn args<T: DeserializeOwned>(p: Value) -> Result<T> {
    serde_json::from_value(p).map_err(invalid)
}
fn path(p: &HostPath) -> Result<PathBuf> {
    p.to_path().map_err(invalid)
}
fn io(error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::InvalidInput {
        return invalid(error);
    }
    domain(
        match error.kind() {
            std::io::ErrorKind::NotFound => "not_found",
            std::io::ErrorKind::PermissionDenied => "permission_denied",
            std::io::ErrorKind::Unsupported => "unsupported",
            _ => "operation_failed",
        },
        error,
    )
}
fn limit(n: Option<u16>) -> Result<usize> {
    let n = n.unwrap_or(64) as usize;
    if !(1..=128).contains(&n) {
        return Err(invalid("limit must be 1–128"));
    }
    Ok(n)
}
fn bounded<T: serde::Serialize>(v: T) -> Result<Value> {
    let v = serde_json::to_value(v)?;
    if serde_json::to_vec(&v)?.len() > 4 * 1024 * 1024 {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Result, Some(4194304), "result exceeds 4 MiB"));
    }
    Ok(v)
}
fn metadata(p: &Path) -> Result<api::FileMetadata> {
    let m = fs::symlink_metadata(p).map_err(io)?;
    // Resolve relative link targets against the parent, preserving exact bytes.
    let target = if m.is_symlink() {
        fs::read_link(p).ok().map(|t| if t.is_absolute() { t } else { p.parent().unwrap_or(Path::new("/")).join(t) }).map(|p| HostPath::from_path(&p))
    } else {
        None
    };
    let effective = &m;
    Ok(api::FileMetadata {
        file: effective.is_file(),
        directory: effective.is_dir(),
        symlink: m.is_symlink(),
        target,
        length: effective.len().to_string(),
        readonly: effective.permissions().readonly(),
    })
}
fn list(a: api::FilesList) -> Result<api::FilesPage> {
    let n = limit(a.limit)?;
    let root = path(&a.path)?;
    let after = a.after.as_ref().map(path).transpose()?;
    if after.as_ref().is_some_and(|p| p.parent() != Some(root.as_path())) {
        return Err(invalid("cursor must name a direct child of the listed directory"));
    }
    // A bounded ordered set: scanning a huge directory never allocates all its entries.
    let mut names = std::collections::BTreeSet::new();
    for e in fs::read_dir(&root).map_err(io)? {
        let p = e.map_err(io)?.path();
        if after.as_ref().is_some_and(|a| &p <= a) {
            continue;
        }
        names.insert(p);
        if names.len() > n + 1 {
            names.pop_last();
        }
    }
    let more = names.len() > n;
    if more {
        names.pop_last();
    }
    let next = more.then(|| HostPath::from_path(names.last().unwrap()));
    let mut entries = Vec::new();
    for p in names {
        match metadata(&p) {
            Ok(metadata) => entries.push(api::FileEntry { path: HostPath::from_path(&p), metadata }),
            Err(e) if !p.exists() => {
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(api::FilesPage { entries, next })
}
fn read(a: api::FileRead) -> Result<api::FileBytes> {
    use std::os::unix::fs::OpenOptionsExt;
    // NOFOLLOW avoids opening a symlink swapped after metadata; NONBLOCK avoids FIFO hangs.
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path(&a.path)?)
        .map_err(io)?;
    read_open_file(f, &a.offset, a.length, a.revision)
}

fn read_open_file(mut f: fs::File, offset: &str, length: Option<u32>, expected_revision: Option<String>) -> Result<api::FileBytes> {
    use std::os::unix::fs::MetadataExt;
    let offset = offset.parse::<u64>().map_err(|_| invalid("offset must be a decimal u64"))?;
    let n = length.unwrap_or(api::FILE_CHUNK_BYTES as u32) as usize;
    if n == 0 || n > api::FILE_CHUNK_BYTES {
        return Err(invalid("length must be 1–1048576"));
    }
    let revision_of = |m: &fs::Metadata| format!("{}:{}:{}:{}:{}:{}:{}", m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec());
    let meta = f.metadata().map_err(io)?;
    let revision = revision_of(&meta);
    if expected_revision.as_ref().is_some_and(|r| r != &revision) {
        return Err(domain("invalid_cursor", "file changed; restart reading"));
    }
    if !meta.is_file() {
        return Err(domain("unsupported", "only regular files can be read"));
    }
    f.seek(SeekFrom::Start(offset)).map_err(io)?;
    let mut bytes = Vec::new();
    (&mut f).take(n as u64).read_to_end(&mut bytes).map_err(io)?;
    let next = offset.checked_add(bytes.len() as u64).ok_or_else(|| invalid("offset overflow"))?;
    if revision_of(&f.metadata().map_err(io)?) != revision {
        return Err(domain("invalid_cursor", "file changed while reading; restart reading"));
    }
    Ok(api::FileBytes { bytes_base64: api::encode_bytes(&bytes), next_offset: next.to_string(), eof: bytes.len() < n || next >= meta.len(), revision })
}
fn session(hub: &Hub, id: &str) -> Result<()> {
    if !sill_core::session::valid_id(id) {
        return Err(invalid("invalid session_id"));
    }
    hub.with_store(|s| if s.session_exact(id)?.is_some() { Ok(()) } else { Err(domain("not_found", "session not found")) })
}
fn policy_text_bound(text: &str) -> Result<()> {
    if text.len() > api::TEXT_BYTES {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(api::TEXT_BYTES as u64), "policy text exceeds 1 MiB"));
    }
    Ok(())
}
fn valid_policy_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name == "." || name == ".." || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(invalid("policy name must contain 1–128 ASCII letters, digits, dots, underscores or hyphens"));
    }
    Ok(())
}
fn policy_view(view: &str, session_id: &str, name: &str, r: sill_core::session::Resolution) -> Result<api::PolicyView> {
    let permissions = r
        .policy
        .permissions
        .states(&r.policy.network, &r.session_permissions)
        .into_iter()
        .map(|state| api::PermissionState {
            capability: state.capability.into(),
            allowed: state.allowed,
            default_enabled: state.default_enabled,
            requested: state.requested,
            blocked: state.blocked,
            source: state.source.into(),
            effective: state.effective,
            available: state.available,
            reason: state.reason,
        })
        .collect();
    let session_hosts = r
        .session_hosts
        .iter()
        .map(|host| api::SessionHostGrant { host: host.clone(), applied: true, reason: String::new() })
        .chain(r.blocked_session_hosts.iter().map(|host| api::SessionHostGrant { host: host.clone(), applied: false, reason: r.session_hosts_reason.clone() }))
        .collect();
    Ok(api::PolicyView {
        filesystem: r.filesystem.as_ref().map(|p| p.view.clone()),
        permissions,
        session_hosts,
        view: view.into(),
        session_id: session_id.into(),
        name: name.into(),
        text: Some(toml::to_string_pretty(&r.policy)?),
        fingerprint: Some(r.fingerprint),
        diagnostics: vec![],
    })
}
fn install_policy(home: &Path, name: &str, text: &str, replace: bool) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = home.join(".config/sill/policies");
    fs::create_dir_all(&dir).map_err(io)?;
    let target = dir.join(format!("{name}.toml"));
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if !meta.is_file() || meta.is_symlink() {
            return Err(domain("conflict", "existing policy is not a regular file"));
        }
        if !replace {
            if fs::read_to_string(&target).ok().as_deref() == Some(text) {
                return Ok(());
            }
            return Err(domain("conflict", "policy exists; use --replace to replace it"));
        }
    }
    let temp = dir.join(format!(".import-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        if replace {
            fs::rename(&temp, &target).map_err(io)?;
        } else {
            fs::hard_link(&temp, &target).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists { domain("conflict", "policy exists; inspect before replacing") } else { io(e) }
            })?;
        }
        fs::File::open(&dir).and_then(|f| f.sync_all()).map_err(io)?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}
/// The latest test desktop picture of each session, for `_sill/desktop.read` (one each: a new
/// capture replaces it).
fn pictures() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<sill_core::desktop::Picture>>> {
    static PICTURES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<sill_core::desktop::Picture>>>> =
        std::sync::OnceLock::new();
    PICTURES.get_or_init(Default::default)
}
fn session_hosts(session_id: &str) -> Result<std::collections::BTreeSet<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = sill_core::shim::runtime_dir(session_id)?.join("session-allowed-hosts");
    let file = match fs::OpenOptions::new().read(true).custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(io(e)),
    };
    if !file.metadata()?.is_file() {
        return Err(domain("unsupported", "host grants must be a regular file"));
    }
    let mut text = String::new();
    file.take((128 * 1025 + 1) as u64).read_to_string(&mut text).map_err(io)?;
    if text.len() > 128 * 1025 {
        return Err(domain("limit_exceeded", "session host grants exceed bound"));
    }
    let hosts: std::collections::BTreeSet<String> = text.lines().map(str::to_owned).collect();
    if hosts.len() > 128 {
        return Err(domain("limit_exceeded", "at most 128 session host grants"));
    }
    Ok(hosts)
}
static SESSION_HOSTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn remember_session_host(session_id: &str, host: &str) -> Result<()> {
    let _guard = SESSION_HOSTS.lock().unwrap();
    let mut hosts = session_hosts(session_id)?;
    hosts.insert(host.into());
    if hosts.len() > 128 {
        return Err(domain("limit_exceeded", "at most 128 session host grants"));
    }
    write_session_hosts(session_id, &hosts)
}
/// Revoke hosts approved during this run for new proxy connections. True when there were any:
/// tunnels already open keep going until the jail is rebuilt.
fn clear_session_hosts(session_id: &str) -> Result<bool> {
    let _guard = SESSION_HOSTS.lock().unwrap();
    if session_hosts(session_id)?.is_empty() {
        return Ok(false);
    }
    write_session_hosts(session_id, &Default::default())?;
    Ok(true)
}
fn write_session_hosts(session_id: &str, hosts: &std::collections::BTreeSet<String>) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text: String = hosts.iter().map(|host| format!("{host}\n")).collect();
    let runtime = sill_core::shim::runtime_dir(session_id)?;
    let temporary = runtime.join("session-allowed-hosts.new");
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        fs::rename(&temporary, runtime.join("session-allowed-hosts")).map_err(io)?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn session_with_pins(hub: &Hub, id: &str) -> Result<(sill_core::store::Session, Vec<PathBuf>)> {
    hub.with_store(|store| {
        let row = store.session_exact(id)?.ok_or_else(|| domain("not_found", "session not found"))?;
        let pins = store.jail_pins(id)?;
        Ok((row, pins))
    })
}
fn preview(hub: &Hub, session_id: &str) -> Result<api::PolicyView> {
    let (row, pins) = session_with_pins(hub, session_id)?;
    let workspace = hub.with_store(|store| store.workspace_mask(session_id))?;
    Ok(match sill_core::session::next_build(&row, &sill_core::changes::home()?, &pins, &workspace) {
        Ok(resolved) => policy_view("preview", session_id, &row.policy, resolved)?,
        Err(e) => api::PolicyView {
            filesystem: None,
            view: "preview".into(),
            session_id: session_id.into(),
            name: row.policy,
            permissions: vec![],
            session_hosts: vec![],
            text: None,
            fingerprint: None,
            diagnostics: vec![format!("{e:#}")],
        },
    })
}
/// The next build after a grant change, and whether the running jail still has another state.
/// Without launch evidence a running jail is never reported as current.
fn permissions_changed(hub: &Hub, session_id: &str, live_revoked: bool) -> Result<api::PermissionsChanged> {
    let preview = preview(hub, session_id)?;
    let restart_required = sill_core::session::is_running(session_id)
        && (live_revoked
            || match sill_core::session::policy_snapshot(session_id)? {
                Some(launch) => {
                    if launch.filesystem.as_ref().map(|p| &p.view.rules) != preview.filesystem.as_ref().map(|p| &p.rules) {
                        return Ok(api::PermissionsChanged { preview, restart_required: true });
                    }
                    let (choices, hosts) = hub.with_store(|store| Ok((store.permission_choices(session_id)?, store.network_hosts(session_id)?)))?;
                    sill_core::session::grants_outdated(&launch, &choices, &hosts)
                }
                None => true,
            });
    Ok(api::PermissionsChanged { preview, restart_required })
}

const PERMISSION_MODES: &str = "sill.permission_modes";
fn worker_mutation(method: &str) -> bool {
    matches!(
        method,
        "_sill/sessions.update"
            | "_sill/sessions.start"
            | "_sill/sessions.reset"
            | "_sill/sessions.pin"
            | "_sill/sessions.unpin"
            | "_sill/sessions.submit"
            | "_sill/sessions.control"
            | "_sill/submissions.resume"
            | "_sill/submissions.send_now"
            | "_sill/questions.answer"
            | "_sill/permissions.set"
            | "_sill/permissions.apply_development"
            | "_sill/permissions.restore_defaults"
            | "_sill/policy.allow_host"
            | "_sill/policy.save_host"
            | "_sill/host_control.activate"
    )
}

/// The daemon side of "clients without the permission-modes capability support Default only"
/// (docs/permission-modes.md, Frontends): such a connection — the line-mode `sill attach`, a
/// script — cannot change session capability choices,
/// nor operate or widen a session whose launch or next build uses Desktop development or Host
/// control capabilities. Actual capabilities decide, not a mode label; stopping stays available.
pub(super) fn frontend_gate(hub: &Hub, conn: &super::Conn, method: &str, params: &Value) -> Result<(), Error> {
    // Internal review workers retain their captured scope and derived policy. Transcript
    // reads/attach remain available, but a public client cannot turn one into an ordinary
    // editing session or widen its permissions while a run is in progress.
    if worker_mutation(method) && params.get("session_id").and_then(Value::as_str).is_some_and(|id| hub.crosscheck_worker(id)) {
        return Err(Error::domain("permission_denied", "Crosscheck workers are controlled by their run; open Crosscheck to stop or rerun it"));
    }
    if conn.has_capability(PERMISSION_MODES) {
        return Ok(());
    }
    let missing = |message: String| {
        let mut error = Error::domain("unsupported", message);
        error.data.as_mut().unwrap()["details"] = json!({"capability": PERMISSION_MODES});
        error
    };
    match method {
        "_sill/permissions.set"
        | "_sill/permissions.apply_development"
        | "_sill/permissions.restore_defaults"
        | "_sill/policy.save_host"
        | "_sill/host_control.activate"
        | "_sill/host_control.revoke" => return Err(missing("permission changes need a frontend that shows permission state".into())),
        "_sill/sessions.submit"
        | "_sill/sessions.start"
        | "_sill/sessions.control"
        | "_sill/submissions.resume"
        | "_sill/submissions.send_now"
        | "_sill/questions.answer"
        | "_sill/policy.allow_host"
        | "_sill/sessions.pin"
        | "_sill/sessions.unpin" => {}
        _ => return Ok(()),
    }
    // Malformed parameters are the method's own error.
    let Some(id) = params.get("session_id").and_then(Value::as_str) else { return Ok(()) };
    match session_uses_modes(hub, id) {
        Ok(false) => Ok(()),
        Ok(true) => Err(missing("this session uses Desktop development or Host control capabilities; operate it from sill-gui or the CLI".into())),
        Err(e) => Err(missing(format!("the session's permission state could not be determined ({e:#}); operate it from sill-gui or the CLI"))),
    }
}
fn session_uses_modes(hub: &Hub, id: &str) -> Result<bool> {
    let Some(row) = hub.with_store(|store| store.session_exact(id))? else { return Ok(false) };
    if sill_core::session::is_running(id) {
        if let Some(launch) = sill_core::session::policy_snapshot(id)? {
            return Ok(launch.policy.permissions.uses_modes());
        }
    }
    let (pins, workspace) = hub.with_store(|store| Ok((store.jail_pins(id)?, store.workspace_mask(id)?)))?;
    Ok(sill_core::session::next_build(&row, &sill_core::changes::home()?, &pins, &workspace)?.policy.permissions.uses_modes())
}

pub fn handle(hub: &Hub, conn: u64, m: &str, p: Value) -> Option<Result<Value>> {
    if let Some(r) = super::discovery::handle(m, p.clone()) {
        return Some(r);
    }
    if let Some(r) = super::memory_api::handle(hub, conn, m, p.clone()) {
        return Some(r);
    }
    if !matches!(
        m,
        "_sill/files.image"
            | "_sill/desktop.capture"
            | "_sill/desktop.read"
            | "_sill/policy.denied"
            | "_sill/policy.quiet_host"
            | "_sill/policy.remember_denied"
            | "_sill/policy.allow_host"
            | "_sill/policy.save_host"
            | "_sill/permissions.set"
            | "_sill/permissions.apply_development"
            | "_sill/permissions.restore_defaults"
            | "_sill/files.home"
            | "_sill/files.metadata"
            | "_sill/files.list"
            | "_sill/files.read"
            | "_sill/files.write"
            | "_sill/settings.get"
            | "_sill/usage.active_time"
            | "_sill/settings.patch"
            | "_sill/settings.instructions.get"
            | "_sill/settings.instructions.set"
            | "_sill/policy.get"
            | "_sill/policy.list"
            | "_sill/policy.preview"
            | "_sill/policy.snapshot"
            | "_sill/policy.check"
            | "_sill/policy.import"
            | "_sill/procs.list"
            | "_sill/procs.tail"
            | "_sill/procs.stop"
            | "_sill/procs.running"
            | "_sill/procs.stop_command"
            | "_sill/procs.dismiss"
    ) {
        return None;
    }
    Some((|| match m {
        "_sill/desktop.capture" => {
            let a: api::DesktopCapture = args(p)?;
            session(hub, &a.session_id)?;
            let dir = sill_core::shim::runtime_dir(&a.session_id)?.join("desktop");
            if !sill_core::session::is_running(&a.session_id) || !dir.join(sill_core::desktop::WAYLAND_SOCKET).exists() {
                return Err(domain("not_found", "this session has no test desktop running"));
            }
            // one capture at a time: each runs a client of a compositor and writes a picture
            static CAPTURING: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _one = CAPTURING.lock().unwrap_or_else(|e| e.into_inner());
            let shot = sill_core::desktop::capture(&dir).map_err(|e| domain("operation_failed", format!("{e:#}")))?;
            let scale = (shot.width() / sill_core::desktop::WIDTH).max(1);
            if a.since.as_deref() == Some(shot.revision.as_str()) {
                // the frontend shows this already: no encoding, nothing to read
                let (width, height) = (shot.width(), shot.height());
                return bounded(api::DesktopFrame { revision: shot.revision, changed: false, width, height, scale, size: "0".into() });
            }
            let picture = shot.picture().map_err(|e| domain("operation_failed", format!("{e:#}")))?;
            let frame = api::DesktopFrame {
                revision: picture.revision.clone(),
                changed: true,
                width: picture.width,
                height: picture.height,
                scale,
                size: picture.png.len().to_string(),
            };
            pictures().lock().unwrap().insert(a.session_id, std::sync::Arc::new(picture));
            bounded(frame)
        }
        "_sill/desktop.read" => {
            let a: api::DesktopRead = args(p)?;
            session(hub, &a.session_id)?;
            let picture = pictures()
                .lock()
                .unwrap()
                .get(&a.session_id)
                .cloned()
                .filter(|p| p.revision == a.revision)
                .ok_or_else(|| domain("conflict", "that picture was replaced or is gone; capture again"))?;
            let offset: usize = a.offset.parse().map_err(|_| invalid("offset must be a decimal u64"))?;
            let n = a.length.unwrap_or(api::FILE_CHUNK_BYTES as u32) as usize;
            if n == 0 || n > api::FILE_CHUNK_BYTES {
                return Err(invalid("length must be 1–1048576"));
            }
            let start = offset.min(picture.png.len());
            let end = start.saturating_add(n).min(picture.png.len());
            bounded(api::FileBytes {
                bytes_base64: api::encode_bytes(&picture.png[start..end]),
                next_offset: end.to_string(),
                eof: end == picture.png.len(),
                revision: picture.revision.clone(),
            })
        }
        "_sill/files.image" => {
            let a: api::SessionImage = args(p)?;
            session(hub, &a.session_id)?;
            let image = super::frontend::open_session_image(hub, &a.session_id, &path(&a.path)?, &sill_core::changes::home()?)
                .map_err(|e| match e.downcast::<std::io::Error>() {
                    Ok(e) => io(e),
                    Err(e) => domain("operation_failed", format!("{e:#}")),
                })?
                .ok_or_else(|| domain("not_found", "image not visible in session"))?;
            bounded(read_open_file(image, &a.offset, a.length, a.revision)?)
        }
        "_sill/policy.denied" => bounded(denied(hub, args(p)?)?),
        "_sill/policy.quiet_host" => {
            let a: api::NetworkHost = args(p)?;
            session(hub, &a.session_id)?;
            let host = a.host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map_or(a.host.as_str(), |(name, _)| name);
            if !sill_core::proxy::valid_hostname(host) {
                return Err(invalid("a concrete hostname is required"));
            }
            let host = host.to_ascii_lowercase();
            sill_core::config::update(|c| {
                if !c.network_quiet_hosts.contains(&host) {
                    c.network_quiet_hosts.push(host);
                }
            })?;
            Ok(json!({}))
        }
        "_sill/policy.remember_denied" | "_sill/policy.allow_host" => {
            let a: api::NetworkHost = args(p)?;
            session(hub, &a.session_id)?;
            if a.host.is_empty() || a.host.len() > 1024 || a.host.chars().any(char::is_control) {
                return Err(invalid("invalid host"));
            }
            if m.ends_with("remember_denied") {
                use std::io::Write;
                let path = sill_core::shim::runtime_dir(&a.session_id)?.join("denied.answered");
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(file, "{}", a.host)?;
                Ok(json!({}))
            } else {
                let row = hub.with_store(|s| s.session_exact(&a.session_id))?.ok_or_else(|| domain("not_found", "session not found"))?;
                let name = a.host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map(|(name, _)| name).unwrap_or(&a.host);
                let name = name.strip_prefix("*.").unwrap_or(name);
                if a.host.contains('*') || !sill_core::proxy::valid_hostname(name) {
                    return Err(invalid("session approvals require a concrete hostname"));
                }
                let home = sill_core::changes::home()?;
                let running = sill_core::session::is_running(&a.session_id);
                let snapshot = if running { sill_core::session::policy_snapshot(&a.session_id)? } else { None };
                let policy = match snapshot {
                    Some(snapshot) => snapshot.policy,
                    None => {
                        let pins = hub.with_store(|store| store.jail_pins(&a.session_id))?;
                        sill_core::session::next_build_base(&row, &home, &pins)?.policy
                    }
                };
                if policy.network.mode == sill_core::policy::NetMode::None {
                    return Err(domain("permission_denied", "network is disabled by the launch policy; change the policy and rebuild"));
                }
                if !policy.network.allow_additional_hosts {
                    return Err(domain("permission_denied", "additional hosts are forbidden by the launch policy"));
                }
                hub.with_store(|store| store.allow_network_host(&a.session_id, &a.host))?;
                if running {
                    remember_session_host(&a.session_id, &a.host)?;
                }
                bounded(api::AllowedHost { agent_name: "this session".into(), live: running })
            }
        }
        "_sill/policy.save_host" => {
            let a: api::NetworkHost = args(p)?;
            if a.host.is_empty() || a.host.len() > 1024 || a.host.chars().any(char::is_control) {
                return Err(invalid("invalid host"));
            }
            let row = hub.with_store(|s| s.session_exact(&a.session_id))?.ok_or_else(|| domain("not_found", "session not found"))?;
            if row.ghost {
                return Err(domain("permission_denied", "ghost sessions do not change future-session defaults"));
            }
            let name = a.host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map(|(name, _)| name).unwrap_or(&a.host);
            if !sill_core::proxy::valid_hostname(name.strip_prefix("*.").unwrap_or(name)) {
                return Err(invalid("invalid host pattern"));
            }
            let home = sill_core::changes::home()?;
            let mut agent = sill_core::agents::load(&home)?.remove(&row.agent).ok_or_else(|| domain("not_found", "unknown agent"))?;
            if !agent.domains.contains(&a.host) {
                if agent.domains.len() >= 256 {
                    return Err(domain("limit_exceeded", "at most 256 agent network defaults"));
                }
                agent.domains.push(a.host.clone());
                sill_core::agents::save_override(&home, &agent)?;
            }
            bounded(api::SavedHost { agent_id: agent.id, agent_name: agent.name })
        }
        "_sill/permissions.set" => {
            let a: api::PermissionChoice = args(p)?;
            let (row, pins) = session_with_pins(hub, &a.session_id)?;
            if row.ghost {
                return Err(domain("permission_denied", "ghost sessions support Default only"));
            }
            if sill_core::permissions::Permissions::default().rule(&a.capability).is_none() {
                return Err(invalid("unknown capability"));
            }
            if a.capability == "host_control" {
                return Err(domain("permission_denied", "Host control needs explicit activation; it is not a session choice"));
            }
            if a.enabled == Some(true) {
                // Refuse widening the current limits would block, rather than remembering it.
                let base = sill_core::session::next_build_base(&row, &sill_core::changes::home()?, &pins)?;
                let mut choices = hub.with_store(|store| store.permission_choices(&a.session_id))?;
                choices.insert(a.capability.clone(), true);
                let candidate = sill_core::session::apply_grants(base, &choices, vec![]);
                if let Some(reason) = candidate.session_permissions.iter().find(|c| c.capability == a.capability).and_then(|c| c.blocked.as_ref()) {
                    return Err(domain("permission_denied", format!("{}: {reason}", a.capability)));
                }
            }
            hub.with_store(|store| store.set_permission_choice(&a.session_id, &a.capability, a.enabled))?;
            bounded(permissions_changed(hub, &a.session_id, false)?)
        }
        "_sill/permissions.apply_development" => {
            // Apply development permissions: the test desktop and the private bus as one choice,
            // all or nothing; what each one would say alone is the refusal's reason.
            let a: sill_proto::sessions::SessionKey = args(p)?;
            let (row, pins) = session_with_pins(hub, &a.session_id)?;
            if row.ghost {
                return Err(domain("permission_denied", "ghost sessions support Default only"));
            }
            let group = sill_core::permissions::DEVELOPMENT_CAPABILITIES;
            let base = sill_core::session::next_build_base(&row, &sill_core::changes::home()?, &pins)?;
            let mut choices = hub.with_store(|store| store.permission_choices(&a.session_id))?;
            for capability in group {
                choices.insert(capability.to_string(), true);
            }
            let candidate = sill_core::session::apply_grants(base, &choices, vec![]);
            let blocked: Vec<String> = candidate
                .session_permissions
                .iter()
                .filter(|c| group.contains(&c.capability.as_str()))
                .filter_map(|c| c.blocked.as_ref().map(|reason| format!("{}: {reason}", c.capability)))
                .collect();
            if !blocked.is_empty() {
                return Err(domain("permission_denied", format!("development permissions not applied; {}", blocked.join("; "))));
            }
            hub.with_store(|store| {
                for capability in group {
                    store.set_permission_choice(&a.session_id, capability, Some(true))?;
                }
                Ok(())
            })?;
            bounded(permissions_changed(hub, &a.session_id, false)?)
        }
        "_sill/permissions.restore_defaults" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            hub.with_store(|store| store.restore_default_permissions(&a.session_id))?;
            let live_revoked = sill_core::session::is_running(&a.session_id) && clear_session_hosts(&a.session_id)?;
            bounded(permissions_changed(hub, &a.session_id, live_revoked)?)
        }

        "_sill/files.home" => {
            let _: sill_proto::Empty = args(p)?;
            bounded(HostPath::from_path(&sill_core::changes::home()?))
        }
        "_sill/files.metadata" => {
            let a: api::FileKey = args(p)?;
            bounded(metadata(&path(&a.path)?)?)
        }
        "_sill/files.list" => bounded(list(args(p)?)?),
        "_sill/files.read" => bounded(read(args(p)?)?),
        "_sill/files.write" => {
            let a: api::FileWrite = args(p)?;
            if a.bytes_base64.len() > api::FILE_CHUNK_BYTES.div_ceil(3) * 4 {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "write exceeds 1 MiB"));
            }
            let bytes = api::decode_bytes(&a.bytes_base64).map_err(invalid)?;
            if bytes.len() > api::FILE_CHUNK_BYTES {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "write exceeds 1 MiB"));
            }
            let path = path(&a.path)?;
            sill_core::safe_fs::replace_file(
                path.parent().ok_or_else(|| invalid("path has no parent"))?,
                path.file_name().ok_or_else(|| invalid("path has no filename"))?,
                &bytes,
            )?;
            Ok(json!({}))
        }
        "_sill/settings.get" => {
            let _: sill_proto::Empty = args(p)?;
            let c: api::Settings = serde_json::from_value(json!(sill_core::config::load()))?;
            bounded(c)
        }
        "_sill/usage.active_time" => {
            let a: api::ActiveTimeQuery = args(p)?;
            if !(1..=366).contains(&a.days) {
                return Err(invalid("days must be 1 to 366"));
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            let hours = hub.with_store(|store| sill_core::sfp::Journal::new(store)?.active_hours(now.saturating_sub(u64::from(a.days) * 86_400_000), now))?;
            Ok(json!(api::ActiveTime { hours: hours.into_iter().map(|(hour, ms)| api::ActiveHour { hour, ms }).collect() }))
        }
        "_sill/settings.patch" => {
            if serde_json::to_vec(&p)?.len() > api::TEXT_BYTES {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "settings input exceeds 1 MiB"));
            }
            let a: api::SettingsPatch = args(p)?;
            if a.changes.len() > 64 {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(64), "at most 64 changes"));
            }
            let _lock = super::frontend::CONFIG.lock().unwrap();
            let mut value = json!(sill_core::config::load());
            for change in a.changes {
                let c = serde_json::to_value(change)?;
                value[c["type"].as_str().unwrap()] = c["value"].clone();
            }
            // the result must read as the protocol's settings; what is saved is the whole config
            let _: api::Settings = serde_json::from_value(value.clone())?;
            sill_core::config::save(&serde_json::from_value(value)?)?;
            Ok(json!({}))
        }
        "_sill/settings.instructions.get" => {
            let _: sill_proto::Empty = args(p)?;
            bounded(sill_proto::Text { text: sill_core::instructions::load_global() })
        }
        "_sill/settings.instructions.set" => {
            let a: api::Text = args(p)?;
            if a.text.len() > api::TEXT_BYTES {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(1048576), "instructions exceed 1 MiB"));
            }
            sill_core::instructions::save_global(&a.text)?;
            Ok(json!({}))
        }
        "_sill/policy.list" => {
            let a: api::NamesList = args(p)?;
            let n = limit(a.limit)?;
            if a.after.as_ref().is_some_and(|s| s.len() > 4096) {
                return Err(invalid("policy cursor too long"));
            }
            let (names, more) = sill_core::policy::available_page(&sill_core::changes::home()?, a.after.as_deref(), n);
            let next = if more { names.last().cloned() } else { None };
            bounded(api::Names { names, next })
        }
        "_sill/policy.get" => {
            let a: api::PolicyKey = args(p)?;
            if a.name.len() > 128 || a.name.contains('/') || a.name == ".." {
                return Err(invalid("invalid policy name"));
            }
            let policy = sill_core::policy::Policy::load(&a.name, &sill_core::changes::home()?)?;
            bounded(api::PolicyEntry { name: a.name, text: toml::to_string_pretty(&policy)? })
        }
        "_sill/policy.preview" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            bounded(preview(hub, &a.session_id)?)
        }
        "_sill/policy.snapshot" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            let snapshot = sill_core::session::policy_snapshot(&a.session_id)?
                .map(|r| {
                    let name = r.policy.name.clone();
                    policy_view("launch", &a.session_id, &name, r)
                })
                .transpose()?;
            bounded(api::PolicySnapshot { snapshot, warnings: sill_core::session::launch_warnings(&a.session_id)? })
        }
        "_sill/policy.check" => {
            let a: api::PolicyCheck = args(p)?;
            policy_text_bound(&a.text)?;
            let checked = sill_core::policy::Policy::parse(&a.text).map(|_| ());
            bounded(api::PolicyChecked { valid: checked.is_ok(), diagnostics: checked.err().map(|e| vec![format!("{e:#}")]).unwrap_or_default() })
        }
        "_sill/policy.import" => {
            let a: api::PolicyImport = args(p)?;
            policy_text_bound(&a.text)?;
            valid_policy_name(&a.name)?;
            let mut policy = sill_core::policy::Policy::parse(&a.text).map_err(|e| invalid(format!("{e:#}")))?;
            policy.name = a.name.clone();
            let text = toml::to_string_pretty(&policy)?;
            install_policy(&sill_core::changes::home()?, &a.name, &text, a.replace)?;
            bounded(api::PolicyEntry { name: a.name, text })
        }
        "_sill/procs.list" => {
            let a: api::ProcessesList = args(p)?;
            let n = limit(a.limit)?;
            session(hub, &a.session_id)?;
            let mut procs =
                sill_core::shim::Control::connect(&sill_core::session::socket_path(&a.session_id)?).and_then(|mut c| c.proc_list()).unwrap_or_default();
            procs.sort_by(|a, b| a.name.cmp(&b.name));
            let mut processes: Vec<api::Process> = procs
                .into_iter()
                .map(|p| api::Process {
                    state: if p.running() {
                        "running"
                    } else if p.exit_code().is_some() {
                        "exited"
                    } else if p.state.as_str() == Some("stopped") {
                        "stopped"
                    } else {
                        "unknown"
                    }
                    .into(),
                    exit_code: p.exit_code(),
                    name: p.name,
                    argv: p.argv,
                    cwd: HostPath::from_path(&p.cwd),
                    port: p.port,
                    pid: p.pid,
                    started: p.started.to_string(),
                    last_line: p.last_line,
                    location: "jail".into(),
                    kept: false,
                })
                .collect();
            processes.extend(hub.with_store(|s| s.host_processes(&a.session_id))?.into_iter().map(|p| api::Process {
                name: format!("host:{}", p.id),
                argv: p.argv,
                cwd: HostPath::from_path(&p.cwd),
                port: None,
                pid: p.pid,
                started: p.started.to_string(),
                state: p.state,
                exit_code: p.exit_code,
                last_line: String::new(),
                location: "host".into(),
                kept: p.kept,
            }));
            processes.sort_by(|a, b| a.name.cmp(&b.name));
            processes.retain(|p| a.after.as_ref().is_none_or(|after| p.name > *after));
            let next = (processes.len() > n).then(|| processes[n - 1].name.clone());
            processes.truncate(n);
            bounded(api::ProcessesPage { processes, next })
        }
        "_sill/procs.tail" => {
            let a: api::ProcessTail = args(p)?;
            session(hub, &a.session_id)?;
            let n = a.lines.unwrap_or(100);
            if n > 10000 {
                return Err(invalid("lines exceeds 10000"));
            }
            if let Some(id) = a.name.strip_prefix("host:") {
                let found = hub.with_store(|s| Ok(s.host_processes(&a.session_id)?.iter().any(|p| p.id == id)))?;
                if !found {
                    return Err(invalid("host process not found"));
                }
                use std::io::{Read, Seek};
                let path = super::host_bridge::host_log(&a.session_id, id)?;
                let mut file = std::fs::File::open(path)?;
                let size = file.metadata()?.len();
                file.seek(std::io::SeekFrom::Start(size.saturating_sub(1024 * 1024)))?;
                let mut bytes = Vec::new();
                file.take(1024 * 1024).read_to_end(&mut bytes)?;
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<_> = text.lines().collect();
                return bounded(sill_proto::Text { text: lines[lines.len().saturating_sub(n as usize)..].join("\n") });
            }
            bounded(sill_proto::Text {
                text: sill_core::shim::Control::connect(&sill_core::session::socket_path(&a.session_id)?)?.proc_tail(&a.name, n as usize)?,
            })
        }
        "_sill/procs.running" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            // no jail to ask is nothing running, not an error
            let report = sill_core::shim::Control::connect(&sill_core::session::socket_path(&a.session_id)?).and_then(|mut c| c.activity()).unwrap_or_default();
            let process =
                |t: sill_core::shim::Tree| api::JailProcess { pid: t.pid, argv: t.argv, started_ms: t.started_ms, count: t.count, name: t.name, port: t.port };
            bounded(api::JailProcesses {
                commands: report.commands.into_iter().map(process).collect(),
                services: report.services.into_iter().map(process).collect(),
            })
        }
        "_sill/procs.stop_command" => {
            let a: api::CommandKey = args(p)?;
            session(hub, &a.session_id)?;
            let mut c = sill_core::shim::Control::connect(&sill_core::session::socket_path(&a.session_id)?)?;
            c.stop_command(a.pid)?;
            Ok(json!({}))
        }
        "_sill/procs.stop" | "_sill/procs.dismiss" => {
            let a: api::ProcessKey = args(p)?;
            session(hub, &a.session_id)?;
            if a.name.is_empty() || a.name.len() > 128 {
                return Err(invalid("a process name is required"));
            }
            if let Some(id) = a.name.strip_prefix("host:") {
                if m.ends_with("stop") {
                    return Err(invalid("host processes require sill-host stop with an active stop grant"));
                }
                // the person clears an ended one from the list; the conversation keeps its notices
                if !hub.with_store(|s| s.dismiss_host_process(&a.session_id, id))? {
                    return Err(invalid("only an ended host process can be dismissed"));
                }
                if let Ok(log) = super::host_bridge::host_log(&a.session_id, id) {
                    let _ = std::fs::remove_file(log);
                }
                super::host_bridge::publish(hub, &a.session_id);
                return Ok(json!({}));
            }
            let mut c = sill_core::shim::Control::connect(&sill_core::session::socket_path(&a.session_id)?)?;
            if m.ends_with("stop") {
                c.proc_stop(&a.name)?;
            } else {
                c.proc_dismiss(&a.name)?;
            }
            Ok(json!({}))
        }
        _ => unreachable!(),
    })())
}

fn denied(hub: &Hub, a: api::DeniedList) -> Result<api::NetworkDenied> {
    use std::io::{BufRead, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let n = limit(a.limit)?;
    session(hub, &a.session_id)?;
    let after: Option<(u8, String)> = a.after.as_ref().map(|s| serde_json::from_str(s)).transpose().map_err(invalid)?;
    if after.as_ref().is_some_and(|(kind, host)| *kind > 2 || host.len() > 4096) {
        return Err(invalid("invalid denial cursor"));
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut add = |kind: u8, host: String| {
        let key = (kind, host);
        if after.as_ref().is_none_or(|a| &key > a) {
            keys.insert(key);
            if keys.len() > n + 1 {
                keys.pop_last();
            }
        }
    };
    let runtime = sill_core::shim::runtime_dir(&a.session_id)?;
    for (kind, name) in [(0, "denied"), (1, "denied.answered")] {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
            .open(runtime.join(name))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io(e)),
        };
        if !file.metadata()?.is_file() {
            return Err(domain("unsupported", "network denial log is not a regular file"));
        }
        let mut reader = std::io::BufReader::new(file);
        loop {
            let mut line = String::new();
            let count = (&mut reader).take(4097).read_line(&mut line)?;
            if count == 0 {
                break;
            }
            if count > 4096 {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4096), "network denial line exceeds 4096 bytes"));
            }
            let host = line.trim();
            if !host.is_empty() {
                add(kind, host.into());
            }
        }
    }
    let row = hub.with_store(|s| Ok(s.session_exact(&a.session_id)?.ok_or_else(|| domain("not_found", "session not found"))?))?;
    let home = sill_core::changes::home()?;
    let quiet = sill_core::policy::Policy::load(&row.policy, &home).map(|p| p.network.quiet).unwrap_or_else(|_| sill_core::policy::quiet_default());
    for host in quiet.into_iter().chain(sill_core::config::load().network_quiet_hosts) {
        if host.len() > 4096 {
            return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4096), "quiet host exceeds 4096 bytes"));
        }
        add(2, host);
    }
    let more = keys.len() > n;
    if more {
        keys.pop_last();
    }
    let next = if more { keys.last().map(serde_json::to_string).transpose()? } else { None };
    let agent_name = sill_core::agents::load(&home)?.get(&row.agent).map(|a| a.name.clone()).unwrap_or(row.agent);
    let mut out = api::NetworkDenied { next, hosts: vec![], answered: vec![], quiet: vec![], agent_name };
    for (kind, host) in keys {
        match kind {
            0 => out.hosts.push(host),
            1 => out.answered.push(host),
            _ => out.quiet.push(host),
        }
    }
    Ok(out)
}
