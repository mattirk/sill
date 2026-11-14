//! Public host operations. Resolve identifiers against daemon-owned configuration, never a
//! frontend-supplied command/distribution. Login/install jobs continue if their viewer leaves.
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sill_proto::{host as api, rpc::Error};
use std::collections::HashSet;
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
fn jobs() -> &'static Mutex<HashSet<String>> { static JOBS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new(); JOBS.get_or_init(Default::default) }
struct Job(String);
impl Job {
    fn reserve(key: String) -> Result<Self> {
        let mut jobs = jobs().lock().unwrap();
        if jobs.contains(&key) { return Err(domain("busy", "this host operation is already running")); }
        if jobs.len() >= api::MAX_JOBS { return Err(domain("limit_exceeded", "too many host jobs")); }
        jobs.insert(key.clone());
        Ok(Self(key))
    }
}
impl Drop for Job { fn drop(&mut self) { jobs().lock().unwrap().remove(&self.0); } }

fn source(entry: &sill_core::registry::Entry) -> &'static str {
    match entry.trust { sill_core::registry::Trust::Snapshot => "snapshot", sill_core::registry::Trust::Live => "live" }
}
fn summary(entry: &sill_core::registry::Entry) -> api::RegistryEntry {
    api::RegistryEntry { entry_id: entry.id.clone(), name: entry.name.clone(), version: entry.version.clone(), source: source(entry).into(), installable: entry.how().is_some(), newer: entry.newer.as_ref().map(|e| Box::new(summary(e))) }
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
fn start(key: String, run: impl FnOnce(&mut ProgressWriter) -> Result<String> + Send + 'static) -> Result<api::HostStream> {
    let job = Job::reserve(key)?;
    let (reader, writer) = UnixStream::pair()?;
    writer.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
    let token = super::streams::socket(reader)?;
    let spawned = std::thread::Builder::new().name("sill-host-job".into()).spawn(move || {
        let _job = job;
        let mut progress = ProgressWriter(Some(writer));
        let event = match run(&mut progress) {
            Ok(agent_id) => api::Progress::Completed { agent_id },
            Err(e) => api::Progress::Failed { code: "operation_failed".into(), message: text(format!("{e:#}")) },
        };
        progress.send(event);
    });
    if let Err(error) = spawned { super::streams::discard(&token); return Err(error.into()); }
    Ok(api::HostStream { token })
}

pub fn handle(hub: &Hub, method: &str, params: Value) -> Option<Result<Value>> {
    if !matches!(method, "_sill/streams.port" | "_sill/agents.login" | "_sill/registry.install" | "_sill/registry.list") { return None; }
    Some((|| {
        match method {
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
            "_sill/agents.login" => {
                let a: api::AgentLogin = args(params)?;
                valid_id(&a.agent_id)?;
                if a.method_id.is_empty() || a.method_id.len() > 256 || a.method_id.chars().any(char::is_control) { return Err(invalid("invalid method_id")); }
                let home = sill_core::changes::home()?;
                let agent = sill_core::agents::load(&home).remove(&a.agent_id).ok_or_else(|| domain("not_found", "agent not found"))?;
                Ok(json!(start(format!("login:{}", agent.id), move |progress| {
                    sill_core::login::run(&agent, &home, &a.method_id, |line| {
                        progress.send(match line {
                            sill_core::login::Line::Status(s) => api::Progress::Status { text: text(s) },
                            sill_core::login::Line::Stderr(s) => api::Progress::Stderr { text: text(s) },
                        });
                    })?;
                    Ok(agent.id)
                })?))
            }
            "_sill/registry.install" => {
                let a: api::RegistryInstall = args(params)?;
                let home = sill_core::changes::home()?;
                let entry = select(&home, &a)?;
                Ok(json!(start(format!("install:{}", entry.id), move |progress| {
                    let agent = sill_core::registry::install(&home, &entry, &mut |s| progress.send(api::Progress::Output { text: text(s) }))?;
                    Ok(agent.id)
                })?))
            }
            "_sill/registry.list" => {
                let a: api::RegistryList = args(params)?;
                let limit = a.limit.unwrap_or(64) as usize;
                if !(1..=128).contains(&limit) { return Err(invalid("limit must be 1–128")); }
                let home = sill_core::changes::home()?;
                let mut entries = sill_core::registry::merged(&home).0;
                entries.sort_by(|a,b| a.id.cmp(&b.id));
                let mut entries = entries.iter().filter(|e| a.after.as_ref().is_none_or(|id| e.id > *id)).peekable();
                let mut page = api::RegistryPage { entries: Vec::new(), next: None };
                let mut bytes = 0;
                while page.entries.len() < limit {
                    let Some(entry) = entries.next() else { break };
                    let entry = summary(entry);
                    let size = serde_json::to_vec(&entry)?.len();
                    if size > 4 * 1024 * 1024 { return Err(domain("limit_exceeded", "registry entry too large")); }
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
    fn selection_preserves_reviewed_metadata_and_refuses_unknown_sources() {
        let home = std::env::temp_dir().join(format!("sill-host-select-{}", std::process::id()));
        let entry = sill_core::registry::snapshot().agents.iter().find(|e| e.npm_lock.is_some()).unwrap();
        let mut request = api::RegistryInstall { entry_id: entry.id.clone(), version: entry.version.clone(), source: "snapshot".into(), allow_unreviewed: false };
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
        let clipped = text("€".repeat(api::MAX_PROGRESS_TEXT));
        assert!(clipped.len() <= api::MAX_PROGRESS_TEXT);
        assert!(clipped.ends_with('€'));
    }
}
