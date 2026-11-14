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
    CancelNotification, ClientCapabilities, ContentBlock, InitializeRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOptions, SessionModeState,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason,
};
use agent_client_protocol::util::MatchDispatch;
pub use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::schema::v1::{AuthCapabilities, AuthMethod, AuthenticateRequest};
use agent_client_protocol::{ActiveSession, Agent, ByteStreams, Client, ErrorCode, SessionMessage};
use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt;
use futures::channel::oneshot;

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// The agent's adjustable session settings (SPEC: modes + config options), as the UI shows them.
#[derive(Debug, Clone, Default)]
pub struct Controls {
    /// (current mode id, [(id, name)])
    pub modes: Option<(String, Vec<(String, String)>)>,
    pub options: Vec<ConfigOption>,
}

#[derive(Debug, Clone)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// `mode` | `model` | `model_config` | `thought_level` | other
    pub category: String,
    pub kind: ConfigKind,
}

#[derive(Debug, Clone)]
pub enum ConfigKind {
    /// (current value id, [(value id, name)])
    Select(String, Vec<(String, String)>),
    Boolean(bool),
}

#[derive(Debug, Clone)]
pub enum ConfigValue {
    Id(String),
    Bool(bool),
}

fn controls_from(modes: Option<&SessionModeState>, options: Option<&[SessionConfigOption]>) -> Controls {
    let modes = modes.map(|m| {
        (
            m.current_mode_id.to_string(),
            m.available_modes.iter().map(|x| (x.id.to_string(), x.name.clone())).collect(),
        )
    });
    let options = options
        .unwrap_or(&[])
        .iter()
        .map(|o| ConfigOption {
            id: o.id.to_string(),
            name: o.name.clone(),
            category: match &o.category {
                Some(SessionConfigOptionCategory::Mode) => "mode".into(),
                Some(SessionConfigOptionCategory::Model) => "model".into(),
                Some(SessionConfigOptionCategory::ModelConfig) => "model_config".into(),
                Some(SessionConfigOptionCategory::ThoughtLevel) => "thought_level".into(),
                Some(SessionConfigOptionCategory::Other(s)) => s.clone(),
                _ => String::new(),
            },
            kind: match &o.kind {
                SessionConfigKind::Select(s) => {
                    let list: Vec<(String, String)> = match &s.options {
                        SessionConfigSelectOptions::Ungrouped(v) => v.iter().map(|x| (x.value.to_string(), x.name.clone())).collect(),
                        SessionConfigSelectOptions::Grouped(g) => g
                            .iter()
                            .flat_map(|grp| grp.options.iter().map(move |x| (x.value.to_string(), format!("{} · {}", grp.name, x.name))))
                            .collect(),
                        _ => Vec::new(),
                    };
                    ConfigKind::Select(s.current_value.to_string(), list)
                }
                SessionConfigKind::Boolean(b) => ConfigKind::Boolean(b.current_value),
                _ => ConfigKind::Boolean(false),
            },
        })
        .collect();
    Controls { modes, options }
}

/// What the caller sees while a session runs.
#[derive(Debug)]
pub enum Event {
    /// The ACP session is up: (agent session id, resumed with history replay?).
    Ready { session_id: String, resumed: bool, controls: Controls },
    /// Modes / config options changed (by us or by the agent).
    Controls(Controls),
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
    /// The agent wants a login before it opens a session; answer with [`Command::Authenticate`]
    /// (agent-side method) or run the terminal method and send [`Command::RetrySession`].
    AuthRequired { message: String, methods: Vec<AuthMethodInfo> },
    /// A prompt turn ended.
    Stopped(StopReason),
    /// The session ended (connection closed or error).
    Ended(Option<String>),
}

/// One way to log in, as the agent advertises it in `initialize`.
#[derive(Debug, Clone)]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Terminal method: run the agent's own command with these extra args and env in a terminal the
    /// person can see; the agent never gets `authenticate` for it. None: the `authenticate` RPC.
    pub terminal: Option<(Vec<String>, std::collections::BTreeMap<String, String>)>,
}

impl From<&AuthMethod> for AuthMethodInfo {
    fn from(m: &AuthMethod) -> Self {
        let terminal = match m {
            AuthMethod::Terminal(t) => Some((t.args.clone(), t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
            _ => None,
        };
        AuthMethodInfo {
            id: m.id().to_string(),
            name: m.name().to_string(),
            description: m.description().map(str::to_string),
            terminal,
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Prompt(String),
    /// Run an agent-side auth method (`authenticate`), then open the session again.
    Authenticate(String),
    /// Open the session again (after a terminal login ran outside the protocol).
    RetrySession,
    SetMode(String),
    SetConfig(String, ConfigValue),
    /// Stop the current turn (`session/cancel`); the agent still reports a stop reason.
    Cancel,
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
                // the agent may hand us a login to run in a terminal (its own CLI's login flow)
                let mut auth_caps = AuthCapabilities::new();
                auth_caps.terminal = true;
                let init = cx
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(ClientCapabilities::new().auth(auth_caps)),
                    )
                    .block_task()
                    .await?;
                let can_load = init.agent_capabilities.load_session;
                let auth_methods: Vec<AuthMethodInfo> = init.auth_methods.iter().map(AuthMethodInfo::from).collect();

                // Open the session and drive it until Quit. A session refused for want of a login is
                // tried again after the person logged in. `session/new` goes through `run_until`:
                // the SDK's `start_session` shuts the whole connection down when the request fails,
                // which would turn "authentication required" into a dead session.
                loop {
                    let resume = opts.resume.clone().filter(|_| can_load);
                    let attempt: Result<(), agent_client_protocol::Error> = match resume {
                        Some(id) => match cx.load_session(id, &opts.cwd).block_task().start_session().await {
                            Ok(restored) => {
                                let mut session = restored.into_session();
                                drive(&mut session, true, &commands, &events_loop).await
                            }
                            Err(e) if e.code == ErrorCode::AuthRequired => Err(e),
                            Err(e) => {
                                tracing::warn!("session/load failed, starting fresh: {e}");
                                cx.build_session(&opts.cwd)
                                    .block_task()
                                    .run_until(async |mut s| drive(&mut s, false, &commands, &events_loop).await)
                                    .await
                            }
                        },
                        None => {
                            cx.build_session(&opts.cwd)
                                .block_task()
                                .run_until(async |mut s| drive(&mut s, false, &commands, &events_loop).await)
                                .await
                        }
                    };
                    match attempt {
                        Ok(()) => return Ok(()),
                        Err(e) if e.code == ErrorCode::AuthRequired => {
                            let mut message = e.message.clone();
                            loop {
                                let _ = events_loop
                                    .send(Event::AuthRequired { message: message.clone(), methods: auth_methods.clone() })
                                    .await;
                                match commands.recv().await {
                                    Ok(Command::Authenticate(id)) => {
                                        // agent-side login (API key from its environment, …): a
                                        // failure is shown with the choices again, not fatal
                                        match cx.send_request(AuthenticateRequest::new(id)).block_task().await {
                                            Ok(_) => break,
                                            Err(err) => message = format!("login failed: {}", err.message),
                                        }
                                    }
                                    Ok(Command::RetrySession) => break,
                                    Ok(Command::Quit) | Err(_) => return Ok(()),
                                    Ok(_) => {}
                                }
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .await
    });
    let err = result.as_ref().err().map(|e| e.to_string());
    let _ = events.send_blocking(Event::Ended(err));
    result.map_err(|e| anyhow!("acp: {e}"))
}

/// Run an open session until `Command::Quit`, the commands channel closes, or the connection
/// fails. Works for fresh and restored sessions alike (the restore's replay arrives as updates).
async fn drive(
    session: &mut ActiveSession<'_, Agent>,
    resumed: bool,
    commands: &async_channel::Receiver<Command>,
    events_loop: &async_channel::Sender<Event>,
) -> Result<(), agent_client_protocol::Error> {
    let _ = events_loop
        .send(Event::Ready {
            session_id: session.session_id().to_string(),
            resumed,
            controls: controls_from(session.modes(), session.config_options()),
        })
        .await;
    let mut last_modes: Option<SessionModeState> = session.modes().cloned();
    let mut last_options: Vec<SessionConfigOption> = session.config_options().map(|o| o.to_vec()).unwrap_or_default();

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
            // a session is open: nothing to log in for, nothing to retry
            Next::Cmd(Some(Command::Authenticate(_) | Command::RetrySession)) => {}
            Next::Cmd(Some(Command::Prompt(text))) => {
                if in_turn {
                    let _ = events_loop.send(Event::Other("busy: prompt dropped".into())).await;
                    continue;
                }
                session.send_prompt(text.as_str())?;
                in_turn = true;
                let _ = events_loop.send(Event::TurnStarted).await;
            }
            Next::Cmd(Some(Command::SetMode(id))) => {
                let sid = session.session_id().clone();
                match session.connection().send_request(SetSessionModeRequest::new(sid, id.clone())).block_task().await {
                    Ok(_) => {
                        if let Some(m) = last_modes.as_mut() {
                            m.current_mode_id = id.into();
                        }
                        let _ = events_loop.send(Event::Controls(controls_from(last_modes.as_ref(), Some(&last_options)))).await;
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::Other(format!("set mode failed: {e}"))).await;
                    }
                }
            }
            Next::Cmd(Some(Command::SetConfig(config_id, value))) => {
                let sid = session.session_id().clone();
                let v = match value {
                    ConfigValue::Id(id) => SessionConfigOptionValue::value_id(id),
                    ConfigValue::Bool(b) => SessionConfigOptionValue::Boolean { value: b },
                };
                match session.connection().send_request(SetSessionConfigOptionRequest::new(sid, config_id, v)).block_task().await {
                    Ok(resp) => {
                        last_options = resp.config_options.clone();
                        let _ = events_loop.send(Event::Controls(controls_from(last_modes.as_ref(), Some(&last_options)))).await;
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::Other(format!("set option failed: {e}"))).await;
                    }
                }
            }
            Next::Cmd(Some(Command::Cancel)) => {
                if !in_turn {
                    continue;
                }
                let sid = session.session_id().clone();
                if let Err(e) = session.connection().send_notification(CancelNotification::new(sid)) {
                    let _ = events_loop.send(Event::Other(format!("cancel failed: {e}"))).await;
                }
            }
            Next::Cmd(Some(Command::Quit)) | Next::Cmd(None) => break,
            Next::Update(update) => match update? {
                SessionMessage::StopReason(reason) => {
                    in_turn = false;
                    let _ = events_loop.send(Event::Stopped(reason)).await;
                }
                SessionMessage::SessionMessage(dispatch) => {
                    let events = events_loop.clone();
                    let modes_cell = std::cell::RefCell::new(&mut last_modes);
                    let options_cell = std::cell::RefCell::new(&mut last_options);
                    MatchDispatch::new(dispatch)
                        .if_notification(async |n: SessionNotification| {
                            match n.update {
                                SessionUpdate::CurrentModeUpdate(u) => {
                                    if let Some(m) = modes_cell.borrow_mut().as_mut() {
                                        m.current_mode_id = u.current_mode_id.clone();
                                    }
                                    let c = controls_from(modes_cell.borrow().as_ref(), Some(options_cell.borrow().as_slice()));
                                    let _ = events.send(Event::Controls(c)).await;
                                }
                                SessionUpdate::ConfigOptionUpdate(u) => {
                                    **options_cell.borrow_mut() = u.config_options.clone();
                                    let c = controls_from(modes_cell.borrow().as_ref(), Some(options_cell.borrow().as_slice()));
                                    let _ = events.send(Event::Controls(c)).await;
                                }
                                other => {
                                    let _ = events.send(to_event(other)).await;
                                }
                            }
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
        // a login request ends the one-shot too: nobody is here to answer it
        let stop = matches!(ev, Event::Stopped(_) | Event::AuthRequired { .. });
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
