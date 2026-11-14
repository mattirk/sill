//! The daemon's lifecycle in bare mode (docs/daemon.md §3.11), against a throwaway runtime and
//! state dir of its own: it needs no systemd, no jail and no agent, so it runs anywhere the
//! binary builds — a sill session included.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A daemon of its own in a temp dir: `$XDG_RUNTIME_DIR`, `$XDG_STATE_HOME` and `$HOME` under it.
struct Daemon {
    dir: PathBuf,
    child: Child,
}

impl Daemon {
    fn dir() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sill-daemon-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["run", "state", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        dir
    }

    fn command(dir: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sill"));
        c.arg("daemon")
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("HOME", dir.join("home"))
            .env("SILL_LAUNCHER", "direct")
            .env_remove("SILL_SESSION")
            .env_remove("INVOCATION_ID")
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log")).unwrap());
        c
    }

    fn start() -> Daemon {
        Self::start_in(Self::dir())
    }

    /// A daemon in a dir another one used (or still uses).
    fn start_in(dir: PathBuf) -> Daemon {
        Self::start_env(dir, &[])
    }

    fn start_env(dir: PathBuf, env: &[(&str, &str)]) -> Daemon {
        let mut cmd = Self::command(&dir);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let d = Daemon { dir, child };
        d.wait_up();
        d
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/sill/daemon.sock")
    }

    fn pid_file(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.join("run/sill/daemon.pid")).ok()?.trim().parse().ok()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }

    fn wait_up(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(mut c) = self.connect()
                && c.call("_sill/daemon.status", json!({})).is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not come up; log:\n{}", self.log());
    }

    fn connect(&self) -> std::io::Result<Conn> {
        let s = UnixStream::connect(self.socket())?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(Conn { reader: BufReader::new(s.try_clone()?), stream: s, next: 1 })
    }

    /// Exit status once the process is gone, within the deadline.
    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit; log:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One connection, one request at a time, raw lines (sill_proto::rpc on the wire).
struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Conn {
    fn send_raw(&mut self, line: &str) -> std::io::Result<()> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")
    }

    fn read(&mut self) -> std::io::Result<Value> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed"));
        }
        Ok(serde_json::from_str(&line).unwrap())
    }

    fn call(&mut self, method: &str, params: Value) -> std::io::Result<Value> {
        let id = self.next;
        self.next += 1;
        self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string())?;
        let r = self.read()?;
        assert_eq!(r["id"], json!(id), "answers come back in order");
        Ok(r)
    }
}

#[test]
fn status_names_the_process_and_the_pid_file_matches() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let r = c.call("_sill/daemon.status", json!({})).unwrap();
    let status = &r["result"];
    assert_eq!(status["pid"], json!(d.child.id()));
    assert_eq!(status["protocol"], json!(sill_proto::PROTOCOL));
    assert_eq!(d.pid_file(), Some(d.child.id()));
    assert!(d.log().contains("started bare"), "{}", d.log());
    let socket = std::fs::metadata(d.socket()).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&socket.permissions()) & 0o777, 0o600);
}

#[test]
fn a_second_daemon_on_the_same_store_is_refused() {
    let d = Daemon::start();
    let out = Daemon::command(&d.dir).stderr(Stdio::piped()).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("another sill daemon (pid {})", d.child.id())), "{err}");
    // and the first one still answers
    d.connect().unwrap().call("_sill/daemon.status", json!({})).unwrap();
}

#[test]
fn a_second_daemon_with_its_own_store_refuses_a_socket_that_answers() {
    let d = Daemon::start();
    let other = Daemon::dir();
    let out = Daemon::command(&other).env("XDG_RUNTIME_DIR", d.dir.join("run")).stderr(Stdio::piped()).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already answers"), "{err}");
    let _ = std::fs::remove_dir_all(other);
}

#[test]
fn a_malformed_line_gets_a_parse_error_and_the_connection_lives() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    c.send_raw("not json").unwrap();
    let r = c.read().unwrap();
    assert_eq!(r["error"]["code"], json!(-32700), "{r}");
    c.send_raw("").unwrap(); // an empty line is skipped, not answered
    c.call("_sill/daemon.status", json!({})).unwrap();
    // an unknown method is an error, not a dropped connection
    let r = c.call("_sill/no.such", json!({})).unwrap();
    assert!(r.get("error").is_some(), "{r}");
    c.call("_sill/daemon.status", json!({})).unwrap();
}

#[test]
fn shutdown_answers_then_exits_clean() {
    let mut d = Daemon::start();
    let mut c = d.connect().unwrap();
    let r = c.call("_sill/daemon.shutdown", json!({})).unwrap();
    assert!(r.get("error").is_none(), "{r}");
    let st = d.wait_exit();
    assert!(st.success(), "{st}");
    assert!(c.read().is_err(), "the connection closes with the daemon");
    assert_eq!(d.pid_file(), None, "a clean exit takes the pid file with it");
}

#[test]
fn a_killed_daemon_leaves_a_stale_socket_the_next_one_replaces() {
    let mut d = Daemon::start();
    let first = d.child.id();
    rustix::process::kill_process(rustix::process::Pid::from_child(&d.child), rustix::process::Signal::KILL).unwrap();
    d.wait_exit();
    assert!(d.socket().exists(), "the socket file outlives a crash");
    assert_eq!(d.pid_file(), Some(first), "so does the pid file");
    assert!(UnixStream::connect(d.socket()).is_err(), "and nobody answers at it");
    let dir = d.dir.clone();
    std::mem::forget(d); // keep the dir: the next daemon starts in it
    let d = Daemon::start_in(dir);
    assert_ne!(d.child.id(), first);
    assert_eq!(d.pid_file(), Some(d.child.id()), "the pid file is the new daemon's");
    let r = d.connect().unwrap().call("_sill/daemon.status", json!({})).unwrap();
    assert_eq!(r["result"]["pid"], json!(d.child.id()));
}

#[cfg(debug_assertions)]
#[test]
fn a_panic_ends_the_daemon_with_a_failure_status() {
    let mut d = Daemon::start();
    let mut c = d.connect().unwrap();
    let _ = c.send_raw(&json!({"jsonrpc": "2.0", "id": 1, "method": "_sill/daemon.panic"}).to_string());
    let st = d.wait_exit();
    assert_eq!(st.code(), Some(101), "{st}");
    let log = d.log();
    assert!(log.contains("panic in thread sill-daemon-conn") && log.contains("sessions keep running"), "{log}");
    assert!(c.read().is_err(), "the connection is gone");
    // a daemon of the same store starts again: the lock went with the process
    let dir = d.dir.clone();
    std::mem::forget(d);
    let d = Daemon::start_in(dir);
    d.connect().unwrap().call("_sill/daemon.status", json!({})).unwrap();
}

#[cfg(debug_assertions)]
#[test]
fn the_watchdog_ends_a_daemon_with_a_stuck_lock() {
    let mut d = Daemon::start_env(Daemon::dir(), &[("SILL_WATCHDOG_SECS", "2")]);
    let mut c = d.connect().unwrap();
    let _ = c.send_raw(&json!({"jsonrpc": "2.0", "id": 1, "method": "_sill/daemon.deadlock"}).to_string());
    let st = d.wait_exit();
    assert_eq!(st.code(), Some(102), "{st}");
    let log = d.log();
    assert!(log.contains("the runners lock has been held for 2 s: stuck"), "{log}");
    let dir = d.dir.clone();
    std::mem::forget(d);
    let d = Daemon::start_in(dir);
    d.connect().unwrap().call("_sill/daemon.status", json!({})).unwrap();
}

#[test]
fn the_unit_hears_ready_watchdog_and_stopping() {
    let dir = Daemon::dir();
    let notify = dir.join("notify.sock");
    let manager = std::os::unix::net::UnixDatagram::bind(&notify).unwrap();
    manager.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut d = Daemon::start_env(dir, &[("NOTIFY_SOCKET", notify.to_str().unwrap()), ("SILL_WATCHDOG_SECS", "2")]);
    let mut next = || {
        let mut buf = [0u8; 256];
        let n = manager.recv(&mut buf).expect("a notification");
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    assert_eq!(next(), "READY=1");
    assert_eq!(next(), "WATCHDOG=1", "a tick within the patience");
    d.connect().unwrap().call("_sill/daemon.shutdown", json!({})).unwrap();
    assert!(d.wait_exit().success());
    let rest: Vec<String> = std::iter::from_fn(|| {
        let mut buf = [0u8; 256];
        manager.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        manager.recv(&mut buf).ok().map(|n| String::from_utf8_lossy(&buf[..n]).to_string())
    })
    .collect();
    assert!(rest.contains(&"STOPPING=1".to_string()), "{rest:?}");
}

#[test]
fn a_call_nobody_answers_times_out_with_advice() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let c = sill_proto::Client::over(ours).unwrap();
    let e = c.call_within::<Value>("_sill/daemon.status", json!({}), Duration::from_millis(200)).unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains("did not answer _sill/daemon.status in 0 s") && msg.contains("sill restart"), "{msg}");
}

#[test]
fn the_schema_is_printed_without_a_daemon() {
    let out = Command::new(env!("CARGO_BIN_EXE_sill")).args(["daemon", "schema"]).env("SILL_NO_DAEMON_START", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["protocol"], json!(sill_proto::PROTOCOL));
    assert!(doc["$defs"]["Event"].is_object() && doc["$defs"]["Command"].is_object());
}

#[test]
fn attach_to_an_unknown_session_is_refused_cleanly() {
    let d = Daemon::start();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sill"));
    cmd.args(["attach", "no-such-session"]).env("XDG_RUNTIME_DIR", d.dir.join("run")).env("XDG_STATE_HOME", d.dir.join("state")).env("HOME", d.dir.join("home")).env_remove("SILL_SESSION");
    let out = cmd.stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no session no-such-session"), "{err}");
}
