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

    fn connect(&self) -> std::io::Result<Conn> { self.connect_caps(json!({"sill.internal":1})) }

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

#[test]
fn frontend_reads_the_daemons_host_and_refuses_links() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let request = |c: &mut Conn, request| c.call(sill_core::frontend::METHOD, serde_json::to_value(request).unwrap()).unwrap();
    use sill_core::frontend::Request;
    let file = d.dir.join("home/remote.txt");
    std::fs::write(&file, "on the box").unwrap();
    assert_eq!(request(&mut c, Request::Home)["result"], json!(d.dir.join("home")));
    assert_eq!(request(&mut c, Request::Read { path: file.clone() })["result"], json!(b"on the box".as_slice()));
    request(&mut c, Request::Write { path: file.clone(), bytes: b"edited from frontend".to_vec() });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited from frontend");
    std::os::unix::fs::symlink(&file, d.dir.join("home/link")).unwrap();
    let link = d.dir.join("home/link");
    assert!(request(&mut c, Request::Read { path: link.clone() }).get("error").is_some());
    assert!(request(&mut c, Request::Write { path: link, bytes: b"bad".to_vec() }).get("error").is_some());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited from frontend");
    let entries = request(&mut c, Request::List { path: d.dir.join("home") });
    assert!(entries["result"].as_array().unwrap().iter().any(|e| e["metadata"]["symlink"] == true));
}

#[test]
fn frontend_store_and_settings_are_daemon_owned() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    use sill_core::frontend::Request;
    let request = |c: &mut Conn, request| c.call(sill_core::frontend::METHOD, serde_json::to_value(request).unwrap()).unwrap();
    assert_eq!(request(&mut c, Request::Sessions)["result"], json!([]));
    let mut config = sill_core::config::Config::default();
    config.memory_recall = Some(7);
    assert!(request(&mut c, Request::SetConfig(config)).get("error").is_none());
    assert_eq!(request(&mut c, Request::Config)["result"]["memory_recall"], json!(7));
    request(&mut c, Request::PatchConfig { fields: [("tray".into(), json!(false))].into() });
    request(&mut c, Request::PatchConfig { fields: [("theme".into(), json!("nord"))].into() });
    let settings = request(&mut c, Request::Config);
    assert_eq!(settings["result"]["memory_recall"], json!(7), "changing presentation preserves memory settings");
    assert_eq!(settings["result"]["tray"], json!(false));
    assert_eq!(settings["result"]["theme"], json!("nord"));
    assert!(request(&mut c, Request::PatchConfig { fields: [("unknown".into(), json!(true))].into() }).get("error").is_some());
    assert!(request(&mut c, Request::SetInstructions { text: "remember the box".into() }).get("error").is_none());
    assert_eq!(request(&mut c, Request::Instructions)["result"], json!("remember the box"));
    assert!(request(&mut c, Request::Upload { session: "abcdef01".into(), name: "../bad".into(), bytes: vec![] }).get("error").is_some());
}

/// Attach a stream to a terminal; the token line and first keystrokes go in one write.
fn attach_terminal(d: &Daemon, c: &mut Conn, id: &str, input: &str) -> UnixStream {
    let r = c.call("_sill/terminals.attach", json!({"terminal_id": id})).unwrap();
    let token = r["result"]["token"].as_str().unwrap_or_else(|| panic!("attach: {r}")).to_string();
    let mut stream = UnixStream::connect(d.socket()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(format!("{}\n{input}", json!({"_sill_stream": token})).as_bytes()).unwrap();
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
        let r = self.read()?;
        assert_eq!(r["id"], json!(id), "answers come back in order");
        Ok(r)
    }
}

#[test]
fn status_names_the_process_and_the_pid_file_matches() {
    let d = Daemon::start();
    let mut c = d.connect().unwrap();
    let r = c.call(sill_proto::method::DAEMON_STATUS, json!({})).unwrap();
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
fn public_and_uninitialized_connections_do_not_receive_internal_change_notifications() {
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
    assert_eq!(public.call(sill_proto::method::DAEMON_STATUS,json!({})).unwrap()["error"]["data"]["code"],"unsupported");
    let mut internal=d.connect().unwrap();
    let request_id=internal.next;
    internal.send_raw(&json!({"jsonrpc":"2.0","id":request_id,"method":sill_proto::method::CHANGES_KEEP,"params":{"session":"missing","items":[]}}).to_string()).unwrap();
    let mut changed=false;
    loop {
        let message=internal.read().unwrap();
        if message.get("method")==Some(&json!(sill_proto::method::CHANGES_CHANGED)) {changed=true;}
        if message.get("id")==Some(&json!(request_id)) {break;}
    }
    assert!(changed,"internal subscriber receives the change notification");
    for mut conn in [public,uninitialized] {
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
    assert_eq!(c.call(sill_proto::method::SESSIONS_LIST,json!({})).unwrap()["error"]["data"]["code"],"unsupported");
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
