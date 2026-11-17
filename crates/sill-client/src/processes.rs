//! Managed processes (docs/tasks.md §2.4, §2.5): what an agent started with `sill-proc` in its
//! jail, read from the daemon, and exposed ports — a managed process listening on a port inside
//! the jail, reachable from the person's browser on this machine's loopback. A [`Bridge`] listens
//! on `127.0.0.1:<port>` and relays every connection through the daemon's byte stream
//! (`_sill/streams.port`), which connects to `127.0.0.1:<port>` inside, only for a port a
//! running managed process declared. Nothing new becomes reachable from inside the jail; the
//! bridge lives as long as its value (the frontend drops it when the process ends).
use sill_proto::method;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};
use sill_proto::Client;
pub use sill_proto::admin::Process;
use sill_proto::admin::{JailProcesses, PolicyEntry, PolicyKey, PolicySnapshot, ProcessesList, ProcessesPage};
use sill_proto::sessions::SessionKey;

use crate::actions::Actions;

/// Lines of output a log view asks for.
pub const LOG_LINES: u16 = 500;

/// A session's managed processes, every page, in name order.
pub fn list(client: &Client, session: &str) -> Result<Vec<Process>> {
    let mut all = vec![];
    let mut after = None;
    loop {
        let page: ProcessesPage = client.call(method::PROCS_LIST, ProcessesList { session_id: session.into(), after, limit: Some(128) })?;
        all.extend(page.processes);
        after = page.next;
        if after.is_none() {
            return Ok(all);
        }
    }
}

/// What runs in the session's jail now (docs/jail-processes.md); nothing from a daemon that
/// cannot say.
pub fn jail(client: &Client, session: &str) -> Result<JailProcesses> {
    if !client.has_capability("sill.jail_processes") {
        return Ok(JailProcesses::default());
    }
    client.call(method::PROCS_RUNNING, SessionKey { session_id: session.into() })
}

pub fn running(p: &Process) -> bool {
    p.state == "running"
}

/// Ended on its own with a nonzero code.
pub fn failed(p: &Process) -> bool {
    p.state == "exited" && p.exit_code.is_some_and(|c| c != 0)
}

/// What a row says of its process: `running · 3m12s`, `exited`, `failed (2)`, `stopped`.
pub fn state(p: &Process, now: u64) -> String {
    match (running(p), p.exit_code) {
        (true, _) => {
            let started = p.started.parse::<u64>().unwrap_or(now);
            format!("running · {}", uptime(now.saturating_sub(started)))
        }
        (_, Some(0)) => "exited".into(),
        (_, Some(code)) => format!("failed ({code})"),
        _ => "stopped".into(),
    }
}

/// `3m12s`, `1h04m`, `2d`.
pub fn uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d", s / 86400),
    }
}

/// The ports a policy lets be exposed without asking: its `[processes] expose` list (`"3000"`,
/// `"3000-3999"`). A policy that does not parse allows none.
pub fn exposable(policy: &str) -> Vec<String> {
    let Ok(table) = policy.parse::<toml::Table>() else {
        return vec![];
    };
    table
        .get("processes")
        .and_then(|p| p.get("expose"))
        .and_then(|e| e.as_array())
        .map(|specs| specs.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// `port` is in one of the policy's `expose` specs (sill-core's `Processes::allows`).
pub fn allows(specs: &[String], port: u16) -> bool {
    specs.iter().any(|spec| match spec.split_once('-') {
        Some((a, b)) => matches!(
            (a.trim().parse::<u16>(), b.trim().parse::<u16>()),
            (Ok(a), Ok(b)) if a <= port && port <= b
        ),
        None => spec.trim().parse::<u16>().ok() == Some(port),
    })
}

/// The `expose` specs of the rules the session was actually launched with: its retained launch
/// snapshot, or its named policy when an older session has none. Automatic exposure must use what
/// is enforced, so a failed read is an error, which grants nothing.
pub fn session_exposable(client: &Client, session: &str, policy: &str) -> Result<Vec<String>> {
    let snapshot: PolicySnapshot = client.call(method::POLICY_SNAPSHOT, SessionKey { session_id: session.into() })?;
    let text = match snapshot.snapshot.and_then(|view| view.text) {
        Some(text) => text,
        None => {
            let entry: PolicyEntry = client.call(method::POLICY_GET, PolicyKey { name: policy.into() })?;
            entry.text
        }
    };
    Ok(exposable(&text))
}

/// A port inside a session's jail, listening on this machine's loopback.
pub struct Bridge {
    /// Where it is here: the jail's number when free, else one the kernel picked.
    pub host_port: u16,
    stop: Arc<AtomicBool>,
    active: Arc<Mutex<Vec<Weak<Connection>>>>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge").field("host_port", &self.host_port).finish()
    }
}

struct Connection {
    socket: std::net::TcpStream,
    closed: AtomicBool,
    interrupt: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Connection {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(interrupt) = self.interrupt.lock().unwrap().take() {
            interrupt();
        }
    }

    /// A bridge may be dropped while the daemon is still opening its stream.
    fn arm(&self, interrupt: Arc<dyn Fn() + Send + Sync>) -> bool {
        let mut slot = self.interrupt.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            interrupt();
            return false;
        }
        *slot = Some(interrupt);
        true
    }
}

impl Bridge {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let open: Vec<_> = self.active.lock().unwrap().drain(..).collect();
        for connection in open.iter().filter_map(Weak::upgrade) {
            connection.close();
        }
    }
}

/// Opens one byte stream to a port inside the session's jail.
pub type Opener = Arc<dyn Fn() -> Result<sill_proto::transport::RawStream> + Send + Sync>;

/// The daemon's stream to `port` in `session`'s jail, asked once per connection (a refused or
/// lost request is not repeated: the browser retries).
pub fn opener(actions: Arc<Actions>, session: String, port: u16) -> Opener {
    Arc::new(move || {
        let opened: sill_proto::host::HostStream =
            actions.once(|client| client.call(method::STREAMS_PORT, sill_proto::host::PortOpen { session_id: session.clone(), port }))?;
        sill_proto::transport::open_stream(&opened.token)
    })
}

/// Listen on this machine's loopback for `port` inside the jail, each connection relayed through
/// a stream from `open`.
pub fn expose(port: u16, open: Opener) -> Result<Bridge> {
    expose_on(port, port, open)
}

/// Listen on exactly `port` of this machine's loopback, each connection relayed through a
/// stream from `open`: for a port that cannot move, such as a login's registered callback.
pub fn expose_exact(port: u16, open: Opener) -> Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("port {port} on this machine is in use"))?;
    serve(listener, port, open)
}

fn expose_on(port: u16, preferred: u16, open: Opener) -> Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", preferred)).or_else(|_| TcpListener::bind(("127.0.0.1", 0))).context("listen on this machine's loopback")?;
    serve(listener, port, open)
}

fn serve(listener: TcpListener, port: u16, open: Opener) -> Result<Bridge> {
    let host_port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let active: Arc<Mutex<Vec<Weak<Connection>>>> = Arc::default();
    let connections = active.clone();
    std::thread::Builder::new().name(format!("sill-port-{port}")).spawn(move || {
        while !flag.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((conn, _)) => {
                    let (flag, connections, open) = (flag.clone(), connections.clone(), open.clone());
                    std::thread::spawn(move || {
                        let _ = conn.set_nonblocking(false);
                        relay(conn, &open, &flag, &connections);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(_) => break,
            }
        }
    })?;
    Ok(Bridge { host_port, stop, active })
}

/// One browser connection ⇄ the daemon ⇄ the process inside.
fn relay(host: std::net::TcpStream, open: &Opener, stopped: &AtomicBool, active: &Mutex<Vec<Weak<Connection>>>) {
    let Ok(socket) = host.try_clone() else { return };
    let connection = Arc::new(Connection { socket, closed: AtomicBool::new(false), interrupt: Mutex::new(None) });
    {
        let mut all = active.lock().unwrap();
        all.retain(|c| c.strong_count() > 0);
        all.push(Arc::downgrade(&connection));
    }
    if stopped.load(Ordering::Acquire) {
        connection.close();
        return;
    }
    let Ok(stream) = open() else { return };
    if !connection.arm(stream.interrupt.clone()) {
        return;
    }
    let Ok(mut host_r) = host.try_clone() else {
        connection.close();
        return;
    };
    let mut host_w = host;
    let mut remote_r = stream.reader;
    let mut remote_w = stream.writer;
    let half_close = stream.shutdown_write;
    let input = std::thread::spawn(move || {
        let _ = std::io::copy(&mut host_r, &mut remote_w);
        half_close();
    });
    let _ = std::io::copy(&mut remote_r, &mut host_w);
    connection.close();
    let _ = input.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn dropping_a_bridge_closes_a_browser_while_its_stream_is_still_opening() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (entered, opening) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let wait = Mutex::new(wait);
        let (interrupted, closed) = mpsc::channel();
        let open: Opener = Arc::new(move || {
            entered.send(()).unwrap();
            wait.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            let interrupted = interrupted.clone();
            Ok(sill_proto::transport::RawStream {
                reader: Box::new(std::io::empty()),
                writer: Box::new(std::io::sink()),
                shutdown_write: Arc::new(|| {}),
                interrupt: Arc::new(move || {
                    let _ = interrupted.send(());
                }),
            })
        });
        let bridge = expose(0, open).unwrap();
        let mut browser = std::net::TcpStream::connect(("127.0.0.1", bridge.host_port)).unwrap();
        browser.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        opening.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(bridge);
        assert_eq!(browser.read(&mut [0u8; 1]).unwrap(), 0, "browser closes before the daemon answers");
        release.send(()).unwrap();
        closed.recv_timeout(Duration::from_secs(5)).expect("late stream is interrupted too");
    }

    #[test]
    fn uptime_reads_naturally() {
        assert_eq!(uptime(42), "42s");
        assert_eq!(uptime(192), "3m12s");
        assert_eq!(uptime(3840), "1h04m");
        assert_eq!(uptime(200_000), "2d");
    }

    #[test]
    fn the_policy_s_expose_list_decides_which_ports_go_without_asking() {
        let specs = exposable("[network]\nallow = []\n[processes]\nexpose = [\"3000\", \"8000-8099\"]\n");
        assert!(allows(&specs, 3000));
        assert!(allows(&specs, 8042));
        assert!(!allows(&specs, 3001));
        assert!(!allows(&specs, 8100));
        assert!(exposable("[processes]\n").is_empty());
        assert!(exposable("not toml [").is_empty(), "an unreadable policy grants nothing");
        assert!(!allows(&["junk".into(), "9-x".into()], 9));
    }

    fn process(state: &str, exit_code: Option<i64>, started: u64) -> Process {
        Process {
            name: "web".into(),
            argv: vec!["npm".into(), "run".into(), "dev".into()],
            cwd: sill_proto::sessions::HostPath::from_path(std::path::Path::new("/tmp")),
            port: Some(3000),
            pid: 7,
            location: "jail".into(),
            kept: false,
            started: started.to_string(),
            state: state.into(),
            exit_code,
            last_line: String::new(),
        }
    }

    #[test]
    fn a_row_says_how_its_process_is() {
        assert_eq!(state(&process("running", None, 100), 292), "running · 3m12s");
        assert_eq!(state(&process("exited", Some(0), 0), 9), "exited");
        assert_eq!(state(&process("exited", Some(2), 0), 9), "failed (2)");
        assert_eq!(state(&process("stopped", None, 0), 9), "stopped");
        assert!(failed(&process("exited", Some(2), 0)));
        assert!(!failed(&process("exited", Some(0), 0)));
        assert!(!failed(&process("running", None, 0)));
    }

    /// An echo "inside the jail" behind a pair of pipes stands for the daemon's stream: bytes go
    /// both ways, and dropping the bridge closes the browser's connection.
    #[test]
    fn a_bridge_relays_connections_and_closes_them_when_dropped() {
        let inside = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let inside_port = inside.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in inside.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut r = conn.try_clone().unwrap();
                    let mut w = conn;
                    let _ = std::io::copy(&mut r, &mut w);
                });
            }
        });
        let open: Opener = Arc::new(move || {
            let conn = std::net::TcpStream::connect(("127.0.0.1", inside_port))?;
            let (reader, writer, half, stop) = (conn.try_clone()?, conn.try_clone()?, conn.try_clone()?, conn);
            Ok(sill_proto::transport::RawStream {
                reader: Box::new(reader),
                writer: Box::new(writer),
                shutdown_write: Arc::new(move || {
                    let _ = half.shutdown(std::net::Shutdown::Write);
                }),
                interrupt: Arc::new(move || {
                    let _ = stop.shutdown(std::net::Shutdown::Both);
                }),
            })
        });
        // a taken port falls back to one the kernel picks
        let taken = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let wanted = taken.local_addr().unwrap().port();
        let bridge = expose_on(wanted, wanted, open).unwrap();
        assert_ne!(bridge.host_port, wanted);
        assert_eq!(bridge.url(), format!("http://127.0.0.1:{}", bridge.host_port));
        let mut browser = std::net::TcpStream::connect(("127.0.0.1", bridge.host_port)).unwrap();
        browser.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        browser.write_all(b"hello").unwrap();
        let mut got = [0u8; 5];
        browser.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        drop(bridge);
        let mut rest = vec![];
        assert_eq!(browser.read_to_end(&mut rest).unwrap_or(0), 0, "closed with the bridge");
    }
}
