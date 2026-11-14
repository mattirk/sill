//! `sill daemon` (docs/daemon.md): sill-core behind a socket — the session processes (M1a), the ACP
//! client of every running session and its transcript (M1b, `runner`), and the read-only methods.
//!
//! One reader thread per connection, a line in, a line out (`sill_proto::rpc`), and one writer
//! thread draining the connection's queue: replies and the sessions' events share it, so an
//! attached frontend gets a session's state before the answer to its `attach`. The queue is bounded
//! (decision 15): a frontend that stops reading is dropped and attaches again with its cursor.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use anyhow::{Context, Result, bail};
use sill_proto::rpc::{Error, Incoming, Response};

mod hub;
mod runner;
pub use hub::Hub;

/// Lines a connection may have waiting before it counts as stuck (decision 15): a streaming turn
/// is a few hundred events a second at most.
const QUEUE: usize = 20_000;

/// One frontend connection, as the hub and the runners see it: where its lines go.
pub struct Conn {
    pub id: u64,
    tx: SyncSender<String>,
    /// To cut a stuck connection off.
    sock: UnixStream,
}

impl Conn {
    /// Queue one line; false when the connection is gone or stuck (it is cut off then).
    pub fn send(&self, line: String) -> bool {
        match self.tx.try_send(line) {
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
    let dir = sill_proto::runtime_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("daemon.pid"), std::process::id().to_string())?;
    eprintln!("sill daemon: {}, pid {}, {} sessions in the store, protocol {}", mode, std::process::id(), hub.session_count(), sill_proto::PROTOCOL);
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
        let hub = hub.clone();
        if let Err(e) = std::thread::Builder::new().name("sill-daemon-conn".into()).spawn(move || {
            if let Err(e) = serve(conn, hub) {
                eprintln!("sill daemon: connection: {e:#}");
            }
        }) {
            eprintln!("sill daemon: thread: {e}");
        }
    }
    Ok(())
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
    Ok(Some(unsafe { UnixListener::from_raw_fd(3) }))
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
    let (tx, rx) = sync_channel::<String>(QUEUE);
    let conn = Arc::new(Conn { id: NEXT_CONN.fetch_add(1, Ordering::Relaxed), tx, sock: stream.try_clone()? });
    hub.connected(&conn);
    let mut writer = stream.try_clone()?;
    std::thread::Builder::new().name(format!("sill-daemon-write-{}", conn.id)).spawn(move || {
        for mut line in rx {
            line.push('\n');
            if writer.write_all(line.as_bytes()).is_err() {
                let _ = writer.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
    })?;
    let r = read_loop(stream, &hub, &conn);
    hub.disconnect(conn.id);
    r
}

fn read_loop(stream: UnixStream, hub: &Arc<Hub>, conn: &Arc<Conn>) -> Result<()> {
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next() {
        let line = match line {
            Ok(l) => l,
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
        let reply = match hub.dispatch(conn, method, msg.params.unwrap_or(serde_json::Value::Null)) {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(Some(id), e),
        };
        conn.reply(&reply);
        if method == sill_proto::method::DAEMON_SHUTDOWN {
            eprintln!("sill daemon: shutdown asked, leaving");
            std::thread::sleep(std::time::Duration::from_millis(200)); // the writer sends the reply
            std::process::exit(0);
        }
    }
    Ok(())
}
