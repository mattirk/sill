//! ACP client over the shim's agent stream (SPEC §4.1). Runtime-neutral: the SDK is driven with
//! `futures::executor::block_on` on a plain thread; the socket is wrapped by `async-io`. The caller
//! talks to the running session through channels: [`Command`]s in, [`Event`]s out. Updates are read
//! continuously (not only during a turn), so agent-initiated updates and `session/load` replays reach
//! the caller too. Permission requests carry a one-shot reply channel; the dispatch loop waits for the
//! answer, as the agent does.

use std::os::unix::net::UnixStream;
use std::path::Path;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, InitializeRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::util::MatchDispatch;
pub use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::{ByteStreams, Client, SessionMessage};
use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt;
use futures::channel::oneshot;

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// What the caller sees while a session runs.
#[derive(Debug)]
pub enum Event {
    /// The ACP session is up: (agent session id, resumed with history replay?).
    Ready { session_id: String, resumed: bool },
    /// A prompt turn started.
    TurnStarted,
    /// Replayed user message (only after a `session/load`).
    UserText(String),
    Text(String),
    Thought(String),
    ToolCall { id: String, title: String, kind: String, status: String },
    ToolCallUpdate { id: String, status: Option<String>, title: Option<String> },
    /// The agent asks for permission; answer with the chosen option id (None = cancel).
    PermissionRequest { title: String, options: Vec<PermissionOption>, reply: oneshot::Sender<Option<String>> },
    Other(String),
    /// A prompt turn ended.
    Stopped(StopReason),
    /// The session ended (connection closed or error).
    Ended(Option<String>),
}

#[derive(Debug)]
pub enum Command {
    Prompt(String),
    Quit,
}

/// How permission requests are answered when no one listens for [`Event::PermissionRequest`]
/// (the M0 CLI). The shelf answers them itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPermission {
    AllowOnce,
    Reject,
    /// Ask through the event channel.
    Ask,
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub cwd: std::path::PathBuf,
    pub auto: AutoPermission,
    /// Resume this agent session with `session/load` if the agent supports it.
    pub resume: Option<String>,
}

/// Run an ACP session over `stream` until `Command::Quit` or the connection ends. Blocking; run it
/// on its own thread.
pub fn run_session(
    stream: UnixStream,
    opts: SessionOptions,
    commands: async_channel::Receiver<Command>,
    events: async_channel::Sender<Event>,
) -> Result<()> {
    stream.set_nonblocking(true)?;
    let stream = async_io::Async::new(stream).context("async socket")?;
    let (reader, writer) = stream.split();
    let transport = ByteStreams::new(writer, reader);
    let events_perm = events.clone();
    let events_loop = events.clone();
    let auto = opts.auto;

    let result: Result<(), agent_client_protocol::Error> = futures::executor::block_on(async move {
        Client
            .builder()
            .name("sill")
            .on_receive_request(
                async move |req: RequestPermissionRequest, responder, _cx| {
                    let options: Vec<PermissionOption> = req
                        .options
                        .iter()
                        .map(|o| PermissionOption { id: o.option_id.to_string(), name: o.name.clone(), kind: o.kind })
                        .collect();
                    let title = req
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| req.tool_call.tool_call_id.to_string());
                    let pick = |kinds: &[PermissionOptionKind]| {
                        kinds.iter().find_map(|k| options.iter().find(|o| &o.kind == k)).map(|o| o.id.clone())
                    };
                    let chosen: Option<String> = match auto {
                        AutoPermission::AllowOnce => pick(&[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]),
                        AutoPermission::Reject => pick(&[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]),
                        AutoPermission::Ask => {
                            let (tx, rx) = oneshot::channel();
                            let _ = events_perm
                                .send(Event::PermissionRequest { title: title.clone(), options: options.clone(), reply: tx })
                                .await;
                            rx.await.unwrap_or(None)
                        }
                    };
                    let outcome = match chosen {
                        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async move |cx| {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(ClientCapabilities::new()))
                    .block_task()
                    .await?;
                let can_load = init.agent_capabilities.load_session;

                // either resume (history replayed as updates) or start fresh
                let resume = opts.resume.clone().filter(|_| can_load);
                let (mut session, resumed) = match resume {
                    Some(id) => match cx.load_session(id, &opts.cwd).block_task().start_session().await {
                        Ok(restored) => (restored.into_session(), true),
                        Err(e) => {
                            tracing::warn!("session/load failed, starting fresh: {e}");
                            (cx.build_session(&opts.cwd).block_task().start_session().await?, false)
                        }
                    },
                    None => (cx.build_session(&opts.cwd).block_task().start_session().await?, false),
                };
                let _ = events_loop.send(Event::Ready { session_id: session.session_id().to_string(), resumed }).await;

                let mut in_turn = false;
                loop {
                    // wait for either a command or an update, whichever comes first; both futures are
                    // dropped before `session` is used again
                    enum Next {
                        Cmd(Option<Command>),
                        Update(Result<SessionMessage, agent_client_protocol::Error>),
                    }
                    let next = {
                        let cmd_fut = Box::pin(commands.recv());
                        let upd_fut = Box::pin(session.read_update());
                        match futures::future::select(cmd_fut, upd_fut).await {
                            futures::future::Either::Left((cmd, _)) => Next::Cmd(cmd.ok()),
                            futures::future::Either::Right((upd, _)) => Next::Update(upd),
                        }
                    };
                    match next {
                        Next::Cmd(Some(Command::Prompt(text))) => {
                            if in_turn {
                                let _ = events_loop.send(Event::Other("busy: prompt dropped".into())).await;
                                continue;
                            }
                            session.send_prompt(text.as_str())?;
                            in_turn = true;
                            let _ = events_loop.send(Event::TurnStarted).await;
                        }
                        Next::Cmd(Some(Command::Quit)) | Next::Cmd(None) => break,
                        Next::Update(update) => match update? {
                            SessionMessage::StopReason(reason) => {
                                in_turn = false;
                                let _ = events_loop.send(Event::Stopped(reason)).await;
                            }
                            SessionMessage::SessionMessage(dispatch) => {
                                let events = events_loop.clone();
                                MatchDispatch::new(dispatch)
                                    .if_notification(async |n: SessionNotification| {
                                        let _ = events.send(to_event(n.update)).await;
                                        Ok(())
                                    })
                                    .await
                                    .otherwise_ignore()?;
                            }
                            _ => {}
                        },
                    }
                }
                Ok(())
            })
            .await
    });
    let err = result.as_ref().err().map(|e| e.to_string());
    let _ = events.send_blocking(Event::Ended(err));
    result.map_err(|e| anyhow!("acp: {e}"))
}

/// One prompt in a fresh session (M0 CLI).
pub fn run_prompt(
    stream: UnixStream,
    cwd: &Path,
    prompt: &str,
    auto: AutoPermission,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> Result<()> {
    let (cmd_tx, cmd_rx) = async_channel::bounded(2);
    let (ev_tx, ev_rx) = async_channel::unbounded();
    cmd_tx.send_blocking(Command::Prompt(prompt.to_string()))?;
    let opts = SessionOptions { cwd: cwd.to_path_buf(), auto, resume: None };
    let worker = std::thread::spawn(move || run_session(stream, opts, cmd_rx, ev_tx));
    while let Ok(ev) = ev_rx.recv_blocking() {
        let stop = matches!(ev, Event::Stopped(_));
        let ended = matches!(ev, Event::Ended(_));
        on_event(ev);
        if stop {
            let _ = cmd_tx.send_blocking(Command::Quit);
        }
        if ended {
            break;
        }
    }
    worker.join().map_err(|_| anyhow!("session thread panicked"))?
}

fn to_event(update: SessionUpdate) -> Event {
    fn text(block: &ContentBlock) -> String {
        match block {
            ContentBlock::Text(t) => t.text.clone(),
            other => format!("[{}]", kind_name(other)),
        }
    }
    match update {
        SessionUpdate::UserMessageChunk(c) => Event::UserText(text(&c.content)),
        SessionUpdate::AgentMessageChunk(c) => Event::Text(text(&c.content)),
        SessionUpdate::AgentThoughtChunk(c) => Event::Thought(text(&c.content)),
        SessionUpdate::ToolCall(t) => Event::ToolCall {
            id: t.tool_call_id.to_string(),
            title: t.title.clone(),
            kind: format!("{:?}", t.kind),
            status: format!("{:?}", t.status),
        },
        SessionUpdate::ToolCallUpdate(u) => Event::ToolCallUpdate {
            id: u.tool_call_id.to_string(),
            status: u.fields.status.map(|s| format!("{s:?}")),
            title: u.fields.title.clone(),
        },
        other => Event::Other(format!("{other:?}").chars().take(120).collect()),
    }
}

fn kind_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text(_) => "text",
        ContentBlock::Image(_) => "image",
        ContentBlock::Audio(_) => "audio",
        ContentBlock::ResourceLink(_) => "resource link",
        _ => "content",
    }
}
