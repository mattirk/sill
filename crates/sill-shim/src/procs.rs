//! Managed processes (docs/tasks.md §2.4): commands an agent asks sill to keep alive after the turn
//! — a dev server, a watcher — owned by the shim, so they survive the agent's restarts and idle
//! stops, are listed with their state and output, and die with the jail.
//!
//! Two doors to the same service: the shelf asks over the control socket (`proc_*` requests in
//! `main.rs`), and anything in the jail — the agent's shell — asks over [`PROC_SOCKET`] with the
//! `sill-proc` command (this binary under that name, [`cli`]). That socket offers only
//! start/list/tail/stop of this session's own processes: nothing the agent could not do already,
//! only with ownership and visibility. Logs are capped files under [`dir`], a tmpfs that goes with
//! the jail.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where the service lives inside the jail (`/run` is a fresh tmpfs there); `SILL_PROC_DIR`
/// moves it for tests and for a shim run outside a jail.
pub fn dir() -> PathBuf {
    std::env::var_os("SILL_PROC_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/sill"))
}

pub fn socket_path() -> PathBuf {
    dir().join("proc.sock")
}

/// A log is kept to this many bytes: when it grows past, the older half goes.
const LOG_CAP: u64 = 2 << 20;
/// SIGTERM, then this long, then SIGKILL.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProcState {
    Running,
    /// Ended on its own with this code (0 = fine, anything else = failed).
    Exited(i32),
    /// Stopped on request.
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub port: Option<u16>,
    pub pid: u32,
    /// Unix seconds.
    pub started: u64,
    pub state: ProcState,
    /// The last line of output seen.
    pub last_line: String,
    pub log: PathBuf,
}

#[derive(Default)]
struct Table {
    procs: Vec<Info>,
}

/// The service's state, shared by the control loop, the in-jail socket and the per-process threads;
/// its logs live under `dir/proc/`.
#[derive(Clone)]
pub struct Procs(Arc<Mutex<Table>>, PathBuf);

impl Default for Procs {
    fn default() -> Self {
        Self::at(dir())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Start { name: String, argv: Vec<String>, cwd: Option<PathBuf>, port: Option<u16> },
    List,
    Tail { name: String, #[serde(default = "default_lines")] lines: usize },
    Stop { name: String },
    StopAll,
    /// Forget an ended process (its log goes too).
    Dismiss { name: String },
}

fn default_lines() -> usize {
    50
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl Procs {
    pub fn at(dir: PathBuf) -> Self {
        Procs(Arc::default(), dir)
    }

    pub fn start(&self, name: &str, argv: Vec<String>, cwd: Option<PathBuf>, port: Option<u16>) -> Result<Info> {
        if !valid_name(name) {
            bail!("a name is letters, digits, `-`, `_` or `.`, up to 40 of them");
        }
        let Some((prog, rest)) = argv.split_first() else { bail!("no command") };
        if let Some(p) = port
            && p < 1024
        {
            bail!("port {p}: privileged ports are not exposed");
        }
        let cwd = match cwd {
            Some(c) => c,
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        };
        let log_dir = self.1.join("proc");
        std::fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
        let log = log_dir.join(format!("{name}.log"));
        {
            let mut t = self.0.lock().unwrap();
            if let Some(i) = t.procs.iter().position(|p| p.name == name) {
                if t.procs[i].state == ProcState::Running {
                    bail!("{name} is running (pid {}); stop it first", t.procs[i].pid);
                }
                t.procs.remove(i); // an ended one of that name makes way
            }
        }
        let _ = std::fs::remove_file(&log);
        // one pipe for stdout and stderr both, in order; the child in its own process group so a
        // stop reaches everything it started
        let (read_end, write_end) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).context("pipe")?;
        let write_err = write_end.try_clone()?;
        let mut cmd = Command::new(prog);
        cmd.args(rest)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::from(write_err))
            .process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("start {prog}"))?;
        let pid = child.id();
        let info = Info { name: name.to_string(), argv, cwd, port, pid, started: now(), state: ProcState::Running, last_line: String::new(), log: log.clone() };
        self.0.lock().unwrap().procs.push(info.clone());
        // output → the capped log and the record's last line
        {
            let procs = self.clone();
            let name = name.to_string();
            std::thread::spawn(move || {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&log).ok();
                for line in BufReader::new(std::fs::File::from(read_end)).lines().map_while(Result::ok) {
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{line}");
                        if f.metadata().map(|m| m.len() > LOG_CAP).unwrap_or(false) {
                            let _ = f.flush();
                            trim_log(&log);
                            file = std::fs::OpenOptions::new().append(true).open(&log).ok();
                        }
                    }
                    let mut t = procs.0.lock().unwrap();
                    if let Some(p) = t.procs.iter_mut().find(|p| p.name == name) {
                        p.last_line = line;
                    }
                }
            });
        }
        // the exit
        {
            let procs = self.clone();
            let name = name.to_string();
            std::thread::spawn(move || {
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                let mut t = procs.0.lock().unwrap();
                if let Some(p) = t.procs.iter_mut().find(|p| p.name == name && p.pid == pid)
                    && p.state == ProcState::Running
                {
                    p.state = ProcState::Exited(code);
                }
            });
        }
        Ok(info)
    }

    pub fn list(&self) -> Vec<Info> {
        self.0.lock().unwrap().procs.clone()
    }

    pub fn tail(&self, name: &str, lines: usize) -> Result<String> {
        let log = self.0.lock().unwrap().procs.iter().find(|p| p.name == name).map(|p| p.log.clone()).with_context(|| format!("no process {name}"))?;
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        let from = all.len().saturating_sub(lines);
        Ok(all[from..].join("\n"))
    }

    /// SIGTERM the process group, SIGKILL after the grace; the record says "stopped".
    pub fn stop(&self, name: &str) -> Result<()> {
        use rustix::process::{Pid, Signal, kill_process_group};
        let pid = {
            let t = self.0.lock().unwrap();
            let p = t.procs.iter().find(|p| p.name == name).with_context(|| format!("no process {name}"))?;
            if p.state != ProcState::Running {
                return Ok(());
            }
            p.pid
        };
        let pgid = Pid::from_raw(pid as i32).context("pid")?;
        let _ = kill_process_group(pgid, Signal::TERM);
        let deadline = std::time::Instant::now() + STOP_GRACE;
        loop {
            if self.0.lock().unwrap().procs.iter().any(|p| p.name == name && p.state != ProcState::Running) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = kill_process_group(pgid, Signal::KILL);
                let _ = rustix::process::kill_process(pgid, Signal::KILL);
                // the waiter marks the exit; give it a moment, then record the stop regardless
                std::thread::sleep(std::time::Duration::from_millis(200));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let mut t = self.0.lock().unwrap();
        if let Some(p) = t.procs.iter_mut().find(|p| p.name == name) {
            p.state = ProcState::Stopped;
        }
        Ok(())
    }

    pub fn stop_all(&self) -> usize {
        let names: Vec<String> = self.0.lock().unwrap().procs.iter().filter(|p| p.state == ProcState::Running).map(|p| p.name.clone()).collect();
        for n in &names {
            let _ = self.stop(n);
        }
        names.len()
    }

    pub fn dismiss(&self, name: &str) -> Result<()> {
        let mut t = self.0.lock().unwrap();
        let i = t.procs.iter().position(|p| p.name == name).with_context(|| format!("no process {name}"))?;
        if t.procs[i].state == ProcState::Running {
            bail!("{name} is running; stop it first");
        }
        let p = t.procs.remove(i);
        let _ = std::fs::remove_file(&p.log);
        Ok(())
    }

    /// One request → one reply object (`ok`, `error`, and the result's fields).
    pub fn handle(&self, op: Op) -> Value {
        fn err(e: impl std::fmt::Display) -> Value {
            json!({ "ok": false, "error": e.to_string() })
        }
        match op {
            Op::Start { name, argv, cwd, port } => match self.start(&name, argv, cwd, port) {
                Ok(info) => json!({ "ok": true, "proc": info }),
                Err(e) => err(format!("{e:#}")),
            },
            Op::List => json!({ "ok": true, "procs": self.list() }),
            Op::Tail { name, lines } => match self.tail(&name, lines) {
                Ok(text) => json!({ "ok": true, "text": text }),
                Err(e) => err(format!("{e:#}")),
            },
            Op::Stop { name } => match self.stop(&name) {
                Ok(()) => json!({ "ok": true }),
                Err(e) => err(format!("{e:#}")),
            },
            Op::StopAll => json!({ "ok": true, "stopped": self.stop_all() }),
            Op::Dismiss { name } => match self.dismiss(&name) {
                Ok(()) => json!({ "ok": true }),
                Err(e) => err(format!("{e:#}")),
            },
        }
    }

    /// The in-jail door: one JSON request line per connection, one reply line.
    pub fn serve(&self, socket: &Path) -> Result<()> {
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).with_context(|| format!("listen on {}", socket.display()))?;
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let procs = self.clone();
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = BufReader::new(conn.try_clone().unwrap()).read_line(&mut line);
                let reply = match serde_json::from_str::<Op>(line.trim()) {
                    Ok(op) => procs.handle(op),
                    Err(e) => json!({ "ok": false, "error": format!("bad request: {e}") }),
                };
                let _ = writeln!(conn, "{reply}");
            });
        }
        Ok(())
    }
}

/// Keep the newer half of a log that grew past the cap, from a line start.
fn trim_log(log: &Path) {
    let Ok(bytes) = std::fs::read(log) else { return };
    let keep_from = bytes.len().saturating_sub((LOG_CAP / 2) as usize);
    let cut = bytes[keep_from..].iter().position(|b| *b == b'\n').map(|i| keep_from + i + 1).unwrap_or(keep_from);
    let _ = std::fs::write(log, &bytes[cut..]);
}

// --- the `sill-proc` command ---------------------------------------------------------------------

/// `sill-proc start --name NAME [--port PORT] [--cwd DIR] -- CMD…` | `list` | `tail NAME [-n N]` |
/// `stop NAME` | `stop-all` | `dismiss NAME`. Exit code 0 on success, 1 otherwise.
pub fn cli(args: Vec<String>) -> i32 {
    match cli_inner(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sill-proc: {e:#}");
            1
        }
    }
}

const USAGE: &str = "usage: sill-proc start --name NAME [--port PORT] [--cwd DIR] -- COMMAND…\n       sill-proc list | tail NAME [-n LINES] | stop NAME | stop-all | dismiss NAME\n\nA command started here keeps running after your turn ends and after you restart; a background `&` does not.";

fn cli_inner(mut args: Vec<String>) -> Result<()> {
    let Some(sub) = args.first().cloned() else { bail!("{USAGE}") };
    args.remove(0);
    let op = match sub.as_str() {
        "start" => {
            let (mut name, mut port, mut cwd) = (None, None, None);
            let mut rest = Vec::new();
            let mut it = args.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--name" => name = it.next(),
                    "--port" => port = Some(it.next().context("--port expects a number")?.parse::<u16>().context("--port expects a number")?),
                    "--cwd" => cwd = it.next().map(PathBuf::from),
                    "--" => {
                        rest.extend(it);
                        break;
                    }
                    other => bail!("unexpected {other}\n{USAGE}"),
                }
            }
            let name = name.context("--name is required")?;
            if rest.is_empty() {
                bail!("no command after --\n{USAGE}");
            }
            json!({ "op": "start", "name": name, "argv": rest, "cwd": cwd, "port": port })
        }
        "list" => json!({ "op": "list" }),
        "tail" => {
            let name = args.first().cloned().context("tail expects a name")?;
            let lines = match args.get(1).map(String::as_str) {
                Some("-n") => args.get(2).context("-n expects a number")?.parse::<usize>().context("-n expects a number")?,
                _ => 50,
            };
            json!({ "op": "tail", "name": name, "lines": lines })
        }
        "stop" => json!({ "op": "stop", "name": args.first().cloned().context("stop expects a name")? }),
        "stop-all" => json!({ "op": "stop_all" }),
        "dismiss" => json!({ "op": "dismiss", "name": args.first().cloned().context("dismiss expects a name")? }),
        _ => bail!("{USAGE}"),
    };
    let reply = call(&socket_path(), &op)?;
    match sub.as_str() {
        "start" => {
            let p = &reply["proc"];
            println!("started {} (pid {}) — `sill-proc tail {}` shows its output, `sill-proc stop {}` ends it", p["name"].as_str().unwrap_or(""), p["pid"], p["name"].as_str().unwrap_or(""), p["name"].as_str().unwrap_or(""));
        }
        "list" => {
            let procs = reply["procs"].as_array().cloned().unwrap_or_default();
            if procs.is_empty() {
                println!("no managed processes");
            }
            for p in procs {
                let state = match &p["state"] {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o.iter().map(|(k, v)| format!("{k} {v}")).next().unwrap_or_default(),
                    _ => "?".into(),
                };
                let up = now().saturating_sub(p["started"].as_u64().unwrap_or(0));
                let port = p["port"].as_u64().map(|x| format!(" port {x}")).unwrap_or_default();
                let argv: Vec<String> = p["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
                println!("{:<16} {:<10} pid {:<7} up {}s{}  {}", p["name"].as_str().unwrap_or(""), state, p["pid"], up, port, argv.join(" "));
                if let Some(l) = p["last_line"].as_str().filter(|l| !l.is_empty()) {
                    println!("{:<16} {l}", "");
                }
            }
        }
        "tail" => println!("{}", reply["text"].as_str().unwrap_or("")),
        "stop" | "dismiss" => println!("ok"),
        "stop-all" => println!("stopped {}", reply["stopped"]),
        _ => {}
    }
    Ok(())
}

/// One request to the service, the reply's fields; an `ok: false` reply is the error.
pub fn call(socket: &Path, op: &Value) -> Result<Value> {
    let mut s = UnixStream::connect(socket).with_context(|| format!("the process service is not running ({})", socket.display()))?;
    writeln!(s, "{op}")?;
    let mut text = String::new();
    s.read_to_string(&mut text)?;
    let v: Value = serde_json::from_str(text.trim()).context("reply")?;
    if v["ok"].as_bool() != Some(true) {
        bail!("{}", v["error"].as_str().unwrap_or("failed"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sill-procs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_process_is_kept_logged_and_stopped() {
        let d = fresh("kept");
        let procs = Procs::at(d.clone());
        let args = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        let info = procs.start("srv", args("sh -c echo_hi;sleep_30").iter().map(|a| a.replace('_', " ")).collect(), None, Some(3000)).unwrap();
        assert_eq!(info.state, ProcState::Running);
        assert!(wait_for(|| procs.tail("srv", 10).unwrap().contains("hi")));
        assert_eq!(procs.list()[0].last_line, "hi");
        assert!(procs.start("srv", args("true"), None, None).is_err(), "a running name is taken");
        procs.stop("srv").unwrap();
        assert_eq!(procs.list()[0].state, ProcState::Stopped);
        // an exit of its own is recorded with the code; the name is then free again
        procs.start("job", vec!["sh".into(), "-c".into(), "exit 3".into()], None, None).unwrap();
        assert!(wait_for(|| procs.list().iter().any(|p| p.name == "job" && p.state == ProcState::Exited(3))));
        procs.dismiss("job").unwrap();
        assert!(procs.list().iter().all(|p| p.name != "job"));
        assert!(procs.start("x", args("true"), None, Some(80)).is_err(), "privileged port refused");
        assert!(procs.start("bad name", args("true"), None, None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_socket_serves_the_command() {
        let d = fresh("sock");
        let procs = Procs::at(d.clone());
        let sock = d.join("proc.sock");
        {
            let p = procs.clone();
            let s = sock.clone();
            std::thread::spawn(move || p.serve(&s));
        }
        assert!(wait_for(|| sock.exists()));
        let r = call(&sock, &json!({ "op": "start", "name": "w", "argv": ["sh", "-c", "echo out; echo err >&2; sleep 30"] })).unwrap();
        assert_eq!(r["proc"]["name"], "w");
        assert!(wait_for(|| call(&sock, &json!({ "op": "tail", "name": "w", "lines": 5 })).map(|r| r["text"].as_str().unwrap_or("").contains("err")).unwrap_or(false)));
        assert!(call(&sock, &json!({ "op": "nope" })).is_err());
        assert_eq!(call(&sock, &json!({ "op": "stop_all" })).unwrap()["stopped"], 1);
        assert_eq!(cli(vec!["bogus".into()]), 1); // the client itself is exercised by hand (see docs/tasks.md §2.4)
        let _ = std::fs::remove_dir_all(&d);
    }
}
