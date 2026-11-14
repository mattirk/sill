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
    CancelNotification, ClientCapabilities, ContentBlock, InitializeRequest, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOptions, SessionModeState,
    ImageContent, ResourceLink, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason,
    ToolCallContent,
};
use agent_client_protocol::util::MatchDispatch;
pub use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::schema::v1::{AuthCapabilities, AuthMethod, AuthenticateRequest, Meta};
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
    /// What the agent accepts in a prompt (from `initialize`), before Ready.
    Capabilities { images: bool },
    /// Modes / config options changed (by us or by the agent).
    Controls(Controls),
    /// A prompt turn started.
    TurnStarted,
    /// Replayed user message (only after a `session/load`).
    UserText(String),
    Text(String),
    Thought(String),
    /// `detail`: what the call does, for the card's body — input fields, content, output.
    /// `diff`: a file edit as a unified diff (the call's `Diff` content blocks), shown live.
    /// `parent`: the delegation (Task/Agent call) this call runs under, for a sub-agent's own calls.
    ToolCall { id: String, title: String, kind: String, status: String, detail: Option<String>, diff: Option<String>, parent: Option<String> },
    ToolCallUpdate { id: String, status: Option<String>, title: Option<String>, detail: Option<String>, diff: Option<String> },
    /// Text a sub-agent produced (shown under its delegation, not as the agent's own words).
    SubagentText { parent: String, text: String },
    /// The agent asks for permission; answer with the chosen option id (None = cancel).
    PermissionRequest { tool_call_id: String, title: String, options: Vec<PermissionOption>, reply: oneshot::Sender<Option<String>> },
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

/// Something sent along with a prompt (SPEC §3.2 attachments): a pasted screenshot, a chosen file.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub kind: AttachmentKind,
}

#[derive(Debug, Clone)]
pub enum AttachmentKind {
    /// Sent inline as an ACP image block (base64).
    Image { data_base64: String },
    /// Copied where the jail can read it; sent as a resource link plus a line of text naming the path.
    File { jail_path: std::path::PathBuf },
}

/// Standard base64 (no external crate for 20 lines).
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (chunk.get(1).copied().unwrap_or(0) as u32) << 8 | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[derive(Debug)]
pub enum Command {
    Prompt(String, Vec<Attachment>),
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
                                .send(Event::PermissionRequest { tool_call_id: req.tool_call.tool_call_id.to_string(), title: title.clone(), options: options.clone(), reply: tx })
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
                let _ = events_loop.send(Event::Capabilities { images: init.agent_capabilities.prompt_capabilities.image }).await;
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
                                drive(&mut session, true, &auth_methods, &commands, &events_loop).await
                            }
                            Err(e) if e.code == ErrorCode::AuthRequired => Err(e),
                            Err(e) => {
                                tracing::warn!("session/load failed, starting fresh: {e}");
                                open_fresh(&cx, &opts.cwd, &auth_methods, &commands, &events_loop).await
                            }
                        },
                        None => open_fresh(&cx, &opts.cwd, &auth_methods, &commands, &events_loop).await,
                    };
                    match attempt {
                        Ok(()) => return Ok(()),
                        Err(e) if e.code == ErrorCode::AuthRequired => {
                            // asked once; other commands (a prompt sent meanwhile) are dropped
                            // without asking again, a failed login asks again with its reason
                            let _ = events_loop
                                .send(Event::AuthRequired { message: e.message.clone(), methods: auth_methods.clone() })
                                .await;
                            loop {
                                match commands.recv().await {
                                    Ok(Command::Authenticate(id)) => {
                                        // agent-side login (API key from its environment, …): a
                                        // failure is shown with the choices again, not fatal
                                        match cx.send_request(AuthenticateRequest::new(id)).block_task().await {
                                            Ok(_) => break,
                                            Err(err) => {
                                                let _ = events_loop
                                                    .send(Event::AuthRequired {
                                                        message: format!("login failed: {}", err.message),
                                                        methods: auth_methods.clone(),
                                                    })
                                                    .await;
                                            }
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

/// How long `session/new` may take before the agent is assumed to be waiting for a login it
/// started on its own (Antigravity opens a browser flow at startup when it has no token and blocks
/// the request until the callback arrives — which never happens inside the jail).
const SESSION_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `session/new` and then the session loop; a request that doesn't come back in time turns into
/// `auth_required`, so the person gets the login choices instead of a silent hang.
async fn open_fresh(
    cx: &agent_client_protocol::ConnectionTo<Agent>,
    cwd: &Path,
    auth_methods: &[AuthMethodInfo],
    commands: &async_channel::Receiver<Command>,
    events_loop: &async_channel::Sender<Event>,
) -> Result<(), agent_client_protocol::Error> {
    let opened = std::rc::Rc::new(std::cell::Cell::new(false));
    let session = {
        let opened = opened.clone();
        cx.build_session(cwd).block_task().run_until(async move |mut s| {
            opened.set(true);
            drive(&mut s, false, auth_methods, commands, events_loop).await
        })
    };
    let stall = async {
        async_io::Timer::after(SESSION_OPEN_TIMEOUT).await;
        if opened.get() {
            std::future::pending::<()>().await;
        }
        let mut e = agent_client_protocol::Error::auth_required();
        e.message = format!(
            "the agent did not open a session within {} s — it is probably waiting for a login it started itself",
            SESSION_OPEN_TIMEOUT.as_secs()
        );
        Err(e)
    };
    match futures::future::select(Box::pin(session), Box::pin(stall)).await {
        futures::future::Either::Left((r, _)) => r,
        futures::future::Either::Right((r, _)) => r,
    }
}

/// Run an open session until `Command::Quit`, the commands channel closes, or the connection
/// fails. Works for fresh and restored sessions alike (the restore's replay arrives as updates).
async fn drive(
    session: &mut ActiveSession<'_, Agent>,
    resumed: bool,
    auth_methods: &[AuthMethodInfo],
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

    // The prompt request is sent by us, not through `ActiveSession::send_prompt`: that one runs
    // in an SDK task whose failure (Claude answers `auth_required` at prompt time, not at
    // session/new) shuts the whole connection down. Held here, a refused prompt is just an
    // error we can show, and the session lives on for the login and the retry.
    type PromptFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<PromptResponse, agent_client_protocol::Error>>>>;
    let mut pending: Option<PromptFut> = None;
    let mut in_turn = false;
    loop {
        // wait for a command, an update or the prompt's answer, whichever comes first; the
        // borrowing futures are dropped before `session` is used again
        enum Next {
            Cmd(Option<Command>),
            Update(Result<SessionMessage, agent_client_protocol::Error>),
            Prompt(Result<PromptResponse, agent_client_protocol::Error>),
        }
        let next = {
            let cmd_fut = Box::pin(commands.recv());
            let upd_fut = Box::pin(session.read_update());
            let inner = futures::future::select(cmd_fut, upd_fut);
            match pending.as_mut() {
                Some(p) => match futures::future::select(inner, p).await {
                    futures::future::Either::Left((futures::future::Either::Left((cmd, _)), _)) => Next::Cmd(cmd.ok()),
                    futures::future::Either::Left((futures::future::Either::Right((upd, _)), _)) => Next::Update(upd),
                    futures::future::Either::Right((r, _)) => Next::Prompt(r),
                },
                None => match inner.await {
                    futures::future::Either::Left((cmd, _)) => Next::Cmd(cmd.ok()),
                    futures::future::Either::Right((upd, _)) => Next::Update(upd),
                },
            }
        };
        match next {
            Next::Prompt(result) => {
                pending = None;
                in_turn = false;
                match result {
                    Ok(resp) => {
                        let _ = events_loop.send(Event::Stopped(resp.stop_reason)).await;
                    }
                    Err(e) if e.code == ErrorCode::AuthRequired => {
                        let _ = events_loop.send(Event::Stopped(StopReason::Cancelled)).await;
                        let _ = events_loop
                            .send(Event::AuthRequired { message: e.message.clone(), methods: auth_methods.to_vec() })
                            .await;
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::Other(format!("prompt failed: {e}"))).await;
                        let _ = events_loop.send(Event::Stopped(StopReason::Cancelled)).await;
                    }
                }
            }
            // a session is open: a login runs through `authenticate` right here, a retry is a no-op
            Next::Cmd(Some(Command::Authenticate(id))) => {
                let conn = session.connection().clone();
                match conn.send_request(AuthenticateRequest::new(id)).block_task().await {
                    Ok(_) => {
                        let _ = events_loop.send(Event::Other("logged in; send the message again".into())).await;
                    }
                    Err(e) => {
                        let _ = events_loop
                            .send(Event::AuthRequired { message: format!("login failed: {}", e.message), methods: auth_methods.to_vec() })
                            .await;
                    }
                }
            }
            Next::Cmd(Some(Command::RetrySession)) => {}
            Next::Cmd(Some(Command::Prompt(text, attachments))) => {
                if in_turn {
                    let _ = events_loop.send(Event::Other("busy: prompt dropped".into())).await;
                    continue;
                }
                let sid = session.session_id().clone();
                let conn = session.connection().clone();
                // the text first, then the attachments: images inline, files as a resource link
                // plus a line naming the path for agents that don't follow links
                let mut blocks: Vec<ContentBlock> = Vec::new();
                let mut text = text;
                for a in &attachments {
                    if let AttachmentKind::File { jail_path } = &a.kind {
                        text.push_str(&format!("\n\nAttached file: {}", jail_path.display()));
                    }
                }
                blocks.push(text.into());
                for a in attachments {
                    blocks.push(match a.kind {
                        AttachmentKind::Image { data_base64 } => ContentBlock::Image(ImageContent::new(data_base64, a.mime)),
                        AttachmentKind::File { jail_path } => ContentBlock::ResourceLink(ResourceLink::new(a.name, format!("file://{}", jail_path.display()))),
                    });
                }
                pending = Some(Box::pin(async move {
                    conn.send_request(PromptRequest::new(sid, blocks)).block_task().await
                }));
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
    cmd_tx.send_blocking(Command::Prompt(prompt.to_string(), vec![]))?;
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
    match update {
        SessionUpdate::UserMessageChunk(c) => Event::UserText(text(&c.content)),
        SessionUpdate::AgentMessageChunk(c) => match parent_tool_use(c.meta.as_ref()) {
            Some(parent) => Event::SubagentText { parent, text: text(&c.content) },
            None => Event::Text(text(&c.content)),
        },
        SessionUpdate::AgentThoughtChunk(c) => Event::Thought(text(&c.content)),
        SessionUpdate::ToolCall(t) => {
            let delegation = is_delegation(t.name.as_deref(), &t.title, t.raw_input.as_ref(), t.meta.as_ref());
            Event::ToolCall {
                id: t.tool_call_id.to_string(),
                // the title carries the mark so it survives the transcript store, which keeps no kind
                title: if delegation && !t.title.to_lowercase().contains("sub-agent") { format!("sub-agent · {}", t.title) } else { t.title.clone() },
                kind: if delegation { "Delegate".to_string() } else { format!("{:?}", t.kind) },
                status: format!("{:?}", t.status),
                detail: tool_detail(t.raw_input.as_ref(), Some(&t.content), t.raw_output.as_ref()),
                diff: tool_diff(Some(&t.content)),
                parent: parent_tool_use(t.meta.as_ref()),
            }
        }
        SessionUpdate::ToolCallUpdate(u) => Event::ToolCallUpdate {
            id: u.tool_call_id.to_string(),
            status: u.fields.status.map(|s| format!("{s:?}")),
            title: u.fields.title.clone(),
            detail: tool_detail(u.fields.raw_input.as_ref(), u.fields.content.as_deref(), u.fields.raw_output.as_ref()),
            diff: tool_diff(u.fields.content.as_deref()),
        },
        other => Event::Other(format!("{other:?}").chars().take(120).collect()),
    }
}

/// The delegation a notification belongs to: claude-agent-acp puts the Task/Agent call's id in
/// `_meta.claudeCode.parentToolUseId` on everything a sub-agent does.
fn parent_tool_use(meta: Option<&Meta>) -> Option<String> {
    meta?.get("claudeCode")?.get("parentToolUseId")?.as_str().map(str::to_string)
}

/// Does this tool call hand work to a sub-agent? ACP's stable protocol has no notion of one (the
/// sub-agents RFD is unstable, SPEC §4.1), so the agent's own tool is recognised: claude-agent-acp's
/// `_meta.claudeCode.toolName` (`Task`/`Agent`), the tool name (Codex's `spawn_agent`, …) or input
/// fields that only a delegation has.
fn is_delegation(name: Option<&str>, title: &str, input: Option<&serde_json::Value>, meta: Option<&Meta>) -> bool {
    if let Some(tool) = meta.and_then(|m| m.get("claudeCode")).and_then(|c| c.get("toolName")).and_then(|v| v.as_str())
        && matches!(tool, "Task" | "Agent")
    {
        return true;
    }
    let name = name.unwrap_or("").to_lowercase();
    if matches!(name.as_str(), "task" | "agent" | "subagent" | "spawn_agent" | "delegate" | "run_agent") || name.contains("subagent") {
        return true;
    }
    if let Some(serde_json::Value::Object(m)) = input {
        if m.contains_key("subagent_type") || m.contains_key("agent_type") || (m.contains_key("prompt") && m.contains_key("description")) {
            return true;
        }
    }
    let t = title.to_lowercase();
    t.starts_with("task:") || t.starts_with("task(") || t.starts_with("subagent") || t.starts_with("agent(")
}

/// A file edit as a unified diff: every `Diff` content block (path, old text → new text; a new file
/// has no old text), clipped. `None` without one.
fn tool_diff(content: Option<&[ToolCallContent]>) -> Option<String> {
    const LIMIT: usize = 40_000;
    let mut parts: Vec<String> = Vec::new();
    for c in content.unwrap_or_default() {
        if let ToolCallContent::Diff(d) = c {
            let name = d.path.display().to_string();
            let old = d.old_text.as_deref().unwrap_or("");
            let from = if d.old_text.is_some() { format!("a/{name}") } else { "/dev/null (new file)".to_string() };
            parts.push(crate::changes::text_diff(old.as_bytes(), d.new_text.as_bytes(), &from, &format!("b/{name}")));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut joined = parts.join("\n");
    if joined.chars().count() > LIMIT {
        joined = joined.chars().take(LIMIT).collect();
        joined.push_str("\n… (clipped)");
    }
    Some(joined)
}

/// Text for a tool card's body: the input as `key: value` lines (pretty JSON when nested), the
/// content blocks (text as is), the output; each part clipped. A `Diff` block is left to
/// [`tool_diff`], and so is the input then (an edit's input is the diff).
fn tool_detail(input: Option<&serde_json::Value>, content: Option<&[ToolCallContent]>, output: Option<&serde_json::Value>) -> Option<String> {
    const LIMIT: usize = 3000;
    fn clip(s: String) -> String {
        if s.chars().count() > LIMIT {
            let mut t: String = s.chars().take(LIMIT).collect();
            t.push_str("…");
            t
        } else {
            s
        }
    }
    fn json(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(m) if m.values().all(|x| !x.is_object() && !x.is_array()) => m
                .iter()
                .map(|(k, x)| match x {
                    serde_json::Value::String(t) => format!("{k}: {t}"),
                    other => format!("{k}: {other}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            serde_json::Value::String(t) => t.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let has_diff = content.unwrap_or_default().iter().any(|c| matches!(c, ToolCallContent::Diff(_)));
    if let Some(v) = input
        && !has_diff
    {
        parts.push(clip(json(v)));
    }
    for c in content.unwrap_or_default() {
        match c {
            ToolCallContent::Content(c) => parts.push(clip(text(&c.content))),
            ToolCallContent::Terminal(_) => parts.push("[terminal]".into()),
            _ => {}
        }
    }
    if let Some(v) = output {
        parts.push(clip(json(v)));
    }
    let joined = parts.into_iter().filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
    if joined.is_empty() { None } else { Some(joined) }
}

fn text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        other => format!("[{}]", kind_name(other)),
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
