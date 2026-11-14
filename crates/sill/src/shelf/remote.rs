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
    matches!(r, Request::Config | Request::Instructions | Request::PolicyDesktop { .. } | Request::Policies)
}

pub fn invalidate() {
    if let Some(cache) = RESPONSES.get() { cache.lock().unwrap().clear(); }
    if let Some(metadata) = META.get() { metadata.lock().unwrap().clear(); }
    if let Some(images) = IMAGES.get() { images.lock().unwrap().clear(); }
}

fn read_only(r:&Request)->bool {
    matches!(r, Request::Config | Request::Instructions | Request::PolicyText {..} | Request::Denied {..} |
        Request::ProcTail {..} | Request::Policies | Request::PolicyDesktop {..})
}

fn public_request(r:&Request)->Result<(&'static str,serde_json::Value,bool)> {
    use serde_json::json;
    let request=match r {
        Request::Denied {session}=>("_sill/policy.denied",json!({"session_id":session}),false),
        Request::RememberDenied {session,host}=>("_sill/policy.remember_denied",json!({"session_id":session,"host":host}),false),
        Request::AllowHost {session,host}=>("_sill/policy.allow_host",json!({"session_id":session,"host":host}),false),
        Request::Policies=>("_sill/policy.list",json!({}),false),
        Request::PolicyDesktop {policy}=>("_sill/policy.get",json!({"name":policy}),false),
        Request::RefreshSnapshot=>("_sill/memory.refresh",json!({}),false),
        Request::Config=>("_sill/settings.get",json!({}),false),
        Request::PatchConfig {fields} => ("_sill/settings.patch",json!({"changes":fields.iter().map(|(k,v)|json!({"type":k,"value":v})).collect::<Vec<_>>()}),false),
        Request::Instructions=>("_sill/settings.instructions.get",json!({}),true),
        Request::SetInstructions {text}=>("_sill/settings.instructions.set",json!({"text":text}),false),
        Request::PolicyText {name}=>("_sill/policy.get",json!({"name":name}),true),
        Request::ProcTail {session,name,lines}=>("_sill/procs.tail",json!({"session_id":session,"name":name,"lines":lines}),true),
        Request::ProcStop {session,name}=>("_sill/procs.stop",json!({"session_id":session,"name":name}),false),
        Request::ProcDismiss {session,name}=>("_sill/procs.dismiss",json!({"session_id":session,"name":name}),false),
    };Ok(request)
}

pub fn request<T: DeserializeOwned>(r: Request) -> Result<T> {
    if let Request::Policies=&r {
        let mut all=Vec::new();let mut after=None;
        loop {let page:sill_proto::admin::Names=public_call("_sill/policy.list",sill_proto::admin::NamesList {after,limit:Some(128)})?;all.extend(page.names);after=page.next;if after.is_none(){return Ok(serde_json::from_value(serde_json::json!(all))?);}}
    }
    if let Request::Denied {session}=&r {
        let mut all=sill_core::frontend::Denied {hosts:vec![],answered:vec![],quiet:vec![],agent_name:String::new()};let mut after=None;
        loop {let page:sill_proto::admin::NetworkDenied=public_call("_sill/policy.denied",sill_proto::admin::DeniedList {session_id:session.clone(),after,limit:Some(128)})?;
            all.hosts.extend(page.hosts);all.answered.extend(page.answered);all.quiet.extend(page.quiet);all.agent_name=page.agent_name;after=page.next;if after.is_none(){return Ok(serde_json::from_value(serde_json::json!(all))?);}
        }
    }

    type Lane = OnceLock<Mutex<Option<Arc<sill_proto::Client>>>>;
    static DATA: Lane = OnceLock::new();
    let cached = cacheable(&r);
    let key = format!("{}:{}", std::env::var("SILL_BOX").unwrap_or_default(), serde_json::to_string(&r)?);
    let cache = RESPONSES.get_or_init(Default::default);
    if cached {
        let mut cache = cache.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < std::time::Duration::from_millis(250));
        if let Some((_, value)) = cache.get(&key) { return Ok(serde_json::from_value(value.clone())?) }
    }
    let slot = DATA.get_or_init(Default::default);
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
    let timeout = sill_proto::client::QUICK_TIMEOUT;
    let (method, params, wrapped_text) = public_request(&r)?;
    let mut result: Result<serde_json::Value> = client.call_within(method, &params, timeout);
    if result.as_ref().is_err_and(sill_proto::Client::is_closed) {
        let mut slot = slot.lock().unwrap();
        if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) { *slot = None; }
        drop(slot);
        // Losing a reply never proves that a mutation did not run.
        if read_only(&r) { result = connect()?.call_within(method, &params, timeout); }
    }
    let value = result?;
    let value = if wrapped_text { value["text"].clone() } else { match &r {
        Request::AllowHost {..}=>value["agent_name"].clone(),
        Request::Policies=>value["names"].clone(),
        Request::PolicyDesktop {..}=>serde_json::json!([value["x11"],value["dbus"]]),
        _=>value,
    } };
    if cached {
        cache.lock().unwrap().insert(key, (std::time::Instant::now(), value.clone()));
    } else if !read_only(&r) {
        // Changes made by this frontend should be visible on its very next read.
        invalidate();
    }
    Ok(serde_json::from_value(value)?)
}
fn public_call<T: DeserializeOwned>(method: &str, params: impl serde::Serialize) -> Result<T> {
    // Client connections own no filesystem state. Mutation calls are sent exactly once.
    let c = sill_proto::Client::connect_or_start()?;
    c.call(method, params)
}
pub fn home() -> Result<PathBuf> { public_call::<sill_proto::sessions::HostPath>("_sill/files.home", sill_proto::Empty::default())?.to_path() }
pub fn read(path: &Path) -> Result<Vec<u8>> {
    use sill_proto::{admin as a, sessions::HostPath};
    let mut bytes=Vec::new(); let mut offset="0".to_owned();let mut revision=None;
    loop {
        let page:a::FileBytes=public_call("_sill/files.read",a::FileRead {path:HostPath::from_path(path),offset:offset.clone(),length:None,revision})?;
        bytes.extend(a::decode_bytes(&page.bytes_base64)?);
        anyhow::ensure!(bytes.len()<=20_000_000,"file is too large to display");
        if page.eof { return Ok(bytes); }
        anyhow::ensure!(page.next_offset!=offset,"file read did not advance");offset=page.next_offset;revision=Some(page.revision);
    }
}
pub fn export_memory()->Result<Vec<sill_core::frontend::ExportFile>> {
    let c=sill_proto::Client::connect_or_start()?;let mut files=Vec::new();
    for session in c.list_sessions()? {if session.ghost {continue;}files.push(sill_core::frontend::ExportFile {name:format!("{}.md",session.session_id),text:c.memory_text(session.session_id,sill_proto::memory::Format::Export,None)?});}Ok(files)
}
pub fn processes(session:&str)->Result<Vec<sill_core::shim::Proc>> {
    use sill_proto::admin as a;
    let mut all=Vec::new();let mut after=None;
    loop {let page:a::ProcessesPage=public_call("_sill/procs.list",a::ProcessesList {session_id:session.into(),after,limit:Some(128)})?;
        for p in page.processes {all.push(sill_core::shim::Proc {name:p.name,argv:p.argv,cwd:p.cwd.to_path()?,port:p.port,pid:p.pid,started:p.started.parse()?,state:if p.state=="exited" {serde_json::json!({"exited":p.exit_code})}else{serde_json::json!(p.state)},last_line:p.last_line});}
        after=page.next;if after.is_none(){return Ok(all);}
    }
}
pub fn session_image(session:&str,src:&str)->Result<Vec<u8>> {
    use sill_proto::{admin as a,sessions::HostPath};
    let src=src.strip_prefix("file://").unwrap_or(src);anyhow::ensure!(!src.contains("://"),"not a session file");
    let p=PathBuf::from(src);let p=if p.is_absolute(){p}else{home()?.join(p)};
    let mut bytes=Vec::new();let mut offset="0".to_owned();let mut revision=None;
    loop {let page:a::FileBytes=public_call("_sill/files.image",a::SessionImage {session_id:session.into(),path:HostPath::from_path(&p),offset:offset.clone(),length:None,revision})?;
        bytes.extend(a::decode_bytes(&page.bytes_base64)?);anyhow::ensure!(bytes.len()<=20_000_000,"image is too large");if page.eof{return Ok(bytes);}anyhow::ensure!(page.next_offset!=offset,"image read did not advance");offset=page.next_offset;revision=Some(page.revision);
    }
}
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    use sill_proto::{admin as a, sessions::HostPath};
    let _:serde_json::Value=public_call("_sill/files.write",a::FileWrite {path:HostPath::from_path(path),bytes_base64:a::encode_bytes(bytes)})?;
    invalidate(); Ok(())
}
fn local_metadata(m:sill_proto::admin::FileMetadata)->Result<Metadata> {
    Ok(Metadata {file:m.file,directory:m.directory,symlink:m.symlink,target:m.target.map(|p|p.to_path()).transpose()?,length:m.length.parse()?,readonly:m.readonly})
}
type MetaCache = Mutex<BTreeMap<PathBuf, (std::time::Instant, Metadata)>>;
static META: OnceLock<MetaCache> = OnceLock::new();
pub fn metadata(path: &Path) -> Result<Metadata> {
    if let Some((at, metadata)) = META.get_or_init(Default::default).lock().unwrap().get(path)
        && at.elapsed() < std::time::Duration::from_secs(2)
    { return Ok(metadata.clone()) }
    let metadata=local_metadata(public_call("_sill/files.metadata",sill_proto::admin::FileKey {path:sill_proto::sessions::HostPath::from_path(path)})?)?;
    META.get_or_init(Default::default).lock().unwrap().insert(path.into(), (std::time::Instant::now(), metadata.clone()));
    Ok(metadata)
}
pub fn is_dir(path: &Path) -> bool { metadata(path).is_ok_and(|m| m.directory) }
pub fn is_file(path: &Path) -> bool { metadata(path).is_ok_and(|m| m.file) }
pub fn exists(path: &Path) -> bool { metadata(path).is_ok() }
pub fn read_link(path: &Path) -> Result<PathBuf> { metadata(path)?.target.context("not a symbolic link") }
pub fn list(path: &Path) -> Result<Vec<DirEntry>> {
    let mut entries=Vec::new();let mut after=None;
    loop {
        let page:sill_proto::admin::FilesPage=public_call("_sill/files.list",sill_proto::admin::FilesList {path:sill_proto::sessions::HostPath::from_path(path),after,limit:Some(128)})?;
        for e in page.entries {entries.push(DirEntry {path:e.path.to_path()?,metadata:local_metadata(e.metadata)?});}
        after=page.next;if after.is_none(){break;}
    }
    let mut cache = META.get_or_init(Default::default).lock().unwrap();
    let now = std::time::Instant::now();
    for entry in &entries { cache.insert(entry.path.clone(), (now, entry.metadata.clone())); }
    Ok(entries)
}
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let r: sill_proto::discovery::Root = super::client::call("_sill/repos.root", sill_proto::discovery::PathKey { path: sill_proto::sessions::HostPath::from_path(path) }).ok()?;
    r.path?.to_path().ok()
}
pub fn head(path: &Path) -> Option<(String, bool)> {
    let r: sill_proto::discovery::HeadResult = super::client::call("_sill/repos.head", sill_proto::discovery::PathKey { path: sill_proto::sessions::HostPath::from_path(path) }).ok()?;
    r.head.map(|h| (h.label,h.detached))
}
pub fn changed_lines(repo: &Path, rel: &Path) -> Option<Vec<(usize, usize)>> {
    let r: sill_proto::discovery::Lines = super::client::call("_sill/repos.changed_lines", sill_proto::discovery::ChangedLines { repo: sill_proto::sessions::HostPath::from_path(repo), path: sill_proto::sessions::HostPath::from_path(&repo.join(rel)) }).ok()?;
    r.ranges?.into_iter().map(|r|Some((r.start.parse().ok()?,r.end.parse().ok()?))).collect()
}
pub fn find(bases: &[PathBuf], query: &str, cap: usize, visits: usize) -> (Vec<PathBuf>, bool) {
    let result = (|| -> Result<_> {
        let r: sill_proto::discovery::Found = super::client::call("_sill/files.find", sill_proto::discovery::Find { bases:bases.iter().map(|p|sill_proto::sessions::HostPath::from_path(p)).collect(),query:query.into(),limit:Some(cap.clamp(1,1000) as u16),visits:Some(visits.clamp(1,100000) as u32) })?;
        Ok((r.paths.iter().map(|p|p.to_path()).collect::<Result<Vec<_>>>()?,r.truncated))
    })();
    result.unwrap_or_default()
}
pub fn git_status(repo: &Path) -> Result<Vec<(PathBuf,String)>> {
    let mut out=Vec::new();let mut after=None;
    loop {
        let page:sill_proto::discovery::StatusPage=super::client::call("_sill/repos.status",sill_proto::discovery::Status {repo:sill_proto::sessions::HostPath::from_path(repo),after,limit:Some(128)})?;
        for entry in page.entries {out.push((entry.path.to_path()?.strip_prefix(repo)?.to_path_buf(),entry.status));}
        after=page.next;if after.is_none(){return Ok(out);}
    }
}
pub fn git_summary(path: &Path) -> Option<(u64,u64,u64)> {
    let r:sill_proto::discovery::SummaryResult=super::client::call("_sill/repos.summary",sill_proto::discovery::PathKey{path:sill_proto::sessions::HostPath::from_path(path)}).ok()?;
    let s=r.summary?;Some((s.insertions.parse().ok()?,s.deletions.parse().ok()?,s.untracked.parse().ok()?))
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
    pub fn load(_home: &std::path::Path) -> std::collections::BTreeMap<String, Agent> {
        let load=||->anyhow::Result<std::collections::BTreeMap<String,Agent>> {
            let mut all=std::collections::BTreeMap::new();let mut after=None;
            loop {let page:sill_proto::agent_admin::Page=super::public_call("_sill/agents.list",sill_proto::agent_admin::List {after,limit:Some(128)})?;
                for a in page.agents {let a:Agent=serde_json::from_value(serde_json::to_value(a)?)?;all.insert(a.id.clone(),a);}
                after=page.next;if after.is_none(){return Ok(all);}
            }
        };load().unwrap_or_default()
    }
    pub fn save_override(_home: &std::path::Path, agent: &Agent) -> anyhow::Result<std::path::PathBuf> { { let a:sill_proto::agent_admin::Definition=serde_json::from_value(serde_json::to_value(agent)?)?; let saved:sill_proto::agent_admin::Saved=super::public_call("_sill/agents.save",a)?;super::invalidate();saved.path.to_path() } }
    pub fn remove_override(_home: &std::path::Path, id: &str) -> anyhow::Result<bool> { { let r:sill_proto::agent_admin::Removed=super::public_call("_sill/agents.remove",sill_proto::agent_admin::AgentKey {agent_id:id.into()})?;super::invalidate();Ok(r.removed) } }
}
pub mod registry {
    use anyhow::Context;
    pub use sill_core::registry::{Trust, is_tested, snapshot};
    pub use sill_core::frontend::RegistryEntry as Entry;
    pub fn builtin_for(entry:&Entry)->Option<&str> {entry.builtin_agent_id.as_deref()}
    fn selector(e:&Entry)->sill_proto::agent_admin::EntryKey {sill_proto::agent_admin::EntryKey {entry_id:e.id.clone(),version:e.version.clone(),source:match e.trust {Trust::Snapshot=>"snapshot",Trust::Live=>"live"}.into()}}
    pub fn merged(_home: &std::path::Path) -> (Vec<Entry>, Option<std::time::SystemTime>) {
        let load=||->anyhow::Result<_> {
            let mut entries=Vec::new();let mut after=None;let mut fetched=None;
            loop {
                let page:sill_proto::host::RegistryPage=super::public_call("_sill/registry.list",sill_proto::host::RegistryList {after,limit:Some(128)})?;
                if let Some(seconds)=page.fetched_at {fetched=std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds.parse()?));}
                entries.extend(page.entries.into_iter().map(Entry::try_from).collect::<anyhow::Result<Vec<_>>>()?);
                after=page.next;if after.is_none(){return Ok((entries,fetched));}
            }
        };
        load().unwrap_or_else(|error| {eprintln!("load agent registry: {error:#}");(Vec::new(),None)})
    }

    pub fn fetch(_home: &std::path::Path) -> anyhow::Result<()> { let _:serde_json::Value=super::public_call("_sill/registry.refresh",sill_proto::Empty::default())?;super::invalidate();Ok(()) }
    pub fn installed(_home: &std::path::Path, id: &str) -> Option<sill_core::registry::Installed> { let i=super::public_call::<sill_proto::agent_admin::Installation>("_sill/registry.installed",sill_proto::agent_admin::AgentKey {agent_id:id.into()}).ok()?.installed?;Some(sill_core::registry::Installed {version:i.version,kind:i.kind,command:i.command,env:i.env,trust:sill_core::frontend::registry_trust(&i.source).ok()?}) }
    pub fn available(agent: &sill_core::agents::Agent, _home: &std::path::Path) -> Option<std::path::PathBuf> { super::public_call::<sill_proto::agent_admin::Available>("_sill/agents.available",sill_proto::agent_admin::AgentKey {agent_id:agent.id.clone()}).ok()?.path.and_then(|p|p.to_path().ok()) }
    pub fn install(home: &std::path::Path, entry: &Entry, log: &mut dyn FnMut(String)) -> anyhow::Result<sill_core::agents::Agent> {
        use std::io::BufRead;
        let opened: sill_proto::host::HostStream = super::super::client::call("_sill/registry.install", sill_proto::host::RegistryInstall {
            operation_id: None,
            entry_id: entry.id.clone(), version: entry.version.clone(),
            source: match entry.trust { Trust::Snapshot => "snapshot", Trust::Live => "live" }.into(),
            allow_unreviewed: entry.trust == Trust::Live,
        })?;
        let stream = sill_proto::transport::open_stream(&opened.token)?;
        struct Stop(std::sync::Arc<dyn Fn() + Send + Sync>);
        impl Drop for Stop { fn drop(&mut self) { (self.0)(); } }
        let _stop = Stop(stream.interrupt);
        for line in std::io::BufReader::new(stream.reader).lines() {
            let event: sill_proto::host::Progress = serde_json::from_str(&line?)?;
            match event {
                sill_proto::host::Progress::Output { text } => log(text),
                sill_proto::host::Progress::Failed { message, .. } => anyhow::bail!("{message}"),
                sill_proto::host::Progress::Completed { agent_id } => {
                    super::invalidate();
                    return super::agents::load(home).remove(&agent_id).context("installed agent not found");
                }
                _ => {}
            }
        }
        anyhow::bail!("installation connection closed; outcome unknown. Check the agent before retrying")
    }
    pub fn uninstall(_home: &std::path::Path, entry: &Entry) -> anyhow::Result<()> { let _:serde_json::Value=super::public_call("_sill/registry.uninstall",selector(entry))?;super::invalidate(); Ok(()) }
    pub fn fetch_icon(_home: &std::path::Path, entry: &Entry) -> Option<std::path::PathBuf> {
        let icon:sill_proto::agent_admin::Icon=super::public_call("_sill/registry.icon",selector(entry)).ok()?;
        let bytes=sill_proto::admin::decode_bytes(&icon.bytes_base64?).ok()?;
        super::cache_image(&format!("agent:{}", entry.id), &bytes, "svg")
    }
}
