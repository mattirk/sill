//! Host data access for frontends. The renderer never opens the daemon's database or paths.
use std::path::Path;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sill_core::frontend::{DirEntry, ExportFile, Metadata, Request};
use super::Hub;

pub fn dispatch(hub: &Hub, params: Value) -> Result<Value> {
    let request: Request = serde_json::from_value(params)?;
    let home = sill_core::changes::home()?;
    let value = match request {
        Request::Home => json!(home),
        Request::Sessions => hub.with_store(|s| Ok(json!(s.sessions()?)))?,
        Request::Pins { session, global } => hub.with_store(|s| Ok(json!(if global { s.jail_pins(&session)? } else { s.pins(&session)? })))?,
        Request::GlobalPins => hub.with_store(|s| Ok(json!(s.global_pins()?)))?,
        Request::Entries { session, before, limit } => hub.with_store(|s| Ok(json!(s.entries_before(&session, before, limit.min(50_000))?)))?,
        Request::EntryCount { session } => hub.with_store(|s| Ok(json!(s.entry_count(&session)?)))?,
        Request::Memories { session } => hub.with_store(|s| Ok(json!(s.memories(&session)?)))?,
        Request::AgentSettings { agent } => hub.with_store(|s| Ok(json!(s.agent_settings(&agent))))?,
        Request::Search { text, limit } => hub.with_store(|s| {
            let q = sill_memory::Query::parse(&text, &home);
            Ok(json!(sill_memory::search(s.conn(), &q, sill_core::store::now(), limit.min(500))?.into_iter().map(|h| h.session.id).collect::<Vec<_>>()))
        })?,
        Request::Export => hub.with_store(|s| {
            let files = sill_memory::sessions(s.conn())?.into_iter().map(|session| Ok(ExportFile {
                name: sill_memory::export_name(&session),
                text: sill_memory::export_markdown(s.conn(), &session, &sill_core::memory::kept_of(&session.id), &home)?,
            })).collect::<Result<Vec<_>>>()?;
            Ok(json!(files))
        })?,
        Request::Recall { key } => hub.with_store(|store| {
            let info = sill_memory::find_session(store.conn(), &key)?.context("no such session")?;
            let session = store.sessions()?.into_iter().find(|s| s.id == info.id).context("no such session")?;
            let pins = store.pins(&session.id)?;
            let block = sill_core::memory::recall(store, &session, &pins, &home, sill_core::config::load().memory_recall());
            let scope = sill_memory::Scope::resolve(&session.memory_scope, !pins.is_empty());
            Ok(json!(format!("scope: {}{}\n{}\n", scope.id(), if session.memory_scope.is_empty() { " (the default)" } else { "" }, if block.is_empty() { "(nothing: memory is off)" } else { &block })))
        })?,
        Request::PolicyText { name } => json!(toml::to_string_pretty(&sill_core::policy::Policy::load(&name, &home)?)?),
        Request::Config => json!(sill_core::config::load()),
        Request::SetConfig(config) => { sill_core::config::save(&config)?; json!(null) },
        Request::PatchConfig { fields } => {
            static CONFIG: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = CONFIG.lock().unwrap();
            let mut value = serde_json::to_value(sill_core::config::load())?;
            let object = value.as_object_mut().context("settings are not an object")?;
            for (key, value) in fields {
                if !object.contains_key(&key) { bail!("unknown setting {key}"); }
                object.insert(key, value);
            }
            sill_core::config::save(&serde_json::from_value(value)?)?;
            json!(null)
        },
        Request::Instructions => json!(sill_core::instructions::load_global()),
        Request::SetInstructions { text } => { sill_core::instructions::save_global(&text)?; json!(null) },
        Request::Agents => json!(sill_core::agents::load(&home)),
        Request::SaveAgent(agent) => { valid_agent(&agent.id)?; json!(sill_core::agents::save_override(&home, &agent)?) },
        Request::RemoveAgent { id } => { valid_agent(&id)?; json!(sill_core::agents::remove_override(&home, &id)?) },
        Request::Registry => {
            let (entries, when) = sill_core::registry::merged(&home);
            json!((entries.into_iter().map(sill_core::frontend::RegistryEntry::from).collect::<Vec<_>>(), when))
        },
        Request::FetchRegistry => { sill_core::registry::fetch(&home)?; json!(null) },
        Request::Installed { id } => { valid_agent(&id)?; json!(sill_core::registry::installed(&home, &id)) },
        Request::Available(agent) => json!(sill_core::registry::available(&agent, &home)),
        Request::Uninstall(entry) => { valid_agent(&entry.id)?; sill_core::registry::uninstall(&home, &entry)?; json!(null) },
        Request::Icon(entry) => {
            valid_agent(&entry.id)?;
            json!(sill_core::registry::fetch_icon(&home, &entry).and_then(|p| std::fs::read(p).ok()))
        },
        Request::Metadata { path } => json!(metadata(&path)?),
        Request::List { path } => {
            let entries = std::fs::read_dir(path)?.filter_map(|e| e.ok()).filter_map(|e| metadata(&e.path()).ok().map(|metadata| DirEntry { path: e.path(), metadata })).collect::<Vec<_>>();
            json!(entries)
        },
        Request::Find { bases, query, cap, visits } => json!(find_files(&bases, &query, cap.min(1000), visits.min(100_000))),
        Request::Read { path } => {
            let meta = metadata(&path)?;
            if !meta.file || meta.symlink { bail!("{} is not a regular file", path.display()); }
            if meta.length > 20_000_000 { bail!("{} is too large to read", path.display()); }
            json!(std::fs::read(path)?)
        },
        Request::Write { path, bytes } => {
            if bytes.len() > 20_000_000 { bail!("file is too large to write"); }
            let dir = path.parent().context("file has no parent")?;
            let name = path.file_name().context("file has no name")?;
            sill_core::safe_fs::replace_file(dir, name, &bytes)?;
            json!(null)
        },
        Request::Head { path } => json!(head_label(&path)),
        Request::RepoRoot { path } => json!(sill_core::changes::repo_root(&path)),
        Request::ChangedLines { repo, rel } => json!(sill_core::changes::git_changed_lines(&repo, &rel)),
        Request::JailOutdated { session, policy } => json!(sill_core::session::jail_outdated(&session, &policy, &home)),
        Request::GitStatus { repo } => json!(sill_core::changes::git_status(&repo)),
        Request::Upload { session, name, bytes } => {
            let s = session_row(hub, &session)?;
            if bytes.len() > 20_000_000 { bail!("attachment is too large"); }
            if Path::new(&name).file_name().and_then(|n| n.to_str()) != Some(name.as_str()) || name == "." || name == ".." { bail!("invalid attachment name"); }
            let policy = sill_core::session::policy_of(&s, &home)?;
            let sdir = sill_core::session::state_dir(rustix::process::getuid().as_raw(), &s.id)?;
            let (host_dir, jail_dir) = sill_core::session::uploads_dir(&policy, &sdir, &home).context("this policy keeps no private uploads directory")?;
            let path = host_dir.join(&name);
            let rel = path.strip_prefix(&sdir)?;
            sill_core::safe_fs::write_file_beneath(&sdir, rel, &bytes, 0o644)?;
            json!(jail_dir.join(name))
        },
        Request::ResolveImage { session, path } => {
            session_row(hub, &session)?;
            let path = resolve_image(hub, &session, &path, &home)?;
            json!(path.and_then(|p| {
                let m = std::fs::symlink_metadata(&p).ok()?;
                (m.is_file() && m.len() <= 20_000_000).then(|| std::fs::read(p).ok()).flatten()
            }))
        },
        Request::Denied { session } => {
            let s = session_row(hub, &session)?;
            let rt = sill_core::shim::runtime_dir(&session)?;
            let lines = |name| std::fs::read_to_string(rt.join(name)).unwrap_or_default().lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>();
            let quiet = sill_core::policy::Policy::load(&s.policy, &home).map(|p| p.network.quiet).unwrap_or_else(|_| sill_core::policy::quiet_default());
            let agent_name = sill_core::agents::load(&home).get(&s.agent).map(|a| a.name.clone()).unwrap_or(s.agent);
            json!(sill_core::frontend::Denied { hosts: lines("denied"), answered: lines("denied.answered"), quiet, agent_name })
        },
        Request::RememberDenied { session, host } => {
            use std::io::Write;
            session_row(hub, &session)?;
            if host.contains(['\n', '\r']) { bail!("invalid host"); }
            let path = sill_core::shim::runtime_dir(&session)?.join("denied.answered");
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(f, "{host}")?;
            json!(null)
        },
        Request::AllowHost { session, host } => {
            let s = session_row(hub, &session)?;
            let name = host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map(|(name, _)| name).unwrap_or(&host);
            let name = name.strip_prefix("*.").unwrap_or(name);
            if !sill_core::proxy::valid_hostname(name) { bail!("invalid host pattern"); }
            let mut agent = sill_core::agents::load(&home).remove(&s.agent).context("unknown agent")?;
            if !agent.domains.contains(&host) { agent.domains.push(host); }
            sill_core::agents::save_override(&home, &agent)?;
            json!(agent.name)
        },
        Request::ProcList { session } => { session_row(hub, &session)?; json!(control(&session)?.proc_list()?) },
        Request::ProcTail { session, name, lines } => { session_row(hub, &session)?; json!(control(&session)?.proc_tail(&name, lines.min(10000))?) },
        Request::ProcStop { session, name } => {
            session_row(hub, &session)?;
            let mut c = control(&session)?;
            if name.is_empty() { c.proc_stop_all()?; } else { c.proc_stop(&name)?; }
            json!(null)
        },
        Request::ProcDismiss { session, name } => { session_row(hub, &session)?; control(&session)?.proc_dismiss(&name)?; json!(null) },
        Request::ReadLog { source, lines } => json!(sill_core::session::read_log(&source, lines.min(10000))),
        Request::Policies => json!(sill_core::policy::available(&home)),
        Request::PolicyDesktop { policy } => {
            let policy = sill_core::policy::Policy::load(&policy, &home)?;
            json!((policy.jail.x11, policy.jail.dbus))
        },
        Request::Kept { session } => { session_row(hub, &session)?; json!(sill_core::memory::kept_of(&session)) },
        Request::RefreshSnapshot => { sill_core::memory::refresh_snapshot()?; json!(null) },
        Request::GitSummary { path } => json!(git_summary(&path)),
        Request::ProcessPolicy { session } => {
            let session = session_row(hub, &session)?;
            json!(sill_core::session::policy_of(&session, &home)?.processes)
        },
    };
    Ok(value)
}

fn control(session: &str) -> Result<sill_core::shim::Control> { sill_core::shim::Control::connect(&sill_core::session::socket_path(session)?) }
fn session_row(hub: &Hub, id: &str) -> Result<sill_core::store::Session> {
    if !sill_core::session::valid_id(id) { bail!("invalid session id"); }
    hub.with_store(|s| s.sessions()?.into_iter().find(|s| s.id == id).context("unknown session"))
}

fn resolve_image(hub: &Hub, session: &str, src: &str, home: &Path) -> Result<Option<std::path::PathBuf>> {
    use std::path::{Component, PathBuf};
    use std::os::unix::fs::FileTypeExt;
    let src = src.strip_prefix("file://").unwrap_or(src);
    if src.contains("://") { return Ok(None) }
    let raw = PathBuf::from(src);
    let raw = if raw.is_absolute() { raw } else { home.join(raw) };
    let mut p = PathBuf::from("/");
    for c in raw.components() { match c { Component::ParentDir => { p.pop(); }, Component::Normal(n) => p.push(n), _ => {} } }
    let image = p.extension().map(|e| e.to_string_lossy().to_lowercase()).is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp"));
    if !image { return Ok(None) }
    let pins = hub.with_store(|s| s.jail_pins(session))?;
    if pins.iter().any(|pin| pin.is_dir() && p.starts_with(pin)) { return Ok(Some(p)) }
    let view = hub.jail_view(session)?;
    if view.masked.iter().any(|mask| p.starts_with(mask)) { return Ok(None) }
    for (host, upper) in view.layers {
        if let Ok(rel) = p.strip_prefix(host) {
            let upper = upper.join(rel);
            if let Ok(m) = std::fs::symlink_metadata(&upper) {
                if m.file_type().is_char_device() { return Ok(None) }
                if m.is_file() { return Ok(Some(upper)) }
            }
        }
    }
    Ok(Some(p))
}

fn valid_agent(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')) || id == "." || id == ".." { bail!("invalid agent id"); }
    Ok(())
}

fn metadata(path: &Path) -> Result<Metadata> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(Metadata { file: m.is_file(), directory: m.is_dir(), symlink: m.is_symlink(), target: if m.is_symlink() { std::fs::read_link(path).ok() } else { None }, length: m.len(), readonly: m.permissions().readonly() })
}

fn head_label(repo: &Path) -> Option<(String, bool)> {
    let dot = repo.join(".git");
    let gitdir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = Path::new(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p.to_path_buf() } else { repo.join(p) }
    } else { dot };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref:") {
        Some(r) => (r.trim().strip_prefix("refs/heads/").unwrap_or(r.trim()).to_string(), false),
        None => (head.chars().take(7).collect(), true),
    })
}

fn find_files(bases: &[std::path::PathBuf], q: &str, cap: usize, visits: usize) -> (Vec<std::path::PathBuf>, bool) {
    let mut found = Vec::new();
    let mut seen = 0;
    let mut cut = false;
    'bases: for base in bases {
        let mut queue = std::collections::VecDeque::from([base.clone()]);
        while let Some(dir) = queue.pop_front() {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                seen += 1;
                if seen > visits || found.len() >= cap { cut = true; break 'bases }
                let name = e.file_name().to_string_lossy().into_owned();
                let path = e.path();
                let hay = if q.contains('/') { path.strip_prefix(base).map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default() } else { name.to_lowercase() };
                if hay.contains(q) { found.push(path.clone()); }
                if e.file_type().is_ok_and(|t| t.is_dir()) && !name.starts_with('.') && !matches!(name.as_str(), "target" | "node_modules" | "__pycache__" | "build" | "dist") { queue.push_back(path); }
            }
        }
    }
    found.sort();
    (found, cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A search finds by name, by path with a slash, skips noise folders, and says when cut.
    #[test]
    fn find_files_by_name_and_path() {
        let d = std::env::temp_dir().join(format!("sill-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["src/main.rs", "src/shelf/tree.rs", "docs/Tree.md", "target/debug/tree.d", "node_modules/x/tree.js"] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        let (found, cut) = find_files(&[d.clone()], "tree", 100, 10_000);
        let rel: Vec<String> = found.iter().map(|p| p.strip_prefix(&d).unwrap().display().to_string()).collect();
        assert_eq!(rel, vec!["docs/Tree.md", "src/shelf/tree.rs"], "noise folders skipped, case folded");
        assert!(!cut);
        let (found, _) = find_files(&[d.clone()], "shelf/tr", 100, 10_000);
        assert_eq!(found, vec![d.join("src/shelf/tree.rs")]);
        let (found, cut) = find_files(&[d.clone()], "", 2, 10_000);
        assert_eq!(found.len(), 2);
        assert!(cut);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The root row's checkout label: a branch by name, a detached HEAD by its short hash, a
    /// worktree through its gitdir file.
    #[test]
    fn head_label_reads_the_checkout() {
        let d = std::env::temp_dir().join(format!("sill-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("repo/.git")).unwrap();
        std::fs::write(d.join("repo/.git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(head_label(&d.join("repo")), Some(("feature/x".to_string(), false)));
        std::fs::write(d.join("repo/.git/HEAD"), "a8cf233deadbeef0123456789abcdef012345678\n").unwrap();
        assert_eq!(head_label(&d.join("repo")), Some(("a8cf233".to_string(), true)));
        std::fs::create_dir_all(d.join("wt")).unwrap();
        std::fs::write(d.join("wt/.git"), format!("gitdir: {}\n", d.join("repo/.git").display())).unwrap();
        assert_eq!(head_label(&d.join("wt")), Some(("a8cf233".to_string(), true)));
        assert_eq!(head_label(&d.join("none")), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn metadata_never_follows_a_link() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("sill-front-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), "abc").unwrap();
        symlink("file", dir.join("link")).unwrap();
        let m = metadata(&dir.join("link")).unwrap();
        assert!(m.symlink);
        assert!(!m.file);
        assert_eq!(m.target, Some("file".into()));
        assert!(metadata(&dir.join("file")).unwrap().file);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

fn git_summary(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let (repo, spec): (std::path::PathBuf, Option<std::path::PathBuf>) = if path.is_dir() {
        (path.to_path_buf(), None)
    } else {
        let root = path.ancestors().skip(1).find(|a| a.join(".git").exists())?;
        (root.to_path_buf(), Some(path.strip_prefix(root).ok()?.to_path_buf()))
    };
    if !repo.join(".git").exists() {
        return None;
    }
    let git = sill_core::changes::Git::open(&repo).ok()?;
    let mut cmd = git.cmd();
    cmd.args(["diff", "--numstat"]).args(sill_core::changes::Git::DIFF_SAFE).args(["HEAD", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    let (mut add, mut del) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split('\t');
        add += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        del += it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    }
    let mut cmd = git.cmd();
    cmd.args(["ls-files", "--others", "--exclude-standard", "--"]);
    if let Some(s) = &spec {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    let untracked = String::from_utf8_lossy(&out.stdout).lines().count() as u64;
    Some((add, del, untracked))
}
