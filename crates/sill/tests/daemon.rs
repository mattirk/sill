//! The daemon's lifecycle in bare mode (docs/daemon.md §3.7), against a throwaway runtime and
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
        let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("daemon-{}-{n}", std::process::id())) /* on disk: each daemon stages a copy of the debug binary, too much for a /tmp tmpfs */;
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
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("HOME", dir.join("home"))
            .env("SILL_LAUNCHER", "direct")
            .env_remove("SILL_SESSION")
            .env_remove("INVOCATION_ID")
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log")).unwrap());
        // a session leader without a terminal, as systemd and a client's bare start make it: a
        // terminal the daemon opens carelessly becomes its own, and its hangup kills the daemon
        // (the host shell's SIGHUP, 2026-11-03)
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
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
                && c.call(sill_proto::method::DAEMON_STATUS, json!({})).is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not come up; log:\n{}", self.log());
    }

    fn connect(&self) -> std::io::Result<Conn> { self.connect_caps(json!({})) }

    fn connect_caps(&self, capabilities:Value) -> std::io::Result<Conn> {
        let s = UnixStream::connect(self.socket())?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut conn = Conn { reader: BufReader::new(s.try_clone()?), stream: s, next: 1 };
        let reply = conn.call(sill_proto::initialize::METHOD, json!({"protocols":[sill_proto::initialize::PROTOCOL_ID], "client":{"name":"test","version":"0"}, "capabilities":capabilities}))?;
        if reply.get("error").is_some() { return Err(std::io::Error::other(reply.to_string())); }
        Ok(conn)
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

/// Attach a stream to a terminal; the token line and first keystrokes go in one write.
fn attach_terminal(d: &Daemon, c: &mut Conn, id: &str, input: &str) -> UnixStream {
    let r = c.call("_sill/terminals.attach", json!({"terminal_id": id})).unwrap();
    let token = r["result"]["token"].as_str().unwrap_or_else(|| panic!("attach: {r}")).to_string();
    let mut stream = UnixStream::connect(d.socket()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(format!("{}\n{input}", json!({"_sill_stream": token})).as_bytes()).unwrap();
    // The daemon serves connections in parallel: the first holds the token only once it has
    // output (its input's echo or the replay). Before that, the second could claim it first.
    let mut first = [0u8; 1];
    let (peeked, _) = rustix::net::recv(&stream, &mut first[..], rustix::net::RecvFlags::PEEK).unwrap();
    assert_eq!(peeked, 1, "the first stream got no output");
    let mut second = UnixStream::connect(d.socket()).unwrap();
    second.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    second.write_all(format!("{}\n", json!({"_sill_stream": token})).as_bytes()).unwrap();
    let mut byte = [0];
    assert_eq!(std::io::Read::read(&mut second, &mut byte).unwrap(), 0, "token cannot be reused");
    stream
}

fn read_until(stream: &mut UnixStream, needle: &str) -> String {
    let mut output = Vec::new();
    let mut bytes = [0u8; 4096];
    while !String::from_utf8_lossy(&output).contains(needle) {
        match std::io::Read::read(stream, &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => output.extend_from_slice(&bytes[..n]),
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[test]
fn a_terminal_outlives_its_stream_and_replays_on_attach() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let home = d.dir.join("home");
    let open = c.call("_sill/terminals.open", json!({
        "label": "test", "argv": ["/bin/sh", "-c", "while read line; do [ \"$line\" = size ] && stty size; [ \"$line\" = quit ] && exit 3; printf 'echo:%s\\n' \"$line\"; done"],
        "cwd": sill_proto::sessions::HostPath::from_path(&home), "cols": 80, "rows": 24,
    })).unwrap();
    let id = open["result"]["terminal_id"].as_str().unwrap_or_else(|| panic!("open: {open}")).to_string();
    assert_eq!(open["result"]["state"], "running");
    let resize = c.call("_sill/terminals.resize", json!({"terminal_id": id, "cols": 92, "rows": 41})).unwrap();
    assert!(resize.get("error").is_none(), "{resize}");

    let mut first = attach_terminal(&d, &mut c, &id, "hello\nsize\n");
    let seen = read_until(&mut first, "41 92");
    assert!(seen.contains("echo:hello") && seen.contains("41 92"), "{seen:?}");
    drop(first);
    std::thread::sleep(Duration::from_millis(300));

    // closing the stream detached: the shell still runs and its output comes back first
    let list = c.call("_sill/terminals.list", json!({})).unwrap();
    assert_eq!(list["result"]["terminals"][0]["state"], "running", "{list}");
    assert_eq!(list["result"]["terminals"][0]["label"], "test");
    let mut second = attach_terminal(&d, &mut c, &id, "again\n");
    let seen = read_until(&mut second, "echo:again");
    assert!(seen.contains("echo:hello"), "replay missing: {seen:?}");
    assert!(seen.contains("echo:again"), "{seen:?}");

    // the program's exit ends the stream; the terminal stays readable with its exit code
    second.write_all(b"quit\n").unwrap();
    let mut rest = Vec::new();
    let _ = std::io::Read::read_to_end(&mut second, &mut rest);
    let mut state = json!(null);
    for _ in 0..50 {
        state = c.call("_sill/terminals.get", json!({"terminal_id": id})).unwrap();
        if state["result"]["exit_code"] == 3 { break }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(state["result"]["state"], "exited", "{state}");
    assert_eq!(state["result"]["exit_code"], 3, "{state}");

    // close forgets it
    assert!(c.call("_sill/terminals.close", json!({"terminal_id": id})).unwrap().get("error").is_none());
    let gone = c.call("_sill/terminals.get", json!({"terminal_id": id})).unwrap();
    assert_eq!(gone["error"]["data"]["code"], "not_found", "{gone}");
}

#[test]
fn unclaimed_stream_tokens_are_bounded() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let open = c.call("_sill/terminals.open", json!({"argv": ["/bin/sh", "-c", "exec sleep 600"], "cols": 80, "rows": 24})).unwrap();
    let id = open["result"]["terminal_id"].as_str().unwrap().to_string();
    for i in 0..64 {
        let r = c.call("_sill/terminals.attach", json!({"terminal_id": id})).unwrap();
        assert!(r["result"]["token"].is_string(), "token {i}: {r}");
    }
    let refused = c.call("_sill/terminals.attach", json!({"terminal_id": id})).unwrap();
    assert_eq!(refused["error"]["data"]["code"], "limit_exceeded", "{refused}");
    c.call("_sill/terminals.close", json!({"terminal_id": id})).unwrap();
}

#[test]
fn closing_a_terminal_hangs_up_its_shell_and_ends_streams() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let open = c.call("_sill/terminals.open", json!({"argv": ["/bin/sh", "-c", "echo up; exec sleep 600"], "cols": 80, "rows": 24})).unwrap();
    let id = open["result"]["terminal_id"].as_str().unwrap_or_else(|| panic!("open: {open}")).to_string();
    let pid = open["result"]["pid"].as_u64().unwrap();
    let mut stream = attach_terminal(&d, &mut c, &id, "");
    assert!(read_until(&mut stream, "up").contains("up"));
    assert!(c.call("_sill/terminals.close", json!({"terminal_id": id})).unwrap().get("error").is_none());
    let mut rest = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stream, &mut rest);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z ")) {
        assert!(std::time::Instant::now() < deadline, "the shell survived its terminal's close");
        std::thread::sleep(Duration::from_millis(50));
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
        loop {
            let r = self.read()?;
            if r.get("method").is_some() { continue; }
            assert_eq!(r["id"], json!(id), "answers come back in order");
            return Ok(r);
        }
    }
}

#[test]
fn status_names_the_process_and_the_pid_file_matches() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let r = c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
    let status = &r["result"];
    assert_eq!(status["pid"], json!(d.child.id()));
    assert_eq!(status["protocol"], json!(sill_proto::initialize::PROTOCOL_ID));
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
    d.connect().unwrap().call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
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
    c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
    // an unknown method is an error, not a dropped connection
    let r = c.call("_sill/no.such", json!({})).unwrap();
    assert!(r.get("error").is_some(), "{r}");
    c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
}

#[test]
fn shutdown_answers_then_exits_clean() {
    let mut d = Daemon::start();
    let mut c = d.connect().unwrap();
    let r = c.call(sill_proto::method::DAEMON_SHUTDOWN, json!({})).unwrap();
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
    let r = d.connect().unwrap().call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
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
    d.connect().unwrap().call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
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
    d.connect().unwrap().call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
}

/// The sweep (idle ends, ghost reverts, the stray reaper, the nightly run) is one thread that holds
/// no lock while it works: stuck in one step, the lock probe never saw it, and everything after
/// that step stopped quietly. Its heartbeat is now the watchdog's (docs/daemon.md §3.7).
#[cfg(debug_assertions)]
#[test]
fn the_watchdog_ends_a_daemon_whose_sweep_is_stuck() {
    let mut d = Daemon::start_env(Daemon::dir(), &[("SILL_WATCHDOG_SECS", "2"), ("SILL_SWEEP_SECS", "1")]);
    d.connect().unwrap().call("_sill/daemon.stall_sweep", json!({})).unwrap();
    let st = d.wait_exit();
    assert_eq!(st.code(), Some(102), "{st}");
    let log = d.log();
    assert!(log.contains("the minute sweep has not finished for") && log.contains(": stuck;"), "{log}");
}

#[test]
fn the_unit_hears_ready_watchdog_and_stopping() {
    let dir = Daemon::dir();
    let notify = dir.join("notify.sock");
    let manager = std::os::unix::net::UnixDatagram::bind(&notify).unwrap();
    manager.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut d = Daemon::start_env(dir, &[("NOTIFY_SOCKET", notify.to_str().unwrap()), ("SILL_WATCHDOG_SECS", "2")]);
    let next = || {
        let mut buf = [0u8; 256];
        let n = manager.recv(&mut buf).expect("a notification");
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    assert_eq!(next(), "READY=1");
    assert_eq!(next(), "WATCHDOG=1", "a tick within the patience");
    d.connect().unwrap().call(sill_proto::method::DAEMON_SHUTDOWN, json!({})).unwrap();
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
    let e = c.call_within::<Value>(sill_proto::method::DAEMON_STATUS, json!({}), Duration::from_millis(200)).unwrap_err();
    let msg = format!("{e:#}");
    assert!(msg.contains(&format!("did not answer {} in 0.2 s", sill_proto::method::DAEMON_STATUS)) && msg.contains("sill restart"), "{msg}");
}

#[test]
fn the_schema_is_printed_without_a_daemon() {
    let out = Command::new(env!("CARGO_BIN_EXE_sill")).args(["daemon", "schema"]).env("SILL_NO_DAEMON_START", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["protocol"], json!(sill_proto::initialize::PROTOCOL_ID));
    assert!(doc["$defs"]["Event"].is_object() && doc["$defs"]["Submission"].is_object());
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

#[test]
fn initialization_gates_requests_and_recovers_after_an_incompatible_offer() {
    let d = Daemon::start();
    let s = UnixStream::connect(d.socket()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut c = Conn { reader: BufReader::new(s.try_clone().unwrap()), stream: s, next: 1 };
    let status = c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
    assert_eq!(status["error"]["data"]["code"], "not_initialized");
    let offer = |protocol: &str| json!({"protocols":[protocol], "client":{"name":"test","version":"0"}, "capabilities":{"sill.internal":1}});
    let denied = c.call(sill_proto::initialize::METHOD, offer("unsupported")).unwrap();
    assert_eq!(denied["error"]["data"]["code"], "incompatible");
    let ok = c.call(sill_proto::initialize::METHOD, offer(sill_proto::initialize::PROTOCOL_ID)).unwrap();
    assert_eq!(ok["result"]["protocol"], sill_proto::initialize::PROTOCOL_ID);
    let repeated = c.call(sill_proto::initialize::METHOD, offer(sill_proto::initialize::PROTOCOL_ID)).unwrap();
    assert_eq!(repeated["error"]["data"]["code"], "invalid_state");
    assert_eq!(c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap()["result"]["pid"], d.child.id());
}

#[test]
fn initialized_connections_receive_public_change_notifications_only_after_handshake() {
    let d=Daemon::start();
    let connect_raw=|| {
        let socket=UnixStream::connect(d.socket()).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(150))).unwrap();
        Conn{reader:BufReader::new(socket.try_clone().unwrap()),stream:socket,next:1}
    };
    let mut uninitialized=connect_raw();
    // Make the server establish a control connection while leaving it uninitialized.
    assert_eq!(uninitialized.call(sill_proto::method::DAEMON_STATUS,json!({})).unwrap()["error"]["data"]["code"],"not_initialized");
    let mut public=connect_raw();
    public.call(sill_proto::initialize::METHOD,json!({"protocols":[sill_proto::initialize::PROTOCOL_ID],"client":{"name":"external","version":"0"},"capabilities":{}})).unwrap();
    assert_eq!(public.call("_sill/internal/sessions.list",json!({})).unwrap()["error"]["code"],-32601);
    let mut publisher=d.connect().unwrap();
    let session=publisher.call("_sill/sessions.create",json!({"create_id":"notifications","agent":"claude","policy":"default"})).unwrap()["result"]["session_id"].clone();
    let request_id=publisher.next;
    publisher.send_raw(&json!({"jsonrpc":"2.0","id":request_id,"method":"_sill/sessions.delete","params":{"session_id":session}}).to_string()).unwrap();
    let mut changed=false;
    loop {
        let message=publisher.read().unwrap();
        if message.get("method")==Some(&json!("_sill/changes.changed")) {changed=true;}
        if message.get("id")==Some(&json!(request_id)) {break;}
    }
    assert!(changed,"initialized subscriber receives the public change notification");
    assert_eq!(public.read().unwrap()["method"], "_sill/changes.changed");
    for mut conn in [uninitialized] {
        let error=conn.read().unwrap_err();
        assert!(matches!(error.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut),"{error}");
    }
}

#[test]
fn public_session_lifecycle_retries_pagination_and_restart() {
    let mut d=Daemon::start();
    let mut c=d.connect_caps(json!({})).unwrap();
    let call=|c:&mut Conn,m:&str,p:Value| {
        let r=c.call(m,p).unwrap();assert!(r.get("error").is_none(),"{m}: {r}");r["result"].clone()
    };
    assert_eq!(c.call("_sill/internal/sessions.list",json!({})).unwrap()["error"]["code"],-32601);
    let input=json!({"create_id":"durable-create","agent":"claude","policy":"default"});
    let first=call(&mut c,"_sill/sessions.create",input.clone());
    assert_eq!(call(&mut c,"_sill/sessions.create",input.clone()),first);
    assert_eq!(call(&mut c,"_sill/sessions.stop",first.clone()),json!({}));
    assert_eq!(call(&mut c,"_sill/sessions.get",first.clone())["ended_at"],Value::Null);
    let conflict=c.call("_sill/sessions.create",json!({"create_id":"durable-create","agent":"codex","policy":"default"})).unwrap();
    assert_eq!(conflict["error"]["data"]["code"],"id_conflict");
    for i in 0..4 {call(&mut c,"_sill/sessions.create",json!({"create_id":format!("page-{i}"),"agent":"claude","policy":"default"}));}
    let page=call(&mut c,"_sill/sessions.list",json!({"limit":2}));
    assert_eq!(page["sessions"].as_array().unwrap().len(),2);
    let mut seen:Vec<Value>=page["sessions"].as_array().unwrap().iter().map(|s|s["session_id"].clone()).collect();
    // Rename a record between pages: stable creation ordering must not repeat or skip it.
    call(&mut c,"_sill/sessions.update",json!({"session_id":first["session_id"],"change":{"type":"title","text":"Public name","kind":"person"}}));
    let mut next=page["next"].clone();
    while !next.is_null() {
        let page=call(&mut c,"_sill/sessions.list",json!({"limit":2,"after":next}));
        for s in page["sessions"].as_array().unwrap() {assert!(!seen.contains(&s["session_id"]));seen.push(s["session_id"].clone());}
        next=page["next"].clone();
    }
    assert_eq!(seen.len(),5);
    assert_eq!(call(&mut c,"_sill/sessions.get",first.clone())["title"],"Public name");
    assert_eq!(c.call("_sill/sessions.get",json!({"session_id":first["session_id"].as_str().unwrap()[..4].to_string()})).unwrap()["error"]["data"]["code"],"not_found");
    assert_eq!(c.call("_sill/sessions.list",json!({"limit":129})).unwrap()["error"]["data"]["code"],"limit_exceeded");
    assert_eq!(c.call("_sill/sessions.list",json!({"after":"bad"})).unwrap()["error"]["data"]["code"],"invalid_cursor");
    // Unknown change kinds cannot mutate permissions or fall back to another operation.
    assert!(c.call("_sill/sessions.update",json!({"session_id":first["session_id"],"change":{"type":"future","enabled":true}})).unwrap().get("error").is_some());
    drop(c);d.child.kill().unwrap();d.wait_exit();
    let dir=d.dir.clone();std::mem::forget(d);
    let d=Daemon::start_in(dir);let mut c=d.connect_caps(json!({})).unwrap();
    assert_eq!(call(&mut c,"_sill/sessions.create",input.clone()),first);
    assert_eq!(call(&mut c,"_sill/sessions.get",first.clone())["title"],"Public name");
    // A no-prompt end deletes the row. Retried creation must never resurrect it.
    let end=call(&mut c,"_sill/sessions.end",json!({"session_id":first["session_id"],"reason":"close"}));
    assert_eq!(end,json!({"pending":false,"deleted":true}));
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        let result=c.call("_sill/sessions.create",input.clone()).unwrap();
        if result["error"]["data"]["code"]=="gone" {break;}
        assert!(Instant::now()<deadline,"deletion did not finish: {result}; {}",d.log());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(c.call("_sill/sessions.start",first).unwrap()["error"]["data"]["code"],"not_found");
}

/// Mock programs and registry only: no network, installed packages, or real login credentials.
fn host_fixture() -> Daemon {
    use std::os::unix::fs::PermissionsExt;
    let dir = Daemon::dir();
    let agents = dir.join("home/.config/sill/agents");
    std::fs::create_dir_all(&agents).unwrap();
    let script = dir.join("login.sh");
    std::fs::write(&script, r#"#!/bin/sh
read init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"authMethods":[{"id":"browser"}]}}'
read auth
sleep 1
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
"#).unwrap();
    std::fs::write(agents.join("fixture.toml"), format!("id = 'fixture'\nname = 'Fixture'\ncommand = ['/bin/sh', '{}']\n", script.display())).unwrap();
    // Fake local installer: one package fails, the other creates only fixture files.
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("npm"), r#"#!/bin/sh
echo fixture-installer-output
case "$*" in
  *fixture-success@1.0.0*)
    mkdir -p node_modules/fixture-success node_modules/.bin
    printf '%s' '{"version":"1.0.0","bin":"fixture-success"}' > node_modules/fixture-success/package.json
    printf '#!/bin/sh\nexit 0\n' > node_modules/.bin/fixture-success
    chmod +x node_modules/.bin/fixture-success
    exit 0 ;;
esac
exit 23
"#).unwrap();
    std::fs::write(bin.join("bwrap"), "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(bin.join("bwrap"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(bin.join("npm"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let registry = dir.join("state/sill/registry");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::write(registry.join("registry.json"), json!({"agents":[{"id":"fixture","name":"Fixture","version":"1.0.0","distribution":{"npx":{"package":"fixture@1.0.0"}}},{"id":"fixture-success","name":"Fixture Success","version":"1.0.0","distribution":{"npx":{"package":"fixture-success@1.0.0"}}}]}).to_string()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    Daemon::start_env(dir, &[("PATH", &path)])
}

fn claim_host(d: &Daemon, reply: &Value) -> BufReader<UnixStream> {
    let token = reply["result"]["token"].as_str().unwrap_or_else(|| panic!("{reply}"));
    let mut socket = UnixStream::connect(d.socket()).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    socket.write_all(format!("{}\n", json!({"_sill_stream":token})).as_bytes()).unwrap();
    BufReader::new(socket)
}

#[test]
fn public_host_streams_validate_selectors_and_report_login_and_install_outcomes() {
    let d = host_fixture();
    let mut c = d.connect_caps(json!({})).unwrap();
    let mut after = Value::Null;
    let mut ids = Vec::new();
    loop {
        let page = c.call("_sill/registry.list", json!({"after":after,"limit":1})).unwrap();
        let entries = page["result"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        ids.push(entries[0]["entry_id"].as_str().unwrap().to_string());
        after = page["result"]["next"].clone();
        if after.is_null() { break; }
    }
    assert!(ids.windows(2).all(|p| p[0] < p[1]));
    assert!(ids.iter().any(|id| id == "fixture"));
    let install = json!({"entry_id":"fixture","version":"1.0.0","source":"live"});
    let denied = c.call("_sill/registry.install", install.clone()).unwrap();
    assert_eq!(denied["error"]["data"]["code"], "permission_denied");
    assert!(!d.dir.join("state/sill/tools/fixture").exists());
    let mut allowed = install;
    allowed["allow_unreviewed"] = json!(true);
    let opened = c.call("_sill/registry.install", allowed).unwrap();
    let events: Vec<Value> = claim_host(&d, &opened).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert!(events.iter().any(|e| e["type"] == "output" && e["text"] == "fixture-installer-output"), "{events:?}");
    assert_eq!(events.last().unwrap()["type"], "failed");
    assert_eq!(events.last().unwrap()["code"], "operation_failed");
    let success = c.call("_sill/registry.install", json!({"entry_id":"fixture-success","version":"1.0.0","source":"live","allow_unreviewed":true})).unwrap();
    let events: Vec<Value> = claim_host(&d, &success).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(events.last().unwrap(), &json!({"type":"completed","agent_id":"fixture-success"}));
    assert!(d.dir.join("home/.config/sill/agents/fixture-success.toml").exists());
    let login = json!({"agent_id":"fixture","method_id":"browser"});
    let opened = c.call("_sill/agents.login", login.clone()).unwrap();
    let busy = c.call("_sill/agents.login", login).unwrap();
    assert_eq!(busy["error"]["data"]["code"], "busy");
    let events: Vec<Value> = claim_host(&d, &opened).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(events.last().unwrap(), &json!({"type":"completed","agent_id":"fixture"}));
    let bad_method = c.call("_sill/agents.login", json!({"agent_id":"fixture","method_id":"unknown"})).unwrap();
    let events: Vec<Value> = claim_host(&d, &bad_method).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(events.last().unwrap()["type"], "failed");
    let bad_port = c.call("_sill/streams.port", json!({"session_id":"abcdef01","port":0})).unwrap();
    assert_eq!(bad_port["error"]["code"], -32602);
    let unknown = c.call("_sill/agents.login", json!({"agent_id":"absent-fixture","method_id":"browser"})).unwrap();
    assert_eq!(unknown["error"]["data"]["code"], "not_found");
}

#[test]
#[ignore = "requires Python jsonschema; run explicitly with SILL_SFP_PYTHON"]
fn independent_python_client_consumes_public_host_streams() {
    let d = host_fixture();
    let mut c = d.connect_caps(json!({})).unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"python-port", "agent":"claude", "policy":"default"})).unwrap();
    let session = created["result"]["session_id"].as_str().unwrap();
    let shim = mock_port(&d, session);
    let output = Command::new(std::env::var("SILL_SFP_PYTHON").unwrap_or_else(|_| "python3".into()))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sfp_host_scenario.py"))
        .arg(d.socket()).arg(session).output().unwrap();
    assert!(output.status.success(), "{}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr), d.log());
    shim.join().unwrap();
}

fn mock_port(d: &Daemon, session: &str) -> std::thread::JoinHandle<()> {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let runtime = d.dir.join("run/sill").join(session);
    std::fs::create_dir_all(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("shim.sock")).unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), json!({"role":"port","port":8123}));
        socket.write_all(b"{\"ok\":true}\n").unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"\0hello\xff");
        // The caller's EOF must leave the response direction writable.
        socket.write_all(b"\xffreply\0").unwrap();
    })
}

#[test]
fn public_port_stream_preserves_bytes_and_half_close() {
    use std::io::Read;
    let d = Daemon::start();
    let mut c = d.connect_caps(json!({})).unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"port-fixture", "agent":"claude", "policy":"default"})).unwrap();
    let session = created["result"]["session_id"].as_str().unwrap();
    let params = json!({"session_id":session,"port":8123});
    assert_eq!(c.call("_sill/streams.port", params.clone()).unwrap()["error"]["data"]["code"], "invalid_state");
    let shim = mock_port(&d, session);
    let opened = c.call("_sill/streams.port", params).unwrap();
    let mut stream = claim_host(&d, &opened);
    stream.get_mut().write_all(b"\0hello\xff").unwrap();
    stream.get_mut().shutdown(std::net::Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"\xffreply\0");
    shim.join().unwrap();
}

#[test]
fn disconnecting_login_keeps_the_job_until_it_finishes() {
    let d = host_fixture();
    let mut c = d.connect_caps(json!({})).unwrap();
    let login = json!({"agent_id":"fixture","method_id":"browser"});
    let opened = c.call("_sill/agents.login", login.clone()).unwrap();
    let stream = claim_host(&d, &opened);
    stream.get_ref().shutdown(std::net::Shutdown::Both).unwrap();
    drop(stream);
    assert_eq!(c.call("_sill/agents.login", login.clone()).unwrap()["error"]["data"]["code"], "busy");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = c.call("_sill/agents.login", login.clone()).unwrap();
        if reply.get("error").is_none() {
            let events: Vec<Value> = claim_host(&d, &reply).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
            assert_eq!(events.last().unwrap()["type"], "completed");
            break;
        }
        assert!(Instant::now() < deadline, "login slot not released: {reply}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn host_operation_receipts_survive_crash_and_do_not_repeat_uncertain_work() {
    let mut d = host_fixture();
    let mut c = d.connect_caps(json!({})).unwrap();
    let complete = json!({"operation_id":"finished-login", "agent_id":"fixture", "method_id":"browser"});
    let opened = c.call("_sill/agents.login", complete.clone()).unwrap();
    let events: Vec<Value> = claim_host(&d, &opened).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(events.last().unwrap()["type"], "completed");
    // An unclaimed stream cannot prevent acceptance or bind the durable receipt to its token.
    let uncertain = json!({"operation_id":"interrupted-login", "agent_id":"fixture", "method_id":"browser"});
    let opened = c.call("_sill/agents.login", uncertain.clone()).unwrap();
    assert_eq!(opened["result"]["operation"]["state"], "running");
    let repeat = c.call("_sill/agents.login", uncertain.clone()).unwrap();
    assert!(repeat["result"]["token"].is_null());
    assert_eq!(repeat["result"]["operation"]["operation_id"], "interrupted-login");
    d.child.kill().unwrap();
    d.wait_exit();
    drop(c);
    let dir = d.dir.clone();
    std::mem::forget(d);
    let d = Daemon::start_in(dir);
    let mut c = d.connect_caps(json!({})).unwrap();
    let finished = c.call("_sill/operations.get", json!({"operation_id":"finished-login"})).unwrap();
    assert_eq!(finished["result"]["state"], "completed");
    assert_eq!(finished["result"]["agent_id"], "fixture");
    let interrupted = c.call("_sill/operations.get", json!({"operation_id":"interrupted-login"})).unwrap();
    assert_eq!(interrupted["result"]["state"], "unknown");
    let repeat = c.call("_sill/agents.login", uncertain).unwrap();
    assert_eq!(repeat["result"]["operation"], interrupted["result"]);
    assert!(repeat["result"]["token"].is_null());
    let repeat = c.call("_sill/agents.login", complete).unwrap();
    assert_eq!(repeat["result"]["operation"], finished["result"]);
    assert!(repeat["result"]["token"].is_null());
    let conflict = c.call("_sill/agents.login", json!({"operation_id":"finished-login","agent_id":"fixture","method_id":"changed"})).unwrap();
    assert_eq!(conflict["error"]["data"]["code"], "id_conflict");
}

#[test]
fn session_images_use_jail_descriptors_across_daemon_restart() {
    use std::os::{fd::AsFd, unix::ffi::OsStrExt};
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use sill_proto::{admin as a, sessions::HostPath};

    let mut d = Daemon::start();
    let mut c = d.connect().unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"image-test", "agent":"claude", "policy":"default"})).unwrap();
    let session = created["result"]["session_id"].as_str().unwrap().to_owned();
    let runtime = d.dir.join("run/sill").join(&session);
    std::fs::create_dir_all(&runtime).unwrap();
    let socket = runtime.join("shim.sock");
    // Simulate the distinct mount view: the host has different bytes at the requested path.
    let path = PathBuf::from(format!("/tmp/sill-image-{}-{session}.jpg", std::process::id()));
    std::fs::write(&path, b"wrong host image").unwrap();
    let jail_image = d.dir.join("jail-image.jpg");
    let bytes = b"\xff\xd8\x00\x80receipt";
    std::fs::write(&jail_image, bytes).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let expected_path = path.as_os_str().as_bytes().to_vec();
    let served_image = jail_image.clone();
    let server = std::thread::spawn(move || {
        for connection in listener.incoming() {
            let mut stream = connection.unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 { continue; }
            if line.trim() == "stop" { break; }
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 { continue; }
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "open_image");
            if request["path_bytes"] == json!(expected_path) {
                let file = std::fs::File::open(&served_image).unwrap();
                let fds = [file.as_fd()];
                let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                control.push(SendAncillaryMessage::ScmRights(&fds));
                // Deliberately split the response: the fd arrives before the JSON line.
                sendmsg(&stream, &[std::io::IoSlice::new(b"{")], &mut control, SendFlags::empty()).unwrap();
                stream.write_all(b"\"ok\":true}\n").unwrap();
            } else {
                writeln!(stream, "{}", json!({"ok":false, "errno":2, "error":"missing in jail"})).unwrap();
            }
        }
    });
    let request = |offset: &str, length, revision: Option<String>| json!(a::SessionImage {
        session_id: session.clone(), path: HostPath::from_path(&path),
        offset: offset.into(), length, revision,
    });
    let first = c.call("_sill/files.image", request("0", Some(2), None)).unwrap();
    let page: a::FileBytes = serde_json::from_value(first["result"].clone()).unwrap();
    assert_eq!(a::decode_bytes(&page.bytes_base64).unwrap(), &bytes[..2]);
    assert!(!page.eof);
    // Keep the same shim/socket and file while replacing only the daemon.
    c.call(sill_proto::method::DAEMON_SHUTDOWN, json!({})).unwrap();
    assert!(d.wait_exit().success());
    let mut restarted = Daemon::start_in(d.dir.clone());
    let mut c = restarted.connect().unwrap();
    let rest = c.call("_sill/files.image", request(&page.next_offset, None, Some(page.revision.clone()))).unwrap();
    let rest: a::FileBytes = serde_json::from_value(rest["result"].clone()).unwrap();
    assert_eq!(a::decode_bytes(&rest.bytes_base64).unwrap(), &bytes[2..]);
    assert!(rest.eof);
    let bad_length = c.call("_sill/files.image", request("0", Some(a::FILE_CHUNK_BYTES as u32 + 1), None)).unwrap();
    assert!(bad_length.get("error").is_some());
    std::fs::write(&jail_image, b"changed image").unwrap();
    let stale = c.call("_sill/files.image", request("2", None, Some(page.revision))).unwrap();
    assert_eq!(stale["error"]["data"]["code"], "invalid_cursor");
    let host_only = path.with_file_name(format!("sill-host-only-{session}.jpg"));
    std::fs::write(&host_only, b"must not fall back to this").unwrap();
    let mut missing = request("0", None, None);
    missing["path"] = json!(HostPath::from_path(&host_only));
    let absent = c.call("_sill/files.image", missing).unwrap();
    assert_eq!(absent["error"]["data"]["code"], "not_found");
    for forbidden in ["/.sill/host/tmp/private.jpg", "/tmp/../private.jpg"] {
        let mut req = request("0", None, None);
        req["path"] = json!(HostPath::from_path(Path::new(forbidden)));
        assert!(c.call("_sill/files.image", req).unwrap().get("error").is_some());
    }
    let mut stop = UnixStream::connect(&socket).unwrap();
    writeln!(stop, "stop").unwrap();
    server.join().unwrap();
    std::fs::remove_file(&socket).unwrap();
    let stopped = c.call("_sill/files.image", request("0", None, None)).unwrap();
    assert_eq!(stopped["error"]["data"]["code"], "not_found", "a stopped jail's tmp image cannot resolve to the host");
    // A deliberately pinned directory under /tmp is a host bind, not private tmpfs.
    let pinned = PathBuf::from(format!("/tmp/sill-image-pin-{session}"));
    std::fs::create_dir(&pinned).unwrap();
    let pinned_image = pinned.join("image.jpg");
    std::fs::write(&pinned_image, bytes).unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"pinned-image-test",
        "agent":"claude", "policy":"default", "pins":[HostPath::from_path(&pinned)]})).unwrap();
    let pinned_reply = c.call("_sill/files.image", json!(a::SessionImage {
        session_id: created["result"]["session_id"].as_str().unwrap().into(),
        path: HostPath::from_path(&pinned_image), offset:"0".into(), length:None, revision:None,
    })).unwrap();
    let pinned_page: a::FileBytes = serde_json::from_value(pinned_reply["result"].clone()).unwrap();
    assert_eq!(a::decode_bytes(&pinned_page.bytes_base64).unwrap(), bytes);
    std::fs::remove_dir_all(pinned).unwrap();
    c.call(sill_proto::method::DAEMON_SHUTDOWN, json!({})).unwrap();
    assert!(restarted.wait_exit().success());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(host_only).unwrap();
}

#[test]
fn public_host_files_and_settings_need_no_internal_capability() {
    use sill_proto::{admin as a, sessions::HostPath};
    use std::os::unix::ffi::OsStringExt;
    let d=Daemon::start();let mut c=d.connect_caps(json!({})).unwrap();
    let p=d.dir.join("home").join(std::ffi::OsString::from_vec(b"raw-\xff".to_vec()));
    let path=HostPath::from_path(&p);
    let write=c.call("_sill/files.write",json!(a::FileWrite {path:path.clone(),bytes_base64:a::encode_bytes(b"abcdef")})).unwrap();
    assert!(write.get("error").is_none(),"{write}");
    let read=c.call("_sill/files.read",json!(a::FileRead {path:path.clone(),offset:"2".into(),length:Some(2),revision:None})).unwrap();
    assert_eq!(read["result"]["bytes_base64"],a::encode_bytes(b"cd"));assert_eq!(read["result"]["next_offset"],"4");assert_eq!(read["result"]["eof"],false);
    std::fs::write(&p,"changed file length").unwrap();
    let stale=c.call("_sill/files.read",json!({"path":path,"offset":"4","length":2,"revision":read["result"]["revision"]})).unwrap();
    assert_eq!(stale["error"]["data"]["code"],"invalid_cursor");
    let root=HostPath::from_path(&d.dir.join("home"));
    std::fs::write(d.dir.join("home/another"),"other").unwrap();
    let first=c.call("_sill/files.list",json!(a::FilesList {path:root.clone(),after:None,limit:Some(1)})).unwrap();
    assert_eq!(first["result"]["entries"].as_array().unwrap().len(),1);assert!(!first["result"]["next"].is_null());
    let last=c.call("_sill/files.list",json!({"path":root,"after":first["result"]["next"],"limit":1})).unwrap();
    assert_eq!(last["result"]["entries"][0]["path"]["bytes_base64"],path.bytes_base64);
    let link=d.dir.join("home/link");std::os::unix::fs::symlink(&p,&link).unwrap();
    assert!(c.call("_sill/files.read",json!(a::FileRead {path:HostPath::from_path(&link),offset:"0".into(),length:None,revision:None})).unwrap().get("error").is_some());
    assert!(c.call("_sill/files.list",json!({"path":root,"limit":0})).unwrap().get("error").is_some());
    let patch=c.call("_sill/settings.patch",json!({"changes":[{"type":"theme","value":"nord"},{"type":"tray","value":false}]})).unwrap();assert!(patch.get("error").is_none(),"{patch}");
    assert_eq!(c.call("_sill/settings.get",json!({})).unwrap()["result"]["theme"],"nord");
    assert!(c.call("_sill/settings.patch",json!({"changes":[{"type":"future","value":true}]})).unwrap().get("error").is_some());
}

#[test]
#[ignore = "requires Python jsonschema; run explicitly with SILL_SFP_PYTHON"]
fn independent_python_public_administration() {
    let d=Daemon::start();
    let output=Command::new(std::env::var("SILL_SFP_PYTHON").unwrap_or_else(|_|"python3".into()))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sfp_admin_scenario.py"))
        .arg(d.socket()).output().unwrap();
    assert!(output.status.success(),"{}\n{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr),d.log());
}

#[test]
fn public_memory_notes_and_text_are_paged_without_internal_apis() {
    let d=Daemon::start();let mut c=d.connect_caps(json!({})).unwrap();
    let created=c.call("_sill/sessions.create",json!({"create_id":"memory-public","agent":"claude","policy":"default"})).unwrap();
    let session=created["result"]["session_id"].as_str().unwrap();
    for text in ["first note","second note"] {
        let r=c.call("_sill/memory.notes.add",json!({"session_id":session,"text":text})).unwrap();assert!(r.get("error").is_none(),"{r}");
    }
    let first=c.call("_sill/memory.notes.list",json!({"session_id":session,"limit":1})).unwrap();
    assert_eq!(first["result"]["notes"][0]["text"],"first note");
    let last=c.call("_sill/memory.notes.list",json!({"session_id":session,"after":first["result"]["next"],"limit":1})).unwrap();
    assert_eq!(last["result"]["notes"][0]["text"],"second note");assert_eq!(last["result"]["next"],Value::Null);
    let note=&first["result"]["notes"][0];
    let removed=c.call("_sill/memory.notes.remove",json!({"session_id":session,"timestamp":note["timestamp"],"text":note["text"]})).unwrap();assert!(removed.get("error").is_none(),"{removed}");
    let read=c.call("_sill/memory.text",json!({"session_id":session,"format":"show","length":8})).unwrap();
    assert!(read["result"]["text"].as_str().unwrap().len()<=8);assert!(read["result"]["next"].is_string());
    c.call("_sill/memory.notes.add",json!({"session_id":session,"text":"changed"})).unwrap();
    let stale=c.call("_sill/memory.text",json!({"session_id":session,"format":"show","length":8,"offset":read["result"]["next"],"fingerprint":read["result"]["fingerprint"]})).unwrap();
    assert_eq!(stale["error"]["data"]["code"],"invalid_cursor");
    let continuation=json!({"session_id":session,"format":"show","snapshot_id":read["result"]["snapshot_id"],"offset":read["result"]["next"],"fingerprint":read["result"]["fingerprint"]});
    let mut stranger=d.connect_caps(json!({})).unwrap();
    assert_eq!(stranger.call("_sill/memory.text",continuation.clone()).unwrap()["error"]["data"]["code"],"snapshot_expired");
    let frozen=c.call("_sill/memory.text",continuation.clone()).unwrap();
    let text=frozen["result"]["text"].as_str().unwrap();assert!(text.contains("second note"));assert!(!text.contains("changed"),"snapshot must remain immutable");
    c.call("_sill/memory.text.close",json!({"snapshot_id":read["result"]["snapshot_id"]})).unwrap();
    assert_eq!(c.call("_sill/memory.text",continuation).unwrap()["error"]["data"]["code"],"snapshot_expired");
    assert!(c.call("_sill/memory.text",json!({"session_id":session,"format":"unknown"})).unwrap().get("error").is_some());
}

#[test]
fn public_changes_preserve_exact_paths_page_history_and_bound_mutations() {
    use sill_proto::sessions::HostPath;
    use std::os::unix::ffi::OsStringExt;
    let d=Daemon::start();let mut c=d.connect_caps(json!({})).unwrap();
    let repo=d.dir.join("home/repo");std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init","--quiet"]).arg(&repo).status().unwrap().success());
    let filename=std::ffi::OsString::from_vec(b"file-\xff\nname".to_vec());let file=repo.join(filename);
    std::fs::write(&file,"fixture contents\n").unwrap();
    let added=c.call("_sill/pins.add",json!({"path":HostPath::from_path(&repo)})).unwrap();assert!(added.get("error").is_none(),"{added}");
    let page=c.call("_sill/changes.list",json!({"limit":1})).unwrap();
    let entry=&page["result"]["entries"][0];assert_eq!(entry["type"],"live");
    let mut path:HostPath=serde_json::from_value(entry["path"].clone()).unwrap();assert_eq!(path.to_path().unwrap(),file);
    path.display="/wrong/display/path".into();
    let diff=c.call("_sill/changes.diff",json!({"target":{"type":"live","repo":HostPath::from_path(&repo),"path":path}})).unwrap();
    assert!(diff["result"]["text"].as_str().unwrap().contains("fixture contents"),"{diff}");
    let bad=c.call("_sill/changes.diff",json!({"target":{"type":"live","repo":HostPath::from_path(&repo),"path":path},"offset":"1","fingerprint":"wrong"})).unwrap();assert_eq!(bad["error"]["data"]["code"],"invalid_cursor");
    let bad=c.call("_sill/changes.list",json!({"after":"nonsense"})).unwrap();assert_eq!(bad["error"]["data"]["code"],"invalid_cursor");
    // History survives exact-byte storage and duplicate timestamps; each next cursor is exclusive.
    let store=sill_core::store::Store::open(&d.dir.join("state/sill/sill.db")).unwrap();
    for ts in [10,10,11] {let mut e=sill_memory::history::Event::new(sill_memory::history::Kind::Kept,&file,ts);e.paths=vec![file.clone()];sill_memory::history::record(store.conn(),&e).unwrap();}
    let mut before=Value::Null;let mut seen=Vec::new();loop{let page=c.call("_sill/changes.history",json!({"limit":1,"before":before,"path":path})).unwrap();let event=&page["result"]["events"][0];assert_eq!(serde_json::from_value::<HostPath>(event["path"].clone()).unwrap().to_path().unwrap(),file);assert!(!seen.contains(&event["event_id"]));seen.push(event["event_id"].clone());before=page["result"]["next"].clone();if before.is_null(){break;}}
    assert_eq!(seen.len(),3);
    // Public mutations use exact session identities and cannot interpret an unknown target.
    let invalid=c.call("_sill/changes.keep",json!({"session_id":"missing","items":[]})).unwrap();assert!(invalid.get("error").is_some());
    let invalid=c.call("_sill/changes.diff",json!({"target":{"type":"future","path":path}})).unwrap();assert_eq!(invalid["error"]["code"],-32602);
    let reverted=c.call("_sill/changes.git-revert",json!({"repo":HostPath::from_path(&repo),"path":path})).unwrap();assert!(reverted.get("error").is_none(),"{reverted}");assert!(!file.exists());
}

#[test]
#[ignore = "requires Python jsonschema; run explicitly with SILL_SFP_PYTHON"]
fn independent_python_public_changes() {
    use std::os::unix::ffi::OsStringExt;
    let d=Daemon::start();let repo=d.dir.join("home/repo");std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init","--quiet"]).arg(&repo).status().unwrap().success());
    std::fs::write(repo.join(std::ffi::OsString::from_vec(b"file-\xff\nname".to_vec())),"fixture contents\n").unwrap();
    let output=Command::new(std::env::var("SILL_SFP_PYTHON").unwrap_or_else(|_|"python3".into()))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sfp_changes_scenario.py"))
        .arg(d.socket()).arg(&repo).output().unwrap();
    assert!(output.status.success(),"{}\n{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr),d.log());
}

#[test]
fn public_network_denials_are_live_bounded_pages() {
    let d=Daemon::start();let mut c=d.connect_caps(json!({})).unwrap();
    let created=c.call("_sill/sessions.create",json!({"create_id":"denial-pages","agent":"claude","policy":"default"})).unwrap();
    let session=created["result"]["session_id"].as_str().unwrap();
    let runtime=d.dir.join("run/sill").join(session);std::fs::create_dir_all(&runtime).unwrap();
    let hosts=(0..201).map(|i|format!("host-{i:03}.example\n")).collect::<String>();std::fs::write(runtime.join("denied"),hosts).unwrap();
    let mut after=Value::Null;let mut found=Vec::new();
    loop {let response=c.call("_sill/policy.denied",json!({"session_id":session,"after":after,"limit":17})).unwrap();assert!(response.get("error").is_none(),"{response}");let page=&response["result"];
        assert!(page["hosts"].as_array().unwrap().len()+page["answered"].as_array().unwrap().len()+page["quiet"].as_array().unwrap().len()<=17);
        found.extend(page["hosts"].as_array().unwrap().iter().filter_map(|s|s.as_str().map(str::to_owned)));after=page["next"].clone();if after.is_null(){break;}
    }
    assert_eq!(found.len(),201);assert_eq!(found[0],"host-000.example");assert_eq!(found[200],"host-200.example");
    let answer=c.call("_sill/policy.remember_denied",json!({"session_id":session,"host":"host-000.example"})).unwrap();
    assert!(answer.get("error").is_none(),"{answer}");
    assert_eq!(std::fs::read_to_string(runtime.join("denied.answered")).unwrap(),"host-000.example\n");
    let allowed=c.call("_sill/policy.allow_host",json!({"session_id":session,"host":"*.example.test"})).unwrap();
    assert!(allowed.get("error").is_none(),"{allowed}");
    let agent=c.call("_sill/agents.get",json!({"agent_id":"claude"})).unwrap();
    assert!(agent["result"]["domains"].as_array().unwrap().contains(&json!("*.example.test")),"{agent}");
    for host in ["bad\nname", "../invalid"] {
        assert!(c.call("_sill/policy.allow_host",json!({"session_id":session,"host":host})).unwrap().get("error").is_some());
    }
    assert!(c.call("_sill/policy.denied",json!({"session_id":session,"limit":0})).unwrap().get("error").is_some());
}

#[test]
fn travelling_policy_check_import_and_explicit_replacement() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let text = "name = 'shared'\n[network]\nmode = 'none'\n";
    let checked = c.call("_sill/policy.check", json!({"text":text})).unwrap();
    assert_eq!(checked["result"]["valid"], true, "{checked}");
    let invalid = c.call("_sill/policy.check", json!({"text":"name = 'bad'\nunknown = true"})).unwrap();
    assert_eq!(invalid["result"]["valid"], false, "{invalid}");
    assert!(!d.dir.join("home/.config/sill/policies/shared.toml").exists());
    let imported = c.call("_sill/policy.import", json!({"name":"shared","text":text})).unwrap();
    assert_eq!(imported["result"]["name"], "shared", "{imported}");
    let saved = c.call("_sill/policy.get", json!({"name":"shared"})).unwrap();
    assert_eq!(saved["result"]["text"], imported["result"]["text"]);
    let changed = "name = 'shared'\n[network]\nmode = 'allow'\n";
    let conflict = c.call("_sill/policy.import", json!({"name":"shared","text":changed})).unwrap();
    assert_eq!(conflict["error"]["data"]["code"], "conflict", "{conflict}");
    let unchanged = c.call("_sill/policy.get", json!({"name":"shared"})).unwrap();
    assert_eq!(saved["result"], unchanged["result"]);
    let replaced = c.call("_sill/policy.import", json!({"name":"shared","text":changed,"replace":true})).unwrap();
    assert!(replaced.get("error").is_none(), "{replaced}");
    let invalid_name = c.call("_sill/policy.import", json!({"name":"../escape","text":text})).unwrap();
    assert_eq!(invalid_name["error"]["code"], -32602, "{invalid_name}");
    let oversized = c.call("_sill/policy.check", json!({"text":"x".repeat(1024*1024+1)})).unwrap();
    assert_eq!(oversized["error"]["data"]["code"], "limit_exceeded", "{oversized}");
}

#[test]
fn travelling_policy_preview_retains_launch_view_when_fragment_changes() {
    use sill_proto::sessions::HostPath;
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let root = d.dir.join("home/repo");
    std::fs::create_dir_all(root.join(".sill")).unwrap();
    std::fs::write(root.join("secret"), "secret").unwrap();
    std::fs::write(root.join(".sill/policy.toml"), "[paths]\nro = ['secret']\n[network]\nallow = ['packages.example']\n").unwrap();
    let pin = c.call("_sill/pins.add", json!({"path":HostPath::from_path(&root)})).unwrap();
    assert!(pin.get("error").is_none(), "{pin}");
    let created = c.call("_sill/sessions.create", json!({"create_id":"policy-preview","agent":"claude","policy":"default"})).unwrap();
    let id = created["result"]["session_id"].as_str().unwrap();
    let key = json!({"session_id":id});
    let empty = c.call("_sill/policy.snapshot", key.clone()).unwrap();
    assert!(empty["result"]["snapshot"].is_null(), "{empty}");
    let preview = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert_eq!(preview["result"]["sources"].as_array().unwrap().len(), 1, "{preview}");
    assert_eq!(preview["result"]["suggestions"][0]["host"], "packages.example");
    let base = sill_core::policy::Policy::builtin_default();
    let resolution = sill_core::policy_fragments::resolve(&base, &[root.clone()], &[]).unwrap();
    // A fixture snapshot tests the public read boundary; no jail is launched in this test.
    let snapshots = d.dir.join("state/sill/policy-snapshots").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::write(snapshots.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    std::fs::write(root.join(".sill/policy.toml"), "[paths]\ndirect = ['secret']\n").unwrap();
    let invalid = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert!(invalid["result"]["text"].is_null(), "{invalid}");
    assert!(!invalid["result"]["diagnostics"].as_array().unwrap().is_empty());
    let retained = c.call("_sill/policy.snapshot", key).unwrap();
    assert_eq!(retained["result"]["snapshot"]["view"], "launch");
    assert_eq!(retained["result"]["snapshot"]["suggestions"][0]["host"], "packages.example");
    let checked = c.call("_sill/policy.check", json!({"text":"[paths]\nmask=['../escape']","fragment":true,"root":HostPath::from_path(&root)})).unwrap();
    assert_eq!(checked["result"]["valid"], false, "{checked}");
}

#[test]
fn travelling_policy_suggestions_are_session_grants_and_offline_is_not_overridden() {
    use sill_proto::sessions::HostPath;
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let root = d.dir.join("home/repo");
    std::fs::create_dir_all(root.join(".sill")).unwrap();
    std::fs::write(root.join(".sill/policy.toml"), "[network]\nallow=['packages.example']").unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"policy-suggestions","agent":"claude","policy":"default","pins":[HostPath::from_path(&root)]})).unwrap();
    let id = created["result"]["session_id"].as_str().unwrap();
    let mut resolution = sill_core::policy_fragments::resolve(&sill_core::policy::Policy::builtin_default(), &[root], &[]).unwrap();
    let snapshot = d.dir.join("state/sill/policy-snapshots").join(id);
    std::fs::create_dir_all(&snapshot).unwrap();
    let runtime = d.dir.join("run/sill").join(id);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(snapshot.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    let denied = c.call("_sill/policy.denied", json!({"session_id":id})).unwrap();
    assert!(denied["result"]["hosts"].as_array().unwrap().contains(&json!("packages.example")), "{denied}");
    let allowed = c.call("_sill/policy.allow_host", json!({"session_id":id,"host":"packages.example"})).unwrap();
    assert_eq!(allowed["result"]["agent_name"], "this session", "{allowed}");
    assert_eq!(std::fs::read_to_string(runtime.join("session-allowed-hosts")).unwrap(), "packages.example\n");
    assert!(!d.dir.join("home/.config/sill/agents/claude.toml").exists());
    resolution.policy.network.mode = sill_core::policy::NetMode::None;
    std::fs::write(snapshot.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    let offline = c.call("_sill/policy.allow_host", json!({"session_id":id,"host":"packages.example"})).unwrap();
    assert_eq!(offline["error"]["data"]["code"], "permission_denied", "{offline}");
}

#[test]
fn travelling_policy_cli_preserves_base_shorthand_and_blocks_jailed_import() {
    let d = Daemon::start();
    let run = |args: &[&str], jailed: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sill"));
        cmd.args(args).env("HOME", d.dir.join("home"))
            .env("XDG_RUNTIME_DIR", d.dir.join("run"))
            .env("XDG_STATE_HOME", d.dir.join("state"))
            .env("XDG_CONFIG_HOME", d.dir.join("config"))
            .env_remove("SILL_BOX");
        if jailed { cmd.env("SILL_SESSION", "1"); } else { cmd.env_remove("SILL_SESSION"); }
        cmd.output().unwrap()
    };
    let old = run(&["policy", "default"], false);
    let explicit = run(&["policy", "show", "default"], false);
    assert!(old.status.success(), "{}", String::from_utf8_lossy(&old.stderr));
    assert!(explicit.status.success(), "{}", String::from_utf8_lossy(&explicit.stderr));
    assert_eq!(old.stdout, explicit.stdout);
    let denied = run(&["policy", "import", "/does-not-exist"], true);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("host administrative actions"));
}
