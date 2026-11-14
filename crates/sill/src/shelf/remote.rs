//! Operations on the selected daemon's host; local paths are presentation values only.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use sill_core::frontend::{DirEntry, Metadata, Request};

type Responses = Mutex<BTreeMap<String, (std::time::Instant, serde_json::Value)>>;
static RESPONSES: OnceLock<Responses> = OnceLock::new();

fn cacheable(r: &Request) -> bool {
    matches!(r, Request::Home |
        Request::Memories { .. } | Request::AgentSettings { .. } | Request::Config | Request::Instructions |
        Request::Agents | Request::AgentMemory { .. } | Request::Registry | Request::Installed { .. } |
        Request::Available(_) | Request::Metadata { .. } | Request::List { .. } | Request::Head { .. } |
        Request::RepoRoot { .. } | Request::PolicyDesktop { .. } | Request::Policies | Request::Kept { .. } |
        Request::ProcessPolicy { .. } | Request::JailOutdated { .. })
}

pub fn invalidate() {
    if let Some(cache) = RESPONSES.get() { cache.lock().unwrap().clear(); }
    if let Some(metadata) = META.get() { metadata.lock().unwrap().clear(); }
    if let Some(images) = IMAGES.get() { images.lock().unwrap().clear(); }
}

pub fn request<T: DeserializeOwned>(r: Request) -> Result<T> {
    type Lane = OnceLock<Mutex<Option<Arc<sill_proto::Client>>>>;
    static DATA: Lane = OnceLock::new();
    static SLOW: Lane = OnceLock::new();
    let slow = matches!(&r, Request::Install(_) | Request::FetchRegistry | Request::Icon(_) | Request::Find { .. } | Request::Export);
    let cached = cacheable(&r);
    let key = format!("{}:{}", std::env::var("SILL_BOX").unwrap_or_default(), serde_json::to_string(&r)?);
    let cache = RESPONSES.get_or_init(Default::default);
    if cached {
        let mut cache = cache.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_millis(250));
        if let Some((_, value)) = cache.get(&key) { return Ok(serde_json::from_value(value.clone())?) }
    }
    let slot = if slow { &SLOW } else { &DATA }.get_or_init(Default::default);
    let connect = || -> Result<Arc<sill_proto::Client>> {
        let mut slot = slot.lock().unwrap();
        if let Some(client) = slot.as_ref() { return Ok(client.clone()) }
        let client = Arc::new(sill_proto::Client::connect_or_start()?);
        // Broadcasts on data connections are redundant; the shelf's event connection handles them.
        // Drain them so a long-lived settings connection cannot accumulate an unbounded queue.
        let events = client.clone();
        std::thread::Builder::new().name("sill-data-events".into()).spawn(move || {
            let notifications = events.notifications.lock().unwrap();
            while notifications.recv().is_ok() {}
        })?;
        *slot = Some(client.clone());
        Ok(client)
    };
    let client = connect()?;
    let timeout = if slow { sill_proto::client::CALL_TIMEOUT } else { sill_proto::client::QUICK_TIMEOUT };
    let mut result: Result<serde_json::Value> = client.call_within(sill_core::frontend::METHOD, &r, timeout);
    if result.as_ref().is_err_and(sill_proto::Client::is_closed) {
        let mut slot = slot.lock().unwrap();
        if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) { *slot = None; }
        drop(slot);
        // These can create processes or start an install. A lost reply is not permission to run
        // them twice; the caller reports the interrupted operation instead.
        if !matches!(&r, Request::OpenPty { .. } | Request::OpenPort { .. } | Request::HostLogin { .. } | Request::Install(_)) {
            result = connect()?.call_within(sill_core::frontend::METHOD, &r, timeout);
        }
    }
    let value = result?;
    if cached {
        cache.lock().unwrap().insert(key, (std::time::Instant::now(), value.clone()));
    } else if !matches!(&r, Request::Read { .. } | Request::Find { .. } | Request::Export | Request::Icon(_) |
        Request::EntryCount { .. } | Request::Entries { .. } | Request::Search { .. } | Request::GitStatus { .. } |
        Request::GitSummary { .. } | Request::ChangedLines { .. } | Request::Denied { .. } | Request::ProcList { .. } |
        Request::ProcTail { .. } | Request::ReadLog { .. } | Request::ResolveImage { .. } | Request::PtyCwd { .. }) {
        // Changes made by this frontend should be visible on its very next read.
        invalidate();
    }
    Ok(serde_json::from_value(value)?)
}
pub fn home() -> Result<PathBuf> { request(Request::Home) }
pub fn read(path: &Path) -> Result<Vec<u8>> { request(Request::Read { path: path.into() }) }
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> { let _: serde_json::Value = request(Request::Write { path: path.into(), bytes: bytes.into() })?; Ok(()) }
type MetaCache = Mutex<BTreeMap<PathBuf, (std::time::Instant, Metadata)>>;
static META: OnceLock<MetaCache> = OnceLock::new();
pub fn metadata(path: &Path) -> Result<Metadata> {
    if let Some((at, metadata)) = META.get_or_init(Default::default).lock().unwrap().get(path)
        && at.elapsed() < std::time::Duration::from_secs(2)
    { return Ok(metadata.clone()) }
    let metadata: Metadata = request(Request::Metadata { path: path.into() })?;
    META.get_or_init(Default::default).lock().unwrap().insert(path.into(), (std::time::Instant::now(), metadata.clone()));
    Ok(metadata)
}
pub fn is_dir(path: &Path) -> bool { metadata(path).is_ok_and(|m| m.directory) }
pub fn is_file(path: &Path) -> bool { metadata(path).is_ok_and(|m| m.file) }
pub fn exists(path: &Path) -> bool { metadata(path).is_ok() }
pub fn read_link(path: &Path) -> Result<PathBuf> { metadata(path)?.target.context("not a symbolic link") }
pub fn list(path: &Path) -> Result<Vec<DirEntry>> {
    let entries: Vec<DirEntry> = request(Request::List { path: path.into() })?;
    let mut cache = META.get_or_init(Default::default).lock().unwrap();
    let now = std::time::Instant::now();
    for entry in &entries { cache.insert(entry.path.clone(), (now, entry.metadata.clone())); }
    Ok(entries)
}
pub fn repo_root(path: &Path) -> Option<PathBuf> { request(Request::RepoRoot { path: path.into() }).ok().flatten() }
pub fn head(path: &Path) -> Option<(String, bool)> { request(Request::Head { path: path.into() }).ok().flatten() }
pub fn changed_lines(repo: &Path, rel: &Path) -> Option<Vec<(usize, usize)>> { request(Request::ChangedLines { repo: repo.into(), rel: rel.into() }).ok().flatten() }
pub fn find(bases: &[PathBuf], query: &str, cap: usize, visits: usize) -> (Vec<PathBuf>, bool) {
    request(Request::Find { bases: bases.into(), query: query.into(), cap, visits }).unwrap_or_default()
}

/// Cache image bytes received from the box under a frontend-owned temporary directory. GTK's
/// image widgets take a filename; this is a rendering cache, never a daemon-host path.
type Images = Mutex<BTreeMap<String, (std::time::Instant, PathBuf)>>;
static IMAGES: OnceLock<Images> = OnceLock::new();
pub fn image(path: &Path) -> Option<PathBuf> {
    let key = format!("{}:{}", std::env::var("SILL_BOX").unwrap_or_default(), path.display());
    if let Some((at, cached)) = IMAGES.get_or_init(Default::default).lock().unwrap().get(&key)
        && at.elapsed() < std::time::Duration::from_secs(2)
    { return Some(cached.clone()) }
    let cached = cache_image(&key, &read(path).ok()?, path.extension().and_then(|e| e.to_str()).unwrap_or("img"))?;
    IMAGES.get_or_init(Default::default).lock().unwrap().insert(key, (std::time::Instant::now(), cached.clone()));
    Some(cached)
}
pub fn cache_image(key: &str, bytes: &[u8], ext: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    static CACHE: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    let mut hash = std::collections::hash_map::DefaultHasher::new(); key.hash(&mut hash); bytes.hash(&mut hash);
    let cache_key = format!("{key}:{:016x}", hash.finish());
    if let Some(path) = cache.get(&cache_key) { return Some(path.clone()) }
    let dir = std::env::temp_dir().join(format!("sill-render-{}", std::process::id()));
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
    let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let path = dir.join(format!("{:016x}.{ext}", hash.finish()));
    std::fs::write(&path, bytes).ok()?;
    cache.insert(cache_key, path.clone());
    Some(path)
}

pub mod config {
    pub use sill_core::config::*;
    pub fn load() -> Config { super::request(super::Request::Config).unwrap_or_default() }
    pub fn update(f: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        let mut c: Config = super::request(super::Request::Config)?;
        let before = serde_json::to_value(&c)?;
        f(&mut c);
        let after = serde_json::to_value(c)?;
        let fields = after.as_object().unwrap().iter().filter(|(key, value)| before.get(key) != Some(value)).map(|(key, value)| (key.clone(), value.clone())).collect();
        let _: serde_json::Value = super::request(super::Request::PatchConfig { fields })?;
        Ok(())
    }
}
pub mod instructions {
    pub fn load_global() -> String { super::request(super::Request::Instructions).unwrap_or_default() }
    pub fn save_global(text: &str) -> anyhow::Result<()> { let _: serde_json::Value = super::request(super::Request::SetInstructions { text: text.into() })?; Ok(()) }
}
pub mod agents {
    pub use sill_core::agents::Agent;
    pub fn load(_home: &std::path::Path) -> std::collections::BTreeMap<String, Agent> { super::request(super::Request::Agents).unwrap_or_default() }
    pub fn save_override(_home: &std::path::Path, agent: &Agent) -> anyhow::Result<std::path::PathBuf> { super::request(super::Request::SaveAgent(agent.clone())) }
    pub fn remove_override(_home: &std::path::Path, id: &str) -> anyhow::Result<bool> { super::request(super::Request::RemoveAgent { id: id.into() }) }
    pub fn memory_dirs(agent: &Agent, _home: &std::path::Path) -> Vec<std::path::PathBuf> { super::request::<sill_core::frontend::AgentMemory>(super::Request::AgentMemory { id: agent.id.clone() }).map(|m| m.dirs).unwrap_or_default() }
    pub fn memory_files(agent: &Agent) -> usize { super::request::<sill_core::frontend::AgentMemory>(super::Request::AgentMemory { id: agent.id.clone() }).map(|m| m.files).unwrap_or_default() }
    pub fn forget_memory(agent: &Agent, _home: &std::path::Path) -> anyhow::Result<usize> { super::request(super::Request::ForgetAgentMemory { id: agent.id.clone() }) }
}
pub mod registry {
    pub use sill_core::registry::{Entry, Trust, builtin_for, is_tested, snapshot};
    pub fn merged(_home: &std::path::Path) -> (Vec<Entry>, Option<std::time::SystemTime>) {
        super::request::<(Vec<sill_core::frontend::RegistryEntry>, Option<std::time::SystemTime>)>(super::Request::Registry)
            .map(|(entries, when)| (entries.into_iter().map(Entry::from).collect(), when)).unwrap_or_default()
    }
    pub fn fetch(_home: &std::path::Path) -> anyhow::Result<()> { let _: serde_json::Value = super::request(super::Request::FetchRegistry)?; Ok(()) }
    pub fn installed(_home: &std::path::Path, id: &str) -> Option<sill_core::registry::Installed> { super::request(super::Request::Installed { id: id.into() }).ok().flatten() }
    pub fn available(agent: &sill_core::agents::Agent, _home: &std::path::Path) -> Option<std::path::PathBuf> { super::request(super::Request::Available(agent.clone())).ok().flatten() }
    pub fn install(_home: &std::path::Path, entry: &Entry, log: &mut dyn FnMut(String)) -> anyhow::Result<sill_core::agents::Agent> {
        use std::io::BufRead;
        let token: String = super::request(super::Request::Install(entry.clone()))?;
        let stream = sill_proto::transport::open_stream(&token)?;
        struct Stop(std::sync::Arc<dyn Fn() + Send + Sync>);
        impl Drop for Stop { fn drop(&mut self) { (self.0)(); } }
        let _stop = Stop(stream.interrupt);
        for line in std::io::BufReader::new(stream.reader).lines() {
            let value: serde_json::Value = serde_json::from_str(&line?)?;
            if let Some(line) = value.get("line").and_then(|v| v.as_str()) { log(line.into()); }
            if let Some(error) = value.get("error").and_then(|v| v.as_str()) { anyhow::bail!("{error}") }
            if let Some(agent) = value.get("agent") { super::invalidate(); return Ok(serde_json::from_value(agent.clone())?) }
        }
        anyhow::bail!("install stream closed before completion")
    }
    pub fn uninstall(_home: &std::path::Path, entry: &Entry) -> anyhow::Result<()> { let _: serde_json::Value = super::request(super::Request::Uninstall(entry.clone()))?; Ok(()) }
    pub fn fetch_icon(_home: &std::path::Path, entry: &Entry) -> Option<std::path::PathBuf> {
        let bytes: Option<Vec<u8>> = super::request(super::Request::Icon(entry.clone())).ok()?;
        super::cache_image(&format!("agent:{}", entry.id), &bytes?, "svg")
    }
}
