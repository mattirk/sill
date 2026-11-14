//! A blocking client: one connection, requests answered in order of arrival by id, notifications
//! handed out on a channel. What the CLI and sill-client use.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::rpc::{Incoming, Request};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>>;

/// The connection is gone: the daemon closed it, a write failed, or a call timed out and the
/// client discarded it. A request may or may not have reached the daemon. Sits anywhere in a
/// call's error chain; `Client::is_closed` finds it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionLost(pub String);

impl std::fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectionLost {}

fn lost<T>(message: impl Into<String>) -> Result<T> {
    Err(ConnectionLost(message.into()).into())
}

/// The longest any request legitimately takes (an attach that starts a jail and an agent on a
/// slow disk), with room; past it the daemon is stuck and the person is told so.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// For `daemon.status` and `shutdown`: answered at once by a daemon that answers at all.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
/// A bare daemon's `daemon.log` is rotated to `.1` past this when the next one starts.
const LOG_KEEP: u64 = 8 * 1024 * 1024;

/// Frontend-declared capabilities this program offers on connections it makes from now on.
static DECLARED: Mutex<std::collections::BTreeSet<&'static str>> = Mutex::new(std::collections::BTreeSet::new());

/// Declare that this program implements a frontend-declared capability's user-facing contract
/// (`initialize::FRONTEND_DECLARED`). Connections made later offer it; existing ones do not.
pub fn declare_capability(name: &'static str) {
    debug_assert!(crate::initialize::FRONTEND_DECLARED.contains(&name), "{name} is not frontend-declared");
    DECLARED.lock().unwrap().insert(name);
}

struct Outbound {
    line: Vec<u8>,
    deadline: Instant,
}

pub struct Client {
    writer: SyncSender<Outbound>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    pending: Pending,
    /// `(method, params)` of every notification the daemon sent.
    pub notifications: Mutex<Receiver<(String, Value)>>,
    /// The capabilities the daemon answered at initialization: what this connection may use.
    capabilities: Mutex<std::collections::BTreeMap<String, u32>>,
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
        let client = Self::over(stream)?;
        client.initialize()?;
        Ok(client)
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

    /// Explicit operator start/restart, independent of the automatic-start preference.
    pub fn connect_or_start_explicit() -> Result<Client> {
        if let Some(target) = crate::transport::selected()? {
            return Self::ssh(&target.host, &target.binary);
        }
        Self::connect_local_with_start(true)
    }

    /// Local-only entry point used by the stdio bridge on the box.
    pub fn connect_local_or_start() -> Result<Client> {
        Self::connect_local_with_start(false)
    }

    fn connect_local_with_start(explicit: bool) -> Result<Client> {
        let path = crate::socket_path()?;
        match UnixStream::connect(&path) {
            Ok(stream) => {
                let client = Self::over(stream)?;
                // Reaching a daemon never authorizes replacing it, even when its handshake fails.
                client.initialize()?;
                return Ok(client);
            }
            Err(e) => {
                if !matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
                    || std::env::var_os("SILL_NO_DAEMON_START").is_some() { return Err(e.into()); }
                if std::env::var_os("SILL_SESSION").is_some() {
                    bail!("no sill daemon is reachable from inside a session");
                }
            }
        }
        if !explicit && !crate::autostart_enabled()? {
            bail!("daemon autostart is disabled; run `sill daemon` to start it manually, or `sill daemon autostart on`");
        }
        let dir = crate::runtime_dir()?;
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let exe = std::env::current_exe()?;
        // `sill-daemon.service`, transient, its log in the journal and its cgroup its own (a bare
        // child of a frontend would live — and die — in the frontend's unit); `systemctl --user stop
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

    /// Initialize a raw control connection. `connect*` and `ssh` do this automatically.
    pub fn initialize(&self) -> Result<crate::initialize::Initialized> {
        let result: crate::initialize::Initialized = self.call_within(
            crate::initialize::METHOD,
            crate::initialize::Initialize {
                protocols: vec![crate::initialize::PROTOCOL_ID.into()],
                client: crate::initialize::Peer { name: "sill".into(), version: env!("CARGO_PKG_VERSION").into() },
                // this build implements every capability it knows of, except the ones that
                // describe a frontend's own contract and were not declared by this program
                capabilities: crate::initialize::CAPABILITIES.iter()
                    .filter(|c| !crate::initialize::FRONTEND_DECLARED.contains(&c.name) || DECLARED.lock().unwrap().contains(c.name))
                    .map(|c| (c.name.to_string(), c.version)).collect(),
            }, QUICK_TIMEOUT,
        ).context("SFP initialization failed")?;
        if result.protocol != crate::initialize::PROTOCOL_ID {
            bail!("daemon selected unsupported protocol {}; explicitly update/restart it", result.protocol);
        }
        *self.capabilities.lock().unwrap() = result.capabilities.clone();
        Ok(result)
    }

    /// The daemon answered `name` at initialization: its methods may be called on this connection.
    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.lock().unwrap().get(name).is_some_and(|v| *v > 0)
    }

    /// This connection can still carry a request. False once the daemon closed it, a write failed
    /// or a call timed out: a request now would never be sent, so the caller connects again first.
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    /// The connection is gone (the daemon left or restarted, a write failed, a call timed out):
    /// the caller connects again. Reads the whole error chain, so a connection lost after a
    /// request was sent is seen on that request's error (wrapped in its method's context), not
    /// only on the next call's.
    pub fn is_closed(e: &anyhow::Error) -> bool {
        e.chain().any(|cause| {
            cause.downcast_ref::<ConnectionLost>().is_some()
                || cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    use std::io::ErrorKind::*;
                    matches!(io.kind(), BrokenPipe | ConnectionReset | ConnectionAborted | UnexpectedEof | NotConnected)
                })
        })
    }

    /// Raw duplex transport; call `initialize` before sending control requests.
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
            let mut reader = BufReader::new(reader);
            while let Ok(Some(line)) = crate::initialize::read_frame(&mut reader, crate::initialize::MAX_MESSAGE_BYTES) {
                let Ok(msg) = serde_json::from_str::<Incoming>(&line) else { continue };
                match (msg.id, msg.method) {
                    (Some(id), None) => {
                        let tx = p2.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            let _ = tx.send(match msg.error {
                                Some(e) => Err(anyhow::Error::new(e)),
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
        Ok(Client { writer: wtx, interrupt, closed, next_id: AtomicU64::new(1), pending, notifications: Mutex::new(nrx), capabilities: Mutex::default() })
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
        let client = Self::over_io(reader, writer, move || {
            if let Ok(mut c) = stop.lock() { let _ = c.kill(); let _ = c.wait(); }
        })?;
        client.initialize().with_context(|| format!(
            "SSH connection to {host} did not initialize Sill. Check SSH's messages above and the remote executable {binary}. Background connections cannot prompt: unlock the key with ssh-add or run sill box add in a terminal to authenticate again"
        ))?;
        Ok(client)
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
        if self.closed.load(Ordering::Acquire) { return lost("the daemon closed the connection"); }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(Some(id), method, serde_json::to_value(params)?);
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        if line.len() > crate::initialize::MAX_MESSAGE_BYTES { bail!("request exceeds max_message_bytes"); }
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut out = Outbound { line, deadline };
        loop {
            if Instant::now() >= deadline { return self.timed_out(id, method, timeout); }
            match self.writer.try_send(out) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => {
                    self.pending.lock().unwrap().remove(&id);
                    return lost("writing to the daemon: connection closed");
                }
                Err(TrySendError::Full(unsent)) => {
                    out = unsent;
                    std::thread::sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())));
                }
            }
        }
        let v = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(v) => v.with_context(|| format!("{method}"))?,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return lost("the daemon went away"),
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
        lost(format!("the daemon did not answer {method} in {} s: it is stuck; `sill restart` (or `systemctl --user restart sill`) starts a fresh one, sessions survive it", timeout.as_secs_f64()))
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
        let _ = tx.send(lost(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Error;
    use std::io::BufRead;

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
    fn incompatible_initialization_sends_no_shutdown_or_other_request() {
        let (socket, server) = UnixStream::pair().unwrap();
        let mut output = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            let mut input = BufReader::new(server);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], crate::initialize::METHOD);
            let error = Error::domain("incompatible", "unsupported protocol");
            writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","id":req["id"],"error":error})).unwrap();
            line.clear();
            assert_eq!(input.read_line(&mut line).unwrap(), 0);
        });
        let client = Client::over(socket).unwrap();
        let error = client.initialize().unwrap_err();
        assert_eq!(error.downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"], "incompatible");
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn a_peer_closing_after_the_request_was_sent_reports_closed_on_that_call() {
        let (socket, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut input = BufReader::new(server);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            // the daemon went away without answering: drop the socket
        });
        let client = Client::over(socket).unwrap();
        let error = client.call::<Value>("request", ()).unwrap_err();
        assert!(Client::is_closed(&error), "{error:#}");
        assert!(format!("{error:#}").contains("closed the connection"), "{error:#}");
        assert!(!client.is_open());
        assert!(Client::is_closed(&client.call::<Value>("next", ()).unwrap_err()));
        worker.join().unwrap();
    }

    #[test]
    fn a_domain_error_from_the_daemon_is_not_closed() {
        let (socket, server) = UnixStream::pair().unwrap();
        let mut output = server.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            let mut input = BufReader::new(server);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let error = Error::domain("not_found", "no such session");
            writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","id":req["id"],"error":error})).unwrap();
            line.clear();
            assert_eq!(input.read_line(&mut line).unwrap(), 0);
        });
        let client = Client::over(socket).unwrap();
        let error = client.call::<Value>("session.get", ()).unwrap_err();
        assert!(!Client::is_closed(&error), "{error:#}");
        assert_eq!(error.downcast_ref::<Error>().unwrap().data.as_ref().unwrap()["code"], "not_found");
        assert!(client.is_open());
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn is_closed_sees_a_connection_lost_under_added_context() {
        let error = anyhow::Error::from(ConnectionLost("the daemon closed the connection".into())).context("session.list");
        assert!(Client::is_closed(&error));
        let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe)).context("writing");
        assert!(Client::is_closed(&io));
        assert!(!Client::is_closed(&anyhow::anyhow!("session.list").context("the daemon closed the connection, or so it says")));
    }
}
