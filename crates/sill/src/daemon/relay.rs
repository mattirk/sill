//! A session's ACP connection survives the main daemon process.
//!
//! This small worker owns the SDK's live request futures and question reply channels. It never
//! opens the store. The main daemon commits each delivered event before acknowledging its sequence;
//! reconnecting replays the unacknowledged tail. Commands have stable ids so a daemon may retry its
//! committed outbox after a crash without submitting a second prompt.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sill_core::{acp, wire};

const JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Delivered {
    /// Zero denotes an already-acknowledged open question, replayed as current state.
    pub seq: u64,
    pub event: wire::Event,
}

#[derive(Serialize, Deserialize)]
enum Input {
    Hello { since: u64 },
    Command { id: u64, command: wire::Command },
    Answer { request: u64, answer: wire::Answer },
    Ack { seq: u64 },
}

#[derive(Serialize, Deserialize)]
enum Output {
    Hello { acknowledged: u64, last: u64 },
    Event(Delivered),
    Accepted { id: u64, error: Option<String> },
    Answered { request: u64, error: Option<String> },
}

struct JournalEntry {
    delivered: Delivered,
    bytes: usize,
}

struct Pending {
    event: wire::Event,
    seq: u64,
    reply: wire::Reply,
}

#[derive(Default)]
struct State {
    journal: VecDeque<JournalEntry>,
    bytes: usize,
    next_seq: u64,
    acknowledged: u64,
    next_request: u64,
    pending: BTreeMap<u64, Pending>,
    answered: BTreeSet<u64>,
    last_command: u64,
    generation: u64,
    subscriber: Option<UnixStream>,
    ended_at: Option<u64>,
    stopping: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    commands: async_channel::Sender<acp::Command>,
}

impl Shared {
    fn append_locked(&self, state: &mut State, event: wire::Event) -> u64 {
        state.next_seq += 1;
        let delivered = Delivered { seq: state.next_seq, event };
        // Counting serialized bytes bounds memory by message size, not just event count. One event
        // may exceed the limit: retain it alone rather than losing part of a conversation.
        let bytes = serde_json::to_vec(&delivered).map_or(0, |v| v.len());
        state.bytes += bytes;
        let seq = delivered.seq;
        state.journal.push_back(JournalEntry { delivered, bytes });
        self.changed.notify_all();
        seq
    }

    fn take_event(&self, event: acp::Event) {
        let mut state = self.state.lock().unwrap();
        while state.bytes >= JOURNAL_BYTES && !state.stopping {
            state = self.changed.wait(state).unwrap();
        }
        if state.stopping {
            return;
        }
        state.next_request += 1;
        let request = state.next_request;
        let (event, reply) = wire::event_from_acp(event, request);
        let ended = matches!(event, wire::Event::Ended(_));
        let seq = self.append_locked(&mut state, event.clone());
        if let Some(reply) = reply {
            state.pending.insert(request, Pending { event, seq, reply });
        }
        if ended {
            state.ended_at = Some(seq);
            state.pending.clear();
        }
    }

    fn answer(&self, request: u64, answer: wire::Answer) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.answered.contains(&request) {
            return Ok(());
        }
        let Some(pending) = state.pending.get(&request) else { bail!("no open question {request}") };
        if !matches!((&pending.event, &answer), (wire::Event::PermissionRequest { .. }, wire::Answer::Permission(_)) | (wire::Event::Elicitation { .. }, wire::Answer::Form(_))) {
            bail!("the answer is not of the question's kind");
        }
        let pending = state.pending.remove(&request).unwrap();
        if !pending.reply.send(answer) {
            bail!("the answer is not of the question's kind");
        }
        state.answered.insert(request);
        self.append_locked(&mut state, wire::Event::Resolved { request });
        Ok(())
    }

    fn acknowledge(&self, seq: u64) {
        let mut state = self.state.lock().unwrap();
        let seq = seq.min(state.next_seq);
        state.acknowledged = state.acknowledged.max(seq);
        while state.journal.front().is_some_and(|e| e.delivered.seq <= seq) {
            let old = state.journal.pop_front().unwrap();
            state.bytes -= old.bytes;
        }
        self.changed.notify_all();
    }
}

/// Run the worker against the session's shim. Its lifetime is the ACP conversation plus delivery
/// of its final Ended event; dropping the daemon connection never closes the agent connection.
pub(crate) fn run(session: &str, socket: &Path, agent_socket: &Path, cwd: &Path, resume: Option<String>) -> Result<()> {
    super::exit_on_panic();
    run_with(session, socket, cwd, resume, || sill_core::shim::agent_stream(agent_socket))
}

#[cfg(test)]
fn run_stream(session: &str, socket: &Path, stream: UnixStream, cwd: &Path, resume: Option<String>) -> Result<()> {
    run_with(session, socket, cwd, resume, || Ok(stream))
}

fn run_with(session: &str, socket: &Path, cwd: &Path, resume: Option<String>, open: impl FnOnce() -> Result<UnixStream>) -> Result<()> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(socket.with_extension("lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).context("another ACP relay already owns this session")?;
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("an ACP relay already answers at {}", socket.display());
        }
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let pid_file = socket.with_extension("pid");
    std::fs::write(&pid_file, std::process::id().to_string())?;
    let cleanup = Cleanup { socket: socket.to_path_buf(), pid_file };
    // Claim the worker lock before opening the shim stream: a duplicate relay must never steal
    // the live SDK's agent connection merely while checking whether another worker exists.
    let stream = open()?;
    let (commands, command_rx) = async_channel::unbounded();
    // Backpressure the SDK too: an absent daemon cannot grow an unbounded second queue.
    let (events, event_rx) = async_channel::bounded(256);
    let shared = Arc::new(Shared { state: Mutex::new(State::default()), changed: Condvar::new(), commands });
    let opts = acp::SessionOptions { cwd: cwd.to_path_buf(), auto: acp::AutoPermission::Ask, resume };
    let worker = std::thread::Builder::new().name(format!("sill-relay-acp-{session}")).spawn(move || {
        let _ = acp::run_session(stream, opts, command_rx, events);
    })?;
    let state = shared.clone();
    let collector = std::thread::Builder::new().name(format!("sill-relay-events-{session}")).spawn(move || {
        while let Ok(event) = event_rx.recv_blocking() {
            let ended = matches!(event, acp::Event::Ended(_));
            state.take_event(event);
            if ended {
                break;
            }
        }
    })?;
    let uid = rustix::process::geteuid();
    loop {
        {
            let state = shared.state.lock().unwrap();
            if state.ended_at.is_some_and(|seq| state.acknowledged >= seq) {
                break;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if !rustix::net::sockopt::socket_peercred(&stream).is_ok_and(|cred| cred.uid == uid) {
                    continue;
                }
                let shared = shared.clone();
                std::thread::Builder::new().name(format!("sill-relay-client-{session}")).spawn(move || {
                    if let Err(error) = serve(stream, shared) {
                        eprintln!("sill relay: {error:#}");
                    }
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.into()),
        }
    }
    {
        let mut state = shared.state.lock().unwrap();
        state.stopping = true;
        if let Some(stream) = state.subscriber.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        shared.changed.notify_all();
    }
    let _ = worker.join();
    let _ = collector.join();
    drop(cleanup);
    Ok(())
}

struct Cleanup {
    socket: PathBuf,
    pid_file: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

fn write_line(writer: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    Ok(())
}

fn serve(mut stream: UnixStream, shared: Arc<Shared>) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let Input::Hello { since } = serde_json::from_str(&first)? else { bail!("a relay connection starts with Hello") };
    stream.set_read_timeout(None)?;
    let (generation, pending) = {
        let mut state = shared.state.lock().unwrap();
        if since < state.acknowledged || since > state.next_seq {
            bail!("relay cursor {since} is outside retained range {}..{}", state.acknowledged, state.next_seq);
        }
        if let Some(old) = state.subscriber.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        state.generation += 1;
        state.subscriber = Some(stream.try_clone()?);
        write_line(&mut stream, &Output::Hello { acknowledged: state.acknowledged, last: state.next_seq })?;
        let pending: Vec<_> = state.pending.values().filter(|p| p.seq <= since).map(|p| Delivered { seq: 0, event: p.event.clone() }).collect();
        shared.changed.notify_all();
        (state.generation, pending)
    };
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let output = writer.clone();
    let state = shared.clone();
    let send = std::thread::spawn(move || -> Result<()> {
        for delivered in pending {
            write_line(&mut output.lock().unwrap(), &Output::Event(delivered))?;
        }
        let mut cursor = since;
        loop {
            let next = {
                let mut current = state.state.lock().unwrap();
                loop {
                    if current.generation != generation || current.subscriber.is_none() || current.stopping {
                        return Ok(());
                    }
                    if let Some(entry) = current.journal.iter().find(|e| e.delivered.seq > cursor) {
                        break entry.delivered.clone();
                    }
                    current = state.changed.wait(current).unwrap();
                }
            };
            cursor = next.seq;
            if let Err(error) = write_line(&mut output.lock().unwrap(), &Output::Event(next)) {
                let state = state.state.lock().unwrap();
                if state.generation == generation && let Some(stream) = &state.subscriber {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                return Err(error);
            }
        }
    });
    let result = (|| -> Result<()> {
        for line in reader.lines() {
            let line = line?;
            let input: Input = serde_json::from_str(&line)?;
            if shared.state.lock().unwrap().generation != generation {
                break;
            }
            match input {
                Input::Hello { .. } => bail!("Hello is only valid on a new relay connection"),
                Input::Ack { seq } => shared.acknowledge(seq),
                Input::Command { id, command } => {
                    let error = {
                        let mut state = shared.state.lock().unwrap();
                        if id == 0 {
                            Some("relay command id must be positive".into())
                        } else if id <= state.last_command {
                            None // retry of a committed outbox entry
                        } else if state.ended_at.is_some() {
                            Some("the ACP conversation has ended".into())
                        } else {
                            let (command, _) = wire::command_into_acp(command);
                            match shared.commands.try_send(command) {
                                Ok(()) => { state.last_command = id; None }
                                Err(_) => Some("the ACP conversation has ended".into()),
                            }
                        }
                    };
                    write_line(&mut writer.lock().unwrap(), &Output::Accepted { id, error })?;
                }
                Input::Answer { request, answer } => {
                    let error = shared.answer(request, answer).err().map(|error| error.to_string());
                    write_line(&mut writer.lock().unwrap(), &Output::Answered { request, error })?;
                }
            }
        }
        Ok(())
    })();
    {
        let mut state = shared.state.lock().unwrap();
        if state.generation == generation {
            state.subscriber = None;
            shared.changed.notify_all();
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = send.join();
    result
}

type Waiters = Mutex<BTreeMap<(bool, u64), mpsc::SyncSender<Result<(), String>>>>;

pub(crate) struct Client {
    writer: Mutex<UnixStream>,
    waiters: Waiters,
    closed: AtomicBool,
}

impl Client {
    fn write(&self, value: &Input) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            bail!("the ACP relay connection closed");
        }
        write_line(&mut self.writer.lock().unwrap(), value)
    }

    fn request(&self, key: (bool, u64), value: Input) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.waiters.lock().unwrap().insert(key, tx);
        let result = self.write(&value).and_then(|()| {
            rx.recv_timeout(IO_TIMEOUT).context("the ACP relay did not acknowledge the request")?.map_err(anyhow::Error::msg)
        });
        self.waiters.lock().unwrap().remove(&key);
        result
    }

    pub fn command(&self, id: u64, command: wire::Command) -> Result<()> {
        self.request((false, id), Input::Command { id, command })
    }

    pub fn answer(&self, request: u64, answer: wire::Answer) -> Result<()> {
        self.request((true, request), Input::Answer { request, answer })
    }

    pub fn ack(&self, seq: u64) -> Result<()> {
        self.write(&Input::Ack { seq })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Client {
    fn drop(&mut self) { self.close(); }
}

/// Reconnect at the last sequence committed to the store. The receiver closes on a transport
/// failure; it never manufactures Ended, since the worker and agent may still be running.
pub(super) fn connect(socket: &Path, since: u64) -> Result<(Arc<Client>, mpsc::Receiver<Delivered>)> {
    let mut stream = UnixStream::connect(socket).with_context(|| format!("connect ACP relay {}", socket.display()))?;
    let credentials = rustix::net::sockopt::socket_peercred(&stream)?;
    if credentials.uid != rustix::process::geteuid() {
        bail!("the ACP relay belongs to another user");
    }
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    write_line(&mut stream, &Input::Hello { since })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let Output::Hello { acknowledged, last } = serde_json::from_str(&first).context("the ACP relay did not accept the cursor")? else { bail!("the ACP relay did not accept Hello") };
    if since < acknowledged || since > last {
        bail!("ACP relay cursor is no longer retained");
    }
    stream.set_read_timeout(None)?;
    let client = Arc::new(Client { writer: Mutex::new(stream), waiters: Mutex::new(BTreeMap::new()), closed: AtomicBool::new(false) });
    let receive = Arc::downgrade(&client);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("sill-relay-reader".into()).spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(output) = serde_json::from_str::<Output>(&line) else { break };
            match output {
                Output::Event(event) => { if tx.send(event).is_err() { break } }
                Output::Accepted { id, error } => {
                    if let Some(client) = receive.upgrade()
                        && let Some(tx) = client.waiters.lock().unwrap().remove(&(false, id))
                    {
                        let _ = tx.send(error.map_or(Ok(()), Err));
                    }
                }
                Output::Answered { request, error } => {
                    if let Some(client) = receive.upgrade()
                        && let Some(tx) = client.waiters.lock().unwrap().remove(&(true, request))
                    {
                        let _ = tx.send(error.map_or(Ok(()), Err));
                    }
                }
                Output::Hello { .. } => break,
            }
        }
        if let Some(client) = receive.upgrade() {
            client.closed.store(true, Ordering::Release);
            for (_, waiter) in std::mem::take(&mut *client.waiters.lock().unwrap()) {
                let _ = waiter.send(Err("the ACP relay connection closed".into()));
            }
        }
    })?;
    Ok((client, rx))
}

/// Stop an orphaned worker before its session/runtime is scrubbed. This uses the authenticated
/// private socket, never a pid read from disk. Deletion deliberately discards the buffered events
/// after acknowledging them; otherwise an Ended worker waits forever with ghost data in memory.
pub(super) fn stop_unowned(socket: &Path, cursor: u64) -> Result<()> {
    let (client, events) = match connect(socket, cursor) {
        Ok(connection) => connection,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => return Ok(()),
        Err(error) => return Err(error.context("connecting orphaned ACP worker before deletion")),
    };
    // A reserved terminal command id is greater than every persisted normal command, and remains
    // idempotent if a prior deletion attempt was interrupted after sending it.
    let quit = client.command(u64::MAX, wire::Command::Quit);
    let deadline = std::time::Instant::now() + IO_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let event = events.recv_timeout(remaining).with_context(|| format!("orphaned ACP worker did not end before deletion (quit: {quit:?})"))?;
        client.ack(event.seq)?;
        if matches!(event.event, wire::Event::Ended(_)) { break; }
    }
    wait_stopped(socket)
}

pub(super) fn wait_stopped(socket: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + IO_TIMEOUT;
    while socket.exists() {
        if UnixStream::connect(socket).is_err() { return Ok(()); }
        if std::time::Instant::now() >= deadline { bail!("ACP worker has not stopped; refusing to remove its runtime directory"); }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn message(stream: &mut UnixStream, value: Value) {
        write_line(stream, &value).unwrap();
    }

    fn read(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "fake agent connection ended");
        serde_json::from_str(&line).unwrap()
    }

    fn until(rx: &mpsc::Receiver<Delivered>, client: &Client, cursor: &mut u64, test: impl Fn(&wire::Event) -> bool) -> Delivered {
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).expect("relay event");
            if event.seq > 0 {
                *cursor = event.seq;
                client.ack(*cursor).unwrap();
            }
            if test(&event.event) {
                return event;
            }
        }
    }

    #[test]
    fn reconnect_preserves_live_turn_question_and_command_identity() {
        let dir = std::env::temp_dir().join(format!("sill-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("relay.sock");
        let (agent, worker_stream) = UnixStream::pair().unwrap();
        agent.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (release, released) = mpsc::channel();
        let fake = std::thread::spawn(move || {
            let mut reader = BufReader::new(agent.try_clone().unwrap());
            let mut writer = agent;
            let initialize = read(&mut reader);
            assert_eq!(initialize["method"], "initialize");
            message(&mut writer, json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[]}}));
            let new_session = read(&mut reader);
            assert_eq!(new_session["method"], "session/new");
            message(&mut writer, json!({"jsonrpc":"2.0", "id":new_session["id"], "result":{"sessionId":"fake-session"}}));
            let prompt = read(&mut reader);
            assert_eq!(prompt["method"], "session/prompt");
            message(&mut writer, json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":"fake-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"before disconnect"}}}}));
            // The main daemon has disappeared; the SDK and its prompt future keep running.
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            message(&mut writer, json!({"jsonrpc":"2.0", "id":99, "method":"session/request_permission", "params":{"sessionId":"fake-session","toolCall":{"toolCallId":"tool","title":"work","status":"pending"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}));
            let answer = read(&mut reader);
            assert_eq!(answer["id"], 99);
            assert_eq!(answer["result"]["outcome"]["outcome"], "selected");
            assert_eq!(answer["result"]["outcome"]["optionId"], "allow");
            message(&mut writer, json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":"fake-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after reconnect"}}}}));
            message(&mut writer, json!({"jsonrpc":"2.0", "id":prompt["id"], "result":{"stopReason":"end_turn"}}));
            // Quit closes ACP. A replayed prompt would appear here and fail this assertion.
            let mut extra = String::new();
            assert_eq!(reader.read_line(&mut extra).unwrap(), 0, "duplicate command reached agent: {extra}");
        });
        let worker_socket = socket.clone();
        let worker = std::thread::spawn(move || run_stream("test", &worker_socket, worker_stream, Path::new("/tmp"), None).unwrap());
        let (client, events) = loop {
            match connect(&socket, 0) {
                Ok(connection) => break connection,
                Err(_) if !socket.exists() => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("connecting relay: {error:#}"),
            }
        };
        let mut cursor = 0;
        until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::Ready { .. }));
        let prompt = wire::Command::Prompt { text: "work".into(), attachments: vec![], entry: None };
        client.command(1, prompt.clone()).unwrap();
        client.command(1, prompt.clone()).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::Text(text) if text == "before disconnect"));
        client.close();
        release.send(()).unwrap();
        let (client, events) = connect(&socket, cursor).unwrap();
        // Retrying the main daemon's outbox after reconnect still submits only one prompt.
        client.command(1, prompt).unwrap();
        let question = until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::PermissionRequest { .. }));
        let wire::Event::PermissionRequest { request, .. } = question.event else { unreachable!() };
        client.close();
        let (client, events) = connect(&socket, cursor).unwrap();
        let replay = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(replay.seq, 0, "an acknowledged pending question is current state");
        assert!(matches!(replay.event, wire::Event::PermissionRequest { request: id, .. } if id == request));
        client.answer(request, wire::Answer::Permission(Some("allow".into()))).unwrap();
        client.answer(request, wire::Answer::Permission(Some("allow".into()))).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::Text(text) if text == "after reconnect"));
        until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::Stopped(_)));
        client.command(2, wire::Command::Quit).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, wire::Event::Ended(_)));
        worker.join().unwrap();
        fake.join().unwrap();
        assert!(!socket.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_answer_of_the_wrong_kind_keeps_the_question_open() {
        let (commands, _) = async_channel::unbounded();
        let shared = Shared { state: Mutex::new(State::default()), changed: Condvar::new(), commands };
        let (tx, rx) = futures::channel::oneshot::channel();
        shared.take_event(acp::Event::PermissionRequest { tool_call_id: "tool".into(), title: "work".into(), options: vec![], reply: tx });
        assert!(shared.answer(1, wire::Answer::Form(None)).is_err());
        assert!(shared.state.lock().unwrap().pending.contains_key(&1));
        shared.answer(1, wire::Answer::Permission(Some("allow".into()))).unwrap();
        assert_eq!(futures::executor::block_on(rx).unwrap().as_deref(), Some("allow"));
    }

    #[test]
    fn a_duplicate_worker_never_opens_the_agent_stream_or_replaces_a_live_socket() {
        let dir = std::env::temp_dir().join(format!("sill-relay-live-socket-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("relay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let opened = AtomicBool::new(false);
        let result = run_with("test", &socket, Path::new("/tmp"), None, || {
            opened.store(true, Ordering::Relaxed);
            bail!("must not open the agent stream")
        });
        assert!(result.unwrap_err().to_string().contains("already answers"));
        assert!(!opened.load(Ordering::Relaxed));
        assert!(socket.exists());
        drop(listener);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
