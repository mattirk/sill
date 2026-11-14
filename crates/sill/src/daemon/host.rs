//! Public host operations. Resolve identifiers against daemon-owned configuration, never a
//! frontend-supplied command/distribution. Login/install jobs continue if their viewer leaves.
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sill_proto::{host as api, rpc::Error};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use super::Hub;

fn domain(code: &'static str, message: &str) -> anyhow::Error {
    sill_core::sfp::DomainError { code, message: message.into(), details: None }.into()
}
fn invalid(message: &str) -> anyhow::Error { Error::invalid_params(message).into() }
fn args<T: DeserializeOwned>(value: Value) -> Result<T> { serde_json::from_value(value).map_err(|e| invalid(&e.to_string())) }
fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || id == "." || id == ".." || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')) {
        return Err(invalid("invalid agent/entry id"));
    }
    Ok(())
}
/// Running jobs by key, each with its operation when it has one.
fn jobs() -> &'static Mutex<HashMap<String, Option<String>>> { static JOBS: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new(); JOBS.get_or_init(Default::default) }
pub(super) struct Job(String);
impl Job {
    pub(super) fn reserve(key: String) -> Result<Self> { Self::reserve_for(key, None) }
    /// `busy` names the running job's operation, so another frontend can cancel a login it did not start.
    fn reserve_for(key: String, operation_id: Option<&str>) -> Result<Self> {
        let mut jobs = jobs().lock().unwrap();
        if let Some(running) = jobs.get(&key) {
            let details = running.as_ref().map(|id| json!({ "operation_id": id }));
            return Err(sill_core::sfp::DomainError { code: "busy", message: "this host operation is already running".into(), details }.into());
        }
        if jobs.len() >= api::MAX_JOBS { return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Capacity, None, "too many host jobs")); }
        jobs.insert(key.clone(), operation_id.map(str::to_owned));
        Ok(Self(key))
    }
}

/// A running login: whether it is cancelled, and the callback port its page reported.
#[derive(Default)]
struct LoginCtl { cancelled: AtomicBool, callback_port: Mutex<Option<u16>> }
fn logins() -> &'static Mutex<HashMap<String, Arc<LoginCtl>>> { static L: OnceLock<Mutex<HashMap<String, Arc<LoginCtl>>>> = OnceLock::new(); L.get_or_init(Default::default) }
/// Registered while its job runs, by operation.
struct LoginEntry(String, Arc<LoginCtl>);
impl LoginEntry {
    fn new(operation_id: &str) -> Self {
        let ctl = Arc::new(LoginCtl::default());
        logins().lock().unwrap().insert(operation_id.to_owned(), ctl.clone());
        Self(operation_id.to_owned(), ctl)
    }
    fn cancelled(&self) -> bool { self.1.cancelled.load(Ordering::Acquire) }
    /// A login line as progress; an `open_url`'s port is the one the callback relay may reach.
    fn progress(&self, line: sill_core::login::Line) -> api::Progress {
        match line {
            sill_core::login::Line::Status(s) => api::Progress::Status { text: text(s) },
            sill_core::login::Line::Stderr(s) => api::Progress::Stderr { text: text(s) },
            sill_core::login::Line::OpenUrl { url, callback_port } => {
                if callback_port.is_some() { *self.1.callback_port.lock().unwrap() = callback_port; }
                api::Progress::OpenUrl { url, callback_port }
            }
        }
    }
}
impl Drop for LoginEntry { fn drop(&mut self) { logins().lock().unwrap().remove(&self.0); } }
fn running_login(operation_id: &str) -> Result<Arc<LoginCtl>> {
    logins().lock().unwrap().get(operation_id).cloned().ok_or_else(|| domain("not_found", "no such login is running"))
}
impl Drop for Job { fn drop(&mut self) { jobs().lock().unwrap().remove(&self.0); } }

fn source(entry: &sill_core::registry::Entry) -> &'static str {
    match entry.trust { sill_core::registry::Trust::Snapshot => "snapshot", sill_core::registry::Trust::Live => "live" }
}
fn summary(entry: &sill_core::registry::Entry) -> api::RegistryEntry {
    api::RegistryEntry { entry_id: entry.id.clone(), name: entry.name.clone(), version: entry.version.clone(), source: source(entry).into(), installable: entry.how().is_some(),
        description:entry.description.clone(),repository:entry.repository.clone(),website:entry.website.clone(),
        install_kind:entry.how().map(str::to_owned),install_spec:entry.install_spec(),package_name:entry.package_name(),has_icon:entry.icon.is_some(),builtin_agent_id:sill_core::registry::builtin_for(entry).map(str::to_owned),
        newer: entry.newer.as_ref().map(|e| Box::new(summary(e))) }
}
fn select(home: &Path, a: &api::RegistryInstall) -> Result<sill_core::registry::Entry> {
    valid_id(&a.entry_id)?;
    if !matches!(a.source.as_str(), "snapshot" | "live") { return Err(invalid("source must be snapshot or live")); }
    let entries = sill_core::registry::merged(home).0;
    let found = entries.into_iter().flat_map(|mut e| {
        let newer = e.newer.take();
        std::iter::once(e).chain(newer.map(|n| *n))
    }).find(|e| e.id == a.entry_id && e.version == a.version && source(e) == a.source)
        .ok_or_else(|| domain("not_found", "entry version/source is no longer in the daemon registry; refresh before installing"))?;
    if found.trust == sill_core::registry::Trust::Live && !a.allow_unreviewed { return Err(domain("permission_denied", "live registry installs require allow_unreviewed")); }
    if found.how().is_none() { return Err(domain("unsupported", "entry cannot be installed on this platform")); }
    Ok(found)
}

/// Progress never blocks the host job indefinitely. A slow/disconnected reader loses its stream;
/// side effects continue and the client must treat EOF without a terminal event as unknown.
struct ProgressWriter(Option<UnixStream>);
impl ProgressWriter {
    fn send(&mut self, event: api::Progress) {
        if let Some(socket) = self.0.as_mut() {
            let result = writeln!(socket, "{}", serde_json::to_string(&event).expect("progress serialization"));
            if result.is_err() { self.0 = None; }
        }
    }
}
fn text(mut value: String) -> String {
    if value.len() > api::MAX_PROGRESS_TEXT {
        let mut end = api::MAX_PROGRESS_TEXT;
        while !value.is_char_boundary(end) { end -= 1; }
        value.truncate(end);
    }
    value
}
fn identity<T: serde::Serialize>(requested: &Option<String>, input: &T) -> Result<(String, Value)> {
    let id = requested.clone().unwrap_or_else(|| format!("{}{}{}{}", sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id(), sill_core::store::new_id()));
    let mut input = serde_json::to_value(input)?;
    input.as_object_mut().unwrap().remove("operation_id");
    Ok((id, input))
}
fn existing(hub: &Hub, id: &str, method: &str, input: &Value) -> Result<Option<api::JobStream>> {
    hub.with_store(|s| Ok(sill_core::operations::Journal::new(s.conn())?.existing(id, method, input)?.map(|operation| api::JobStream { token: None, operation })))
}
fn start(hub: &Hub, id: String, method: &str, input: Value, key: String, run: impl FnOnce(&mut ProgressWriter) -> Result<String> + Send + 'static) -> Result<api::JobStream> {
    let job = Job::reserve_for(key, Some(&id))?;
    let (reader, writer) = UnixStream::pair()?;
    writer.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
    let token = super::streams::socket(reader)?;
    // Open the worker's database before accepting or running any external effect.
    let accepted = (|| {
        let store = sill_core::store::Store::open_default()?;
        let (operation, fresh) = hub.with_store(|s| sill_core::operations::Journal::new(s.conn())?.begin(&id, method, &input))?;
        Ok::<_, anyhow::Error>((store, operation, fresh))
    })();
    let (store, operation, fresh) = match accepted {
        Ok(result) => result,
        Err(error) => { super::streams::discard(&token); return Err(error); }
    };
    if !fresh { super::streams::discard(&token); return Ok(api::JobStream { token: None, operation }); }
    let operation_id = id.clone();
    let spawned = std::thread::Builder::new().name("sill-host-job".into()).spawn(move || {
        let _job = job;
        let mut progress = ProgressWriter(Some(writer));
        let (agent_id, code, event) = match run(&mut progress) {
            Ok(agent_id) => (Some(agent_id.clone()), None, api::Progress::Completed { agent_id }),
            Err(e) => {
                let code = if e.downcast_ref::<sill_core::login::Cancelled>().is_some() { api::CANCELLED } else { "operation_failed" };
                (None, Some(code), api::Progress::Failed { code: code.into(), message: text(format!("{e:#}")) })
            }
        };
        let saved = sill_core::operations::Journal::new(store.conn()).and_then(|j| j.finish(&operation_id, agent_id.as_deref(), code));
        if let Err(error) = saved {
            // No terminal progress event may claim a durable outcome that wasn't recorded.
            eprintln!("sill host operation outcome persistence failed: {error:#}");
            return;
        }
        progress.send(event);
    });
    if let Err(error) = spawned {
        super::streams::discard(&token);
        hub.with_store(|s| sill_core::operations::Journal::new(s.conn())?.finish(&id, None, Some("not_started")))?;
        return Err(error.into());
    }
    Ok(api::JobStream { token: Some(token), operation })
}

pub fn handle(hub: &Hub, method: &str, params: Value) -> Option<Result<Value>> {
    if !matches!(method, "_sill/streams.port" | "_sill/streams.login_callback" | "_sill/operations.cancel" | "_sill/agents.login" | "_sill/agents.discover" | "_sill/agents.reset" | "_sill/registry.install" | "_sill/registry.list" | "_sill/operations.get") { return None; }
    Some((|| {
        match method {
            "_sill/operations.get" => {
                let a: api::OperationKey = args(params)?;
                hub.with_store(|s| Ok(json!(sill_core::operations::Journal::new(s.conn())?.get(&a.operation_id)?)))
            }
            "_sill/streams.port" => {
                let a: api::PortOpen = args(params)?;
                if !sill_core::session::valid_id(&a.session_id) || a.port == 0 { return Err(invalid("valid session_id and nonzero port required")); }
                hub.with_store(|s| {
                    if !s.sessions()?.iter().any(|s| s.id == a.session_id) { return Err(domain("not_found", "session not found")); }
                    Ok(())
                })?;
                let socket = sill_core::session::socket_path(&a.session_id)?;
                if !socket.exists() { return Err(domain("invalid_state", "session jail is not running")); }
                Ok(json!(api::HostStream { token: super::streams::port(&a.session_id, a.port)? }))
            }
            "_sill/streams.login_callback" => {
                let a: api::OperationKey = args(params)?;
                let login = running_login(&a.operation_id)?;
                let Some(port) = *login.callback_port.lock().unwrap() else { return Err(domain("invalid_state", "this login reported no callback port")) };
                let tcp = std::net::TcpStream::connect(("127.0.0.1", port))
                    .or_else(|_| std::net::TcpStream::connect(("::1", port)))
                    .map_err(|e| domain("invalid_state", &format!("nothing answers the login's callback port {port}: {e}")))?;
                Ok(json!(api::HostStream { token: super::streams::tcp(tcp)? }))
            }
            "_sill/operations.cancel" => {
                let a: api::OperationKey = args(params)?;
                running_login(&a.operation_id)?.cancelled.store(true, Ordering::Release);
                Ok(json!(sill_proto::Empty {}))
            }
            "_sill/agents.login" => {
                let a: api::AgentLogin = args(params)?;
                let (id, input) = identity(&a.operation_id, &a)?;
                if let Some(result) = existing(hub, &id, method, &input)? { return Ok(json!(result)); }
                valid_id(&a.agent_id)?;
                if a.method_id.is_empty() || a.method_id.len() > 256 || a.method_id.chars().any(char::is_control) { return Err(invalid("invalid method_id")); }
                let home = sill_core::changes::home()?;
                let agent = sill_core::agents::load(&home)?.remove(&a.agent_id).ok_or_else(|| domain("not_found", "agent not found"))?;
                agent.ready(&home)?;
                let ghosts = hub.with_store(|s| Ok(sill_core::session::ghost_credential_homes(&s.sessions()?, &agent.id)))?;
                let login = LoginEntry::new(&id);
                Ok(json!(start(hub, id, method, input, format!("login:{}", agent.id), move |progress| {
                    sill_core::login::run_cancellable(&agent, &home, &ghosts, &a.method_id, &|| login.cancelled(), |line| progress.send(login.progress(line)))?;
                    Ok(agent.id)
                })?))
            }
            "_sill/agents.discover" => {
                let a: api::AgentDiscover = args(params)?;
                let (id, input) = identity(&a.operation_id, &a)?;
                if let Some(result) = existing(hub, &id, method, &input)? { return Ok(json!(result)); }
                valid_id(&a.agent_id)?;
                let home = sill_core::changes::home()?;
                let agent = sill_core::agents::load(&home)?.remove(&a.agent_id).ok_or_else(|| domain("not_found", "agent not found"))?;
                if sill_core::registry::installed(&home, &agent.id).is_none() { return Err(domain("invalid_state", "the agent is not installed by sill; discovery runs the installed command")); }
                if let Some(m) = &a.method_id && (m.is_empty() || m.len() > 256 || m.chars().any(char::is_control)) { return Err(invalid("invalid method_id")); }
                // the same key as its install: never two first starts, or one during an install
                let login = a.method_id.is_some().then(|| LoginEntry::new(&id));
                Ok(json!(start(hub, id, method, input, format!("install:{}", agent.id), move |progress| {
                    match &a.method_id {
                        None => { sill_core::registry::discover(&home, &agent, &mut |s| progress.send(api::Progress::Output { text: text(s) }))?; }
                        Some(m) => {
                            let login = login.as_ref().expect("registered for a login");
                            sill_core::login::setup_cancellable(&agent, &home, m, &|| login.cancelled(), |line| progress.send(login.progress(line)))?;
                        }
                    }
                    Ok(agent.id)
                })?))
            }
            "_sill/agents.reset" => {
                let a: api::AgentReset = args(params)?;
                valid_id(&a.agent_id)?;
                let home = sill_core::changes::home()?;
                let agent = sill_core::agents::load(&home)?.remove(&a.agent_id).ok_or_else(|| domain("not_found", "agent not found"))?;
                // Reset must not race a daemon host login that will copy credentials back later.
                let _job = Job::reserve(format!("login:{}", agent.id))?;
                // first the running sessions: a running agent holds its login and writes to its home
                let stopped = hub.stop_sessions_of(&agent.id);
                let mut homes = vec![sill_core::session::agent_home(&home, &agent.id)];
                homes.extend(hub.ghost_homes_of(&agent.id));
                let mut credentials = hub.credentials.lock().unwrap();
                let removed = sill_core::agents::reset_homes(&homes)? + sill_core::agents::remove_api_keys(&home, &agent.id)?;
                *credentials = Default::default();
                Ok(json!(api::AgentResetDone { removed: removed as u32, stopped }))
            }
            "_sill/registry.install" => {
                let a: api::RegistryInstall = args(params)?;
                let (id, input) = identity(&a.operation_id, &a)?;
                if let Some(result) = existing(hub, &id, method, &input)? { return Ok(json!(result)); }
                let home = sill_core::changes::home()?;
                let entry = select(&home, &a)?;
                Ok(json!(start(hub, id, method, input, format!("install:{}", entry.id), move |progress| {
                    let agent = sill_core::registry::install(&home, &entry, &mut |s| progress.send(api::Progress::Output { text: text(s) }))?;
                    Ok(agent.id)
                })?))
            }
            "_sill/registry.list" => {
                let a: api::RegistryList = args(params)?;
                let limit = a.limit.unwrap_or(64) as usize;
                if !(1..=128).contains(&limit) { return Err(invalid("limit must be 1–128")); }
                let home = sill_core::changes::home()?;
                let (mut entries, fetched_at) = sill_core::registry::merged(&home);
                entries.sort_by(|a,b| a.id.cmp(&b.id));
                let mut entries = entries.iter().filter(|e| a.after.as_ref().is_none_or(|id| e.id > *id)).peekable();
                let mut page = api::RegistryPage { entries: Vec::new(), next: None, fetched_at:fetched_at.and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|t|t.as_secs().to_string()) };
                let mut bytes = 0;
                while page.entries.len() < limit {
                    let Some(entry) = entries.next() else { break };
                    let entry = summary(entry);
                    let size = serde_json::to_vec(&entry)?.len();
                    if size > 4 * 1024 * 1024 { return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Record, Some(4194304), "registry entry too large")); }
                    if bytes + size > 4 * 1024 * 1024 { page.next = page.entries.last().map(|e| e.entry_id.clone()); break; }
                    bytes += size;
                    page.entries.push(entry);
                }
                if entries.peek().is_some() && page.next.is_none() { page.next = page.entries.last().map(|e| e.entry_id.clone()); }
                Ok(json!(page))
            }
            _ => unreachable!(),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_summary_preserves_ui_metadata_without_install_manifests() {
        let entry=sill_core::registry::snapshot().agents.iter().find(|e|e.npm_lock.is_some()).unwrap();
        let wire=summary(entry);
        assert_eq!(wire.source,"snapshot");
        assert_eq!(wire.description,entry.description);
        assert_eq!(wire.install_kind.as_deref(),entry.how());
        assert_eq!(wire.install_spec,entry.install_spec());
        assert_eq!(wire.package_name,entry.package_name());
        assert_eq!(wire.builtin_agent_id.as_deref(),sill_core::registry::builtin_for(entry));
        let json=serde_json::to_value(wire).unwrap();
        assert!(json.get("npm_lock").is_none());assert!(json.get("distribution").is_none());
    }
    #[test]
    fn selection_preserves_reviewed_metadata_and_refuses_unknown_sources() {
        let home = std::env::temp_dir().join(format!("sill-host-select-{}", std::process::id()));
        let entry = sill_core::registry::snapshot().agents.iter().find(|e| e.npm_lock.is_some()).unwrap();
        let mut request = api::RegistryInstall { operation_id: None, entry_id: entry.id.clone(), version: entry.version.clone(), source: "snapshot".into(), allow_unreviewed: false };
        let resolved = select(&home, &request).unwrap();
        assert_eq!(resolved.trust, sill_core::registry::Trust::Snapshot);
        assert_eq!(resolved.npm_lock, entry.npm_lock);
        request.source = "future".into();
        assert!(select(&home, &request).unwrap_err().downcast_ref::<Error>().is_some());
        request.source = "live".into();
        assert_eq!(select(&home, &request).unwrap_err().downcast_ref::<sill_core::sfp::DomainError>().unwrap().code, "not_found");
    }
    #[test]
    fn a_stalled_progress_reader_is_dropped_instead_of_blocking_the_job() {
        let (_reader, writer) = UnixStream::pair().unwrap();
        writer.set_write_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let mut progress = ProgressWriter(Some(writer));
        let started = std::time::Instant::now();
        for _ in 0..256 {
            progress.send(api::Progress::Output { text: "x".repeat(api::MAX_PROGRESS_TEXT) });
            if progress.0.is_none() { break; }
        }
        assert!(progress.0.is_none(), "the stalled reader still holds the writer");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        progress.send(api::Progress::Completed { agent_id: "fixture".into() });
    }
    #[test]
    fn job_slots_are_bounded_exclusive_and_released_on_drop() {
        let mut held: Vec<Job> = (0..api::MAX_JOBS).map(|n| Job::reserve(format!("test:{n}")).unwrap()).collect();
        assert_eq!(Job::reserve("test:0".into()).err().unwrap().downcast_ref::<sill_core::sfp::DomainError>().unwrap().code, "busy");
        assert_eq!(Job::reserve("test:extra".into()).err().unwrap().downcast_ref::<sill_core::sfp::DomainError>().unwrap().code, "limit_exceeded");
        held.pop();
        let replacement = Job::reserve("test:extra".into()).unwrap();
        drop((replacement, held));
        assert!(jobs().lock().unwrap().is_empty());
        let _running = Job::reserve_for("test:login".into(), Some("op-1")).unwrap();
        let busy = Job::reserve("test:login".into()).err().unwrap();
        assert_eq!(busy.downcast_ref::<sill_core::sfp::DomainError>().unwrap().details, Some(json!({"operation_id": "op-1"})));
        let clipped = text("€".repeat(api::MAX_PROGRESS_TEXT));
        assert!(clipped.len() <= api::MAX_PROGRESS_TEXT);
        assert!(clipped.ends_with('€'));
    }
}
