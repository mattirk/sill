//! Public host data methods. No core structs are serialized across this boundary.
use super::Hub;
use anyhow::{Context, Result};
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
    sill_core::sfp::DomainError {
        code,
        message: text.to_string(),
        details: None,
    }
    .into()
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
        fs::read_link(p)
            .ok()
            .map(|t| {
                if t.is_absolute() {
                    t
                } else {
                    p.parent().unwrap_or(Path::new("/")).join(t)
                }
            })
            .map(|p| HostPath::from_path(&p))
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
    if after
        .as_ref()
        .is_some_and(|p| p.parent() != Some(root.as_path()))
    {
        return Err(invalid(
            "cursor must name a direct child of the listed directory",
        ));
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
            Ok(metadata) => entries.push(api::FileEntry {
                path: HostPath::from_path(&p),
                metadata,
            }),
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
    let offset = offset
        .parse::<u64>()
        .map_err(|_| invalid("offset must be a decimal u64"))?;
    let n = length.unwrap_or(api::FILE_CHUNK_BYTES as u32) as usize;
    if n == 0 || n > api::FILE_CHUNK_BYTES {
        return Err(invalid("length must be 1–1048576"));
    }
    let revision_of = |m: &fs::Metadata| {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        )
    };
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
    (&mut f)
        .take(n as u64)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    let next = offset
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| invalid("offset overflow"))?;
    if revision_of(&f.metadata().map_err(io)?) != revision {
        return Err(domain(
            "invalid_cursor",
            "file changed while reading; restart reading",
        ));
    }
    Ok(api::FileBytes {
        bytes_base64: api::encode_bytes(&bytes),
        next_offset: next.to_string(),
        eof: bytes.len() < n || next >= meta.len(),
        revision,
    })
}
fn session(hub: &Hub, id: &str) -> Result<()> {
    if !sill_core::session::valid_id(id) {
        return Err(invalid("invalid session_id"));
    }
    hub.with_store(|s| {
        if s.session_exact(id)?.is_some() {
            Ok(())
        } else {
            Err(domain("not_found", "session not found"))
        }
    })
}
fn policy_text_bound(text: &str) -> Result<()> {
    if text.len() > api::TEXT_BYTES {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Request, Some(api::TEXT_BYTES as u64), "policy text exceeds 1 MiB"));
    }
    Ok(())
}
fn valid_policy_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name == "." || name == ".."
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(invalid("policy name must contain 1–128 ASCII letters, digits, dots, underscores or hyphens"));
    }
    Ok(())
}
fn policy_view(view: &str, session_id: &str, name: &str, r: sill_core::policy_fragments::Resolution) -> Result<api::PolicyView> {
    Ok(api::PolicyView {
        view: view.into(), session_id: session_id.into(), name: name.into(),
        text: Some(toml::to_string_pretty(&r.policy)?),
        fragment_ro: r.fragment_ro.iter().map(|p| HostPath::from_path(p)).collect(),
        fragment_masks: r.fragment_masks.iter().map(|p| HostPath::from_path(p)).collect(),
        sources: r.sources.into_iter().map(|s| api::PolicySource {
            root: HostPath::from_path(&s.root), path: HostPath::from_path(&s.path),
            text: s.text, sha256: s.sha256, ignored: s.ignored, error: s.error,
        }).collect(),
        suggestions: r.suggestions.into_iter().map(|s| api::PolicySuggestion { host: s.host, source: HostPath::from_path(&s.source) }).collect(),
        fingerprint: Some(r.fingerprint), diagnostics: vec![],
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
        if !meta.is_file() || meta.is_symlink() { return Err(domain("conflict", "existing policy is not a regular file")); }
        if !replace {
            if fs::read_to_string(&target).ok().as_deref() == Some(text) { return Ok(()); }
            return Err(domain("conflict", "policy exists; use --replace to replace it"));
        }
    }
    let temp = dir.join(format!(".import-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        if replace { fs::rename(&temp, &target).map_err(io)?; }
        else { fs::hard_link(&temp, &target).map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { domain("conflict", "policy exists; inspect before replacing") } else { io(e) })?; }
        fs::File::open(&dir).and_then(|f| f.sync_all()).map_err(io)?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}
fn session_hosts(session_id: &str) -> Result<std::collections::BTreeSet<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = sill_core::shim::runtime_dir(session_id)?.join("session-allowed-hosts");
    let file = match fs::OpenOptions::new().read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(io(e)),
    };
    if !file.metadata()?.is_file() { return Err(domain("unsupported", "host grants must be a regular file")); }
    let mut text = String::new();
    file.take((128 * 1025 + 1) as u64).read_to_string(&mut text).map_err(io)?;
    if text.len() > 128 * 1025 { return Err(domain("limit_exceeded", "session host grants exceed bound")); }
    let hosts: std::collections::BTreeSet<String> = text.lines().map(str::to_owned).collect();
    if hosts.len() > 128 { return Err(domain("limit_exceeded", "at most 128 session host grants")); }
    Ok(hosts)
}
fn remember_session_host(session_id: &str, host: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let mut hosts = session_hosts(session_id)?;
    hosts.insert(host.into());
    if hosts.len() > 128 { return Err(domain("limit_exceeded", "at most 128 session host grants")); }
    let text = hosts.into_iter().collect::<Vec<_>>().join("\n") + "\n";
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
            | "_sill/policy.denied"
            | "_sill/policy.remember_denied"
            | "_sill/policy.allow_host"
            | "_sill/files.home"
            | "_sill/files.metadata"
            | "_sill/files.list"
            | "_sill/files.read"
            | "_sill/files.write"
            | "_sill/settings.get"
            | "_sill/settings.patch"
            | "_sill/settings.instructions.get"
            | "_sill/settings.instructions.set"
            | "_sill/policy.get"
            | "_sill/policy.list"
            | "_sill/policy.start"
            | "_sill/policy.preview"
            | "_sill/policy.snapshot"
            | "_sill/policy.check"
            | "_sill/policy.import"
            | "_sill/procs.list"
            | "_sill/procs.tail"
            | "_sill/procs.stop"
            | "_sill/procs.dismiss"
    ) {
        return None;
    }
    Some((|| match m {
        "_sill/files.image" => {
            let a: api::SessionImage = args(p)?;
            session(hub, &a.session_id)?;
            let image = super::frontend::open_session_image(
                hub,
                &a.session_id,
                &path(&a.path)?,
                &sill_core::changes::home()?,
            ).map_err(|e| match e.downcast::<std::io::Error>() {
                Ok(e) => io(e),
                Err(e) => domain("operation_failed", format!("{e:#}")),
            })?
            .ok_or_else(|| domain("not_found", "image not visible in session"))?;
            bounded(read_open_file(image, &a.offset, a.length, a.revision)?)
        }
        "_sill/policy.denied" => bounded(denied(hub, args(p)?)?),
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
                if !sill_core::proxy::valid_hostname(name) { return Err(invalid("invalid host pattern")); }
                let home = sill_core::changes::home()?;
                let snapshot = sill_core::session::policy_snapshot(&a.session_id)?;
                let suggested = snapshot.as_ref().is_some_and(|s| s.suggestions.iter().any(|suggestion| suggestion.host == a.host));
                let policy = match snapshot {
                    Some(snapshot) => snapshot.policy,
                    None => sill_core::policy::Policy::load(&row.policy, &home)?,
                };
                if policy.network.mode == sill_core::policy::NetMode::None {
                    return Err(domain("permission_denied", "network is disabled by the launch policy; change the policy and rebuild"));
                }
                if suggested {
                    remember_session_host(&a.session_id, &a.host)?;
                    return bounded(api::AllowedHost { agent_name: "this session".into() });
                }
                let mut agent = sill_core::agents::load(&home).remove(&row.agent).context("unknown agent")?;
                if !agent.domains.contains(&a.host) { agent.domains.push(a.host); }
                sill_core::agents::save_override(&home, &agent)?;
                bounded(api::AllowedHost { agent_name: agent.name })
            }
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
                path.file_name()
                    .ok_or_else(|| invalid("path has no filename"))?,
                &bytes,
            )?;
            Ok(json!({}))
        }
        "_sill/settings.get" => {
            let _: sill_proto::Empty = args(p)?;
            let c: api::Settings = serde_json::from_value(json!(sill_core::config::load()))?;
            bounded(c)
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
            let typed: api::Settings = serde_json::from_value(value)?;
            sill_core::config::save(&serde_json::from_value(json!(typed))?)?;
            Ok(json!({}))
        }
        "_sill/settings.instructions.get" => {
            let _: sill_proto::Empty = args(p)?;
            bounded(sill_proto::Text {
                text: sill_core::instructions::load_global(),
            })
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
            let (names, more) = sill_core::policy::available_page(
                &sill_core::changes::home()?,
                a.after.as_deref(),
                n,
            );
            let next = if more { names.last().cloned() } else { None };
            bounded(api::Names { names, next })
        }
        "_sill/policy.get" => {
            let a: api::PolicyKey = args(p)?;
            if a.name.len() > 128 || a.name.contains('/') || a.name == ".." {
                return Err(invalid("invalid policy name"));
            }
            let policy = sill_core::policy::Policy::load(&a.name, &sill_core::changes::home()?)?;
            bounded(api::PolicyEntry {
                name: a.name,
                text: toml::to_string_pretty(&policy)?,
                x11: policy.jail.x11,
                dbus: policy.jail.dbus,
            })
        }
        "_sill/policy.start" => {
            let a: api::PolicyStart = args(p)?;
            session(hub, &a.session_id)?;
            if a.ignore_roots.len() > 128 { return Err(invalid("at most 128 ignored roots")); }
            let roots = a.ignore_roots.iter().map(path).collect::<Result<Vec<_>>>()?;
            let row = hub.with_store(|s| s.session_exact(&a.session_id))?.ok_or_else(|| domain("not_found", "session not found"))?;
            if row.ended_at.is_some() { return Err(domain("invalid_state", "session has ended; reopen explicitly")); }
            if !roots.is_empty() && sill_core::session::is_running(&a.session_id) {
                return Err(domain("invalid_state", "jail is running; stop it before requesting a one-build exception"));
            }
            let started = hub.start_session_ignoring(&a.session_id, &roots)?;
            bounded(sill_proto::sessions::Started { session_id: a.session_id, fresh: started.fresh })
        }
        "_sill/policy.preview" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            let (row, pins) = hub.with_store(|store| {
                let row = store.session_exact(&a.session_id)?.ok_or_else(|| domain("not_found", "session not found"))?;
                Ok((row, store.jail_pins(&a.session_id)?))
            })?;
            let home = sill_core::changes::home()?;
            let cfg = sill_core::config::load();
            let (x11, dbus) = sill_core::config::desktop_grants(&row.desktop, &cfg);
            let pins = if row.ghost { vec![] } else { pins };
            let result = sill_core::session::session_base_policy(&row.policy, &home, x11, dbus, row.ghost, Some(&row.agent))
                .and_then(|base| sill_core::policy_fragments::resolve(&base, &pins, &[]));
            let view = match result {
                Ok(resolved) => policy_view("preview", &a.session_id, &row.policy, resolved)?,
                Err(e) => api::PolicyView {
                    view: "preview".into(), session_id: a.session_id, name: row.policy,
                    text: None, fragment_ro: vec![], fragment_masks: vec![], sources: vec![],
                    suggestions: vec![], fingerprint: None, diagnostics: vec![format!("{e:#}")],
                },
            };
            bounded(view)
        }
        "_sill/policy.snapshot" => {
            let a: sill_proto::sessions::SessionKey = args(p)?;
            session(hub, &a.session_id)?;
            let snapshot = sill_core::session::policy_snapshot(&a.session_id)?
                .map(|r| { let name = r.policy.name.clone(); policy_view("launch", &a.session_id, &name, r) }).transpose()?;
            bounded(api::PolicySnapshot { snapshot })
        }
        "_sill/policy.check" => {
            let a: api::PolicyCheck = args(p)?;
            policy_text_bound(&a.text)?;
            let checked = if a.fragment {
                let root = a.root.as_ref().ok_or_else(|| invalid("fragment check requires a daemon-host root"))?;
                sill_core::policy_fragments::check_fragment(&a.text, &path(root)?)
            } else {
                if a.root.is_some() { return Err(invalid("root is only valid for fragment checks")); }
                sill_core::policy::Policy::parse(&a.text).map(|_| ())
            };
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
            bounded(api::PolicyEntry { name: a.name, text, x11: policy.jail.x11, dbus: policy.jail.dbus })
        }
        "_sill/procs.list" => {
            let a: api::ProcessesList = args(p)?;
            let n = limit(a.limit)?;
            session(hub, &a.session_id)?;
            let mut procs = sill_core::shim::Control::connect(&sill_core::session::socket_path(
                &a.session_id,
            )?)?
            .proc_list()?;
            procs.sort_by(|a, b| a.name.cmp(&b.name));
            procs.retain(|p| a.after.as_ref().is_none_or(|after| p.name > *after));
            let next = (procs.len() > n).then(|| procs[n - 1].name.clone());
            procs.truncate(n);
            bounded(api::ProcessesPage {
                processes: procs
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
                    })
                    .collect(),
                next,
            })
        }
        "_sill/procs.tail" => {
            let a: api::ProcessTail = args(p)?;
            session(hub, &a.session_id)?;
            let n = a.lines.unwrap_or(100);
            if n > 10000 {
                return Err(invalid("lines exceeds 10000"));
            }
            bounded(sill_proto::Text {
                text: sill_core::shim::Control::connect(&sill_core::session::socket_path(
                    &a.session_id,
                )?)?
                .proc_tail(&a.name, n as usize)?,
            })
        }
        "_sill/procs.stop" | "_sill/procs.dismiss" => {
            let a: api::ProcessKey = args(p)?;
            session(hub, &a.session_id)?;
            if a.name.is_empty() || a.name.len() > 128 {
                return Err(invalid("a process name is required"));
            }
            let mut c = sill_core::shim::Control::connect(&sill_core::session::socket_path(
                &a.session_id,
            )?)?;
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
    let after: Option<(u8, String)> = a
        .after
        .as_ref()
        .map(|s| serde_json::from_str(s))
        .transpose()
        .map_err(invalid)?;
    if after
        .as_ref()
        .is_some_and(|(kind, host)| *kind > 2 || host.len() > 4096)
    {
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
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(runtime.join(name))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io(e)),
        };
        if !file.metadata()?.is_file() {
            return Err(domain(
                "unsupported",
                "network denial log is not a regular file",
            ));
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
    let row = hub.with_store(|s| {
        Ok(s.session_exact(&a.session_id)?
            .ok_or_else(|| domain("not_found", "session not found"))?)
    })?;
    let home = sill_core::changes::home()?;
    if let Some(snapshot) = sill_core::session::policy_snapshot(&a.session_id)? {
        if snapshot.policy.network.mode != sill_core::policy::NetMode::None {
            let approved = session_hosts(&a.session_id)?;
            for suggestion in snapshot.suggestions {
                if !approved.contains(&suggestion.host) {
                    add(0, suggestion.host);
                }
            }
        }
    }
    let quiet = sill_core::policy::Policy::load(&row.policy, &home)
        .map(|p| p.network.quiet)
        .unwrap_or_else(|_| sill_core::policy::quiet_default());
    for host in quiet {
        if host.len() > 4096 {
            return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4096), "quiet host exceeds 4096 bytes"));
        }
        add(2, host);
    }
    let more = keys.len() > n;
    if more {
        keys.pop_last();
    }
    let next = if more {
        keys.last().map(serde_json::to_string).transpose()?
    } else {
        None
    };
    let agent_name = sill_core::agents::load(&home)
        .get(&row.agent)
        .map(|a| a.name.clone())
        .unwrap_or(row.agent);
    let mut out = api::NetworkDenied {
        next,
        hosts: vec![],
        answered: vec![],
        quiet: vec![],
        agent_name,
    };
    for (kind, host) in keys {
        match kind {
            0 => out.hosts.push(host),
            1 => out.answered.push(host),
            _ => out.quiet.push(host),
        }
    }
    Ok(out)
}
