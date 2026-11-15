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
        // Keep fixtures independent of installed host policies (which may use retired fields).
        let policies = dir.join("home/.config/sill/policies");
        std::fs::create_dir_all(&policies).unwrap();
        std::fs::write(policies.join("default.toml"), include_str!("../../../policies/default.toml")).unwrap();
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
    std::fs::write(agents.join("fixture.toml"), format!("id = 'fixture'\nname = 'Fixture'\ncommand = ['/bin/sh', '{}']\nhome_paths = []\nhost_login = ['browser']\n", script.display())).unwrap();
    // A browser login as Codex's: the page goes to `BROWSER`, the agent waits on a loopback port
    // for the page's callback. `hang` never finishes.
    let page = dir.join("page_login.py");
    std::fs::write(&page, r#"import json, os, socket, subprocess, sys
sys.stdin.readline()
print(json.dumps({'jsonrpc':'2.0','id':1,'result':{'authMethods':[{'id':'browser'},{'id':'hang'}]}}), flush=True)
auth = json.loads(sys.stdin.readline())
if auth['params']['methodId'] == 'hang':
    sys.stdin.readline()
    sys.exit(0)
server = socket.socket()
server.bind(('127.0.0.1', 0))
server.listen(1)
port = server.getsockname()[1]
subprocess.run([os.environ['BROWSER'], f'https://auth.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcb'], check=True)
connection, _ = server.accept()
request = connection.makefile('rb').readline()
assert request == b'GET /cb?code=x\n', request
connection.sendall(b'ok\n')
connection.close()
print(json.dumps({'jsonrpc':'2.0','id':auth['id'],'result':{}}), flush=True)
"#).unwrap();
    std::fs::write(agents.join("page-fixture.toml"), format!("id = 'page-fixture'\nname = 'Page fixture'\ncommand = ['/usr/bin/python3', '{}']\nhome_paths = []\nhost_login = ['browser', 'hang']\n", page.display())).unwrap();
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

/// docs/login.md "The browser belongs to the frontend": the daemon hands the page over instead of
/// opening it, relays its loopback callback only for that running login, and cancels on request.
#[test]
fn a_login_hands_over_its_page_relays_its_callback_and_can_be_cancelled() {
    use std::io::BufRead;
    let d = host_fixture();
    let mut c = d.connect_caps(json!({"sill.login_browser": 1})).unwrap();
    let opened = c.call("_sill/agents.login", json!({"operation_id":"page-login","agent_id":"page-fixture","method_id":"browser"})).unwrap();
    let mut events = claim_host(&d, &opened);
    let next = |events: &mut BufReader<UnixStream>| -> Value {
        let mut line = String::new();
        events.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}; {}", d.log()))
    };
    let page = loop {
        let event = next(&mut events);
        if event["type"] == "open_url" { break event; }
        assert!(matches!(event["type"].as_str(), Some("status" | "stderr")), "{event}");
    };
    assert!(page["url"].as_str().unwrap().starts_with("https://auth.example/authorize?"), "{page}");
    let port = page["callback_port"].as_u64().unwrap();
    assert!(port > 0);
    assert_eq!(c.call("_sill/streams.login_callback", json!({"operation_id":"other-login"})).unwrap()["error"]["data"]["code"], "not_found");
    let relay = c.call("_sill/streams.login_callback", json!({"operation_id":"page-login"})).unwrap();
    let mut callback = claim_host(&d, &relay);
    callback.get_mut().write_all(b"GET /cb?code=x\n").unwrap();
    let mut answer = String::new();
    callback.read_line(&mut answer).unwrap();
    assert_eq!(answer, "ok\n");
    let rest: Vec<Value> = events.lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(rest.last().unwrap(), &json!({"type":"completed","agent_id":"page-fixture"}), "{rest:?}");
    // the relay ends with its login
    assert_eq!(c.call("_sill/streams.login_callback", json!({"operation_id":"page-login"})).unwrap()["error"]["data"]["code"], "not_found");

    let hang = json!({"operation_id":"hanging-login","agent_id":"page-fixture","method_id":"hang"});
    let opened = c.call("_sill/agents.login", hang).unwrap();
    let busy = c.call("_sill/agents.login", json!({"agent_id":"page-fixture","method_id":"hang"})).unwrap();
    assert_eq!(busy["error"]["data"]["code"], "busy");
    assert_eq!(busy["error"]["data"]["details"]["operation_id"], "hanging-login", "{busy}");
    assert_eq!(c.call("_sill/streams.login_callback", json!({"operation_id":"hanging-login"})).unwrap()["error"]["data"]["code"], "invalid_state");
    let events = claim_host(&d, &opened);
    assert_eq!(c.call("_sill/operations.cancel", json!({"operation_id":"hanging-login"})).unwrap()["result"], json!({}));
    let events: Vec<Value> = events.lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(events.last().unwrap()["code"], "cancelled", "{events:?}");
    let receipt = c.call("_sill/operations.get", json!({"operation_id":"hanging-login"})).unwrap();
    assert_eq!((receipt["result"]["state"].as_str(), receipt["result"]["code"].as_str()), (Some("failed"), Some("cancelled")));
    assert_eq!(c.call("_sill/operations.cancel", json!({"operation_id":"hanging-login"})).unwrap()["error"]["data"]["code"], "not_found");
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
    let files=d.dir.join("home/files");std::fs::create_dir_all(&files).unwrap();
    let p=files.join(std::ffi::OsString::from_vec(b"raw-\xff".to_vec()));
    let path=HostPath::from_path(&p);
    let write=c.call("_sill/files.write",json!(a::FileWrite {path:path.clone(),bytes_base64:a::encode_bytes(b"abcdef")})).unwrap();
    assert!(write.get("error").is_none(),"{write}");
    let read=c.call("_sill/files.read",json!(a::FileRead {path:path.clone(),offset:"2".into(),length:Some(2),revision:None})).unwrap();
    assert_eq!(read["result"]["bytes_base64"],a::encode_bytes(b"cd"));assert_eq!(read["result"]["next_offset"],"4");assert_eq!(read["result"]["eof"],false);
    std::fs::write(&p,"changed file length").unwrap();
    let stale=c.call("_sill/files.read",json!({"path":path,"offset":"4","length":2,"revision":read["result"]["revision"]})).unwrap();
    assert_eq!(stale["error"]["data"]["code"],"invalid_cursor");
    let root=HostPath::from_path(&files);
    std::fs::write(files.join("another"),"other").unwrap();
    let first=c.call("_sill/files.list",json!(a::FilesList {path:root.clone(),after:None,limit:Some(1)})).unwrap();
    assert_eq!(first["result"]["entries"].as_array().unwrap().len(),1);assert!(!first["result"]["next"].is_null());
    let last=c.call("_sill/files.list",json!({"path":root,"after":first["result"]["next"],"limit":1})).unwrap();
    assert_eq!(last["result"]["entries"][0]["path"]["bytes_base64"],path.bytes_base64);
    let link=files.join("link");std::os::unix::fs::symlink(&p,&link).unwrap();
    assert!(c.call("_sill/files.read",json!(a::FileRead {path:HostPath::from_path(&link),offset:"0".into(),length:None,revision:None})).unwrap().get("error").is_some());
    assert!(c.call("_sill/files.list",json!({"path":root,"limit":0})).unwrap().get("error").is_some());
    // settings go through the daemon: a patch is read back, an unknown change is refused
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
    // Changes lists the repositories open sessions have pinned (global pins are only shortcuts since 2026-11-09).
    let session=c.call("_sill/sessions.create",json!({"create_id":"public-changes","agent":"claude","policy":"default"})).unwrap()["result"]["session_id"].clone();
    let added=c.call("_sill/sessions.pin",json!({"session_id":session,"path":HostPath::from_path(&repo)})).unwrap();assert!(added.get("error").is_none(),"{added}");
    let page=c.call("_sill/changes.list",json!({"limit":1})).unwrap();
    let entry=&page["result"]["entries"][0];assert_eq!(entry["type"],"live","{page}");
    let mut path:HostPath=serde_json::from_value(entry["path"].clone()).unwrap();assert_eq!(path.to_path().unwrap(),file);
    path.display="/wrong/display/path".into();
    let diff=c.call("_sill/changes.diff",json!({"target":{"type":"live","repo":HostPath::from_path(&repo),"path":path}})).unwrap();
    assert!(diff["result"]["text"].as_str().unwrap().contains("fixture contents"),"{diff}");
    let bad=c.call("_sill/changes.diff",json!({"target":{"type":"live","repo":HostPath::from_path(&repo),"path":path},"offset":"1","fingerprint":"wrong"})).unwrap();assert_eq!(bad["error"]["data"]["code"],"invalid_cursor");
    let bad=c.call("_sill/changes.list",json!({"after":"nonsense"})).unwrap();assert_eq!(bad["error"]["data"]["code"],"invalid_cursor");
    // A listing is computed once and its pages read that one: what changes between two pages is in the next listing, not in this one.
    std::fs::write(repo.join("b"),"b\n").unwrap();std::fs::write(repo.join("c"),"c\n").unwrap();
    let first=c.call("_sill/changes.list",json!({"limit":1})).unwrap();let mut names=vec![first["result"]["entries"][0]["path"]["display"].as_str().unwrap().to_string()];
    std::fs::write(repo.join("a-later"),"later\n").unwrap();
    let mut after=first["result"]["next"].clone();assert!(after.is_string(),"{first}");
    let second_cursor=after.clone();
    while !after.is_null(){let page=c.call("_sill/changes.list",json!({"limit":1,"after":after})).unwrap();assert!(page.get("error").is_none(),"{page}");names.push(page["result"]["entries"][0]["path"]["display"].as_str().unwrap().to_string());after=page["result"]["next"].clone();}
    assert_eq!(names.len(),3,"{names:?}");assert!(!names.iter().any(|n|n.ends_with("a-later")),"{names:?}");
    let whole=c.call("_sill/changes.list",json!({})).unwrap();assert_eq!(whole["result"]["entries"].as_array().unwrap().len(),4);assert!(whole["result"]["next"].is_null());
    // Read to its end, a listing is no longer held; nor is one the daemon never made.
    let gone=c.call("_sill/changes.list",json!({"limit":1,"after":second_cursor})).unwrap();assert_eq!(gone["error"]["data"]["code"],"snapshot_expired","{gone}");
    let gone=c.call("_sill/changes.list",json!({"after":"[\"00\",1]"})).unwrap();assert_eq!(gone["error"]["data"]["code"],"snapshot_expired");
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
    // A permanent quiet choice survives another session and never grants access.
    for host in ["SDMNTPRCENTRALUS.oaiusercontent.com:443", "sdmntprcentralus.oaiusercontent.com"] {
        let saved=c.call("_sill/policy.quiet_host",json!({"session_id":session,"host":host})).unwrap();
        assert!(saved.get("error").is_none(),"{saved}");
    }
    for host in ["*.oaiusercontent.com", "bad\nname", "../invalid"] {
        let saved=c.call("_sill/policy.quiet_host",json!({"session_id":session,"host":host})).unwrap();
        assert!(saved.get("error").is_some(),"{saved}");
    }
    let config=std::fs::read_to_string(d.dir.join("config/sill/sill.toml")).unwrap();
    assert_eq!(config.matches("sdmntprcentralus.oaiusercontent.com").count(),1);
    let other=c.call("_sill/sessions.create",json!({"create_id":"quiet-other","agent":"claude","policy":"default"})).unwrap();
    let other_id=other["result"]["session_id"].as_str().unwrap();
    let page=c.call("_sill/policy.denied",json!({"session_id":other_id,"limit":128})).unwrap();
    assert!(page["result"]["quiet"].as_array().unwrap().contains(&json!("sdmntprcentralus.oaiusercontent.com")),"{page}");
    assert!(!runtime.join("session-allowed-hosts").exists());
    let settings=c.call("_sill/settings.get",json!({})).unwrap();
    assert_eq!(settings["result"]["network_quiet_hosts"],json!(["sdmntprcentralus.oaiusercontent.com"]));
    let restored=c.call("_sill/settings.patch",json!({"changes":[{"type":"network_quiet_hosts","value":[]}]})).unwrap();
    assert!(restored.get("error").is_none(),"{restored}");
    for id in [session, other_id] {
        let page=c.call("_sill/policy.denied",json!({"session_id":id,"limit":128})).unwrap();
        assert!(!page["result"]["quiet"].as_array().unwrap().contains(&json!("sdmntprcentralus.oaiusercontent.com")),"{page}");
    }
    let config=std::fs::read_to_string(d.dir.join("config/sill/sill.toml")).unwrap();
    assert!(!config.contains("sdmntprcentralus.oaiusercontent.com"));
    assert!(!runtime.join("session-allowed-hosts").exists());
    // Allow is session-only and concrete (2026-11-06); a wildcard is only an agent-wide default,
    // saved deliberately by a frontend that shows permission state
    let allowed=c.call("_sill/policy.allow_host",json!({"session_id":session,"host":"*.example.test"})).unwrap();
    assert_eq!(allowed["error"]["code"],-32602,"{allowed}");
    let saved=c.call("_sill/policy.save_host",json!({"session_id":session,"host":"*.example.test"})).unwrap();
    assert_eq!(saved["error"]["data"]["details"]["capability"],"sill.permission_modes","{saved}");
    let mut modes=d.connect_caps(json!({"sill.permission_modes":1})).unwrap();
    let saved=modes.call("_sill/policy.save_host",json!({"session_id":session,"host":"*.example.test"})).unwrap();
    assert_eq!(saved["result"]["agent_id"],"claude","{saved}");
    let agent=c.call("_sill/agents.get",json!({"agent_id":"claude"})).unwrap();
    assert!(agent["result"]["domains"].as_array().unwrap().contains(&json!("*.example.test")),"{agent}");
    // a running session gets an exact host at once, as a live grant its proxy reads per
    // connection: the agent goes on without a restart (2026-11-05)
    let _shim=std::os::unix::net::UnixListener::bind(runtime.join("shim.sock")).unwrap();
    let allowed=c.call("_sill/policy.allow_host",json!({"session_id":session,"host":"api.example.test"})).unwrap();
    assert_eq!(allowed["result"]["live"],true,"{allowed}");
    assert_eq!(std::fs::read_to_string(runtime.join("session-allowed-hosts")).unwrap(),"api.example.test\n");
    let agent=c.call("_sill/agents.get",json!({"agent_id":"claude"})).unwrap();
    assert!(!agent["result"]["domains"].as_array().unwrap().contains(&json!("api.example.test")),"agent defaults unchanged: {agent}");
    // Restore defaults also stops hosts approved during this run from matching new connections
    let restored=modes.call("_sill/permissions.restore_defaults",json!({"session_id":session})).unwrap();
    assert_eq!(restored["result"]["restart_required"],true,"{restored}");
    assert_eq!(std::fs::read_to_string(runtime.join("session-allowed-hosts")).unwrap(),"");
    assert!(!restored["result"]["preview"]["text"].as_str().unwrap().contains("api.example.test"),"{restored}");
    for host in ["bad\nname", "../invalid"] {
        assert!(c.call("_sill/policy.allow_host",json!({"session_id":session,"host":host})).unwrap().get("error").is_some());
    }
    assert!(c.call("_sill/policy.denied",json!({"session_id":session,"limit":0})).unwrap().get("error").is_some());
}

#[test]
fn policy_check_import_and_explicit_replacement() {
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
    // a check is of a complete policy and nothing else: a request for another kind is refused, not answered as one
    let other = c.call("_sill/policy.check", json!({"text":text,"fragment":true})).unwrap();
    assert_eq!(other["error"]["code"], -32602, "{other}");
}

#[test]
fn policy_preview_follows_today_and_the_launch_snapshot_stays() {
    use sill_proto::sessions::HostPath;
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let root = d.dir.join("home/repo");
    std::fs::create_dir_all(&root).unwrap();
    let policy = d.dir.join("home/preview.toml");
    std::fs::write(&policy, "name='preview'\n[network]\nmode='allow'\nallow=['launch.example']\n").unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"policy-preview","agent":"claude","policy":policy.to_str().unwrap(),"pins":[HostPath::from_path(&root)]})).unwrap();
    let id = created["result"]["session_id"].as_str().unwrap();
    let key = json!({"session_id":id});
    let empty = c.call("_sill/policy.snapshot", key.clone()).unwrap();
    assert!(empty["result"]["snapshot"].is_null(), "{empty}");
    let preview = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert_eq!(preview["result"]["view"], "preview", "{preview}");
    assert!(preview["result"]["diagnostics"].as_array().unwrap().is_empty(), "{preview}");
    assert!(preview["result"]["text"].as_str().unwrap().contains("launch.example"), "{preview}");
    let pinned = preview["result"]["filesystem"]["rules"].as_array().unwrap().iter()
        .any(|rule| rule["behavior"] == "pin" && rule["target"]["display"] == root.to_str().unwrap());
    assert!(pinned, "{preview}");
    let base = sill_core::policy::Policy::load(policy.to_str().unwrap(), &d.dir.join("home")).unwrap();
    let resolution = sill_core::session::resolve(&base, &[root.clone()]).unwrap();
    // A fixture snapshot tests the public read boundary; no jail is launched in this test.
    let snapshots = d.dir.join("state/sill/policy-snapshots").join(id);
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::write(snapshots.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    // the policy changes: the preview is the next build's, the snapshot what was launched
    std::fs::write(&policy, "name='preview'\n[network]\nmode='allow'\nallow=['next.example']\n").unwrap();
    let next = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert!(next["result"]["text"].as_str().unwrap().contains("next.example"), "{next}");
    assert_ne!(next["result"]["fingerprint"], preview["result"]["fingerprint"]);
    let retained = c.call("_sill/policy.snapshot", key.clone()).unwrap();
    assert_eq!(retained["result"]["snapshot"]["view"], "launch", "{retained}");
    assert!(retained["result"]["snapshot"]["text"].as_str().unwrap().contains("launch.example"), "{retained}");
    assert_eq!(retained["result"]["snapshot"]["fingerprint"], resolution.fingerprint.as_str());
    // a policy that no longer parses, or a pinned folder that is gone: diagnostics and no rules,
    // while the launch stays readable
    std::fs::write(&policy, "name='preview'\nunknown = true\n").unwrap();
    let invalid = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert!(invalid["result"]["text"].is_null(), "{invalid}");
    assert!(!invalid["result"]["diagnostics"].as_array().unwrap().is_empty(), "{invalid}");
    std::fs::write(&policy, "name='preview'\n").unwrap();
    std::fs::remove_dir(&root).unwrap();
    let unpinned = c.call("_sill/policy.preview", key.clone()).unwrap();
    assert!(unpinned["result"]["diagnostics"][0].as_str().unwrap().contains("resolve pinned path"), "{unpinned}");
    let retained = c.call("_sill/policy.snapshot", key).unwrap();
    assert_eq!(retained["result"]["snapshot"]["view"], "launch", "{retained}");
}

#[test]
fn session_host_grants_wait_for_the_next_build_and_offline_is_not_overridden() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let policy = d.dir.join("home/grants.toml");
    let online = "name='grants'\n[network]\nmode='allow'\n";
    std::fs::write(&policy, online).unwrap();
    let created = c.call("_sill/sessions.create", json!({"create_id":"host-grants","agent":"claude","policy":policy.to_str().unwrap()})).unwrap();
    let id = created["result"]["session_id"].as_str().unwrap();
    let mut resolution = sill_core::session::resolve(&sill_core::policy::Policy::parse(online).unwrap(), &[]).unwrap();
    let snapshot = d.dir.join("state/sill/policy-snapshots").join(id);
    std::fs::create_dir_all(&snapshot).unwrap();
    let runtime = d.dir.join("run/sill").join(id);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(snapshot.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    let allowed = c.call("_sill/policy.allow_host", json!({"session_id":id,"host":"packages.example"})).unwrap();
    assert_eq!(allowed["result"]["agent_name"], "this session", "{allowed}");
    // Not running: the grant waits in the session store for the next build, no live file
    assert_eq!(allowed["result"]["live"], false, "{allowed}");
    assert!(!runtime.join("session-allowed-hosts").exists());
    let preview = c.call("_sill/policy.preview", json!({"session_id":id})).unwrap();
    assert!(preview["result"]["text"].as_str().unwrap().contains("packages.example"), "{preview}");
    assert_eq!(preview["result"]["session_hosts"], json!([{"host":"packages.example","applied":true,"reason":""}]), "{preview}");
    assert!(!d.dir.join("home/.config/sill/agents/claude.toml").exists());
    // A stopped session resolves its policy as it is now before approving
    std::fs::write(&policy, "name='grants'\n[network]\nmode='none'\n").unwrap();
    let offline = c.call("_sill/policy.allow_host", json!({"session_id":id,"host":"other.example"})).unwrap();
    assert_eq!(offline["error"]["data"]["code"], "permission_denied", "{offline}");
    // …and its next build leaves the remembered host out, saying why, without forgetting it
    let preview = c.call("_sill/policy.preview", json!({"session_id":id})).unwrap();
    let hosts = &preview["result"]["session_hosts"];
    assert!(hosts[0]["host"] == "packages.example" && hosts[0]["applied"] == false && hosts[0]["reason"].as_str().unwrap().contains("offline"), "{preview}");
    // A running session is held to its recorded launch policy
    std::fs::write(&policy, online).unwrap();
    let _shim = std::os::unix::net::UnixListener::bind(runtime.join("shim.sock")).unwrap();
    resolution.policy.network.mode = sill_core::policy::NetMode::None;
    std::fs::write(snapshot.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    let offline = c.call("_sill/policy.allow_host", json!({"session_id":id,"host":"packages.example"})).unwrap();
    assert_eq!(offline["error"]["data"]["code"], "permission_denied", "{offline}");
}

#[test]
fn host_control_is_an_activation_a_displaying_frontend_holds() {
    let d = Daemon::start();
    let mut plain = d.connect().unwrap();
    let mut modes = d.connect_caps(json!({"sill.permission_modes":1})).unwrap();
    let mut host = d.connect_caps(json!({"sill.permission_modes":1,"sill.host_control":1})).unwrap();
    let policies = d.dir.join("home/host-control-policies");
    std::fs::create_dir_all(&policies).unwrap();
    let mut create = |name: &str, text: &str, ghost: bool| {
        let path = policies.join(format!("{name}.toml"));
        std::fs::write(&path, format!("name='{name}'\n{text}")).unwrap();
        let created = modes.call("_sill/sessions.create", json!({"create_id":name,"agent":"claude","policy":path.to_str().unwrap(),"ghost":ghost})).unwrap();
        created["result"]["session_id"].as_str().unwrap().to_string()
    };
    let stopped = create("stopped", "[network]\nmode='allow'", false);
    let forbidden = create("forbidden", "[permissions]\nhost_control='forbidden'", false);
    let ghost = create("ghost", "", true);
    let activate = |c: &mut Conn, session: &str, operations: Value|
        c.call("_sill/host_control.activate", json!({"session_id":session,"operations":operations})).unwrap();

    // only a frontend that shows Host control may activate: the permission-modes gate first, then
    // the host-control capability itself
    let refused = activate(&mut plain, &stopped, json!(["launch"]));
    assert_eq!(refused["error"]["data"]["details"]["capability"], "sill.permission_modes", "{refused}");
    let refused = activate(&mut modes, &stopped, json!(["launch"]));
    assert_eq!((refused["error"]["data"]["code"].clone(), refused["error"]["data"]["details"]["capability"].clone()),
        (json!("unsupported"), json!("sill.host_control")), "{refused}");
    // never for a ghost, never against a forbidding policy, never without a jail
    assert_eq!(activate(&mut host, &ghost, json!(["launch"]))["error"]["data"]["code"], "permission_denied");
    let refused = activate(&mut host, &forbidden, json!(["launch"]));
    assert_eq!(refused["error"]["data"]["code"], "permission_denied", "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("forbidden"), "{refused}");
    let refused = activate(&mut host, &stopped, json!(["launch", "stop"]));
    assert_eq!(refused["error"]["data"]["code"], "invalid_state", "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("not running"), "{refused}");
    // the scope is named operations, at least one
    assert_eq!(activate(&mut host, &stopped, json!(["launch", "teleport"]))["error"]["code"], -32602);
    assert_eq!(activate(&mut host, &stopped, json!([]))["error"]["code"], -32602);
    // the state shows to frontends that show permission state (records carry the brief to all);
    // nothing above left an activation behind
    assert_eq!(plain.call("_sill/host_control.state", json!({"session_id":stopped})).unwrap()["error"]["data"]["code"], "unsupported");
    let state = modes.call("_sill/host_control.state", json!({"session_id":stopped})).unwrap();
    assert_eq!((state["result"]["active"].clone(), state["result"]["generation"].clone(), state["result"]["displaying"].clone()),
        (json!(false), json!(0), json!(0)), "{state}");
    // …with the policy's ban where there is one, so a frontend can say so before anyone tries
    assert!(state["result"].get("forbidden").is_none(), "{state}");
    let banned = modes.call("_sill/host_control.state", json!({"session_id":forbidden})).unwrap();
    assert!(banned["result"]["forbidden"].as_str().unwrap().contains("forbidden by the session's policy"), "{banned}");
    let record = plain.call("_sill/sessions.get", json!({"session_id":stopped})).unwrap();
    assert!(record["result"].get("host_control").is_none(), "{record}");
    // revoking needs the capability too; revoking nothing is nothing
    assert_eq!(plain.call("_sill/host_control.revoke", json!({"session_id":stopped})).unwrap()["error"]["data"]["code"], "unsupported");
    let revoked = host.call("_sill/host_control.revoke", json!({"session_id":stopped})).unwrap();
    assert_eq!(revoked["result"]["active"], false, "{revoked}");
    assert_eq!(host.call("_sill/host_control.state", json!({"session_id":"nope"})).unwrap()["error"]["data"]["code"], "not_found");
}

/// An orderly stop right after the frontend that displayed Host control left: the revocation
/// its leaving began is finished before the daemon exits, so the host process it launched is
/// stopped (found by the host scenario, 2026-11-10: the exit cut the revocation off).
#[test]
fn an_orderly_stop_finishes_a_revocation_under_way() {
    for round in 0..4 {
        let mut d = Daemon::start();
        let mut modes = d.connect_caps(json!({"sill.permission_modes":1})).unwrap();
        let mut host = d.connect_caps(json!({"sill.permission_modes":1,"sill.host_control":1})).unwrap();
        let created = modes.call("_sill/sessions.create", json!({"create_id":"orderly","agent":"claude","policy":"default"})).unwrap();
        let id = created["result"]["session_id"].as_str().unwrap().to_string();
        // a jail's shim socket and bridge directory, as its launcher leaves them
        let runtime = d.dir.join("run/sill").join(&id);
        std::fs::create_dir_all(runtime.join("bridge")).unwrap();
        let _shim = std::os::unix::net::UnixListener::bind(runtime.join("shim.sock")).unwrap();
        let state = host.call("_sill/host_control.activate", json!({"session_id":id,"operations":["launch"]})).unwrap();
        let generation = state["result"]["generation"].as_u64().unwrap_or_else(|| panic!("{state}"));
        let mut bridge = UnixStream::connect(runtime.join("bridge/host.sock")).unwrap();
        bridge.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        writeln!(bridge, "{}", json!({"generation":generation,"operation":"launch","args":{"argv":["/bin/sleep","4321"],"cwd":"/tmp"}})).unwrap();
        let mut answer = String::new();
        BufReader::new(&bridge).read_line(&mut answer).unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        let pid = answer["result"]["pid"].as_u64().unwrap_or_else(|| panic!("{answer}"));
        let running = || std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "));
        assert!(running(), "the host process runs: {answer}");
        // the frontend leaves and the stop follows at once
        drop(host);
        let r = modes.call(sill_proto::method::DAEMON_SHUTDOWN, json!({})).unwrap();
        assert!(r.get("error").is_none(), "{r}");
        assert!(d.wait_exit().success());
        let gone = !running();
        if !gone {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        assert!(gone, "round {round}: the daemon left with its unkept host process running; log:\n{}", d.log());
    }
}

#[test]
fn development_permissions_apply_together_or_not_at_all() {
    let d = Daemon::start();
    // bare: a connection without the permission-modes capability (line mode, scripts)
    let mut bare = d.connect().unwrap();
    let mut modes = d.connect_caps(json!({"sill.permission_modes":1})).unwrap();
    let policies = d.dir.join("home/development-policies");
    std::fs::create_dir_all(&policies).unwrap();
    let mut create = |name: &str, text: &str, ghost: bool| {
        let path = policies.join(format!("{name}.toml"));
        std::fs::write(&path, format!("name='{name}'\n{text}")).unwrap();
        let created = modes.call("_sill/sessions.create", json!({"create_id":name,"agent":"claude","policy":path.to_str().unwrap(),"ghost":ghost})).unwrap();
        created["result"]["session_id"].as_str().unwrap().to_string()
    };
    let plain = create("plain", "[network]\nmode='allow'", false);
    let half = create("half", "[permissions]\nprivate_bus='forbidden'", false);
    let ghost = create("ghost", "", true);
    let state = |view: &Value, capability: &str| view["permissions"].as_array().unwrap().iter()
        .find(|p| p["capability"] == capability).cloned().unwrap();
    let apply = |c: &mut Conn, session: &str| c.call("_sill/permissions.apply_development", json!({"session_id":session})).unwrap();

    let refused = apply(&mut bare, &plain);
    assert_eq!(refused["error"]["data"]["details"]["capability"], "sill.permission_modes", "{refused}");
    let preview = modes.call("_sill/policy.preview", json!({"session_id":plain})).unwrap();
    let available = ["test_desktop", "private_bus"].iter().all(|c| state(&preview["result"], c)["available"] == true);
    let applied = apply(&mut modes, &plain);
    if available {
        for capability in ["test_desktop", "private_bus"] {
            let s = state(&applied["result"]["preview"], capability);
            assert_eq!((s["requested"].clone(), s["source"].clone(), s["effective"].clone()), (json!(true), json!("session"), json!(true)), "{s}");
        }
        let restored = modes.call("_sill/permissions.restore_defaults", json!({"session_id":plain})).unwrap();
        assert_eq!(state(&restored["result"]["preview"], "test_desktop")["requested"], Value::Null);
    } else {
        // this machine lacks Weston or dbus-daemon: the group is refused as a whole, nothing recorded
        assert_eq!(applied["error"]["data"]["code"], "permission_denied", "{applied}");
        let preview = modes.call("_sill/policy.preview", json!({"session_id":plain})).unwrap();
        assert!(["test_desktop", "private_bus"].iter().all(|c| state(&preview["result"], c)["requested"].is_null()));
    }
    // a forbidden half refuses the group, naming it, and the other half is not recorded either
    let refused = apply(&mut modes, &half);
    assert_eq!(refused["error"]["data"]["code"], "permission_denied", "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("private_bus"), "{refused}");
    let preview = modes.call("_sill/policy.preview", json!({"session_id":half})).unwrap();
    assert_eq!(state(&preview["result"], "test_desktop")["requested"], Value::Null, "{preview}");
    assert_eq!(apply(&mut modes, &ghost)["error"]["data"]["code"], "permission_denied");
}

#[test]
fn session_permission_choices_restore_defaults_and_frontend_gate() {
    let d = Daemon::start();
    // bare: a connection without the permission-modes capability (line mode, scripts)
    let mut bare = d.connect().unwrap();
    let mut modes = d.connect_caps(json!({"sill.permission_modes":1})).unwrap();
    let policies = d.dir.join("home/permission-policies");
    std::fs::create_dir_all(&policies).unwrap();
    let mut create = |name: &str, text: &str, ghost: bool| {
        let path = policies.join(format!("{name}.toml"));
        std::fs::write(&path, format!("name='{name}'\n{text}")).unwrap();
        let created = modes.call("_sill/sessions.create", json!({"create_id":name,"agent":"claude","policy":path.to_str().unwrap(),"ghost":ghost})).unwrap();
        created["result"]["session_id"].as_str().unwrap().to_string()
    };
    let web = create("web", "[network]\nmode='allow'", false);
    let other = create("other", "[network]\nmode='allow'", false);
    let desktop = create("desktop", "[permissions]\ntest_desktop='on'", false);
    let ghost = create("ghost", "[network]\nmode='allow'", true);
    let state = |view: &Value, capability: &str| view["permissions"].as_array().unwrap().iter()
        .find(|p| p["capability"] == capability).cloned().unwrap();
    let set = |c: &mut Conn, session: &str, capability: &str, enabled: Value|
        c.call("_sill/permissions.set", json!({"session_id":session,"capability":capability,"enabled":enabled})).unwrap();

    // Only a frontend that shows permission state may change choices.
    let refused = set(&mut bare, &web, "public_web", json!(true));
    assert_eq!(refused["error"]["data"]["code"], "unsupported", "{refused}");
    assert_eq!(refused["error"]["data"]["details"]["capability"], "sill.permission_modes");
    let changed = set(&mut modes, &web, "public_web", json!(true));
    assert_eq!(changed["result"]["restart_required"], false, "{changed}");
    let web_state = state(&changed["result"]["preview"], "public_web");
    assert_eq!((web_state["effective"].clone(), web_state["requested"].clone(), web_state["source"].clone()), (json!(true), json!(true), json!("session")), "{web_state}");
    let unchanged = modes.call("_sill/policy.preview", json!({"session_id":other})).unwrap();
    assert_eq!(state(&unchanged["result"], "public_web")["effective"], false, "grants belong to one session");

    // Widening is refused where no backend or a limit blocks it; declining always applies.
    // (the GPU has no backend; the test desktop's and the private bus's depend on what is installed)
    let blocked = set(&mut modes, &web, "gpu", json!(true));
    assert_eq!(blocked["error"]["data"]["code"], "permission_denied", "{blocked}");
    let reason = blocked["error"]["message"].as_str().unwrap();
    assert!(reason.contains("backend") || reason.contains("no accessible Intel/AMD GPU render node"), "{blocked}");
    assert_eq!(set(&mut modes, &web, "host_control", json!(true))["error"]["data"]["code"], "permission_denied");
    assert_eq!(set(&mut modes, &web, "unknown", json!(true))["error"]["code"], -32602);
    assert!(set(&mut modes, &web, "test_desktop", json!(false)).get("error").is_none());
    assert_eq!(set(&mut modes, &ghost, "public_web", json!(true))["error"]["data"]["code"], "permission_denied");

    // A newly imposed ceiling blocks the remembered choice instead of being bypassed.
    std::fs::write(policies.join("web.toml"), "name='web'\n[network]\nmode='allow'\nallow_additional_hosts=false").unwrap();
    let preview = modes.call("_sill/policy.preview", json!({"session_id":web})).unwrap();
    let web_state = state(&preview["result"], "public_web");
    assert_eq!((web_state["effective"].clone(), web_state["blocked"].clone()), (json!(false), json!(true)), "{web_state}");
    std::fs::write(policies.join("web.toml"), "name='web'\n[network]\nmode='allow'").unwrap();

    // Restore defaults removes added grants and keeps the selected policy.
    assert!(bare.call("_sill/policy.allow_host", json!({"session_id":web,"host":"api.example.test"})).unwrap().get("error").is_none());
    let restored = modes.call("_sill/permissions.restore_defaults", json!({"session_id":web})).unwrap();
    let web_state = state(&restored["result"]["preview"], "public_web");
    assert_eq!((web_state["effective"].clone(), web_state["requested"].clone()), (json!(false), Value::Null), "{web_state}");
    assert!(!restored["result"]["preview"]["text"].as_str().unwrap().contains("api.example.test"));
    let row = modes.call("_sill/sessions.get", json!({"session_id":web})).unwrap();
    assert!(row["result"]["policy"].as_str().unwrap().ends_with("web.toml"), "{row}");

    // A bare client operates Default sessions, never ones requesting Desktop development capabilities.
    for (method, params) in [
        ("_sill/sessions.submit", json!({"session_id":desktop,"submission_id":"s1","content":[]})),
        ("_sill/questions.answer", json!({"session_id":desktop,"question_id":"q"})),
        ("_sill/policy.allow_host", json!({"session_id":desktop,"host":"example.com"})),
        ("_sill/sessions.start", json!({"session_id":desktop})),
    ] {
        let refused = bare.call(method, params).unwrap();
        assert_eq!(refused["error"]["data"]["details"]["capability"], "sill.permission_modes", "{method}: {refused}");
    }
    let permitted = modes.call("_sill/policy.allow_host", json!({"session_id":desktop,"host":"example.com"})).unwrap();
    assert!(permitted.get("error").is_none(), "{permitted}");
    assert!(bare.call("_sill/policy.preview", json!({"session_id":desktop})).unwrap().get("error").is_none(), "inspection stays available");
    // Declining the default makes it a Default session again, operable by a bare client.
    assert!(set(&mut modes, &desktop, "test_desktop", json!(false)).get("error").is_none());
    assert!(bare.call("_sill/policy.allow_host", json!({"session_id":desktop,"host":"example.org"})).unwrap().get("error").is_none());
}

#[test]
fn policy_cli_shows_the_default_and_blocks_jailed_import() {
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
    // without a subcommand: the default policy, as `policy show default`
    let old = run(&["policy"], false);
    let explicit = run(&["policy", "show", "default"], false);
    assert!(old.status.success(), "{}", String::from_utf8_lossy(&old.stderr));
    assert!(explicit.status.success(), "{}", String::from_utf8_lossy(&explicit.stderr));
    assert_eq!(old.stdout, explicit.stdout);
    let denied = run(&["policy", "import", "/does-not-exist"], true);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("host administrative action"));
}

#[test]
fn daemon_only_restart_replaces_process_and_keeps_store() {
    let mut d = Daemon::start();
    let created = d.connect().unwrap().call("_sill/sessions.create", json!({"create_id":"restart-keeps-session","agent":"claude","policy":"default"})).unwrap();
    assert!(created.get("error").is_none(), "{created}");
    let before = d.connect().unwrap().call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
    let status = Daemon::command(&d.dir).arg("restart").status().unwrap();
    // Always ask the replacement to exit, including if the restart command reports failure.
    let after = d.connect().and_then(|mut c| {
        let result = c.call(sill_proto::method::DAEMON_STATUS, json!({}));
        let _ = c.call(sill_proto::method::DAEMON_SHUTDOWN, json!({}));
        result
    });
    assert!(status.success(), "{}", d.log());
    let after = after.unwrap();
    assert_ne!(before["result"]["pid"], after["result"]["pid"]);
    assert_eq!(before["result"]["sessions"], 1);
    assert_eq!(before["result"]["sessions"], after["result"]["sessions"]);
    assert!(d.wait_exit().success());
}

/// `_sill/workspaces.*` over the public protocol (docs/parallel-coding.md §12), without a jail:
/// gated by `sill.workspaces`; a session's mode from `sessions.create` or Settings → Automatic
/// workspaces; the session's view before its jail starts; refusals with their reason. Making,
/// applying, discarding and undoing are covered against real repositories by
/// `sill_core::workspace` tests.
#[test]
fn public_workspaces_without_a_jail() {
    use sill_proto::sessions::HostPath;
    let d = Daemon::start();
    let repo = d.dir.join("home/repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());

    let mut plain = d.connect().unwrap();
    let refused = plain.call("_sill/workspaces.list", json!({})).unwrap();
    assert_eq!(refused["error"]["data"]["code"], "unsupported", "{refused}");
    assert_eq!(refused["error"]["data"]["details"]["capability"], "sill.workspaces");

    let mut c = d.connect_caps(json!({"sill.workspaces": 1})).unwrap();
    let create = |c: &mut Conn, id: &str, workspaces: Value| {
        let made = c.call("_sill/sessions.create", json!({"create_id": id, "agent": "claude", "policy": "default", "pins": [HostPath::from_path(&repo)], "workspaces": workspaces})).unwrap();
        assert!(made.get("error").is_none(), "{made}");
        made["result"]["session_id"].as_str().unwrap().to_string()
    };
    let chosen = create(&mut c, "ws-on", json!(true));
    let view = c.call("_sill/workspaces.session", json!({"session_id": chosen})).unwrap();
    assert_eq!(view["result"]["workspaces_mode"], true, "{view}");
    assert_eq!(view["result"]["workspaces"], json!([]), "made when its jail starts, not before");
    let default = create(&mut c, "ws-default", Value::Null);
    let view = c.call("_sill/workspaces.session", json!({"session_id": default})).unwrap();
    assert_eq!(view["result"]["workspaces_mode"], false, "Automatic workspaces is off unless set");
    let on = c.call("_sill/settings.patch", json!({"changes": [{"type": "automatic_workspaces", "value": true}]})).unwrap();
    assert!(on.get("error").is_none(), "{on}");
    let automatic = create(&mut c, "ws-automatic", Value::Null);
    let view = c.call("_sill/workspaces.session", json!({"session_id": automatic})).unwrap();
    assert_eq!(view["result"]["workspaces_mode"], true);
    let live = create(&mut c, "ws-live", json!(false));
    let view = c.call("_sill/workspaces.session", json!({"session_id": live})).unwrap();
    assert_eq!(view["result"]["workspaces_mode"], false, "the live button");

    let listed = c.call("_sill/workspaces.list", json!({})).unwrap();
    assert_eq!(listed["result"]["workspaces"], json!([]), "{listed}");
    let missing = c.call("_sill/workspaces.get", json!({"workspace_id": "ws-none"})).unwrap();
    assert_eq!(missing["error"]["data"]["code"], "not_found", "{missing}");
    let undo = c.call("_sill/workspaces.undo", json!({"operation_id": "u", "discard_id": "wsd-none"})).unwrap();
    assert_eq!(undo["error"]["data"]["code"], "not_found", "{undo}");
}

#[test]
fn autostart_off_blocks_fallback_but_allows_a_manual_daemon() {
    let dir = Daemon::dir();
    let mut off = Daemon::command(&dir);
    off.args(["autostart", "off"]).env_remove("SILL_BOX");
    assert!(off.status().unwrap().success());
    assert!(dir.join("config/sill/daemon-autostart-disabled").is_file());
    let mut status = Daemon::command(&dir);
    status.arg("status").env_remove("SILL_BOX").stderr(Stdio::piped());
    let output = status.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon autostart is disabled"));
    assert!(!dir.join("run/sill/daemon.sock").exists());
    let mut setup = Command::new(env!("CARGO_BIN_EXE_sill"));
    let output = setup.args(["init", "--user", "--dry-run"])
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("HOME", dir.join("home")).env_remove("SILL_BOX")
        .output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("autostart disabled"));
    let d = Daemon::start_in(dir);
    let mut status = Daemon::command(&d.dir);
    status.arg("status").env_remove("SILL_BOX");
    assert!(status.status().unwrap().success());
}

#[test]
fn public_crosscheck_workers_are_hidden_and_cannot_be_widened_or_kept() {
    use sill_proto::sessions::HostPath;
    let d=Daemon::start();
    let mut c=d.connect_caps(json!({"sill.permission_modes":1,"sill.host_control":1})).unwrap();
    let made=c.call("_sill/sessions.create",json!({"create_id":"internal-review","agent":"claude","policy":"default"})).unwrap();
    let id=made["result"]["session_id"].as_str().unwrap();
    let store=sill_core::store::Store::open(&d.dir.join("state/sill/sill.db")).unwrap();
    store.conn().execute("INSERT INTO crosscheck_workers VALUES(?1,'run')",[id]).unwrap();
    let repo=d.dir.join("home/reviewer-copy"); std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init","-q"]).arg(&repo).status().unwrap().success());
    let file=repo.join("generated"); std::fs::write(&file,"disposable").unwrap(); store.pin(id,&repo).unwrap();
    let listed=c.call("_sill/sessions.list",json!({})).unwrap();
    assert_eq!(listed["result"]["sessions"],json!([]),"{listed}");
    let get=c.call("_sill/sessions.get",json!({"session_id":id})).unwrap();
    assert_eq!(get["result"]["session_id"],id,"transcript metadata remains readable: {get}");
    for method in ["_sill/sessions.update","_sill/sessions.pin","_sill/sessions.unpin","_sill/sessions.submit","_sill/sessions.control","_sill/sessions.reset","_sill/permissions.set","_sill/permissions.apply_development","_sill/permissions.restore_defaults","_sill/policy.allow_host","_sill/host_control.activate"] {
        let refused=c.call(method,json!({"session_id":id})).unwrap();
        assert_eq!(refused["error"]["data"]["code"],"permission_denied","{method}: {refused}");
    }
    let changes=c.call("_sill/changes.list",json!({})).unwrap();
    assert_eq!(changes["result"]["entries"],json!([]),"reviewer pins must not become live changes: {changes}");
    let keep=c.call("_sill/changes.keep",json!({"session_id":id,"items":[{"path":HostPath::from_path(&file),"expected_hash":null}]})).unwrap();
    assert_eq!(keep["error"]["data"]["code"],"permission_denied","{keep}");
    assert_eq!(std::fs::read_to_string(file).unwrap(),"disposable");
}

/// A scripted agent in place of a Crosscheck worker's jail. `SILL_LAUNCHER=attach` makes the
/// daemon wait for a shim on the worker session's socket instead of launching one: this
/// provides it, for every worker the daemon records, and plays the Reviewer on its ACP
/// connection. Everything else is the daemon's real path: the capture, the copy, the worker
/// session, its relay, the prompt, the journal, the report and the teardown.
struct FakeReviewers {
    /// The prompt each Reviewer was given, as it arrived.
    prompts: std::sync::mpsc::Receiver<String>,
    /// What the next Reviewer answers with. Unanswered, it works until it is stopped.
    answers: std::sync::mpsc::Sender<String>,
    /// The options each worker was asked to take: (option id, value).
    chosen: std::sync::mpsc::Receiver<(String, String)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sleepers: std::sync::Arc<std::sync::Mutex<Vec<Child>>>,
}

impl FakeReviewers {
    fn start(dir: &Path) -> Self {
        use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc};
        let (prompt_tx, prompts) = mpsc::channel();
        let (answers, answer_rx) = mpsc::channel::<String>();
        let (chosen_tx, chosen) = mpsc::channel();
        let answer_rx = Arc::new(Mutex::new(answer_rx));
        let stop = Arc::new(AtomicBool::new(false));
        let sleepers = Arc::new(Mutex::new(Vec::new()));
        let (dir, stopping, kept) = (dir.to_path_buf(), stop.clone(), sleepers.clone());
        std::thread::spawn(move || {
            let mut served = std::collections::HashSet::new();
            while !stopping.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
                let Ok(db) = rusqlite::Connection::open_with_flags(dir.join("state/sill/sill.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { continue };
                let Ok(mut q) = db.prepare("SELECT session_id FROM crosscheck_workers") else { continue };
                let Ok(rows) = q.query_map([], |r| r.get::<_, String>(0)) else { continue };
                for id in rows.flatten() {
                    if !served.insert(id.clone()) { continue; }
                    let runtime = dir.join("run/sill").join(&id);
                    std::fs::create_dir_all(&runtime).unwrap();
                    // what stopping a session signals: a process that is the session's launcher
                    let sleeper = Command::new("sh").args(["-c", "while :; do sleep 1; done", "sill", "launch", "--session", &id]).spawn().unwrap();
                    std::fs::write(runtime.join("launch.pid"), sleeper.id().to_string()).unwrap();
                    kept.lock().unwrap().push(sleeper);
                    let listener = std::os::unix::net::UnixListener::bind(runtime.join("shim.sock")).unwrap();
                    let (prompt_tx, answer_rx, chosen_tx) = (prompt_tx.clone(), answer_rx.clone(), chosen_tx.clone());
                    std::thread::spawn(move || {
                        for stream in listener.incoming().flatten() {
                            let (prompt_tx, answer_rx, chosen_tx) = (prompt_tx.clone(), answer_rx.clone(), chosen_tx.clone());
                            std::thread::spawn(move || Self::serve(stream, prompt_tx, answer_rx, chosen_tx));
                        }
                    });
                }
            }
        });
        Self { prompts, answers, chosen, stop, sleepers }
    }

    fn serve(mut stream: UnixStream, prompts: std::sync::mpsc::Sender<String>, answers: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<String>>>, chosen: std::sync::mpsc::Sender<(String, String)>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut read = move || -> Option<Value> {
            let mut line = String::new();
            (reader.read_line(&mut line).ok()? > 0).then(|| serde_json::from_str(&line).unwrap())
        };
        let send = |stream: &mut UnixStream, message: Value| writeln!(stream, "{message}").is_ok();
        let Some(role) = read() else { return };
        if role["role"] == "control" {
            while let Some(request) = read() {
                let answer = if request["op"] == "status" { json!({"ok":true,"running":true,"pid":std::process::id(),"exit":null,"buffered_lines":0}) } else { json!({"ok":true}) };
                if !send(&mut stream, answer) { return; }
            }
            return;
        }
        let options = |model: &str| json!([{"id":"model","name":"Model","category":"model","type":"select","currentValue":model,
            "options":[{"value":"small","name":"Small"},{"value":"large","name":"Large"}]}]);
        while let Some(request) = read() {
            let id = request["id"].clone();
            match request["method"].as_str() {
                Some("initialize") => { send(&mut stream, json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}})); }
                Some("session/new") => { send(&mut stream, json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"fake-review","configOptions":options("small")}})); }
                Some("session/set_config_option") => {
                    let value = request["params"]["value"].as_str().unwrap_or_default().to_string();
                    let _ = chosen.send((request["params"]["configId"].as_str().unwrap_or_default().to_string(), value.clone()));
                    send(&mut stream, json!({"jsonrpc":"2.0","id":id,"result":{"configOptions":options(&value)}}));
                }
                Some("session/prompt") => {
                    let text = request["params"]["prompt"].as_array().unwrap().iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
                    let _ = prompts.send(text);
                    let update = |update: Value| json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-review","update":update}});
                    send(&mut stream, update(json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read src/lib.rs","kind":"read","status":"completed"})));
                    // works until told what to say, or until its session is stopped
                    let Ok(answer) = answers.lock().unwrap().recv_timeout(Duration::from_secs(30)) else { return };
                    send(&mut stream, update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":answer}})));
                    send(&mut stream, json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                }
                _ if !id.is_null() => { send(&mut stream, json!({"jsonrpc":"2.0","id":id,"result":{}})); }
                _ => {}
            }
        }
    }
}

impl Drop for FakeReviewers {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for mut sleeper in self.sleepers.lock().unwrap().drain(..) { let _ = sleeper.kill(); let _ = sleeper.wait(); }
    }
}

/// A whole review, start to end, against a scripted Reviewer (docs/crosscheck.md): what it is
/// given to compare, what the person is shown, and that nothing of it is left behind — for a
/// review that finds problems, one whose answer cannot be read, and one that is cancelled.
#[test]
fn public_crosscheck_runs_a_whole_review_against_a_scripted_reviewer() {
    use sill_proto::sessions::HostPath;
    let dir = Daemon::dir();
    let home = dir.join("home");
    let git = |at: &Path, args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(at).args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"]).args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    // the person's checkout, and a workspace started from it
    let source = home.join("project");
    std::fs::create_dir_all(source.join("src")).unwrap();
    git(&source, &["init", "-q", "--initial-branch=main"]);
    std::fs::write(source.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-q", "-m", "start"]);
    let start = git(&source, &["rev-parse", "HEAD"]);
    let clone = dir.join("state/workspaces/project");
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(&home, &["clone", "-q", source.to_str().unwrap(), clone.to_str().unwrap()]);
    git(&clone, &["checkout", "-q", "-b", "sill/ws"]);
    git(&clone, &["update-ref", "refs/sill/base", &start]);
    // the work: one commit and one uncommitted new file
    std::fs::write(clone.join("src/lib.rs"), "pub fn one() -> u32 { 2 }\n").unwrap();
    git(&clone, &["commit", "-q", "-am", "the work"]);
    std::fs::write(clone.join("src/new.rs"), "pub fn new() {}\n").unwrap();
    // meanwhile the checkout gained another session's commit, which this work never had
    std::fs::write(source.join("other.txt"), "another session's work\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-q", "-m", "another session"]);

    let d = Daemon::start_env(dir, &[("SILL_LAUNCHER", "attach")]);
    let fake = FakeReviewers::start(&d.dir);
    let mut c = d.connect_caps(json!({"sill.crosscheck": 1})).unwrap();
    let made = c.call("_sill/sessions.create", json!({"create_id":"author","agent":"claude","policy":"default"})).unwrap();
    let author = made["result"]["session_id"].as_str().unwrap().to_string();
    // an agent the person also uses: it reviews when no Reviewer is saved
    let other = c.call("_sill/sessions.create", json!({"create_id":"other","agent":"codex","policy":"default"})).unwrap()["result"]["session_id"].as_str().unwrap().to_string();
    {
        // the session's workspace and its launch record, as a started workspace session has them
        let store = sill_core::store::Store::open(&d.dir.join("state/sill/sill.db")).unwrap();
        use std::os::unix::ffi::OsStrExt;
        store.conn().execute("INSERT INTO workspaces(id,name,session_id,source,source_branch,base,branch,path,state,shallow,discards,created_by,created_at,updated_at) VALUES('ws-1','project',?1,?2,'main',?3,'sill/ws',?4,'open',0,0,'test',1,1)",
            rusqlite::params![author, source.as_os_str().as_bytes(), start, clone.as_os_str().as_bytes()]).unwrap();
        let base = sill_core::policy::Policy::load("default", &home).unwrap();
        let resolution = sill_core::session::resolve(&base, &[]).unwrap();
        let resolution = sill_core::session::freeze_filesystem(resolution, "claude", &home, &d.dir.join("state/author-state"), &[], false, &[]).unwrap();
        let snapshots = d.dir.join("state/sill/policy-snapshots").join(&author);
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(snapshots.join("latest.json"), serde_json::to_vec(&resolution).unwrap()).unwrap();
    }
    let current = |c: &mut Conn| c.call("_sill/crosscheck.get", json!({"workspace_id":"ws-1"})).unwrap()["result"]["run"].clone();
    let wait = |c: &mut Conn, what: &str, done: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let run = current(c);
            if done(&run) { return run; }
            assert!(Instant::now() < deadline, "{what}: {run}\n{}", d.log());
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    let workers = || -> Vec<(String, bool)> {
        let db = rusqlite::Connection::open(d.dir.join("state/sill/sill.db")).unwrap();
        let mut q = db.prepare("SELECT w.session_id, s.ended_at IS NOT NULL FROM crosscheck_workers w JOIN sessions s ON s.id=w.session_id ORDER BY s.created_at, s.rowid").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    let leftovers = || std::fs::read_dir(d.dir.join("home/.local/share/sill/crosschecks")).map(|d| d.count()).unwrap_or(0);
    assert!(current(&mut c).is_null(), "no review before one is asked for");

    // the Reviewer setting: what each agent offers is unknown until one has run
    let setup = c.call("_sill/crosscheck.reviewers", json!({})).unwrap();
    assert_eq!(setup["result"]["reviewers"], json!([]), "{setup}");
    let agents: Vec<&str> = setup["result"]["agents"].as_array().unwrap().iter().map(|a| a["agent"].as_str().unwrap()).collect();
    assert!(agents.contains(&"claude") && agents.contains(&"codex"), "{setup}");
    assert!(setup["result"]["agents"].as_array().unwrap().iter().all(|a| a["options"] == json!([])), "{setup}");

    // 1. a review that finds a problem. No Reviewer is saved: another agent than the author's.
    let started = c.call("_sill/crosscheck.start", json!({"operation_id":"press-1","workspace_id":"ws-1"})).unwrap();
    let run_id = started["result"]["run_id"].as_str().unwrap_or_else(|| panic!("{started}\n{}", d.log())).to_string();
    assert_eq!(started["result"]["base"], start, "compared with where the work leaves the checkout, not the checkout's tip: {started}");
    assert_eq!(started["result"]["reviewers"][0]["reviewer"]["agent"], "codex", "{started}");
    let prompt = fake.prompts.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("the Reviewer was never asked: {}\n{}", current(&mut c), d.log()));
    assert!(prompt.contains(&format!("refs/crosscheck/base ({start})")), "{prompt}");
    let copy = PathBuf::from(prompt.split("Work only in ").nth(1).unwrap().split(": a disposable copy").next().unwrap());
    // what the Reviewer sees is this work alone: the commit, the uncommitted file, and not the
    // other session's commit as something removed (2026-11-13)
    assert_eq!(git(&copy, &["diff", "--name-only", "refs/crosscheck/base"]), "src/lib.rs\nsrc/new.rs");
    assert!(!copy.join("other.txt").exists());
    assert!(copy.starts_with(d.dir.join("home/.local/share/sill/crosschecks").join(&run_id)), "{}", copy.display());
    let working = wait(&mut c, "the Reviewer's step", &|run| run["reviewers"][0]["activity"] == json!(["Read src/lib.rs"]));
    assert_eq!((working["state"].as_str(), working["reviewers"][0]["state"].as_str()), (Some("reviewing"), Some("running")), "{working}");
    // the worker is nobody's session: not listed, and a second press starts nothing
    let listed = c.call("_sill/sessions.list", json!({})).unwrap();
    let mut ids: Vec<&str> = listed["result"]["sessions"].as_array().unwrap().iter().map(|s| s["session_id"].as_str().unwrap()).collect();
    ids.sort();
    let mut mine = [author.as_str(), other.as_str()];
    mine.sort();
    assert_eq!(ids, mine, "{listed}");
    let again = c.call("_sill/crosscheck.start", json!({"operation_id":"press-1","workspace_id":"ws-1"})).unwrap();
    assert_eq!(again["result"]["run_id"], run_id.as_str(), "{again}");
    let second = c.call("_sill/crosscheck.start", json!({"operation_id":"press-1b","workspace_id":"ws-1"})).unwrap();
    assert!(second["error"]["message"].as_str().unwrap().contains("already under way"), "{second}");
    fake.answers.send("Done.\n```json\n{\"summary\":\"It does what was asked. One thing to look at.\",\"problems\":[{\"title\":\"one() returns 2.\",\"path\":\"src/lib.rs\",\"line\":1,\"detail\":\"The name says one.\"}]}\n```".into()).unwrap();
    let done = wait(&mut c, "the review", &|run| run["state"] != "reviewing" && run["state"] != "capturing");
    assert_eq!(done["state"], "complete", "{done}\n{}", d.log());
    assert_eq!(done["reviewers"][0]["summary"], "It does what was asked. One thing to look at.", "{done}");
    assert_eq!(done["problems"], json!([{"id":"r0-p0","reviewer_index":0,"title":"one() returns 2.","path":"src/lib.rs","line":1,"detail":"The name says one."}]), "{done}");
    assert_eq!(done["stale"], false, "{done}");
    assert_eq!(workers().len(), 1);
    wait(&mut c, "the worker's end", &|_| workers().iter().all(|(_, ended)| *ended) && leftovers() == 0);
    assert!(!copy.exists(), "a review keeps its words, not its copies");
    // the work changes: the review no longer describes it
    std::fs::write(clone.join("src/new.rs"), "pub fn new() { /* fixed */ }\n").unwrap();
    assert_eq!(current(&mut c)["stale"], true);
    // codex has run now: its options are known, and a Reviewer can be saved with one
    let setup = c.call("_sill/crosscheck.reviewers", json!({})).unwrap();
    let reviewer = done["reviewers"][0]["reviewer"]["agent"].as_str().unwrap();
    let offered = setup["result"]["agents"].as_array().unwrap().iter().find(|a| a["agent"] == reviewer).unwrap();
    assert_eq!(offered["options"][0]["option_id"], "model", "{setup}");
    let saved = c.call("_sill/crosscheck.set_reviewers", json!({"reviewers":[{"name":"Careful","agent":reviewer,"options":{"model":"large"}}]})).unwrap();
    assert_eq!(saved["result"]["reviewers"][0]["name"], "Careful", "{saved}");

    // 2. a review whose answer cannot be read is not "no problems"; it replaces the first
    let started = c.call("_sill/crosscheck.start", json!({"operation_id":"press-2","workspace_id":"ws-1"})).unwrap();
    let second_id = started["result"]["run_id"].as_str().unwrap_or_else(|| panic!("{started}")).to_string();
    assert_ne!(second_id, run_id);
    assert_eq!(started["result"]["reviewers"][0]["reviewer"]["name"], "Careful", "{started}");
    fake.prompts.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("the second Reviewer was never asked: {}\n{}", current(&mut c), d.log()));
    // the saved Reviewer's option was taken before the prompt went
    assert_eq!(fake.chosen.try_recv().ok(), Some(("model".to_string(), "large".to_string())));
    fake.answers.send("Looks good to me.".into()).unwrap();
    let failed = wait(&mut c, "the unreadable answer", &|run| run["state"] == "failed");
    assert_eq!(failed["run_id"], second_id.as_str());
    assert!(failed["detail"].as_str().unwrap().contains("not the report"), "{failed}");
    assert_eq!(failed["problems"], json!([]));
    wait(&mut c, "the second worker's end", &|_| workers().iter().all(|(_, ended)| *ended) && leftovers() == 0);

    // 3. the cross while the Reviewer works: the review is gone and its worker stopped
    let started = c.call("_sill/crosscheck.start", json!({"operation_id":"press-3","workspace_id":"ws-1"})).unwrap();
    let third = started["result"]["run_id"].as_str().unwrap_or_else(|| panic!("{started}")).to_string();
    fake.prompts.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("the third Reviewer was never asked: {}\n{}", current(&mut c), d.log()));
    assert_eq!(workers().iter().filter(|(_, ended)| !ended).count(), 1);
    let removed = c.call("_sill/crosscheck.remove", json!({"run_id":third})).unwrap();
    assert!(removed.get("error").is_none(), "{removed}");
    assert!(current(&mut c).is_null(), "removed at once");
    wait(&mut c, "the cancelled worker's end", &|_| workers().len() == 3 && workers().iter().all(|(_, ended)| *ended) && leftovers() == 0);
    let twice = c.call("_sill/crosscheck.remove", json!({"run_id":third})).unwrap();
    assert!(twice.get("error").is_none(), "a review already gone is not an error: {twice}");

    // nothing to review once the work is where the checkout is
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "more"]);
    git(&source, &["pull", "-q", "--no-rebase", clone.to_str().unwrap(), "sill/ws"]);
    git(&clone, &["fetch", "-q", "origin"]);
    git(&clone, &["reset", "-q", "--hard", "origin/main"]);
    let nothing = c.call("_sill/crosscheck.start", json!({"operation_id":"press-4","workspace_id":"ws-1"})).unwrap();
    assert!(nothing["error"]["message"].as_str().unwrap().contains("nothing to review"), "{nothing}");

    // 4. Discard while the Reviewer works: the review goes with the work and its worker is
    // stopped, since the line with the cross is gone from the shelf
    std::fs::write(clone.join("src/more.rs"), "pub fn more() {}\n").unwrap();
    let started = c.call("_sill/crosscheck.start", json!({"operation_id":"press-5","workspace_id":"ws-1"})).unwrap();
    assert!(started["result"]["run_id"].is_string(), "{started}\n{}", d.log());
    fake.prompts.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("the fourth Reviewer was never asked: {}\n{}", current(&mut c), d.log()));
    assert_eq!(workers().iter().filter(|(_, ended)| !ended).count(), 1);
    let mut w = d.connect_caps(json!({"sill.workspaces": 1, "sill.crosscheck": 1})).unwrap();
    let detail = w.call("_sill/workspaces.get", json!({"workspace_id":"ws-1"})).unwrap();
    let discarded = w.call("_sill/workspaces.discard", json!({"operation_id":"discard-1","workspace_id":"ws-1","revision":detail["result"]["revision"]})).unwrap();
    assert!(discarded.get("error").is_none(), "{discarded}\n{}", d.log());
    assert!(current(&mut c).is_null(), "the review went with the work");
    wait(&mut c, "the discarded work's worker's end", &|_| workers().len() == 4 && workers().iter().all(|(_, ended)| *ended) && leftovers() == 0);
    let _ = HostPath::from_path(&source);
}

#[test]
fn storage_cleanup_refuses_a_live_launcher_and_requires_capability() {
    let d = Daemon::start();
    let mut baseline = d.connect().unwrap();
    let reply = baseline.call("_sill/storage.caches", json!({})).unwrap();
    assert_eq!(reply["error"]["data"]["code"], "unsupported");
    let mut c = d.connect_caps(json!({"sill.storage": 1})).unwrap();
    let reply = c.call("_sill/sessions.create", json!({"create_id": "storage-active", "agent": "claude", "policy": "default"})).unwrap();
    let id = reply["result"]["session_id"].as_str().unwrap();
    // A launcher still booting need not have a shim socket. It must still be protected.
    let mut launcher = Command::new("python3")
        .args(["-c", "import time; time.sleep(30)", "launch", "--session", id])
        .spawn().unwrap();
    let rt = d.dir.join("run/sill").join(id);
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::write(rt.join("launch.pid"), launcher.id().to_string()).unwrap();
    let reply = c.call("_sill/storage.clear_build_cache", json!({"session_id": id})).unwrap();
    assert!(reply["error"]["message"].as_str().unwrap().contains("running"), "{reply}");
    assert!(launcher.try_wait().unwrap().is_none(), "cleanup must not stop a session");
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let reply = c.call("_sill/storage.clear_build_cache", json!({"session_id": "../escape"})).unwrap();
    assert!(reply.get("error").is_some());
    let reply = c.call("_sill/storage.clear_build_cache", json!({"session_id": id, "path": "/tmp"})).unwrap();
    assert_eq!(reply["error"]["code"], -32602);
}

#[test]
fn provider_limits_capabilities_legacy_saves_and_credential_binding() {
    let d = Daemon::start();
    let mut old = d.connect_caps(json!({"sill.providers":1})).unwrap();
    let mut new = d.connect_caps(json!({"sill.providers":1,"sill.provider_limits":1})).unwrap();
    let provider = json!({"id":"lan","name":"LAN","api":"openai","base_url":"http://server.example:11434/v1","models":["qwen3"],"model_limits":{"qwen3":{"context":32768,"output":4096}}});
    let denied = old.call("_sill/providers.save", json!({"provider":provider})).unwrap();
    assert!(denied.get("error").is_some(), "{denied}");
    let detect = json!({"base_url":"http://server.example:11434/v1","provider_id":"lan","model":"qwen3"});
    let denied = old.call("_sill/providers.detect_limits", detect.clone()).unwrap();
    assert!(denied.get("error").is_some(), "{denied}");
    let saved = new.call("_sill/providers.save", json!({"provider":provider,"key":"test-secret"})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    let mut legacy = provider.clone();
    legacy.as_object_mut().unwrap().remove("model_limits");
    legacy["name"] = json!("Renamed");
    let saved = old.call("_sill/providers.save", json!({"provider":legacy})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    let listed = new.call("_sill/providers.list", json!({})).unwrap();
    assert_eq!(listed["result"]["providers"][0]["provider"]["model_limits"], provider["model_limits"]);
    let text = std::fs::read_to_string(d.dir.join("home/.config/sill/providers/lan.toml")).unwrap();
    assert!(text.contains("[model_limits.qwen3]"), "the limits are part of the one record: {text}");
    for address in ["http://other.example:11434/v1", "https://server.example:11434/v1", "http://server.example:11434/other/v1", "http://server.example:8080/v1"] {
        for method in ["_sill/providers.detect_limits", "_sill/providers.test"] {
            let mut args = detect.clone();
            args["base_url"] = json!(address);
            if method.ends_with(".test") { args.as_object_mut().unwrap().remove("model"); }
            let refused = new.call(method, args).unwrap();
            assert!(refused["error"]["message"].as_str().unwrap().contains("address changed"), "{refused}");
        }
    }
}

// Run only on a host that can reach a real, already-loaded Ollama model.
#[test]
#[ignore = "requires SILL_TEST_OLLAMA_URL and SILL_TEST_OLLAMA_MODEL"]
fn provider_limits_live_ollama_rpc() {
    let url = std::env::var("SILL_TEST_OLLAMA_URL").unwrap();
    let model = std::env::var("SILL_TEST_OLLAMA_MODEL").unwrap();
    let direct = sill_core::providers::detect_context(&url, None, &model).unwrap();
    let d = Daemon::start();
    let mut c = d.connect_caps(json!({"sill.providers":1,"sill.provider_limits":1})).unwrap();
    let result = c.call("_sill/providers.detect_limits", json!({"base_url":url,"model":model})).unwrap();
    assert_eq!(result["result"]["context"], direct, "{result}");
    let saved = c.call("_sill/providers.save", json!({"provider":{"id":"live","name":"Live","api":"openai","base_url":url,"models":[model.clone()],"model_limits":{model.clone():{"context":direct}}}})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    let env = sill_core::providers::agent_env(&d.dir.join("home"), "opencode").unwrap();
    let config: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    assert_eq!(config["provider"]["sill-live"]["models"][&model]["limit"]["context"], direct);
}

#[test]
fn provider_limits_detection_rpc_saves_the_active_allocation() {
    use std::os::unix::fs::PermissionsExt;
    // Stub only curl's transport: exercise RPC, saved credentials, parsing, save and launch config.
    // This is not evidence of a real Ollama/OpenCode run.
    let dir = Daemon::dir();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    std::fs::write(&curl, r##"#!/bin/sh
config=$(cat)
case "$config" in
  *'url = "http://server.example:11434/api/ps"'*'Authorization: Bearer test-secret'*) ;;
  *) exit 2 ;;
esac
printf '%s\n200' '{"models":[{"name":"qwen3:latest","context_length":32768}]}'
"##).unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let d = Daemon::start_env(dir, &[("PATH", &path)]);
    let mut c = d.connect_caps(json!({"sill.providers":1,"sill.provider_limits":1})).unwrap();
    let mut provider = json!({"id":"lan","name":"LAN","api":"openai","base_url":"http://server.example:11434/v1","models":["qwen3"]});
    let saved = c.call("_sill/providers.save", json!({"provider":provider,"key":"test-secret"})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    let result = c.call("_sill/providers.detect_limits", json!({"base_url":provider["base_url"],"provider_id":"lan","model":"qwen3"})).unwrap();
    assert_eq!(result["result"]["context"], 32768, "{result}");
    provider["model_limits"] = json!({"qwen3":{"context":result["result"]["context"],"output":4096}});
    let saved = c.call("_sill/providers.save", json!({"provider":provider})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    let env = sill_core::providers::agent_env(&d.dir.join("home"), "opencode").unwrap();
    let config: Value = serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap();
    assert_eq!(config["provider"]["sill-lan"]["models"]["qwen3"]["limit"], json!({"context":32768,"output":4096}));
    provider["model_limits"] = json!({});
    let saved = c.call("_sill/providers.save", json!({"provider":provider})).unwrap();
    assert!(saved.get("result").is_some(), "{saved}");
    assert!(sill_core::providers::load(&d.dir.join("home")).unwrap()["lan"].model_limits.is_empty());
}
