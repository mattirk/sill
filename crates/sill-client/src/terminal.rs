//! A session's jail shell: the terminal the daemon keeps in the jail (docs/protocol.md, terminals),
//! under the label the shelf uses, so either frontend finds the shell the other one opened. Its
//! bytes come and go over the daemon's byte stream, never a local pty; the emulator is the
//! frontend's.
//!
//! A [`Link`] owns one attached stream. Dropping it detaches — the stream closes and both its
//! threads leave, even if the connection is still being made — and the shell keeps running, to be
//! found again. A terminal takes at most eight viewers, so a link that outlived its pane would
//! eventually lock the person out of their own shell.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use sill_proto::terminals as api;
use sill_proto::transport::RawStream;

/// The shelf's name for a session's plain shell, shared so the two frontends find each other's.
pub const LABEL: &str = "shell";

/// The requests that steer a terminal besides its bytes.
pub enum Control {
    Resize(u16, u16),
    /// End the shell itself, not only this view of it.
    Close,
}

/// An attached terminal: its byte stream, and the requests that steer it.
pub struct Connection {
    /// Older output was dropped before this attach; the screen starts partway.
    pub discarded: bool,
    /// The terminal's size when its recent output was drawn: read the replay at that size.
    pub size: (u16, u16),
    pub stream: RawStream,
    pub control: Box<dyn FnMut(Control) -> anyhow::Result<()> + Send>,
}

/// Where the output goes: the frontend's emulator. Called on the link's reading thread.
pub trait Screen: Send + 'static {
    /// Before any output: the size the replay was drawn at, and whether earlier output is gone.
    fn begin(&mut self, cols: u16, rows: u16, discarded: bool);
    /// Bytes from the shell. False stops reading.
    fn output(&mut self, bytes: &[u8]) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Opened,
    Failed(String),
    /// The stream ended: the shell exited or was closed, or this viewer was cut off.
    Ended,
}

enum Op {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
    Detach,
}

/// Stops a link from any thread, before or after its stream exists.
#[derive(Default)]
struct Cancel {
    done: AtomicBool,
    interrupt: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Cancel {
    fn trigger(&self) {
        self.done.store(true, Ordering::Release);
        if let Some(interrupt) = self.interrupt.lock().unwrap().take() {
            interrupt();
        }
    }

    /// The stream exists: interrupt it now if the link was already dropped, else on drop.
    fn arm(&self, interrupt: Arc<dyn Fn() + Send + Sync>) -> bool {
        let mut slot = self.interrupt.lock().unwrap();
        if self.done.load(Ordering::Acquire) {
            interrupt();
            return false;
        }
        *slot = Some(interrupt);
        true
    }

    fn cancelled(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Bytes to the shell that do not keep the link alive: the emulator's own answers (device
/// attributes, colour queries) go through one of these.
#[derive(Clone)]
pub struct Replies(mpsc::Sender<Op>);

impl Replies {
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.0.send(Op::Input(bytes));
    }
}

pub struct Link {
    ops: mpsc::Sender<Op>,
    cancel: Arc<Cancel>,
}

impl Link {
    /// Connect off the caller's thread with `connect` (cols, rows of the pane), feed `screen`,
    /// report to `events`. Returns at once.
    pub fn open(
        cols: u16,
        rows: u16,
        connect: impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static,
        screen: impl Screen,
        events: impl Fn(Event) -> bool + Send + Sync + 'static,
    ) -> Self {
        let (ops, commands) = mpsc::channel();
        let cancel = Arc::new(Cancel::default());
        let stop = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("sill-terminal".into())
            .spawn(move || match connect(cols, rows) {
                Err(error) => {
                    if !stop.cancelled() {
                        events(Event::Failed(format!("{error:#}")));
                    }
                }
                Ok(connection) => run(connection, screen, &stop, commands, events),
            });
        if let Err(error) = spawned {
            eprintln!("sill-client: starting a terminal: {error}");
        }
        Self { ops, cancel }
    }

    pub fn replies(&self) -> Replies {
        Replies(self.ops.clone())
    }

    pub fn input(&self, bytes: Vec<u8>) {
        let _ = self.ops.send(Op::Input(bytes));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.ops.send(Op::Resize(cols, rows));
    }

    /// End the shell itself; the link then reports that it ended.
    pub fn close(&self) {
        let _ = self.ops.send(Op::Close);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.ops.send(Op::Detach);
        self.cancel.trigger();
    }
}

/// Bytes both ways until the stream ends or the link goes.
fn run(
    connection: Connection,
    mut screen: impl Screen,
    cancel: &Cancel,
    commands: mpsc::Receiver<Op>,
    events: impl Fn(Event) -> bool + Send + Sync + 'static,
) {
    let Connection {
        discarded,
        size: (cols, rows),
        stream,
        mut control,
    } = connection;
    let RawStream {
        mut reader,
        mut writer,
        interrupt,
        ..
    } = stream;
    if !cancel.arm(interrupt.clone()) {
        return;
    }
    screen.begin(cols, rows, discarded);
    let events = Arc::new(events);
    if !events(Event::Opened) {
        interrupt();
        return;
    }
    let ended = events.clone();
    let stop = interrupt.clone();
    let reading = std::thread::Builder::new()
        .name("sill-terminal-output".into())
        .spawn(move || {
            let mut bytes = [0u8; 32 * 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !screen.output(&bytes[..n]) {
                            break;
                        }
                    }
                }
            }
            ended(Event::Ended);
            stop();
        });
    if reading.is_err() {
        events(Event::Failed("could not read the terminal".into()));
        interrupt();
        return;
    }
    while let Ok(op) = commands.recv() {
        match op {
            Op::Input(bytes) => {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            Op::Resize(cols, rows) => {
                if let Err(error) = control(Control::Resize(cols, rows)) {
                    eprintln!("sill-client: resizing the terminal: {error:#}");
                }
            }
            Op::Close => {
                if let Err(error) = control(Control::Close) {
                    events(Event::Failed(format!("{error:#}")));
                }
                break;
            }
            Op::Detach => break,
        }
    }
    interrupt();
}

/// The session's jail shell over a connection of its own: the running one under [`LABEL`] when
/// there is one (the shelf's, or this frontend's from before), else a new login shell.
pub fn jail_shell(
    connect: crate::Connector,
    session_id: String,
) -> impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static {
    jail_terminal(connect, session_id, LABEL, vec![], BTreeMap::new())
}

/// A terminal in the session's jail running `argv` (empty: the login shell) with `env` added,
/// under `label`: the running one of that label when there is one, else a new one.
pub fn jail_terminal(
    connect: crate::Connector,
    session_id: String,
    label: &'static str,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
) -> impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static {
    named_terminal(connect, Some(session_id), label.into(), argv, env, None)
}

/// A host shell owned by the daemon, sharing the GTK shelf's per-session label.
pub fn host_shell(
    connect: crate::Connector,
    session_id: String,
    cwd: Option<sill_proto::sessions::HostPath>,
) -> impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static {
    named_terminal(
        connect,
        None,
        format!("host:{session_id}"),
        vec![],
        [
            ("SILL_HOST_SHELL".into(), "1".into()),
            (
                "GIT_CONFIG_PARAMETERS".into(),
                "'core.fsmonitor=false' 'core.useBuiltinFSMonitor=false'".into(),
            ),
        ]
        .into(),
        cwd,
    )
}

fn named_terminal(
    connect: crate::Connector,
    session_id: Option<String>,
    label: String,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<sill_proto::sessions::HostPath>,
) -> impl FnOnce(u16, u16) -> anyhow::Result<Connection> + Send + 'static {
    move |cols, rows| {
        let client = connect()?;
        let listed: api::Terminals = client.call(
            "_sill/terminals.list",
            api::List {
                session_id: session_id.clone(),
            },
        )?;
        let found = listed
            .terminals
            .into_iter()
            .rev()
            .find(|t| t.session_id == session_id && t.label == label && t.state == "running");
        let terminal = match found {
            Some(t) => t,
            None => client.call(
                "_sill/terminals.open",
                api::Open {
                    session_id,
                    label: label.into(),
                    argv,
                    // the frontend emulates xterm, whatever the jail's default is
                    env: [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .chain(env)
                        .collect(),
                    cwd,
                    cols,
                    rows,
                },
            )?,
        };
        let id = terminal.terminal_id.clone();
        let key = |id: &str| api::TerminalKey {
            terminal_id: id.to_string(),
        };
        let stream: api::Stream = client.call("_sill/terminals.attach", key(&id))?;
        let stream = sill_proto::transport::open_stream(&stream.token)?;
        let control = move |control: Control| -> anyhow::Result<()> {
            let _: serde_json::Value = match control {
                Control::Resize(cols, rows) => client.call(
                    "_sill/terminals.resize",
                    api::Resize {
                        terminal_id: id.clone(),
                        cols,
                        rows,
                    },
                )?,
                Control::Close => client.call("_sill/terminals.close", key(&id))?,
            };
            Ok(())
        };
        Ok(Connection {
            discarded: terminal.discarded_bytes != "0",
            size: (terminal.cols, terminal.rows),
            stream,
            control: Box::new(control),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    struct Collect(Arc<Mutex<Vec<u8>>>);
    impl Screen for Collect {
        fn begin(&mut self, _: u16, _: u16, _: bool) {}
        fn output(&mut self, bytes: &[u8]) -> bool {
            self.0.lock().unwrap().extend_from_slice(bytes);
            true
        }
    }

    fn over(socket: UnixStream) -> Connection {
        let close = socket.try_clone().unwrap();
        Connection {
            discarded: false,
            size: (80, 24),
            stream: RawStream {
                reader: Box::new(socket.try_clone().unwrap()),
                writer: Box::new(socket),
                interrupt: Arc::new(move || {
                    let _ = close.shutdown(std::net::Shutdown::Both);
                }),
                shutdown_write: Arc::new(|| {}),
            },
            control: Box::new(|_| Ok(())),
        }
    }

    /// Dropping the link closes its stream, though the emulator's reply handle (held inside the
    /// emulator, which the reading thread shares) is still alive — the leak the review found.
    #[test]
    fn dropping_a_link_detaches_its_stream_while_a_reply_handle_lives() {
        let (ours, mut remote) = UnixStream::pair().unwrap();
        remote
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let link = Link::open(
            80,
            24,
            move |_, _| Ok(over(ours)),
            Collect(seen.clone()),
            move |e| tx.send(e).is_ok(),
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Opened
        );
        remote.write_all(b"hello").unwrap();
        let replies = link.replies();
        link.input(b"ls\r".to_vec());
        let mut typed = [0u8; 3];
        remote.read_exact(&mut typed).unwrap();
        assert_eq!(&typed, b"ls\r");
        drop(link);
        let mut rest = [0u8; 16];
        assert_eq!(remote.read(&mut rest).unwrap(), 0, "the stream must close");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Ended
        );
        replies.send(b"late".to_vec());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !seen.lock().unwrap().starts_with(b"hello") {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A pane dropped while its shell is still being found never attaches it.
    #[test]
    fn a_link_dropped_while_connecting_never_keeps_the_stream() {
        let (ours, mut remote) = UnixStream::pair().unwrap();
        remote
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (go, wait) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        let link = Link::open(
            80,
            24,
            move |_, _| {
                wait.recv().unwrap();
                Ok(over(ours))
            },
            Collect(Arc::default()),
            move |e| tx.send(e).is_ok(),
        );
        drop(link);
        go.send(()).unwrap();
        let mut rest = [0u8; 16];
        assert_eq!(remote.read(&mut rest).unwrap(), 0);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing opened"
        );
    }
}
