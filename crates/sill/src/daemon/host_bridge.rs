//! Session-bound agent endpoint. It has no administrative RPC dispatch or caller-supplied
//! session identity. One bounded request per connection; every action carries its generation.
use super::{Hub, host_control};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{net::UnixListener, process::CommandExt},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};

pub fn gate() -> MutexGuard<'static, ()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(Default::default).lock().unwrap()
}
fn socket(session: &str) -> Result<PathBuf> {
    Ok(sill_core::shim::runtime_dir(session)?.join("bridge/host.sock"))
}
pub fn listen(session: &str) -> Result<UnixListener> {
    let path = socket(session)?;
    // The launcher creates and binds the directory, including for currently inactive sessions.
    anyhow::ensure!(path.parent().unwrap().is_dir(), "restart this session to install its host bridge");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}
pub fn serve(hub: Arc<Hub>, session: String, generation: u64, listener: UnixListener) {
    std::thread::spawn(move || {
        loop {
            if host_control::brief(&session).is_none_or(|b| b.generation != generation) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Sequential, bounded IO: a jailed client cannot grow host threads or buffers.
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let result = (|| -> Result<Value> {
                        let mut line = String::new();
                        BufReader::new(&stream).take(65537).read_line(&mut line)?;
                        anyhow::ensure!(line.len() <= 65536 && line.ends_with('\n'), "request too large or incomplete");
                        let request: Request = serde_json::from_str(&line)?;
                        let _gate = gate();
                        let active = host_control::brief(&session).context("Host control is inactive")?;
                        anyhow::ensure!(sill_core::session::is_running(&session), "session jail is gone");
                        anyhow::ensure!(active.generation == generation, "stale bridge");
                        if request.operation == "state" {
                            // whether the Mac's operations can reach a Mac (Sill.app's VM)
                            let mut state = json!(active);
                            state["mac"] = json!(super::host_mac::attached());
                            return Ok(state);
                        }
                        anyhow::ensure!(request.generation == Some(generation), "stale activation generation");
                        anyhow::ensure!(active.operations.contains(&request.operation), "operation not granted");
                        // Activity notices are persisted in the session journal. Arguments,
                        // results and errors may contain typed text, environment values or output.
                        // Keep those on the direct reply channel, not in these notices.
                        host_control::note(&hub, &session, format!("HOST action · generation {generation}: {}", request.operation));
                        let result = action(&hub, &session, generation, &request, _gate);
                        host_control::note(
                            &hub,
                            &session,
                            match &result {
                                Ok(_) => {
                                    format!("HOST action {} completed", request.operation)
                                }
                                Err(_) => {
                                    format!("HOST action {} failed", request.operation)
                                }
                            },
                        );
                        result
                    })();
                    let reply = match result {
                        Ok(v) => json!({"result":v}),
                        Err(e) => json!({"error":format!("{e:#}")}),
                    };
                    let _ = writeln!(stream, "{reply}");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => {
                    let _ = host_control::revoke(&hub, &session, "host bridge failed");
                    break;
                }
            }
        }
    });
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    generation: Option<u64>,
    operation: String,
    #[serde(default)]
    args: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    argv: Vec<String>,
    cwd: PathBuf,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}
fn identity(pid: u32) -> Result<(u64, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat.rsplit_once(')').context("bad process stat")?.1.split_whitespace().collect();
    Ok((fields[19].parse()?, fields[2].parse()?))
}
fn signal(pid: u32, ticks: u64) -> Result<()> {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    anyhow::ensure!(pid != std::process::id(), "refusing to stop the production daemon");
    let fd = pidfd_open(Pid::from_raw(pid as i32).context("invalid pid")?, PidfdFlags::empty())?;
    anyhow::ensure!(identity(pid)?.0 == ticks, "process identity changed");
    pidfd_send_signal(&fd, Signal::KILL)?;
    Ok(())
}
fn stop_group(p: &sill_core::store::HostProcess) -> Result<()> {
    anyhow::ensure!(identity(p.pid)?.0 == p.start_ticks, "managed group leader is gone or changed; descendants unknown");
    // Each target is pinned by pidfd before its start-time check. Never signal a reused PID.
    // Descendants that deliberately escape the managed group are unrestricted host code.
    for entry in std::fs::read_dir("/proc")?.flatten() {
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
            && let Ok((ticks, group)) = identity(pid)
            && group == p.pid
        {
            let _ = signal(pid, ticks);
        }
    }
    if identity(p.pid).is_ok_and(|(ticks, _)| ticks == p.start_ticks) {
        signal(p.pid, p.start_ticks)?;
    }
    Ok(())
}
fn action(hub: &Arc<Hub>, session: &str, generation: u64, r: &Request, _gate: MutexGuard<'static, ()>) -> Result<Value> {
    match r.operation.as_str() {
        "launch" | "commands" => {
            let a: Launch = serde_json::from_value(r.args.clone())?;
            anyhow::ensure!(!a.argv.is_empty() && a.cwd.is_absolute(), "argv and an absolute host cwd are required");
            let id = super::sfp::id();
            let dir = host_dir(session)?;
            std::fs::create_dir_all(&dir)?;
            let log = dir.join(format!("{id}.log"));
            let file = std::fs::OpenOptions::new().create_new(true).write(true).open(log)?;
            let mut cmd = std::process::Command::new(&a.argv[0]);
            cmd.args(&a.argv[1..]).current_dir(&a.cwd).envs(a.env).stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
            unsafe {
                cmd.pre_exec(|| {
                    rustix::process::setsid()?;
                    Ok(())
                });
            }
            let mut child = cmd.spawn()?;
            let pid = child.id();
            let saved = (|| -> Result<_> {
                let p = sill_core::store::HostProcess {
                    id: id.clone(),
                    session_id: session.into(),
                    generation,
                    pid,
                    start_ticks: identity(pid)?.0,
                    argv: a.argv,
                    cwd: a.cwd,
                    started: sill_core::store::now(),
                    ended: None,
                    exit_code: None,
                    kept: false,
                    state: "running".into(),
                };
                hub.with_store(|s| s.add_host_process(&p))?;
                Ok(p)
            })();
            if let Err(e) = saved {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
            let owned = saved.unwrap();
            publish(hub, session);
            let hub = hub.clone();
            let session = session.to_owned();
            let owned_id = id.clone();
            std::thread::spawn(move || {
                use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
                // Leave the leader waitable until group cleanup: its PID cannot be reused.
                let _ = waitid(WaitId::Pid(Pid::from_raw(pid as i32).unwrap()), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT);
                let _gate = gate();
                let _ = stop_group(&owned);
                let result = child.wait();
                let code = result.as_ref().ok().and_then(|s| s.code()).map(i64::from);
                let _ = hub.with_store(|s| s.end_host_process(&owned_id, if result.is_ok() { "exited" } else { "unknown" }, code));
                publish(&hub, &session);
                host_control::note(&hub, &session, format!("HOST process host:{owned_id} ended: {result:?}"));
            });
            Ok(json!({"name":format!("host:{id}"), "pid":pid, "location":"host"}))
        }
        "inspect" => {
            let owned = hub.with_store(|s| s.host_processes(session))?;
            if let Some(name) = r.args["name"].as_str() {
                let p = owned.iter().find(|p| format!("host:{}", p.id) == name).context("session-owned host process not found")?;
                use std::io::Seek;
                let mut file = std::fs::File::open(host_log(session, &p.id)?)?;
                let length = file.metadata()?.len();
                file.seek(std::io::SeekFrom::Start(length.saturating_sub(64 * 1024)))?;
                let mut bytes = Vec::new();
                file.take(64 * 1024).read_to_end(&mut bytes)?;
                return Ok(
                    json!({"name":name,"state":p.state,"kept":p.kept,"exit_code":p.exit_code,"output":String::from_utf8_lossy(&bytes),"truncated":length > 64 * 1024,"location":"host"}),
                );
            }
            let managed: Vec<_> =
                owned.iter().map(|p| json!({"name":format!("host:{}",p.id),"pid":p.pid,"state":p.state,"kept":p.kept,"exit_code":p.exit_code})).collect();
            let mut processes = vec![];
            for entry in std::fs::read_dir("/proc")?.flatten() {
                if processes.len() >= 4096 {
                    break;
                }
                if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
                    && let Ok((ticks, _)) = identity(pid)
                {
                    let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                    processes.push(json!({"pid":pid,"start_ticks":ticks,"name":comm.trim()}));
                }
            }
            Ok(json!({"processes":processes,"managed":managed,"location":"host"}))
        }
        "stop" => {
            let name = r.args["name"].as_str().context("name required")?;
            let p = hub
                .with_store(|s| Ok(s.host_processes(session)?.into_iter().find(|p| format!("host:{}", p.id) == name)))?
                .context("session-owned host process not found")?;
            anyhow::ensure!(p.ended.is_none(), "process already ended");
            stop_group(&p)?;
            hub.with_store(|s| s.end_host_process(&p.id, "stopped", None))?;
            publish(hub, session);
            Ok(json!({"stopped":name}))
        }
        "stop_existing" => {
            let pid = r.args["pid"].as_u64().and_then(|v| u32::try_from(v).ok()).context("pid required")?;
            let ticks = r.args["start_ticks"].as_u64().context("start_ticks from inspect required")?;
            // Sill infrastructure is never an implicit target of the stop-existing interface.
            let exe = std::fs::read_link(format!("/proc/{pid}/exe"))?;
            anyhow::ensure!(!exe.file_name().unwrap_or_default().to_string_lossy().starts_with("sill"), "refusing to stop Sill infrastructure");
            signal(pid, ticks)?;
            Ok(json!({"stopped":pid}))
        }
        "view" => {
            super::host_display::availability()?;
            match super::host_display::view(&r.args)? {
                super::host_display::View::Windows(windows) => Ok(json!({"windows":windows,"location":"host"})),
                super::host_display::View::Picture { raw, window, x, y } => {
                    let revision = sill_core::desktop_capture::revision(&raw);
                    let png = sill_core::desktop_capture::png(&raw)?;
                    // readable in the jail through the bridge directory; only the newest few stay
                    let dir = views(session)?;
                    std::fs::create_dir_all(&dir)?;
                    let id = super::sfp::id();
                    std::fs::write(dir.join(format!("{id}.png")), &png)?;
                    let mut old: Vec<_> = std::fs::read_dir(&dir)?.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
                    old.sort();
                    for (_, path) in old.iter().rev().skip(KEPT_VIEWS) {
                        let _ = std::fs::remove_file(path);
                    }
                    Ok(json!({
                        "path": format!("{}/view/{id}.png", sill_core::jail::BRIDGE_DIR),
                        "width": raw.width, "height": raw.height, "x": x, "y": y,
                        "window": window, "revision": revision, "location": "host",
                    }))
                }
            }
        }
        "input" => {
            super::host_display::availability()?;
            super::host_display::input(&r.args)
        }
        "mac_commands" => {
            super::host_mac::check_commands(&r.args)?;
            let (_gate, (result, _)) = forward_mac(_gate, session, generation, || super::host_mac::request(session, generation, "commands", &r.args))?;
            let mut reply = result;
            reply["location"] = json!("mac");
            Ok(reply)
        }
        "mac_input" => {
            super::host_mac::check_input(&r.args)?;
            let (_gate, (result, _)) = forward_mac(_gate, session, generation, || super::host_mac::request(session, generation, "input", &r.args))?;
            let mut reply = result;
            reply["location"] = json!("mac");
            Ok(reply)
        }
        "mac_view" => {
            super::host_mac::check_view(&r.args)?;
            let (_gate, (result, picture)) = forward_mac(_gate, session, generation, || super::host_mac::request(session, generation, "view", &r.args))?;
            let Some(png) = picture else {
                return Ok(json!({"result": result, "location": "mac"}));
            };
            // readable in the jail through the bridge directory, apart from the host's pictures;
            // only the newest few stay
            let dir = mac_views(session)?;
            std::fs::create_dir_all(&dir)?;
            let id = super::sfp::id();
            std::fs::write(dir.join(format!("{id}.png")), &png)?;
            let mut old: Vec<_> = std::fs::read_dir(&dir)?.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
            old.sort();
            for (_, path) in old.iter().rev().skip(KEPT_VIEWS) {
                let _ = std::fs::remove_file(path);
            }
            let mut reply = result;
            reply["path"] = json!(format!("{}/mac/{id}.png", sill_core::jail::BRIDGE_DIR));
            reply["location"] = json!("mac");
            Ok(reply)
        }
        _ => bail!("operation backend unavailable"),
    }
}
/// Only the remote wait is outside the lifecycle gate. The Mac checks the grant before
/// acting (and between input actions). Recheck under the gate before publishing a late
/// result or writing a picture, so revoke/cleanup cannot be undone by that reply.
pub(super) fn forward_mac<T>(
    held: MutexGuard<'static, ()>,
    session: &str,
    generation: u64,
    forward: impl FnOnce() -> Result<T>,
) -> Result<(MutexGuard<'static, ()>, T)> {
    drop(held);
    let result = forward();
    let held = gate();
    anyhow::ensure!(
        host_control::brief(session).is_some_and(|b| b.generation == generation),
        "Host control changed while awaiting the Mac; outcome unknown, do not replay automatically"
    );
    Ok((held, result?))
}

/// How many of a session's pictures stay readable in the jail.
const KEPT_VIEWS: usize = 8;
fn views(session: &str) -> Result<PathBuf> {
    Ok(sill_core::shim::runtime_dir(session)?.join("bridge/view"))
}
/// The session's pictures of the Mac, readable in the jail as `/.sill/bridge/mac/ID.png`.
fn mac_views(session: &str) -> Result<PathBuf> {
    Ok(sill_core::shim::runtime_dir(session)?.join("bridge/mac"))
}
/// The session's host processes as its jail sees them (read-only through the bridge directory):
/// `processes.json`, the records, and `<id>.log`, each one's captured output. They stay after
/// revocation, like the records: ownership evidence, not authority.
fn host_dir(session: &str) -> Result<PathBuf> {
    Ok(sill_core::shim::runtime_dir(session)?.join("bridge").join(sill_core::jail::BRIDGE_HOST_DIR))
}
pub fn host_log(session: &str, id: &str) -> Result<PathBuf> {
    Ok(host_dir(session)?.join(format!("{id}.log")))
}
/// Rewrite the jail-visible record list after any change to the session's host processes.
pub fn publish(hub: &Hub, session: &str) {
    let result = (|| -> Result<()> {
        let processes = hub.with_store(|s| s.host_processes(session))?;
        let dir = host_dir(session)?;
        if processes.is_empty() && !dir.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)?;
        let rows: Vec<Value> = processes
            .iter()
            .map(|p| {
                json!({
                    "name": format!("host:{}", p.id), "pid": p.pid, "state": p.state, "kept": p.kept,
                    "exit_code": p.exit_code, "started": p.started, "ended": p.ended,
                    "argv": p.argv, "cwd": p.cwd, "generation": p.generation, "location": "host",
                    "log": format!("{}/{}/{}.log", sill_core::jail::BRIDGE_DIR, sill_core::jail::BRIDGE_HOST_DIR, p.id),
                })
            })
            .collect();
        let tmp = dir.join("processes.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&rows)?)?;
        std::fs::rename(tmp, dir.join("processes.json"))?;
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("sill daemon: session {session}: publishing host processes to the jail: {e:#}");
    }
}
pub fn cleanup(hub: &Hub, session: &str) {
    if let Ok(path) = socket(session) {
        let _ = std::fs::remove_file(path);
    }
    if let Ok(dir) = mac_views(session) {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Ok(dir) = views(session) {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Ok(processes) = hub.with_store(|s| s.host_processes(session)) {
        for p in processes.into_iter().filter(|p| p.ended.is_none() && !p.kept) {
            let outcome = stop_group(&p);
            let _ = hub.with_store(|s| s.end_host_process(&p.id, if outcome.is_ok() { "stopped" } else { "unknown" }, None));
            host_control::note(hub, session, format!("HOST process host:{} revocation stop: {outcome:?}", p.id));
        }
    }
    publish(hub, session);
}

/// Recovery restores ownership evidence, never authority. Stop unkept survivors; kept ones stay
/// visible. A missing/reused leader is uncertain and must not cause an unrelated group kill.
pub fn recover(hub: &Hub) {
    if let Ok(sessions) = hub.with_store(|s| s.sessions()) {
        for session in sessions {
            cleanup(hub, &session.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_does_not_accept_a_session_selector_or_admin_envelope() {
        assert!(serde_json::from_value::<Request>(json!({"generation":1,"operation":"launch","session_id":"other","args":{}})).is_err());
        assert!(serde_json::from_value::<Request>(json!({"method":"_sill/host_control.activate","params":{}})).is_err());
        assert!(serde_json::from_value::<Launch>(json!({"argv":["true"],"cwd":"/","shell":true})).is_err());
    }

    #[test]
    fn stale_process_identity_cannot_stop_a_live_process() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let ticks = identity(child.id()).unwrap().0;
        assert!(signal(child.id(), ticks + 1).is_err());
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(signal(std::process::id(), identity(std::process::id()).unwrap().0).is_err());
    }

    #[test]
    fn cleanup_stops_the_managed_group_and_preserves_other_processes() {
        let mut other = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let p = sill_core::store::HostProcess {
            id: "test".into(),
            session_id: "test".into(),
            generation: 1,
            pid: child.id(),
            start_ticks: identity(child.id()).unwrap().0,
            argv: vec![],
            cwd: "/".into(),
            started: 0,
            ended: None,
            exit_code: None,
            kept: false,
            state: "running".into(),
        };
        stop_group(&p).unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(other.try_wait().unwrap().is_none());
        other.kill().unwrap();
        other.wait().unwrap();
        assert!(stop_group(&p).is_err());
    }
}
