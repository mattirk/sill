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
use sill_core::acp;

const JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The relay's own protocol — the shape of `Input` and `Output` and of the relay types they carry
/// ([`Event`], [`Command`], [`Answer`] and the `acp` data inside them). Both hellos say it; a
/// daemon that finds a relay of another version after a deploy replaces it (`evict`) instead of
/// leaving the session with a worker it cannot talk to (docs/daemon.md §3.9 item 1). Bump it with
/// any change to those types; a change to the frontend protocol's `wire` types alone needs none.
pub(crate) const RELAY_PROTOCOL: u32 = 2;

/// The relay answered but will not cooperate: another version, a cursor it no longer retains, an
/// answer that is not a Hello. Not a transport failure — the socket answers — so the hub evicts it.
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

// --- what the relay carries ------------------------------------------------------------------------
//
// The conversation between the ACP worker and the main daemon, in the relay's own types: never
// the frontends' public types, so a frontend protocol change cannot change
// these bytes or evict a running relay (and the agent conversation it holds). The runner converts
// at the daemon's boundary, and its stored state (`daemon_runners`) holds these too.

/// One thing the agent's side said: `acp::Event` with a request number in place of a question's
/// reply channel, and `Resolved` once the relay delivered the answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Event {
    V2Ready { session_id: String, resumed: bool, controls: acp::Controls },
    V2Update { update: serde_json::Value },
    V2PermissionsCancelled,
    PromptAccepted { message_id: String },
    V2PromptFailed { code: String, message: String },
    V2PermissionRequest { request: u64, tool_call_id: Option<String>, title: String, subject: Option<serde_json::Value>, options: Vec<acp::PermissionOption> },
    Ready { session_id: String, resumed: bool, controls: acp::Controls },
    Capabilities { images: bool },
    Controls(acp::Controls),
    TurnStarted,
    UserText(String),
    Text(String),
    Thought(String),
    ToolCall { id: String, title: String, kind: String, status: String, detail: Option<String>, diff: Option<String>, parent: Option<String> },
    ToolCallUpdate { id: String, status: Option<String>, title: Option<String>, detail: Option<String>, diff: Option<String> },
    SubagentText { parent: String, text: String },
    PermissionRequest { request: u64, tool_call_id: String, title: String, options: Vec<acp::PermissionOption> },
    AutoAnswered { tool_call_id: String, title: String, allowed: bool },
    Elicitation { request: u64, message: String, fields: Vec<acp::FormField> },
    Resolved { request: u64 },
    Other(String),
    Usage { used: u64, size: u64 },
    AuthRequired { message: String, methods: Vec<acp::AuthMethodInfo> },
    Stopped(acp::StopReason),
    Ended(Option<String>),
    Plan(Vec<acp::PlanEntry>),
    Commands(Vec<acp::AgentCommand>),
    AgentTitle(String),
    Notice { severity: String, title: String, description: Option<String> },
}

/// What the daemon asks of the ACP conversation. `text` is what the agent gets: the daemon has
/// composed it already (context prefix included).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Command {
    Prompt { text: String, attachments: Vec<acp::Attachment> },
    Authenticate(String),
    /// A login with an API key the daemon saved: the relay reads `key_file` right before sending,
    /// so neither this command nor the daemon's saved outbox holds the key.
    AuthenticateKey { method: String, key_file: std::path::PathBuf },
    RetrySession,
    SetMode(String),
    SetConfig(String, acp::ConfigValue),
    Cancel,
    Quit,
}

/// The answer to a question: the chosen permission option or the form's values (None = cancel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Answer {
    Permission(Option<String>),
    Form(Option<BTreeMap<String, serde_json::Value>>),
}

impl Command {
    pub fn into_acp(self) -> acp::Command {
        match self {
            Command::Prompt { text, attachments } => acp::Command::Prompt(text, attachments),
            Command::Authenticate(m) => acp::Command::Authenticate(m),
            Command::AuthenticateKey { method, key_file } => acp::Command::AuthenticateKey { method, key_file },
            Command::RetrySession => acp::Command::RetrySession,
            Command::SetMode(m) => acp::Command::SetMode(m),
            Command::SetConfig(o, v) => acp::Command::SetConfig(o, v),
            Command::Cancel => acp::Command::Cancel,
            Command::Quit => acp::Command::Quit,
        }
    }

    /// A control the daemon restores itself (`context::Context::restore`).
    pub fn from_acp_control(command: acp::Command) -> Command {
        match command {
            acp::Command::SetMode(m) => Command::SetMode(m),
            acp::Command::SetConfig(k, v) => Command::SetConfig(k, v),
            _ => unreachable!("control restoration only emits control commands"),
        }
    }
}

/// Where an agent's question waits for its answer, in the relay.
enum Reply {
    Permission(futures::channel::oneshot::Sender<Option<String>>),
    Form(futures::channel::oneshot::Sender<Option<BTreeMap<String, serde_json::Value>>>),
}

impl Reply {
    /// Deliver to the live SDK request handler; false for a wrong kind or dropped receiver.
    /// ACP v1 has no acknowledgement that the remote agent received a response.
    fn send(self, answer: Answer) -> bool {
        match (self, answer) {
            (Reply::Permission(tx), Answer::Permission(a)) => tx.send(a).is_ok(),
            (Reply::Form(tx), Answer::Form(a)) => tx.send(a).is_ok(),
            _ => false,
        }
    }
}

/// An event of the session loop as the relay sends it: a question gets `request`, and its reply
/// channel stays in the relay until the question is answered.
fn event_from_acp(event: acp::Event, request: u64) -> (Event, Option<Reply>) {
    use acp::Event as A;
    let event = match event {
        A::V2Ready {session_id,resumed,controls} => Event::V2Ready {session_id,resumed,controls},
        A::V2Update {update} => Event::V2Update {update},
        A::V2PermissionsCancelled => Event::V2PermissionsCancelled,
        A::PromptAccepted {message_id} => Event::PromptAccepted {message_id},
        A::V2PromptFailed {code,message} => Event::V2PromptFailed {code,message},
        A::V2PermissionRequest {tool_call_id,title,subject,options,reply} => return (Event::V2PermissionRequest {request,tool_call_id,title,subject,options},Some(Reply::Permission(reply))),
        A::Ready { session_id, resumed, controls } => Event::Ready { session_id, resumed, controls },
        A::Capabilities { images } => Event::Capabilities { images },
        A::Controls(c) => Event::Controls(c),
        A::TurnStarted => Event::TurnStarted,
        A::UserText(t) => Event::UserText(t),
        A::Text(t) => Event::Text(t),
        A::Thought(t) => Event::Thought(t),
        A::ToolCall { id, title, kind, status, detail, diff, parent } => Event::ToolCall { id, title, kind, status, detail, diff, parent },
        A::ToolCallUpdate { id, status, title, detail, diff } => Event::ToolCallUpdate { id, status, title, detail, diff },
        A::SubagentText { parent, text } => Event::SubagentText { parent, text },
        A::PermissionRequest { tool_call_id, title, options, reply } => {
            return (Event::PermissionRequest { request, tool_call_id, title, options }, Some(Reply::Permission(reply)));
        }
        A::AutoAnswered { tool_call_id, title, allowed } => Event::AutoAnswered { tool_call_id, title, allowed },
        A::Elicitation { message, fields, reply } => return (Event::Elicitation { request, message, fields }, Some(Reply::Form(reply))),
        A::Other(t) => Event::Other(t),
        A::Usage { used, size } => Event::Usage { used, size },
        A::AuthRequired { message, methods } => Event::AuthRequired { message, methods },
        A::Stopped(r) => Event::Stopped(r),
        A::Ended(e) => Event::Ended(e),
        A::Plan(p) => Event::Plan(p),
        A::Commands(c) => Event::Commands(c),
        A::AgentTitle(t) => Event::AgentTitle(t),
        A::Notice { severity, title, description } => Event::Notice { severity, title, description },
        A::UserMessage { text, .. } => Event::UserText(text),
    };
    (event, None)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Delivered {
    /// Zero denotes an already-acknowledged open question, replayed as current state.
    pub seq: u64,
    pub event: Event,
}

#[derive(Serialize, Deserialize)]
enum Input {
    Hello { since: u64, version: u32 },
    Command { id: u64, command: Command },
    Answer { request: u64, answer: Answer },
    Ack { seq: u64 },
}

#[derive(Serialize, Deserialize)]
enum Output {
    Hello {
        acknowledged: u64, last: u64, version: u32,
    },
    Event(Delivered),
    Accepted { id: u64, error: Option<String> },
    Answered { request: u64, error: Option<String> },
}

struct JournalEntry {
    delivered: Delivered,
    bytes: usize,
}

struct Pending {
    event: Event,
    seq: u64,
    reply: Reply,
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
    fn append_locked(&self, state: &mut State, event: Event) -> u64 {
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
        if matches!(&event,acp::Event::V2PermissionsCancelled) {
            state.pending.retain(|_,p|!matches!(&p.event,Event::V2PermissionRequest{..}));
        }
        state.next_request += 1;
        let request = state.next_request;
        let (event, reply) = event_from_acp(event, request);
        let ended = matches!(event, Event::Ended(_));
        let seq = self.append_locked(&mut state, event.clone());
        if let Some(reply) = reply {
            state.pending.insert(request, Pending { event, seq, reply });
        }
        if ended {
            state.ended_at = Some(seq);
            state.pending.clear();
        }
    }

    fn answer(&self, request: u64, answer: Answer) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.answered.contains(&request) {
            return Ok(());
        }
        let Some(pending) = state.pending.get(&request) else { bail!("no open question {request}") };
        if !matches!((&pending.event, &answer), (Event::PermissionRequest { .. } | Event::V2PermissionRequest { .. }, Answer::Permission(_)) | (Event::Elicitation { .. }, Answer::Form(_))) {
            bail!("the answer is not of the question's kind");
        }
        let pending = state.pending.remove(&request).unwrap();
        if !pending.reply.send(answer) {
            bail!("the agent question no longer has a live reply handler");
        }
        state.answered.insert(request);
        self.append_locked(&mut state, Event::Resolved { request });
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
pub(crate) fn run(session: &str, socket: &Path, agent_socket: &Path, cwd: &Path, resume: Option<String>, experimental_acp_v2: bool) -> Result<()> {
    super::exit_on_panic();
    run_with(session, socket, cwd, resume, experimental_acp_v2, || sill_core::shim::agent_stream(agent_socket))
}

#[cfg(test)]
fn run_stream(session: &str, socket: &Path, stream: UnixStream, cwd: &Path, resume: Option<String>) -> Result<()> {
    run_with(session, socket, cwd, resume, false, || Ok(stream))
}

fn run_with(session: &str, socket: &Path, cwd: &Path, resume: Option<String>, experimental_acp_v2: bool, open: impl FnOnce() -> Result<UnixStream>) -> Result<()> {
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
        let _ = if experimental_acp_v2 { sill_core::acp_v2::run_session(stream, opts, command_rx, events) } else { acp::run_session(stream, opts, command_rx, events) };
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
    let Input::Hello { since, version } = serde_json::from_str(&first)? else { bail!("a relay connection starts with Hello") };
    stream.set_read_timeout(None)?;
    let (generation, pending) = {
        let mut state = shared.state.lock().unwrap();
        if version != RELAY_PROTOCOL {
            // say which version this is, so the daemon knows it is looking at another build's relay
            write_line(&mut stream, &Output::Hello { acknowledged: state.acknowledged, last: state.next_seq, version: RELAY_PROTOCOL })?;
            bail!("a daemon of relay protocol {version} connected; this relay speaks {RELAY_PROTOCOL}");
        }
        if since < state.acknowledged || since > state.next_seq {
            bail!("relay cursor {since} is outside retained range {}..{}", state.acknowledged, state.next_seq);
        }
        if let Some(old) = state.subscriber.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        state.generation += 1;
        state.subscriber = Some(stream.try_clone()?);
        write_line(&mut stream, &Output::Hello { acknowledged: state.acknowledged, last: state.next_seq, version: RELAY_PROTOCOL })?;
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
                            match shared.commands.try_send(command.into_acp()) {
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

    pub fn command(&self, id: u64, command: Command) -> Result<()> {
        self.request((false, id), Input::Command { id, command })
    }

    pub fn answer(&self, request: u64, answer: Answer) -> Result<()> {
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
    write_line(&mut stream, &Input::Hello { since, version: RELAY_PROTOCOL })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    // from here the relay answered: whatever is wrong with its answer is a refusal, not transport
    if first.is_empty() {
        return Err(Refused("the ACP relay closed the connection at Hello (its cursor range, or an older relay)".into()).into());
    }
    let Ok(Output::Hello { acknowledged, last, version }) = serde_json::from_str::<Output>(&first) else {
        return Err(Refused("the ACP relay did not answer Hello with a Hello".into()).into());
    };
    if version != RELAY_PROTOCOL {
        return Err(Refused(format!("the ACP relay speaks relay protocol {version}; this daemon speaks {RELAY_PROTOCOL}")).into());
    }
    if since < acknowledged || since > last {
        return Err(Refused(format!("the ACP relay no longer retains cursor {since} ({acknowledged}..{last})")).into());
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
pub(super) fn stop_unowned(socket: &Path, session: &str, cursor: u64) -> Result<()> {
    let (client, events) = match connect(socket, cursor) {
        Ok(connection) => connection,
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)) => return Ok(()),
        Err(error) if error.downcast_ref::<Refused>().is_some() => {
            // a worker this daemon cannot talk to goes the hard way; its journal was for a
            // session being deleted anyway
            eprintln!("sill daemon: session {session}: orphaned ACP relay refused ({error:#}); evicting it");
            return evict(socket.parent().unwrap_or(Path::new(".")), session);
        }
        Err(error) => return Err(error.context("connecting orphaned ACP worker before deletion")),
    };
    // A reserved terminal command id is greater than every persisted normal command, and remains
    // idempotent if a prior deletion attempt was interrupted after sending it.
    let quit = client.command(u64::MAX, Command::Quit);
    let deadline = std::time::Instant::now() + IO_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let event = events.recv_timeout(remaining).with_context(|| format!("orphaned ACP worker did not end before deletion (quit: {quit:?})"))?;
        client.ack(event.seq)?;
        if matches!(event.event, Event::Ended(_)) { break; }
    }
    wait_stopped(socket)
}

/// A relay that answers but will not cooperate — another build's after a deploy, a cursor it no
/// longer retains — is replaced (docs/daemon.md §3.9 item 1): the process `relay.pid` names, once
/// checked to be ours and this session's relay, gets TERM, then KILL, and its files go. Its
/// journal is lost with it: the conversation starts again from the store (`session/load` where
/// the agent has it), as after a lost worker.
pub(super) fn evict(dir: &Path, session: &str) -> Result<()> {
    use rustix::process::{Pid, Signal, kill_process};
    let socket = dir.join("relay.sock");
    let pid: Option<u32> = std::fs::read_to_string(dir.join("relay.pid")).ok().and_then(|t| t.trim().parse().ok());
    let answers = || UnixStream::connect(&socket).is_ok();
    if let Some(pid) = pid.filter(|p| is_relay_of(*p, session)) {
        let p = Pid::from_raw(pid as i32).context("relay pid")?;
        let _ = kill_process(p, Signal::TERM);
        let deadline = std::time::Instant::now() + IO_TIMEOUT;
        while std::time::Instant::now() < deadline && answers() && is_relay_of(pid, session) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if answers() && is_relay_of(pid, session) {
            let _ = kill_process(p, Signal::KILL);
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline && is_relay_of(pid, session) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        eprintln!("sill daemon: session {session}: evicted ACP relay pid {pid}");
    }
    // the kernel closes a dead process's listener at once; a moment for that to show
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while answers() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if answers() {
        bail!("the ACP relay of session {session} still answers at {} and relay.pid ({pid:?}) does not name it", socket.display());
    }
    for f in ["relay.sock", "relay.pid", "relay.lock"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    Ok(())
}

/// `pid` is a live process of ours whose command line is a relay of `session`: what `relay.pid`
/// is trusted to name, before a signal goes to it.
fn is_relay_of(pid: u32, session: &str) -> bool {
    let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
    let mine = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u32>().ok()))
        == Some(rustix::process::geteuid().as_raw());
    let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
    mine && args.contains(&&b"relay"[..]) && args.contains(&session.as_bytes())
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

    fn until(rx: &mpsc::Receiver<Delivered>, client: &Client, cursor: &mut u64, test: impl Fn(&Event) -> bool) -> Delivered {
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
        until(&events, &client, &mut cursor, |event| matches!(event, Event::Ready { .. }));
        let prompt = Command::Prompt { text: "work".into(), attachments: vec![] };
        client.command(1, prompt.clone()).unwrap();
        client.command(1, prompt.clone()).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, Event::Text(text) if text == "before disconnect"));
        client.close();
        release.send(()).unwrap();
        let (client, events) = connect(&socket, cursor).unwrap();
        // Retrying the main daemon's outbox after reconnect still submits only one prompt.
        client.command(1, prompt).unwrap();
        let question = until(&events, &client, &mut cursor, |event| matches!(event, Event::PermissionRequest { .. }));
        let Event::PermissionRequest { request, .. } = question.event else { unreachable!() };
        client.close();
        let (client, events) = connect(&socket, cursor).unwrap();
        let replay = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(replay.seq, 0, "an acknowledged pending question is current state");
        assert!(matches!(replay.event, Event::PermissionRequest { request: id, .. } if id == request));
        client.answer(request, Answer::Permission(Some("allow".into()))).unwrap();
        client.answer(request, Answer::Permission(Some("allow".into()))).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, Event::Text(text) if text == "after reconnect"));
        until(&events, &client, &mut cursor, |event| matches!(event, Event::Stopped(_)));
        client.command(2, Command::Quit).unwrap();
        until(&events, &client, &mut cursor, |event| matches!(event, Event::Ended(_)));
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
        assert!(shared.answer(1, Answer::Form(None)).is_err());
        assert!(shared.state.lock().unwrap().pending.contains_key(&1));
        shared.answer(1, Answer::Permission(Some("allow".into()))).unwrap();
        assert_eq!(futures::executor::block_on(rx).unwrap().as_deref(), Some("allow"));
    }

    #[test]
    fn a_dropped_question_handler_never_claims_delivery() {
        let (commands, _) = async_channel::unbounded();
        let shared = Shared { state: Mutex::new(State::default()), changed: Condvar::new(), commands };
        let (tx, rx) = futures::channel::oneshot::channel();
        shared.take_event(acp::Event::PermissionRequest { tool_call_id: "tool".into(), title: "work".into(), options: vec![], reply: tx });
        drop(rx);
        assert!(shared.answer(1, Answer::Permission(None)).is_err());
        let state = shared.state.lock().unwrap();
        assert!(!state.answered.contains(&1));
        assert_eq!(state.journal.len(), 1, "no Resolved event can claim a dropped reply was delivered");
    }

    #[test]
    fn a_form_answer_is_delivered_once_and_its_resolution_is_replayable() {
        let (commands, _) = async_channel::unbounded();
        let shared = Shared { state: Mutex::new(State::default()), changed: Condvar::new(), commands };
        let (tx, rx) = futures::channel::oneshot::channel();
        shared.take_event(acp::Event::Elicitation {
            message: "Choose a name".into(),
            fields: vec![acp::FormField { key: "name".into(), title: "Name".into(), description: String::new(), kind: acp::FieldKind::Text }],
            reply: tx,
        });
        let values = BTreeMap::from([("name".into(), json!("example"))]);
        shared.answer(1, Answer::Form(Some(values.clone()))).unwrap();
        assert_eq!(futures::executor::block_on(rx).unwrap(), Some(values.clone()));
        // A daemon losing the answer acknowledgement retries the same intent after reconnect.
        shared.answer(1, Answer::Form(Some(values))).unwrap();
        let state = shared.state.lock().unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.journal.len(), 2);
        assert!(matches!(state.journal.back().unwrap().delivered.event, Event::Resolved { request: 1 }));
    }

    #[test]
    fn a_duplicate_worker_never_opens_the_agent_stream_or_replaces_a_live_socket() {
        let dir = std::env::temp_dir().join(format!("sill-relay-live-socket-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("relay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let opened = AtomicBool::new(false);
        let result = run_with("test", &socket, Path::new("/tmp"), None, false, || {
            opened.store(true, Ordering::Relaxed);
            bail!("must not open the agent stream")
        });
        assert!(result.unwrap_err().to_string().contains("already answers"));
        assert!(!opened.load(Ordering::Relaxed));
        assert!(socket.exists());
        drop(listener);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn incompatible_daemon_is_refused_before_replacing_subscriber() {
        let (commands,_)=async_channel::unbounded();
        let shared=Arc::new(Shared{state:Mutex::new(State::default()),changed:Condvar::new(),commands});
        let (existing,_peer)=UnixStream::pair().unwrap();
        shared.state.lock().unwrap().subscriber=Some(existing);
        let (mut client,server)=UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let state=shared.clone();let worker=std::thread::spawn(move||serve(server,state));
        message(&mut client,json!({"Hello":{"since":0,"version":1}}));
        let response=read(&mut BufReader::new(client));assert_eq!(response["Hello"]["version"],RELAY_PROTOCOL);
        assert!(worker.join().unwrap().is_err());
        let state=shared.state.lock().unwrap();
        assert!(state.subscriber.is_some());assert_eq!(state.generation,0);
    }
    #[test]
    fn native_cancellation_removes_pending_handlers_without_false_answer_confirmation() {
        let (commands,_)=async_channel::unbounded();let shared=Shared{state:Mutex::new(State::default()),changed:Condvar::new(),commands};
        let (reply,_)=futures::channel::oneshot::channel();
        shared.take_event(acp::Event::V2PermissionRequest{tool_call_id:None,title:"Run?".into(),subject:None,options:vec![],reply});
        shared.take_event(acp::Event::V2PermissionsCancelled);
        let state=shared.state.lock().unwrap();assert!(state.pending.is_empty());assert!(state.answered.is_empty());
        assert!(matches!(state.journal.back().unwrap().delivered.event,Event::V2PermissionsCancelled));
    }

}
