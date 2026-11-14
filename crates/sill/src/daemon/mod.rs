//! `sill daemon` (docs/daemon.md): sill-core behind a socket — the session processes (M1a), the ACP
//! client of every running session and its transcript (M1b, `runner`), and the read-only methods.
//!
//! One reader thread per connection, a line in, a line out (`sill_proto::rpc`), and one writer
//! thread draining the connection's queue: replies and the sessions' events share it, so an
//! attached frontend gets a session's state before the answer to its `attach`. The queue is bounded
//! (decision 15): a frontend that stops reading is dropped and attaches again with its cursor.

use std::io::{BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use anyhow::{Context, Result, bail};
use sill_proto::rpc::{Error, Incoming, Response};

mod hub;
mod adapter;
mod sfp;
mod uploads;
mod sfp_adapter;
mod runner;
mod streams;
mod terminals;
mod host;
mod context;
mod frontend;
pub mod relay;
pub use hub::Hub;

/// How long a hub lock may stay taken before the watchdog calls it a deadlock: a jail start holds
/// `starting` for seconds, a big scan the store for a few more; nothing legitimate holds one for
/// this long. `SILL_WATCHDOG_SECS` overrides it (the tests use 2).
const LOCK_PATIENCE_SECS: u64 = 30;
/// The exit status of a daemon that found itself stuck (a panic is 101).
const EXIT_STUCK: i32 = 102;

/// Lines a connection may have waiting before it counts as stuck (decision 15): a streaming turn
/// is a few hundred events a second at most.
const QUEUE: usize = 20_000;

/// One frontend connection, as the hub and the runners see it: where its lines go.
enum WriteItem { Line(String), Flush(std::sync::mpsc::Sender<()>) }

pub struct Conn {
    pub id: u64,
    internal: AtomicBool,
    tx: SyncSender<WriteItem>,
    /// To cut a stuck connection off.
    sock: UnixStream,
}

impl Conn {
    /// Queue one line; false when the connection is gone or stuck (it is cut off then).
    pub fn send(&self, line: String) -> bool {
        match self.tx.try_send(WriteItem::Line(line)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                eprintln!("sill daemon: connection {} is not reading; dropping it", self.id);
                let _ = self.sock.shutdown(std::net::Shutdown::Both);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    fn reply(&self, r: &Response) {
        if let Ok(line) = serde_json::to_string(r) {
            self.send(line);
        }
    }
}

impl Conn {
    fn drain(&self) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx.try_send(WriteItem::Flush(tx)).map_err(|_| anyhow::anyhow!("connection writer unavailable"))?;
        rx.recv_timeout(std::time::Duration::from_secs(5)).context("draining shutdown response")?;
        Ok(())
    }
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Run until `_sill/daemon.shutdown` or a fatal socket error. The listener is systemd's when the
/// unit passed one (`LISTEN_FDS`), ours otherwise (direct mode: spawned by the first client).
pub fn run() -> Result<()> {
    // a jail has no store and no host socket: nothing to run here, and `lock_store` would leave a
    // daemon.lock in the session's overlay (one more thing in Changes)
    if std::env::var_os("SILL_SESSION").is_some() {
        bail!("sill daemon does not run inside a session");
    }
    // one daemon per store, whatever socket it listens on: two would be two writers of sill.db
    let _store_lock = lock_store()?;
    exit_on_panic();
    let (listener, mode) = match activated_listener()? {
        Some(l) => (l, "socket-activated (sill.service)"),
        // systemd sets INVOCATION_ID for whatever it runs: the transient unit a client started
        None if std::env::var_os("INVOCATION_ID").is_some() => (bind_direct()?, "started by a client in its own unit (sill-daemon.service)"),
        None => (bind_direct()?, "started bare by a client (no unit: see daemon.log)"),
    };
    let hub = Arc::new(Hub::new(mode)?);
    {
        // the minute sweep: idle sessions end, memory's nightly run (docs/daemon.md §3.6)
        let hub = hub.clone();
        std::thread::Builder::new().name("sill-daemon-sweep".into()).spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
                hub.sweep();
            }
        })?;
    }
    {
        // the watchdog (docs/daemon.md §3.7): a lock nobody can take within the patience is a
        // deadlock, and a deadlocked daemon is worse than a dead one — it leaves, the unit or the
        // next client starts a fresh one and the sessions are adopted. While every lock answers,
        // systemd hears `WATCHDOG=1`; a daemon too wedged to run even this thread is killed by
        // `WatchdogSec=` in the unit.
        let hub = hub.clone();
        let patience = std::env::var("SILL_WATCHDOG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(LOCK_PATIENCE_SECS);
        let patience = std::time::Duration::from_secs(patience.max(1));
        std::thread::Builder::new().name("sill-daemon-watchdog".into()).spawn(move || {
            loop {
                std::thread::sleep(patience / 2);
                match hub.health(patience) {
                    Ok(()) => notify("WATCHDOG=1"),
                    Err(lock) => {
                        eprintln!("sill daemon: the {lock} lock has been held for {} s: stuck; leaving so the next daemon starts clean (sessions keep running)", patience.as_secs());
                        std::process::exit(EXIT_STUCK);
                    }
                }
            }
        })?;
    }
    let dir = sill_proto::runtime_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("daemon.pid"), std::process::id().to_string())?;
    notify("READY=1");
    eprintln!("sill daemon: {}, pid {}, {} sessions in the store, protocol {}", mode, std::process::id(), hub.session_count(), sill_proto::PROTOCOL);
    {
        // recovery after READY, on a thread (docs/daemon.md §3.9 item 3): reconnecting a relay can
        // take seconds each, and the unit's start timeout must not depend on how many there are
        let hub = hub.clone();
        std::thread::Builder::new().name("sill-daemon-recover".into()).spawn(move || hub.recover())?;
    }
    let me = rustix::process::geteuid();
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("sill daemon: accept: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        // the socket is 0600 in a 0700 dir; this is the belt to that: only the person's own processes
        match rustix::net::sockopt::socket_peercred(&conn) {
            Ok(cred) if cred.uid == me => {}
            Ok(cred) => {
                eprintln!("sill daemon: refused a connection from uid {}", cred.uid.as_raw());
                continue;
            }
            Err(e) => {
                eprintln!("sill daemon: peer credentials: {e}");
                continue;
            }
        }
        // every connection is a thread: a bound keeps a runaway client from exhausting them
        let Some(slot) = ConnSlot::take() else {
            eprintln!("sill daemon: refused a connection: {MAX_CONNECTIONS} already open");
            continue;
        };
        let hub = hub.clone();
        if let Err(e) = std::thread::Builder::new().name("sill-daemon-conn".into()).spawn(move || {
            let _slot = slot;
            if let Err(e) = serve(conn, hub) {
                eprintln!("sill daemon: connection: {e:#}");
            }
        }) {
            eprintln!("sill daemon: thread: {e}");
        }
    }
    Ok(())
}

/// Open connections at once, control and byte streams together.
const MAX_CONNECTIONS: usize = 256;
static OPEN_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// One of [`MAX_CONNECTIONS`], given back when the connection's thread ends.
struct ConnSlot;
impl ConnSlot {
    fn take() -> Option<Self> {
        use std::sync::atomic::Ordering;
        OPEN_CONNECTIONS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_CONNECTIONS).then_some(n + 1)).ok().map(|_| ConnSlot)
    }
}
impl Drop for ConnSlot {
    fn drop(&mut self) { OPEN_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel); }
}

/// `sd_notify` without libsystemd: a datagram to `$NOTIFY_SOCKET` (a path, or abstract with a
/// leading `@`) when the unit is `Type=notify`; nothing otherwise. Never an error: a daemon that
/// cannot reach its manager still serves.
fn notify(state: &str) {
    let Some(addr) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let addr = addr.to_string_lossy();
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let sent = match addr.strip_prefix('@') {
        Some(abstract_name) => {
            use std::os::linux::net::SocketAddrExt;
            std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()).and_then(|a| sock.send_to_addr(state.as_bytes(), &a))
        }
        None => sock.send_to(state.as_bytes(), &*addr),
    };
    if let Err(e) = sent {
        eprintln!("sill daemon: notify {}: {e}", state.lines().next().unwrap_or_default());
    }
}

/// A panic anywhere in the daemon ends the whole process (docs/daemon.md §3.11): a connection
/// thread that panics would otherwise leave its runner subscriptions and writer thread behind, and
/// a panic under one of the hub's locks poisons it, so every later request panics too — a daemon
/// that answers the socket and does nothing. A dead daemon is the case the design handles: the
/// sessions are scopes of their own, `Restart=on-failure` (or the next client) starts a new one
/// and it adopts them. Installed only here — the shelf and the CLI share this binary and keep
/// Rust's default. `exit`, not `abort`: no core dump of the person's conversations, and a non-zero
/// status is what the unit restarts on.
fn exit_on_panic() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        eprintln!("sill daemon: panic in thread {}{at}: {msg}; leaving so the next daemon starts clean (sessions keep running)", thread.name().unwrap_or("?"));
        std::process::exit(101);
    }));
}

/// An exclusive lock beside `sill.db` for as long as this daemon runs (released by the kernel when
/// it exits, a crash included). A second daemon on the same store — started with another
/// `$XDG_RUNTIME_DIR`, say, so its socket did not collide — refuses to start.
fn lock_store() -> Result<std::fs::File> {
    let db = sill_core::store::default_path()?;
    let dir = db.parent().context("store path")?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("daemon.lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).with_context(|| format!("open {}", path.display()))?;
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = f.set_len(0);
            let _ = std::io::Write::write_all(&mut &f, format!("{}\n", std::process::id()).as_bytes());
            Ok(f)
        }
        Err(rustix::io::Errno::WOULDBLOCK) => {
            let holder = std::fs::read_to_string(&path).unwrap_or_default();
            bail!("another sill daemon (pid {}) already uses {}", holder.trim(), db.display())
        }
        Err(e) => Err(e).with_context(|| format!("lock {}", path.display())),
    }
}

/// systemd's socket, when this process was started by it: fd 3, `LISTEN_PID` ours.
fn activated_listener() -> Result<Option<UnixListener>> {
    let pid_ok = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let fds: u32 = std::env::var("LISTEN_FDS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    if !pid_ok || fds == 0 {
        return Ok(None);
    }
    if fds > 1 {
        bail!("sill.socket passed {fds} descriptors; one listening socket is expected");
    }
    // SAFETY: systemd put a listening socket on fd 3 before exec, and nothing else owns it.
    let l = unsafe { UnixListener::from_raw_fd(3) };
    // systemd passes it without CLOEXEC: every relay and host shell would inherit the listener
    // (docs/daemon.md §3.9 item 4)
    rustix::io::fcntl_setfd(&l, rustix::io::FdFlags::CLOEXEC).context("CLOEXEC on the activated socket")?;
    Ok(Some(l))
}

/// Our own socket at `socket_path`: refuses to replace a daemon that answers, replaces a stale file.
fn bind_direct() -> Result<UnixListener> {
    let path = sill_proto::socket_path()?;
    let dir = path.parent().context("socket path")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("a sill daemon already answers at {}", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let l = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(l)
}

/// One connection: requests answered in order, notifications from the client ignored.
fn serve(stream: UnixStream, hub: Arc<Hub>) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(sill_proto::initialize::INITIALIZE_TIMEOUT_SECONDS)))?;
    let mut reader = BufReader::new(stream);
    let Some(first) = sill_proto::initialize::read_frame(&mut reader, sill_proto::initialize::MAX_MESSAGE_BYTES)? else { return Ok(()) };
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&first)
        && let Some(token) = value.get("_sill_stream").and_then(|v| v.as_str())
    { reader.get_ref().set_read_timeout(None)?; return streams::claim(token, reader); }
    let stream = reader.get_ref();
    let (tx, rx) = sync_channel::<WriteItem>(QUEUE);
    let conn = Arc::new(Conn { id: NEXT_CONN.fetch_add(1, Ordering::Relaxed), internal: AtomicBool::new(false), tx, sock: stream.try_clone()? });
    let mut writer = stream.try_clone()?;
    std::thread::Builder::new().name(format!("sill-daemon-write-{}", conn.id)).spawn(move || {
        for item in rx {
            match item {
                WriteItem::Line(mut line) => {
                    line.push('\n');
                    if writer.write_all(line.as_bytes()).is_err() { let _ = writer.shutdown(std::net::Shutdown::Both); break; }
                }
                WriteItem::Flush(done) => { if writer.flush().is_ok() { let _ = done.send(()); } }
            }
        }
    })?;
    let r = read_loop(reader, first, &hub, &conn);
    hub.disconnect(conn.id);
    r
}

fn read_loop(mut reader: BufReader<UnixStream>, first: String, hub: &Arc<Hub>, conn: &Arc<Conn>) -> Result<()> {
    let mut first = Some(first);
    let mut state = sill_proto::initialize::ConnectionState::default();
    let handshake_deadline = std::time::Instant::now() + std::time::Duration::from_secs(sill_proto::initialize::INITIALIZE_TIMEOUT_SECONDS);
    loop {
        if !state.is_initialized() && std::time::Instant::now() >= handshake_deadline { break; }
        let frame = match first.take() {
            Some(line) => Ok(Some(line)),
            None => sill_proto::initialize::read_frame(&mut reader, sill_proto::initialize::MAX_MESSAGE_BYTES),
        };
        let line = match frame {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Incoming = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                conn.reply(&Response::err(None, Error::new(Error::PARSE, e.to_string())));
                continue;
            }
        };
        let (Some(id), Some(method)) = (msg.id, msg.method.as_deref()) else {
            continue; // a notification, or a stray response
        };
        if method == sill_proto::initialize::METHOD {
            let result = serde_json::from_value(msg.params.unwrap_or(serde_json::Value::Null))
                .map_err(Error::invalid_params)
                .and_then(|params| state.initialize(params, env!("CARGO_PKG_VERSION")));
            match result {
                Ok(initialized) => {
                    reader.get_ref().set_read_timeout(None)?;
                    let internal=initialized.capabilities.get(sill_proto::initialize::INTERNAL_CAPABILITY)==Some(&1);
                    conn.internal.store(internal,Ordering::Release);
                    conn.reply(&Response::ok(id, serde_json::to_value(initialized)?));
                    if internal { hub.connected(conn); }
                }
                Err(error) => conn.reply(&Response::err(Some(id), error)),
            }
            continue;
        }
        if let Err(error) = state.require_initialized() {
            conn.reply(&Response::err(Some(id), error));
            continue;
        }
        if method.starts_with("_sill/internal/") && !conn.internal.load(Ordering::Acquire) {
            conn.reply(&Response::err(Some(id), Error::domain("unsupported", "method requires the sill.internal capability")));
            continue;
        }
        if hub.sfp_request(conn, id, method, msg.params.clone().unwrap_or(serde_json::Value::Null))? { continue; }
        // debug builds only: how tests/daemon.rs sees the panic hook end the process
        #[cfg(debug_assertions)]
        if method == "_sill/daemon.panic" {
            panic!("asked to by a client (tests/daemon.rs)");
        }
        // debug builds only: how tests/daemon.rs sees the watchdog end a stuck daemon
        #[cfg(debug_assertions)]
        if method == "_sill/daemon.deadlock" {
            hub.hold_runners_forever();
        }
        let reply = match hub.dispatch(conn, method, msg.params.unwrap_or(serde_json::Value::Null)) {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(Some(id), e),
        };
        conn.reply(&reply);
        if method == sill_proto::method::DAEMON_SHUTDOWN {
            eprintln!("sill daemon: shutdown asked, leaving");
            notify("STOPPING=1");
            // a clean exit leaves no pid file; a crash does, and the next start replaces it
            if let Ok(dir) = sill_proto::runtime_dir() {
                let _ = std::fs::remove_file(dir.join("daemon.pid"));
            }
            if let Err(e) = conn.drain() { eprintln!("sill daemon: {e:#}"); }
            std::process::exit(0);
        }
    }
    Ok(())
}
