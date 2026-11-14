//! A blocking client: one connection, requests answered in order of arrival by id, notifications
//! handed out on a channel. What the CLI uses; the shelf wraps the same thing in a GLib channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::rpc::{Error, Incoming, Request};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, Error>>>>>;

/// The longest any request legitimately takes (an attach that starts a jail and an agent on a
/// slow disk), with room; past it the daemon is stuck and the person is told so.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// For `daemon.status` and `shutdown`: answered at once by a daemon that answers at all.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
/// A bare daemon's `daemon.log` is rotated to `.1` past this when the next one starts.
const LOG_KEEP: u64 = 8 * 1024 * 1024;

struct Outbound {
    line: Vec<u8>,
    deadline: Instant,
}

pub struct Client {
    writer: SyncSender<Outbound>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    closed: Arc<AtomicBool>,
    methods: Option<std::collections::HashSet<String>>,
    next_id: AtomicU64,
    pending: Pending,
    /// `(method, params)` of every notification the daemon sent.
    pub notifications: Mutex<Receiver<(String, Value)>>,
}

impl Client {
    /// Connect to the running daemon; an error when there is none.
    pub fn connect() -> Result<Client> {
        if let Some(target) = crate::transport::selected()? {
            return Self::ssh(&target.host, &target.binary);
        }
        Self::connect_local()
    }

    pub fn connect_local() -> Result<Client> {
        let path = crate::socket_path()?;
        let stream = UnixStream::connect(&path).with_context(|| format!("sill daemon at {}", path.display()))?;
        Self::over(stream)
    }

    /// Connect, starting the daemon when nothing answers: the systemd socket starts it by itself
    /// where the units are installed; otherwise `sill daemon` is started — as a transient user unit
    /// with `Delegate=yes` where the user manager answers, so sessions get their cgroups, else
    /// spawned detached — and the socket waited for (direct mode, docs/daemon.md §3.1).
    pub fn connect_or_start() -> Result<Client> {
        if let Some(target) = crate::transport::selected()? {
            return Self::ssh(&target.host, &target.binary);
        }
        Self::connect_local_or_start()
    }

    /// Local-only entry point used by the stdio bridge on the box.
    pub fn connect_local_or_start() -> Result<Client> {
        match Self::connect_local() {
            Ok(c) => match c.protocol()? {
                p if p == crate::PROTOCOL => return Ok(c),
                p => {
                    // an older (or newer) binary's daemon, left running across an upgrade: it is
                    // asked to leave and comes back as this binary — the socket unit starts the
                    // installed one on the next connection, else it is started below
                    eprintln!("sill: the daemon speaks protocol {p}, this binary {}: restarting it", crate::PROTOCOL);
                    let _ = c.call_within::<Value>(crate::method::DAEMON_SHUTDOWN, crate::Empty::default(), QUICK_TIMEOUT);
                    drop(c);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(100));
                        match Self::connect_local() {
                            Ok(c) if c.protocol().ok() == Some(crate::PROTOCOL) => return Ok(c),
                            Ok(_) => continue, // the old one is still leaving
                            Err(_) => break,   // gone: start one
                        }
                    }
                }
            },
            Err(e) => {
                if std::env::var_os("SILL_NO_DAEMON_START").is_some() {
                    return Err(e);
                }
                // a jail has its own tmpfs /run and no host socket: a daemon started here would
                // die at once on the hidden sill.db and leave a dead socket behind
                if std::env::var_os("SILL_SESSION").is_some() {
                    bail!("no sill daemon is reachable from inside a session");
                }
            }
        }
        let dir = crate::runtime_dir()?;
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let exe = std::env::current_exe()?;
        // `sill-daemon.service`, transient, its log in the journal and its cgroup its own (a bare
        // child of a shelf would live — and die — in the shelf's unit); `systemctl --user stop
        // sill-daemon` ends it and sessions stay, they are scopes of their own. Where that fails —
        // no user manager, or a unit of that name still loaded — the daemon is spawned bare.
        // the installed units exist but gave up (docs/daemon.md §3.7: the start limit after
        // repeated failures stops sill.socket too): starting a transient daemon beside them would
        // hide why — the person is sent to the journal instead
        if let Some(failed) = failed_unit() {
            bail!("{failed} failed and systemd stopped trying; `journalctl --user -u sill` has why, then `systemctl --user reset-failed sill.socket sill.service && systemctl --user start sill.socket`");
        }
        let mut why_bare = String::from("no systemd user instance");
        let unit_started = systemd_user_usable() && {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "reset-failed", "--quiet", "sill-daemon.service"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match std::process::Command::new("systemd-run")
                 .args(["--user", "--unit", "sill-daemon", "--collect", "--quiet", "--service-type=notify",
                    "-p", "Slice=app.slice", "-p", "KillMode=process", "-p", "NotifyAccess=main", "-p", "WatchdogSec=90",
                    "-p", "Restart=on-failure", "-p", "RestartSec=2", "-p", "StartLimitIntervalSec=60", "-p", "StartLimitBurst=10"])
                .args(["--setenv=HOME", "--setenv=USER", "--setenv=TERM", "--setenv=LANG"])
                .arg("--")
                .arg(&exe)
                .arg("daemon")
                .stdin(std::process::Stdio::null())
                .output()
            {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    why_bare = format!("systemd-run: {}", String::from_utf8_lossy(&o.stderr).trim());
                    false
                }
                Err(e) => {
                    why_bare = format!("systemd-run: {e}");
                    false
                }
            }
        };
        if !unit_started {
            // bare mode has no journal: the log is a file, kept to one generation of LOG_KEEP
            let log_path = dir.join("daemon.log");
            if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_KEEP) {
                let _ = std::fs::rename(&log_path, dir.join("daemon.log.1"));
            }
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
            // the person sees why in the daemon's own log, not on a CLI command's stderr
            let _ = std::io::Write::write_all(&mut log, format!("sill client: starting the daemon bare ({why_bare})\n").as_bytes());
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
            // its own session: no terminal hangup reaches it, and it is nobody's job
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                    let _ = rustix::process::setsid();
                    Ok(())
                });
            }
            cmd.spawn().with_context(|| format!("starting {} daemon", exe.display()))?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Self::connect_local() {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() >= deadline => {
                    bail!("the daemon did not come up in 5 s ({e:#}); its log is {}", dir.join("daemon.log").display())
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// The protocol the daemon on this connection speaks (`daemon.status`).
    fn protocol(&self) -> Result<u32> {
        let s: crate::DaemonStatus = self.call_within(crate::method::DAEMON_STATUS, crate::Empty::default(), QUICK_TIMEOUT)?;
        Ok(s.protocol)
    }

    /// The connection is gone (the daemon left or restarted): the caller connects again.
    pub fn is_closed(e: &anyhow::Error) -> bool {
        e.to_string().contains("closed the connection") || e.to_string().contains("went away") || e.to_string().contains("writing to the daemon")
    }

    pub fn over(stream: UnixStream) -> Result<Client> {
        let reader = stream.try_clone()?;
        let close = stream.try_clone()?;
        Self::over_io(reader, stream, move || { let _ = close.shutdown(std::net::Shutdown::Both); })
    }

    /// Build a client over any duplex pipe. `interrupt` must unblock both halves (close a socket
    /// or terminate its transport child); it runs on timeout and when the client is dropped.
    pub fn over_io<R, W, F>(reader: R, mut writer: W, interrupt: F) -> Result<Client>
    where R: Read + Send + 'static, W: Write + Send + 'static, F: Fn() + Send + Sync + 'static {
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let interrupt: Arc<dyn Fn() + Send + Sync> = Arc::new(interrupt);
        let (ntx, nrx) = channel();
        let p2 = pending.clone();
        let c2 = closed.clone();
        let i2 = interrupt.clone();
        std::thread::Builder::new().name("sill-proto-reader".into()).spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Incoming>(&line) else { continue };
                match (msg.id, msg.method) {
                    (Some(id), None) => {
                        let tx = p2.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            let _ = tx.send(match msg.error {
                                Some(e) => Err(e),
                                None => Ok(msg.result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    (None, Some(method)) => { let _ = ntx.send((method, msg.params.unwrap_or(Value::Null))); }
                    _ => {}
                }
            }
            c2.store(true, Ordering::Release);
            i2();
            fail_pending(&p2, "the daemon closed the connection");
        })?;
        let (wtx, wrx) = sync_channel::<Outbound>(64);
        let p3 = pending.clone();
        let c3 = closed.clone();
        let i3 = interrupt.clone();
        std::thread::Builder::new().name("sill-proto-writer".into()).spawn(move || {
            for out in wrx {
                if c3.load(Ordering::Acquire) || Instant::now() >= out.deadline {
                    break;
                }
                if writer.write_all(&out.line).and_then(|()| writer.flush()).is_err() { break; }
            }
            c3.store(true, Ordering::Release);
            i3();
            fail_pending(&p3, "writing to the daemon: connection closed");
        })?;
        Ok(Client { writer: wtx, interrupt, closed, methods: None, next_id: AtomicU64::new(1), pending, notifications: Mutex::new(nrx) })
    }

    /// Connect over the system SSH client. It owns authentication, host verification, proxies
    /// and hardware keys; a protocol mismatch never stops or replaces the server's daemon.
    pub fn ssh(host: &str, binary: &str) -> Result<Client> {
        let mut cmd = crate::transport::ssh_command(host, binary)?;
        let mut child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit()).spawn().context("starting ssh to the sill box")?;
        let reader = child.stdout.take().context("ssh stdout")?;
        let writer = child.stdin.take().context("ssh stdin")?;
        let child = Arc::new(Mutex::new(child));
        let stop = child.clone();
        let mut client = Self::over_io(reader, writer, move || {
            if let Ok(mut c) = stop.lock() { let _ = c.kill(); let _ = c.wait(); }
        })?;
        client.negotiate_remote()?;
        Ok(client)
    }

    fn negotiate_remote(&mut self) -> Result<()> {
        let protocol = self.protocol()?;
        if protocol == crate::PROTOCOL { return Ok(()); }
        let schema: Value = self.call_within(crate::method::DAEMON_SCHEMA, crate::Empty::default(), QUICK_TIMEOUT)
            .with_context(|| format!("box speaks protocol {protocol}, this client {}; the box needs a daemon with schema negotiation (update sill on the box)", crate::PROTOCOL))?;
        let remote = schema["methods"].as_array().context("the box returned no protocol method table")?;
        let local = crate::schema::document();
        let methods = local["methods"].as_array().unwrap().iter().filter(|m| {
            // Opaque payloads cannot be compared structurally across protocol revisions.
            if m["name"] == crate::method::FRONTEND_REQUEST || m["name"] == crate::method::CHANGES_HISTORY || m["name"] == crate::method::SETTINGS_GET { return false; }
            remote.iter().find(|r| r["name"] == m["name"]).is_some_and(|r| {
                r["kind"] == m["kind"]
                    && schema_matches(&m["params"], &r["params"], &local, &schema, &mut std::collections::HashSet::new())
                    && schema_matches(&m["result"], &r["result"], &local, &schema, &mut std::collections::HashSet::new())
            })
        }).filter_map(|m| m["name"].as_str().map(str::to_string)).collect();
        self.methods = Some(methods);
        Ok(())
    }

    /// One request, its answer — within `CALL_TIMEOUT`: an attach waits for a jail to start and an
    /// end for the agent's summary, nothing waits longer than this, and a daemon that does not
    /// answer at all must not hang its client forever (docs/daemon.md §3.7).
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T> {
        self.call_within(method, params, CALL_TIMEOUT)
    }

    /// `call` with a timeout of its own: short for `daemon.status` and `shutdown`, which answer
    /// at once or not at all.
    pub fn call_within<T: DeserializeOwned>(&self, method: &str, params: impl Serialize, timeout: Duration) -> Result<T> {
        let deadline = Instant::now() + timeout;
        if self.closed.load(Ordering::Acquire) { bail!("the daemon closed the connection"); }
        if self.methods.as_ref().is_some_and(|m| !m.contains(method)) {
            bail!("the box does not support a compatible {method}; update sill on the box");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(Some(id), method, serde_json::to_value(params)?);
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut out = Outbound { line, deadline };
        loop {
            if Instant::now() >= deadline { return self.timed_out(id, method, timeout); }
            match self.writer.try_send(out) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => {
                    self.pending.lock().unwrap().remove(&id);
                    bail!("writing to the daemon: connection closed");
                }
                Err(TrySendError::Full(unsent)) => {
                    out = unsent;
                    std::thread::sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())));
                }
            }
        }
        let v = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(v) => v.map_err(|e| anyhow::anyhow!("{method}: {e}"))?,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("the daemon went away"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return self.timed_out(id, method, timeout),
        };
        serde_json::from_value(v).with_context(|| format!("{method}: unexpected answer shape"))
    }

    fn timed_out<T>(&self, id: u64, method: &str, timeout: Duration) -> Result<T> {
        self.pending.lock().unwrap().remove(&id);
        // A timed-out write may have sent only part of a JSON frame. Discard the entire
        // connection so no subsequent request can be appended to that partial frame.
        self.closed.store(true, Ordering::Release);
        (self.interrupt)();
        fail_pending(&self.pending, "the daemon closed the connection after a call timed out");
        bail!("the daemon did not answer {method} in {} s: it is stuck; `sill restart` (or `systemctl --user restart sill`) starts a fresh one, sessions survive it", timeout.as_secs_f64())
    }

}

/// The first of `sill.socket`, `sill.service` in the failed state, if any (`is-failed` exits 0
/// for exactly that; a unit that is not installed is "inactive").
fn failed_unit() -> Option<&'static str> {
    ["sill.socket", "sill.service"].into_iter().find(|u| {
        std::process::Command::new("systemctl")
            .args(["--user", "is-failed", "--quiet", u])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// `systemd-run --user` works here: `$XDG_RUNTIME_DIR` set, systemd the init, the user manager answering.
fn systemd_user_usable() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() || !std::path::Path::new("/run/systemd/system").is_dir() {
        return false;
    }
    std::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "running" | "degraded" | "starting" | "initializing" | "maintenance"))
        .unwrap_or(false)
}


impl Drop for Client {
    fn drop(&mut self) { (self.interrupt)(); }
}

fn fail_pending(pending: &Pending, message: &str) {
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(Error::new(Error::INTERNAL, message)));
    }
}

/// Compare the reachable schema, resolving references rather than assuming equal type names
/// imply equal shapes. New methods on either side are harmless; changed method shapes are
/// unavailable until both binaries agree. Recursive wire types are compared once per pair.
fn schema_matches(local: &Value, remote: &Value, local_doc: &Value, remote_doc: &Value, seen: &mut std::collections::HashSet<(String, String)>) -> bool {
    if let (Some(l), Some(r)) = (local.get("$ref").and_then(Value::as_str), remote.get("$ref").and_then(Value::as_str)) {
        if !seen.insert((l.to_string(), r.to_string())) { return true; }
        let (Some(l), Some(r)) = (local_doc.pointer(l.trim_start_matches('#')), remote_doc.pointer(r.trim_start_matches('#'))) else { return false };
        return schema_matches(l, r, local_doc, remote_doc, seen);
    }
    match (local, remote) {
        (Value::Object(l), Value::Object(r)) => {
            let meaningful = |key: &str| !matches!(key, "description" | "title" | "examples" | "$comment");
            l.keys().filter(|k| meaningful(k)).count() == r.keys().filter(|k| meaningful(k)).count()
                && l.iter().filter(|(k, _)| meaningful(k)).all(|(k, v)| r.get(k).is_some_and(|rv| schema_matches(v, rv, local_doc, remote_doc, seen)))
        }
        (Value::Array(l), Value::Array(r)) => l.len() == r.len() && l.iter().zip(r).all(|(l, r)| schema_matches(l, r, local_doc, remote_doc, seen)),
        _ => local == remote,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_includes_a_blocked_large_write() {
        let (socket, _not_reading) = UnixStream::pair().unwrap();
        let client = Client::over(socket).unwrap();
        let start = Instant::now();
        let err = client.call_within::<Value>("large", "x".repeat(4 * 1024 * 1024), Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(2), "write exceeded deadline: {:?}", start.elapsed());
        assert!(Client::is_closed(&client.call::<Value>("next", ()).unwrap_err()));
    }

    #[test]
    fn a_call_queued_behind_a_blocked_writer_has_its_own_deadline() {
        let (socket, _not_reading) = UnixStream::pair().unwrap();
        let client = Arc::new(Client::over(socket).unwrap());
        let other = client.clone();
        let blocked = std::thread::spawn(move || other.call_within::<Value>("large", "x".repeat(4 * 1024 * 1024), Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        let err = client.call_within::<Value>("queued", (), Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(blocked.join().unwrap().is_err(), "blocked waiter was not interrupted");
        assert!(client.pending.lock().unwrap().is_empty(), "pending requests leaked after interruption");
    }

    #[test]
    fn a_duplex_pipe_dispatches_replies_and_notifications() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut output = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(server).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","method":"event","params":{"n":1}})).unwrap();
            writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":{"ok":true}})).unwrap();
        });
        let c = Client::over(client).unwrap();
        assert_eq!(c.call::<Value>("request", ()).unwrap(), serde_json::json!({"ok":true}));
        assert_eq!(c.notifications.lock().unwrap().recv().unwrap().0, "event");
        worker.join().unwrap();
    }

    #[test]
    fn schema_negotiation_ignores_added_methods_and_rejects_changed_shapes() {
        let local = crate::schema::document();
        let mut remote = local.clone();
        remote["protocol"] = serde_json::json!(crate::PROTOCOL + 1);
        remote["methods"].as_array_mut().unwrap().push(serde_json::json!({"name":"_sill/new.method","kind":"request","params":{},"result":{}}));
        let session = &local["$defs"]["Session"];
        assert!(schema_matches(session, &remote["$defs"]["Session"], &local, &remote, &mut Default::default()));
        remote["$defs"]["Session"]["type"] = serde_json::json!("number");
        assert!(!schema_matches(session, &remote["$defs"]["Session"], &local, &remote, &mut Default::default()));
    }

    #[test]
    fn mismatch_negotiation_never_asks_the_daemon_to_shutdown() {
        let (socket, server) = UnixStream::pair().unwrap();
        let mut output = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            let mut input = BufReader::new(server);
            for expected in [crate::method::DAEMON_STATUS, crate::method::DAEMON_SCHEMA, crate::method::SESSIONS_LIST] {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(req["method"], expected);
                let result = match expected {
                    crate::method::DAEMON_STATUS => serde_json::json!({"version":"test","protocol":crate::PROTOCOL+1,"pid":1,"started":0,"mode":"test","launcher":"test","limits":false,"journal":false,"socket":"remote","sessions":0,"running":0}),
                    crate::method::DAEMON_SCHEMA => crate::schema::document(),
                    _ => serde_json::json!([]),
                };
                writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":result})).unwrap();
            }
        });
        let mut client = Client::over(socket).unwrap();
        client.negotiate_remote().unwrap();
        assert_eq!(client.call::<Value>(crate::method::SESSIONS_LIST, crate::Empty::default()).unwrap(), serde_json::json!([]));
        worker.join().unwrap();
    }

}
