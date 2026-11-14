//! Mac processes for sill-mac (docs/sill-mac.md, slice 3): programs an agent runs on the Mac as
//! the person, under the `mac_commands` grant. Each runs in a process group of its own, in a
//! shared Mac folder, with the person's login shell's environment; its output (stdout and stderr
//! together) goes to a private log. They stop when the session's grant goes and when Sill quits
//! (the person's decisions of 2026-11-13).
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// The most of one session's Mac processes running at once.
const MAX_RUNNING: usize = 16;
/// The most of a process's output one read returns: its end.
const OUTPUT_TAIL: u64 = 64 << 10;

struct Proc {
    session: String,
    argv: Vec<String>,
    cwd: PathBuf,
    /// The process and its group (the same number: it leads its own group).
    pid: i32,
    started: u64,
    /// How it ended, once it has: "exit 0", "signal 15".
    ended: Option<String>,
    log: PathBuf,
}

static PROCS: Mutex<BTreeMap<u64, Proc>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// The person's login shell's environment, read once: PATH and the rest as in their Terminal.
fn login_env() -> &'static BTreeMap<String, String> {
    static ENV: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let read = Command::new(&shell)
            .args(["-l", "-c", "/usr/bin/env -0"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let mut env: BTreeMap<String, String> = match read {
            Ok(out) if out.status.success() => out
                .stdout
                .split(|b| *b == 0)
                .filter_map(|entry| {
                    let entry = String::from_utf8_lossy(entry);
                    let (k, v) = entry.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect(),
            _ => std::env::vars().collect(),
        };
        // the shell's own bookkeeping, not the person's environment
        for k in ["_", "SHLVL", "PWD", "OLDPWD"] {
            env.remove(k);
        }
        env.entry("PATH".into()).or_insert_with(|| "/usr/bin:/bin:/usr/sbin:/sbin".into());
        env
    })
}

/// `program` as a path: as given when it has a slash, else found on `path`.
fn resolve(program: &str, path: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    path.split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0)
        })
}

fn number(name: &str) -> Result<u64, String> {
    name.strip_prefix("mac:")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("{name} is not a Mac process name (mac:N)"))
}

/// One `mac_commands` request of `session`; `shared` are the folders shared with the VM.
pub fn handle(session: &str, args: &Value, shared: &[PathBuf]) -> Result<Value, String> {
    if let Some(run) = args.get("run") {
        return start(session, run, shared);
    }
    if args.get("list").is_some() {
        return Ok(list(session));
    }
    if let Some(name) = args.get("output").and_then(Value::as_str) {
        return output(session, name);
    }
    if let Some(name) = args.get("stop").and_then(Value::as_str) {
        return stop(session, number(name)?).map(|ended| json!({"stopped": name, "ended": ended}));
    }
    Err("one of run, list, output or stop".into())
}

fn start(session: &str, run: &Value, shared: &[PathBuf]) -> Result<Value, String> {
    let argv: Vec<String> = run["argv"]
        .as_array()
        .ok_or("run needs argv")?
        .iter()
        .map(|a| a.as_str().map(str::to_owned).ok_or("argv is strings"))
        .collect::<Result<_, _>>()?;
    let program = argv.first().ok_or("run needs a program")?;
    let cwd = run["cwd"].as_str().ok_or("run needs cwd")?;
    let cwd = std::fs::canonicalize(cwd).map_err(|e| format!("{cwd}: {e}"))?;
    if !shared.iter().any(|folder| cwd.starts_with(folder)) {
        return Err(format!(
            "{} is not inside a folder shared with Sill's VM: the person shares it (Pin a folder) first",
            cwd.display()
        ));
    }
    let running = PROCS
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.session == session && p.ended.is_none())
        .count();
    if running >= MAX_RUNNING {
        return Err(format!("this session already runs {MAX_RUNNING} Mac processes: stop one first"));
    }
    let mut env = login_env().clone();
    if let Some(extra) = run["env"].as_object() {
        for (k, v) in extra {
            env.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
        }
    }
    let path = env.get("PATH").cloned().unwrap_or_default();
    let executable = resolve(program, &path).ok_or_else(|| format!("{program}: not found on the person's PATH"))?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = sill_proto::runtime_dir().map_err(|e| e.to_string())?.join("mac-procs");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let log = dir.join(format!("{id}.log"));
    let out = std::fs::File::create(&log).map_err(|e| e.to_string())?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(&executable)
        .args(&argv[1..])
        .current_dir(&cwd)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        // a group of its own: stopping it stops what it started
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let pid = child.id() as i32;
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    PROCS.lock().unwrap().insert(
        id,
        Proc { session: session.into(), argv: argv.clone(), cwd: cwd.clone(), pid, started, ended: None, log },
    );
    // its end, recorded when it comes
    std::thread::Builder::new()
        .name("sill-gui-mac-proc".into())
        .spawn(move || {
            let ended = match child.wait() {
                Ok(status) => match (status.code(), std::os::unix::process::ExitStatusExt::signal(&status)) {
                    (Some(code), _) => format!("exit {code}"),
                    (None, Some(signal)) => format!("signal {signal}"),
                    _ => "ended".into(),
                },
                Err(e) => format!("unknown: {e}"),
            };
            if let Some(p) = PROCS.lock().unwrap().get_mut(&id) {
                p.ended = Some(ended);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(json!({"name": format!("mac:{id}"), "pid": pid, "cwd": cwd, "argv": argv}))
}

fn list(session: &str) -> Value {
    let procs = PROCS.lock().unwrap();
    let list: Vec<Value> = procs
        .iter()
        .filter(|(_, p)| p.session == session)
        .map(|(id, p)| {
            json!({
                "name": format!("mac:{id}"),
                "argv": p.argv,
                "cwd": p.cwd,
                "pid": p.pid,
                "started": p.started,
                "state": if p.ended.is_some() { "ended" } else { "running" },
                "ended": p.ended,
            })
        })
        .collect();
    json!({"processes": list})
}

fn output(session: &str, name: &str) -> Result<Value, String> {
    let id = number(name)?;
    let log = {
        let procs = PROCS.lock().unwrap();
        let p = procs.get(&id).filter(|p| p.session == session).ok_or_else(|| format!("no {name} in this session"))?;
        p.log.clone()
    };
    let mut file = std::fs::File::open(&log).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let from = size.saturating_sub(OUTPUT_TAIL);
    file.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(json!({"name": name, "output": String::from_utf8_lossy(&bytes), "truncated": from > 0, "bytes": size}))
}

/// Stop the process `id` of `session` and its group: asked first, then made to.
fn stop(session: &str, id: u64) -> Result<Option<String>, String> {
    let (pid, done) = {
        let procs = PROCS.lock().unwrap();
        let p = procs.get(&id).filter(|p| p.session == session).ok_or_else(|| format!("no mac:{id} in this session"))?;
        (p.pid, p.ended.is_some())
    };
    if done {
        return Ok(PROCS.lock().unwrap().get(&id).and_then(|p| p.ended.clone()));
    }
    // SAFETY: signals to a process group this module started and still records as running
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if PROCS.lock().unwrap().get(&id).is_some_and(|p| p.ended.is_some()) {
            break;
        }
    }
    if PROCS.lock().unwrap().get(&id).is_some_and(|p| p.ended.is_none()) {
        // SAFETY: as above; the leader has not been reaped, so the group is still this one
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(PROCS.lock().unwrap().get(&id).and_then(|p| p.ended.clone()))
}

/// Sessions with Mac processes still running.
pub fn sessions_running() -> Vec<String> {
    let mut sessions: Vec<String> =
        PROCS.lock().unwrap().values().filter(|p| p.ended.is_none()).map(|p| p.session.clone()).collect();
    sessions.dedup();
    sessions
}

/// Stop every running Mac process of `session` (its grant went).
pub fn stop_session(session: &str) {
    let ids: Vec<u64> = PROCS
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, p)| p.session == session && p.ended.is_none())
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        let _ = stop(session, id);
    }
}

/// Stop every running Mac process (Sill quits).
pub fn stop_all() {
    for session in sessions_running() {
        stop_session(&session);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn a_program_runs_in_a_shared_folder_and_stops_with_its_group() {
        let shared = std::env::temp_dir().canonicalize().unwrap();
        let dir = shared.join(format!("sill-mac-procs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // outside every shared folder: refused
        let refused = super::handle("s1", &json!({"run":{"argv":["/bin/echo","x"],"cwd":"/"}}), &[dir.clone()]);
        assert!(refused.unwrap_err().contains("not inside a folder shared"));
        // a quick one: its output and its end
        let done = super::handle("s1", &json!({"run":{"argv":["/bin/echo","hello from the Mac"],"cwd":dir}}), &[dir.clone()]).unwrap();
        let name = done["name"].as_str().unwrap().to_string();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = super::handle("s1", &json!({"output": name}), &[]).unwrap();
        assert_eq!(out["output"], "hello from the Mac\n");
        let list = super::handle("s1", &json!({"list": true}), &[]).unwrap();
        assert!(list["processes"].as_array().unwrap().iter().any(|p| p["name"] == name && p["ended"] == "exit 0"));
        // another session sees nothing of it
        assert!(super::handle("s2", &json!({"output": name}), &[]).is_err());
        // a long one with a child: stopping takes the group
        let long = super::handle("s1", &json!({"run":{"argv":["/bin/sh","-c","sleep 60 & sleep 60"],"cwd":dir}}), &[dir.clone()]).unwrap();
        let long = long["name"].as_str().unwrap().to_string();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let stopped = super::handle("s1", &json!({"stop": long}), &[]).unwrap();
        assert_eq!(stopped["ended"], "signal 15");
        assert!(super::sessions_running().iter().all(|s| s != "s1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
