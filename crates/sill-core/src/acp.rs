//! ACP client over the shim's agent stream (SPEC §4.1). Runtime-neutral: the SDK is driven with
//! `futures::executor::block_on` on a plain thread; the socket is wrapped by `async-io`. The caller
//! talks to the running session through channels: [`Command`]s in, [`Event`]s out. Updates are read
//! continuously (not only during a turn), so agent-initiated updates and `session/load` replays reach
//! the caller too. Permission requests carry a one-shot reply channel; the dispatch loop waits for the
//! answer, as the agent does.

use std::os::unix::net::UnixStream;
use std::path::Path;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp_v1;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction,
    ElicitationCapabilities, ElicitationContentValue, ElicitationFormCapabilities, ElicitationMode, ElicitationPropertySchema, ElicitationSchema, ImageContent,
    InitializeRequest, MultiSelectItems, PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResourceLink, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionModeState, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, ToolCallContent,
};
use agent_client_protocol::util::MatchDispatch;
use serde::{Deserialize, Serialize};

// The agent's side as sill keeps it: ACP reduced to what sill shows. These are sill-core's own,
// not the frontend protocol's (`sill_proto::wire` has its own copies, converted in `crate::wire`):
// the private relay protocol and the daemon's stored runner state carry them, so their serde form
// is that protocol's (docs/protocol-history.md §7.1) and changes only with a relay protocol bump.

/// What a permission option means (ACP `PermissionOptionKind`, the same strings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind sill does not know (a newer ACP, or an agent's own `_…`): its string as is.
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// Why a turn ended (ACP `StopReason`, the same strings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    /// A reason sill does not know (a newer ACP): its string as is.
    #[serde(untagged)]
    Other(String),
}

/// ACP's kind as sill's (the same names on the wire).
fn option_kind(k: acp_v1::PermissionOptionKind) -> PermissionOptionKind {
    match k {
        acp_v1::PermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
        acp_v1::PermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
        acp_v1::PermissionOptionKind::RejectOnce => PermissionOptionKind::RejectOnce,
        acp_v1::PermissionOptionKind::RejectAlways => PermissionOptionKind::RejectAlways,
        other => {
            PermissionOptionKind::Other(serde_json::to_value(other).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{other:?}")))
        }
    }
}

/// ACP's stop reason as sill's.
fn stop_reason(r: acp_v1::StopReason) -> StopReason {
    match r {
        acp_v1::StopReason::EndTurn => StopReason::EndTurn,
        acp_v1::StopReason::MaxTokens => StopReason::MaxTokens,
        acp_v1::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        acp_v1::StopReason::Refusal => StopReason::Refusal,
        acp_v1::StopReason::Cancelled => StopReason::Cancelled,
        other => StopReason::Other(serde_json::to_value(other).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{other:?}"))),
    }
}
use agent_client_protocol::schema::v1::{AuthCapabilities, AuthMethod, AuthenticateRequest, Meta};
use agent_client_protocol::{ActiveSession, Agent, ByteStreams, Client, ErrorCode, SessionMessage};
use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt;
use futures::channel::oneshot;

/// The agent's adjustable session settings (SPEC: modes + config options), as the UI shows them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Controls {
    /// (current mode id, [(id, name)])
    pub modes: Option<(String, Vec<(String, String)>)>,
    pub options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// `mode` | `model` | `model_config` | `thought_level` | other
    pub category: String,
    pub kind: ConfigKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigKind {
    /// (current value id, [(value id, name)])
    Select(String, Vec<(String, String)>),
    Boolean(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigValue {
    Id(String),
    Bool(bool),
}

fn controls_from(modes: Option<&SessionModeState>, options: Option<&[SessionConfigOption]>) -> Controls {
    let modes = modes.map(|m| (m.current_mode_id.to_string(), m.available_modes.iter().map(|x| (x.id.to_string(), x.name.clone())).collect()));
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
                        SessionConfigSelectOptions::Grouped(g) => {
                            g.iter().flat_map(|grp| grp.options.iter().map(move |x| (x.value.to_string(), format!("{} · {}", grp.name, x.name)))).collect()
                        }
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

/// One field of a form the agent asks the person to fill (ACP `elicitation/create`, form mode —
/// Claude's AskUserQuestion arrives this way: one choice field per question plus an "Other" text).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    pub title: String,
    pub description: String,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FieldKind {
    /// One of these: (value, title, description).
    Choice(Vec<(String, String, String)>),
    /// Any number of these.
    Choices(Vec<(String, String, String)>),
    Text,
    Bool,
    Number,
}

/// The form's fields from its JSON schema: what sill can show, in the schema's order.
pub fn form_fields(schema: &ElicitationSchema) -> Vec<FormField> {
    let opts = |o: &[agent_client_protocol::schema::v1::EnumOption]| -> Vec<(String, String, String)> {
        o.iter().map(|e| (e.value.clone(), e.title.clone(), e.description.clone().unwrap_or_default())).collect()
    };
    let plain = |v: &[String]| -> Vec<(String, String, String)> { v.iter().map(|s| (s.clone(), s.clone(), String::new())).collect() };
    schema
        .properties
        .iter()
        .map(|(key, p)| {
            let (title, description, kind) = match p {
                ElicitationPropertySchema::String(s) => (
                    s.title.clone(),
                    s.description.clone(),
                    match (&s.one_of, &s.enum_values) {
                        (Some(o), _) if !o.is_empty() => FieldKind::Choice(opts(o)),
                        (_, Some(v)) if !v.is_empty() => FieldKind::Choice(plain(v)),
                        _ => FieldKind::Text,
                    },
                ),
                ElicitationPropertySchema::Array(m) => (
                    m.title.clone(),
                    m.description.clone(),
                    match &m.items {
                        MultiSelectItems::Titled(t) => FieldKind::Choices(opts(&t.options)),
                        MultiSelectItems::String(s) => FieldKind::Choices(plain(&s.values)),
                        _ => FieldKind::Text,
                    },
                ),
                ElicitationPropertySchema::Boolean(b) => (b.title.clone(), b.description.clone(), FieldKind::Bool),
                ElicitationPropertySchema::Number(n) => (n.title.clone(), n.description.clone(), FieldKind::Number),
                ElicitationPropertySchema::Integer(n) => (n.title.clone(), n.description.clone(), FieldKind::Number),
                _ => (None, None, FieldKind::Text),
            };
            FormField { key: key.clone(), title: title.unwrap_or_default(), description: description.unwrap_or_default(), kind }
        })
        .collect()
}

/// What the caller sees while a session runs.
#[derive(Debug)]
pub enum Event {
    /// Native ACP v2 evidence. Message identities and patch semantics are retained to SFP.
    V2Ready {
        session_id: String,
        resumed: bool,
        controls: Controls,
    },
    V2Update {
        update: serde_json::Value,
    },
    V2PermissionsCancelled,
    PromptAccepted {
        message_id: String,
    },
    V2PromptFailed {
        code: String,
        message: String,
    },
    V2PermissionRequest {
        tool_call_id: Option<String>,
        title: String,
        subject: Option<serde_json::Value>,
        options: Vec<PermissionOption>,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The ACP session is up: (agent session id, resumed with history replay?).
    Ready {
        session_id: String,
        resumed: bool,
        controls: Controls,
    },
    /// What the agent accepts in a prompt (from `initialize`), before Ready.
    Capabilities {
        images: bool,
    },
    /// Modes / config options changed (by us or by the agent).
    Controls(Controls),
    /// A settings request was rejected by the agent. `setting` is `mode` or `config:<option id>`.
    ControlFailed {
        setting: String,
        reason: String,
    },
    /// A prompt turn started.
    TurnStarted,
    /// A turn the agent started with no prompt open, as the agent itself says ([`ActivitySource`]):
    /// Claude Code answering a background task's notification, a Codex goal turn. Only between
    /// prompt turns, and apart from them: no `TurnStarted` or `Stopped` belongs to it.
    AgentTurn(AgentTurn),
    /// Replayed user message (only after a `session/load`).
    UserText(String),
    Text(String),
    Thought(String),
    /// `detail`: what the call does, for the card's body — input fields, content, output.
    /// `diff`: a file edit as a unified diff (the call's `Diff` content blocks), shown live.
    /// `parent`: the delegation (Task/Agent call) this call runs under, for a sub-agent's own calls.
    /// `command` is what the agent declared as the call's command ([`input_command`]), when it did.
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        detail: Option<String>,
        diff: Option<String>,
        parent: Option<String>,
        command: Option<String>,
    },
    ToolCallUpdate {
        id: String,
        status: Option<String>,
        title: Option<String>,
        detail: Option<String>,
        diff: Option<String>,
        command: Option<String>,
    },
    /// Text a sub-agent produced (shown under its delegation, not as the agent's own words).
    SubagentText {
        parent: String,
        text: String,
    },
    /// An image block in the agent's own message (`parent` names a sub-agent's tool call).
    /// The daemon keeps the bytes as a session upload; they never enter a conversation record.
    Image {
        mime: String,
        data_base64: String,
        parent: Option<String>,
    },
    /// Frontend only: kept agent images by session upload, as `sfp_client` renders them.
    AgentImages(Vec<UserAttachment>),
    /// The agent asks for permission; answer with the chosen option id (None = cancel).
    PermissionRequest {
        tool_call_id: String,
        title: String,
        options: Vec<PermissionOption>,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The daemon answered a permission request itself (the session's auto-allow, docs/daemon.md
    /// §3.3): `allowed` false when no allow option was offered and it was refused. Never sent by
    /// [`run_session`]; a frontend marks the call's line with it.
    AutoAnswered {
        tool_call_id: String,
        title: String,
        allowed: bool,
    },
    /// The agent asks the person to fill a form (a multiple-choice question, say); answer with the
    /// values by field key — strings, arrays of strings, booleans, numbers — or None to cancel.
    Elicitation {
        message: String,
        fields: Vec<FormField>,
        reply: oneshot::Sender<Option<std::collections::BTreeMap<String, serde_json::Value>>>,
    },
    Other(String),
    /// Context used so far, of the model's window, in tokens (`usage_update`, after each turn
    /// from agents that report it: Claude, Codex, Antigravity…).
    Usage {
        used: u64,
        size: u64,
    },
    /// The agent wants a login before it opens a session; answer with [`Command::Authenticate`]
    /// (agent-side method) or run the terminal method and send [`Command::RetrySession`].
    AuthRequired {
        message: String,
        methods: Vec<AuthMethodInfo>,
    },
    /// A prompt turn ended.
    Stopped(StopReason),
    /// The session ended (connection closed or error).
    Ended(Option<String>),
    /// The agent's whole current plan (ACP `plan`: each update replaces the last).
    Plan(Vec<PlanEntry>),
    /// The slash commands the agent offers now (ACP `available_commands_update`).
    Commands(Vec<AgentCommand>),
    /// The agent named or renamed the session (ACP `session_info_update` with a title).
    AgentTitle(String),
    /// Something the agent wants shown (ACP `notice`): severity as ACP's string.
    Notice {
        severity: String,
        title: String,
        description: Option<String>,
    },
    /// A prompt as a frontend shows it, with its attachments. Never sent by [`run_session`]: the
    /// daemon's view of a submission (`sfp_client`) produces it.
    UserMessage {
        text: String,
        attachments: Vec<UserAttachment>,
    },
}

/// An attachment as a view shows it: inline data, or an upload fetched from the daemon on display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAttachment {
    pub name: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

/// One step of the agent's plan (ACP `PlanEntry`): status and priority as ACP's strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// A slash command the agent offers (ACP `AvailableCommand`); `hint` describes free-text input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    pub name: String,
    pub description: String,
    pub hint: Option<String>,
}

/// One way to log in, as the agent advertises it in `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMethodInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Terminal method: run the agent's own command with these extra args and env in a terminal the
    /// person can see; the agent never gets `authenticate` for it. None: the `authenticate` RPC.
    pub terminal: Option<(Vec<String>, std::collections::BTreeMap<String, String>)>,
    /// The method takes an API key the person types (`_meta["api-key"]`, Gemini CLI and codex-acp):
    /// the provider it names, empty when none. The key goes along with [`Command::AuthenticateKey`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// The agent's login method as sill keeps it.
fn auth_method_info(m: &AuthMethod) -> AuthMethodInfo {
    let terminal = match m {
        AuthMethod::Terminal(t) => Some((t.args.clone(), t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
        _ => None,
    };
    let api_key = m.meta().and_then(|meta| meta.get("api-key")).map(|mark| mark.get("provider").and_then(|p| p.as_str()).unwrap_or_default().to_string());
    AuthMethodInfo { id: m.id().to_string(), name: m.name().to_string(), description: m.description().map(str::to_string), terminal, api_key }
}

/// `authenticate` with an API key the daemon saved in `key_file` (read here, at the last moment:
/// commands carry the file, never the key). Adapters read the key from `_meta` in different
/// shapes: Gemini CLI `{"api-key": "<key>"}`, codex-acp `{"api-key": {"apiKey": "<key>"}}`. The
/// order matters (both checked against the real adapters, 2026-11-03): codex-acp refuses Gemini's
/// shape ("CODEX_API_KEY or OPENAI_API_KEY is not set") so its own goes next, but Gemini CLI
/// accepts codex's shape without reading a key and only fails when the session opens. Neither
/// checks the key itself at login: a wrong one shows at the first prompt. The last refusal is the
/// reason given.
async fn authenticate_with_key(cx: &agent_client_protocol::ConnectionTo<Agent>, method: &str, key_file: &Path) -> Result<(), String> {
    let key = std::fs::read_to_string(key_file).map_err(|e| format!("the saved API key could not be read: {e}"))?;
    let key = key.trim();
    let mut refused = String::from("the agent refused the API key");
    for shape in [serde_json::json!({"api-key": key}), serde_json::json!({"api-key": {"apiKey": key}})] {
        let Ok(meta) = serde_json::from_value::<Meta>(shape) else { continue };
        match cx.send_request(AuthenticateRequest::new(method.to_string()).meta(meta)).block_task().await {
            Ok(_) => return Ok(()),
            Err(e) => refused = e.message,
        }
    }
    Err(refused)
}

/// Something sent along with a prompt (SPEC §3.2 attachments): a pasted screenshot, a chosen file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub kind: AttachmentKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// `authenticate` a method marked `api_key` with the key saved in `key_file` (see
    /// [`AuthMethodInfo::api_key`]); the file is read when the request goes, never carried.
    AuthenticateKey {
        method: String,
        key_file: std::path::PathBuf,
    },
    /// Open the session again (after a terminal login ran outside the protocol).
    RetrySession,
    SetMode(String),
    SetConfig(String, ConfigValue),
    /// Stop the current turn (`session/cancel`); the agent still reports a stop reason.
    Cancel,
    Quit,
}

/// How permission requests are answered when no one listens for [`Event::PermissionRequest`]
/// (the M0 CLI). The daemon handles them itself.
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
    /// The session's pinned folders, sent as ACP `additionalDirectories` to an agent that
    /// advertises them: `cwd` is always home, and an agent that confines its tools to its
    /// workspace roots (Antigravity) refuses the pins without this.
    pub roots: Vec<std::path::PathBuf>,
    pub auto: AutoPermission,
    /// Resume this agent session with `session/load` if the agent supports it.
    pub resume: Option<String>,
}

/// Run an ACP session over `stream` until `Command::Quit` or the connection ends. Blocking; run it
/// on its own thread.
pub fn run_session(stream: UnixStream, opts: SessionOptions, commands: async_channel::Receiver<Command>, events: async_channel::Sender<Event>) -> Result<()> {
    stream.set_nonblocking(true)?;
    let stream = async_io::Async::new(stream).context("async socket")?;
    let (reader, writer) = stream.split();
    let transport = ByteStreams::new(writer, reader);
    let events_perm = events.clone();
    let events_form = events.clone();
    let events_loop = events.clone();
    let auto = opts.auto;
    // every session's notifications in the order they came: the open session's and its subagents'
    let session_open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (notified_tx, notified_rx) = async_channel::unbounded();
    let notifications = SessionNotifications { open: session_open.clone(), queue: notified_tx };

    let result: Result<(), agent_client_protocol::Error> = futures::executor::block_on(async move {
        Client
            .builder()
            .name("sill")
            .with_handler(notifications)
            .on_receive_request(
                async move |req: RequestPermissionRequest, responder, _cx| {
                    let options: Vec<PermissionOption> =
                        req.options.iter().map(|o| PermissionOption { id: o.option_id.to_string(), name: o.name.clone(), kind: option_kind(o.kind) }).collect();
                    let title = req.tool_call.fields.title.clone().unwrap_or_else(|| req.tool_call.tool_call_id.to_string());
                    let pick = |kinds: &[PermissionOptionKind]| kinds.iter().find_map(|k| options.iter().find(|o| &o.kind == k)).map(|o| o.id.clone());
                    let chosen: Option<String> = match auto {
                        AutoPermission::AllowOnce => pick(&[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]),
                        AutoPermission::Reject => pick(&[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]),
                        AutoPermission::Ask => {
                            let (tx, rx) = oneshot::channel();
                            let _ = events_perm
                                .send(Event::PermissionRequest {
                                    tool_call_id: req.tool_call.tool_call_id.to_string(),
                                    title: title.clone(),
                                    options: options.clone(),
                                    reply: tx,
                                })
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
            .on_receive_request(
                async move |req: CreateElicitationRequest, responder, _cx| {
                    // a form: shown as a card, answered with the values; anything else (a URL flow)
                    // is declined — nothing in the jail could open it anyway
                    let ElicitationMode::Form(form) = req.mode else {
                        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Decline));
                    };
                    if auto != AutoPermission::Ask {
                        return responder.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
                    }
                    let fields = form_fields(&form.requested_schema);
                    let (tx, rx) = oneshot::channel();
                    let _ = events_form.send(Event::Elicitation { message: req.message.clone(), fields, reply: tx }).await;
                    let action = match rx.await.unwrap_or(None) {
                        Some(values) => {
                            let content: std::collections::BTreeMap<String, ElicitationContentValue> = values
                                .into_iter()
                                .filter_map(|(k, v)| {
                                    let v = match v {
                                        serde_json::Value::String(s) => ElicitationContentValue::String(s),
                                        serde_json::Value::Bool(b) => ElicitationContentValue::Boolean(b),
                                        serde_json::Value::Number(n) => ElicitationContentValue::Number(n.as_f64()?),
                                        serde_json::Value::Array(a) => {
                                            ElicitationContentValue::StringArray(a.into_iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                                        }
                                        _ => return None,
                                    };
                                    Some((k, v))
                                })
                                .collect();
                            ElicitationAction::Accept(ElicitationAcceptAction::new().content(content))
                        }
                        None => ElicitationAction::Cancel,
                    };
                    responder.respond(CreateElicitationResponse::new(action))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async move |cx| {
                // the agent may hand us a login to run in a terminal (its own CLI's login flow)
                let mut auth_caps = AuthCapabilities::new();
                auth_caps.terminal = true;
                // forms: the agent's multiple-choice questions come as elicitations (SPEC §3.3)
                // subagents: an agent that knows them announces each as a session of its own
                // (`subagent_update`) and sends that session's updates on this connection
                let caps = ClientCapabilities::new()
                    .auth(auth_caps)
                    .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()))
                    .subagents(acp_v1::SubagentCapabilities::new());
                let init = cx.send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps)).block_task().await?;
                let can_load = init.agent_capabilities.load_session;
                let source = ActivitySource::of(init.agent_info.as_ref().map(|agent| agent.name.as_str()));
                let roots = if init.agent_capabilities.session_capabilities.additional_directories.is_some() { opts.roots.clone() } else { vec![] };
                let _ = events_loop.send(Event::Capabilities { images: init.agent_capabilities.prompt_capabilities.image }).await;
                let auth_methods: Vec<AuthMethodInfo> = init.auth_methods.iter().map(auth_method_info).collect();

                // Open the session and drive it until Quit. A session refused for want of a login is
                // tried again after the person logged in. `session/new` goes through `run_until`:
                // the SDK's `start_session` shuts the whole connection down when the request fails,
                // which would turn "authentication required" into a dead session.
                loop {
                    let resume = opts.resume.clone().filter(|_| can_load);
                    let link = SessionLink { source, open: &session_open, notified: &notified_rx };
                    let attempt: Result<(), agent_client_protocol::Error> = match resume {
                        Some(id) => match cx
                            .load_session_from(
                                agent_client_protocol::schema::v1::LoadSessionRequest::new(id, opts.cwd.as_path())
                                    .additional_directories(roots.clone())
                                    .meta(source.session_meta()),
                            )
                            .block_task()
                            .start_session()
                            .await
                        {
                            Ok(restored) => {
                                let mut session = restored.into_session();
                                drive(&mut session, true, link, &auth_methods, &commands, &events_loop).await
                            }
                            Err(e) if e.code == ErrorCode::AuthRequired => Err(e),
                            Err(e) => {
                                tracing::warn!("session/load failed, starting fresh: {e}");
                                open_fresh(&cx, &opts.cwd, &roots, link, &auth_methods, &commands, &events_loop).await
                            }
                        },
                        None => open_fresh(&cx, &opts.cwd, &roots, link, &auth_methods, &commands, &events_loop).await,
                    };
                    match attempt {
                        Ok(()) => return Ok(()),
                        Err(e) if e.code == ErrorCode::AuthRequired => {
                            // asked once; other commands (a prompt sent meanwhile) are dropped
                            // without asking again, a failed login asks again with its reason
                            let _ = events_loop.send(Event::AuthRequired { message: e.message.clone(), methods: auth_methods.clone() }).await;
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
                                    Ok(Command::AuthenticateKey { method, key_file }) => match authenticate_with_key(&cx, &method, &key_file).await {
                                        Ok(()) => break,
                                        Err(reason) => {
                                            let _ = events_loop
                                                .send(Event::AuthRequired { message: format!("login failed: {reason}"), methods: auth_methods.clone() })
                                                .await;
                                        }
                                    },
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

/// A step of a turn the agent started itself ([`Event::AgentTurn`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentTurn {
    Started,
    /// The agent said it is idle, or a prompt went out and took the turn over.
    Ended,
    /// The person stopped it. No prompt is open whose answer would say so, so the cancel is sent
    /// and the turn ends at once.
    Cancelled,
}

/// What an agent says of its own work: ACP v2's `state_update`, which v1 lacks. On v1 only an
/// agent's own extension can say it, read by an [`ActivitySource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Running,
    Idle,
}

/// Where an ACP v1 agent says that it works with no prompt open (a turn it starts itself, for a
/// background task's notification or a goal). Chosen by the name the agent gives at `initialize`.
/// An agent without one is never shown working outside a prompt: sill does not guess from output
/// or open tool calls (a background command keeps its call open across turns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivitySource {
    None,
    /// claude-agent-acp, asked with session `_meta.claudeCode.emitRawSDKMessages`, forwards Claude
    /// Code's `session_state_changed` messages as `_claude/sdkMessage` notifications.
    Claude,
    /// codex-acp sends each thread status as `session_info_update` with `_meta.codex.threadStatus`.
    Codex,
}

impl ActivitySource {
    fn of(agent: Option<&str>) -> Self {
        match agent.unwrap_or_default().rsplit('/').next().unwrap_or_default() {
            "claude-agent-acp" | "claude-code-acp" => Self::Claude,
            "codex-acp" => Self::Codex,
            _ => Self::None,
        }
    }

    /// `_meta` for `session/new` and `session/load`, when the agent must be asked for its state.
    fn session_meta(self) -> Option<serde_json::Map<String, serde_json::Value>> {
        (self == Self::Claude).then(|| {
            let mut meta = serde_json::Map::new();
            meta.insert("claudeCode".into(), serde_json::json!({"emitRawSDKMessages": [{"type": "system", "subtype": "session_state_changed"}]}));
            meta
        })
    }

    /// The state in a message that is no session update (Claude's `_claude/sdkMessage`).
    fn notification(self, dispatch: &agent_client_protocol::Dispatch) -> Option<Activity> {
        let agent_client_protocol::Dispatch::Notification(n) = dispatch else { return None };
        if self != Self::Claude || n.method() != "_claude/sdkMessage" {
            return None;
        }
        let message = n.params().get("message")?;
        if message.get("type")? != "system" || message.get("subtype")? != "session_state_changed" {
            return None;
        }
        // `requires_action` (a permission asked) is work too; the question itself makes it waiting
        Some(if message.get("state")?.as_str()? == "idle" { Activity::Idle } else { Activity::Running })
    }

    /// The state a session update carries (Codex's thread status). `active` is a running turn,
    /// with flags one that waits for an approval; every other status has none.
    fn update(self, update: &SessionUpdate) -> Option<Activity> {
        let SessionUpdate::SessionInfoUpdate(info) = update else { return None };
        if self != Self::Codex {
            return None;
        }
        let status = info.meta.as_ref()?.get("codex")?.get("threadStatus")?.get("type")?.as_str()?;
        Some(if status == "active" { Activity::Running } else { Activity::Idle })
    }
}

/// The turns an agent starts itself, from what it says of its state ([`Activity`]) and what the
/// loop knows of its own prompt. Each method answers with the [`Event::AgentTurn`] to send, if any.
#[derive(Debug, Default)]
struct AgentTurns {
    /// The agent's last word: it works.
    running: bool,
    /// A turn of the agent's own is open.
    open: bool,
    /// The person stopped the agent, and it has neither gone idle nor been prompted since.
    halted: bool,
}

impl AgentTurns {
    /// The agent said its state. Its running while a prompt is open is that prompt's.
    fn said(&mut self, activity: Activity, in_prompt: bool) -> Option<AgentTurn> {
        let was = std::mem::replace(&mut self.running, activity == Activity::Running);
        match activity {
            Activity::Running if !was && !in_prompt && !self.open => {
                self.open = true;
                Some(AgentTurn::Started)
            }
            Activity::Idle => {
                self.halted = false;
                std::mem::take(&mut self.open).then_some(AgentTurn::Ended)
            }
            _ => None,
        }
    }

    /// The model spoke or called a tool. An agent that never said idle after its prompt (Claude
    /// Code holds it back while background agents run) starts its next turn without a new running:
    /// by its own word it works, and this output is that turn's.
    fn output(&mut self, in_prompt: bool) -> Option<AgentTurn> {
        (self.running && !in_prompt && !self.open).then(|| {
            self.open = true;
            self.halted = false;
            AgentTurn::Started
        })
    }

    /// A subagent called a tool: the agent's work too, as [`Self::output`]. Not after a stop:
    /// a subagent that the cancel did not reach works on alone, and its calls are no turn of
    /// the agent's until the agent itself speaks.
    fn subagent_output(&mut self, in_prompt: bool) -> Option<AgentTurn> {
        if self.halted { None } else { self.output(in_prompt) }
    }

    /// A prompt goes out: the agent takes it into the turn it runs, which is the prompt's from here.
    fn prompt(&mut self) -> Option<AgentTurn> {
        self.halted = false;
        std::mem::take(&mut self.open).then_some(AgentTurn::Ended)
    }

    /// The person stopped the agent. Its own turn, if one is open, ends here, not at an idle the
    /// agent may never say. An agent that works on regardless still says it runs, and its next
    /// output opens a turn again ([`Self::output`]); its subagents' calls do not
    /// ([`Self::subagent_output`]).
    fn stopped(&mut self) -> Option<AgentTurn> {
        self.halted = true;
        std::mem::take(&mut self.open).then_some(AgentTurn::Cancelled)
    }
}

/// What `drive` needs of the connection beside its session: the agent's kind, and every
/// session's notifications ([`SessionNotifications`]).
#[derive(Clone, Copy)]
struct SessionLink<'a> {
    /// Where the agent says that it works with no prompt open.
    source: ActivitySource,
    open: &'a std::sync::atomic::AtomicBool,
    notified: &'a async_channel::Receiver<agent_client_protocol::Dispatch>,
}

/// Takes every notification that names a session, once one is open, and queues it for `drive`.
/// The SDK gives a session only its own, and a subagent's (ACP subagent sessions) come on the
/// same connection under the subagent's session id: one queue keeps the order they were sent in.
struct SessionNotifications {
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
    queue: async_channel::Sender<agent_client_protocol::Dispatch>,
}

impl<Counterpart: agent_client_protocol::Role> agent_client_protocol::HandleDispatchFrom<Counterpart> for SessionNotifications {
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        _connection: agent_client_protocol::ConnectionTo<Counterpart>,
    ) -> Result<agent_client_protocol::Handled<agent_client_protocol::Dispatch>, agent_client_protocol::Error> {
        if !self.open.load(std::sync::atomic::Ordering::SeqCst) || notified_session(&message).is_none() {
            return Ok(agent_client_protocol::Handled::No { message, retry: false });
        }
        let _ = self.queue.try_send(message);
        Ok(agent_client_protocol::Handled::Yes)
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "SessionNotifications"
    }
}

/// The session a notification names.
fn notified_session(dispatch: &agent_client_protocol::Dispatch) -> Option<&str> {
    let agent_client_protocol::Dispatch::Notification(n) = dispatch else { return None };
    n.params().get("sessionId")?.as_str()
}

/// The id of a subagent's card in the conversation: the delegation its calls and text go under.
/// A subagent session has no tool call of its own, so the card is named by the session.
pub fn subagent_card(session: &str) -> String {
    format!("subagent:{session}")
}

/// A subagent the agent announced (ACP `subagent_update`).
#[derive(Debug, Default)]
struct Subagent {
    /// The subagent session that announced it; none for the open session's own.
    parent: Option<String>,
    title: Option<String>,
    description: Option<String>,
    /// It takes a `session/cancel` of its own (its `cancel` capability).
    cancel: bool,
    /// Its last state was idle: its work ended.
    idle: bool,
}

impl Subagent {
    /// The card's title: the mark that makes it a delegation, then what the agent calls it.
    fn card_title(&self) -> String {
        let name = self.title.as_deref().or(self.description.as_deref()).map(str::trim).filter(|name| !name.is_empty()).unwrap_or("subagent");
        format!("sub-agent · {name}")
    }
}

/// A message between two sessions, as one session's transcript has it.
#[derive(Debug, Default)]
struct Relayed {
    sender: Option<String>,
    recipient: Option<String>,
    text: String,
    /// How much of `text` the conversation has.
    shown: usize,
}

/// The agent's subagents as ACP subagent sessions tell them, turned into what the conversation
/// already shows for a delegation: a `Delegate` card per subagent ([`subagent_card`]), the brief
/// as its body, the subagent's calls and words under it. Each method answers with the events
/// to send.
#[derive(Debug, Default)]
struct Subagents {
    by_session: std::collections::HashMap<String, Subagent>,
    /// By the session whose transcript it is in, and its id there.
    messages: std::collections::HashMap<(String, String), Relayed>,
}

/// The card's status for a subagent's reported state.
fn subagent_status(state: &acp_v1::StateUpdate) -> &'static str {
    use acp_v1::{IdleStopReason, StateUpdate};
    match state {
        StateUpdate::Running(_) => "InProgress",
        StateUpdate::RequiresAction(_) => "RequiresAction",
        StateUpdate::Idle(idle) => match idle.stop_reason {
            None | Some(IdleStopReason::EndTurn) => "Completed",
            Some(_) => "Failed",
        },
        _ => "Unknown",
    }
}

/// A patch field onto what is kept: omitted leaves it, `null` clears it. True when it changed.
fn patch(kept: &mut Option<String>, field: agent_client_protocol::schema::MaybeUndefined<String>) -> bool {
    use agent_client_protocol::schema::MaybeUndefined;
    let new = match field {
        MaybeUndefined::Undefined => return false,
        MaybeUndefined::Null => None,
        MaybeUndefined::Value(value) => Some(value),
    };
    let changed = *kept != new;
    *kept = new;
    changed
}

impl Subagents {
    /// `announcer` (a subagent session, none for the open session) announced a subagent or
    /// changed what it says of one.
    fn announced(&mut self, announcer: Option<&str>, update: acp_v1::SubagentUpdate) -> Vec<Event> {
        use agent_client_protocol::schema::MaybeUndefined;
        let session = update.session_id.to_string();
        let id = subagent_card(&session);
        let status = match &update.state {
            MaybeUndefined::Value(state) => Some(subagent_status(state)),
            MaybeUndefined::Null => Some("Unknown"),
            MaybeUndefined::Undefined => None,
        };
        let known = self.by_session.contains_key(&session);
        let subagent = self.by_session.entry(session).or_insert_with(|| Subagent { parent: announcer.map(str::to_string), ..Subagent::default() });
        let renamed = patch(&mut subagent.title, update.title) | patch(&mut subagent.description, update.description);
        match &update.capabilities {
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => subagent.cancel = false,
            MaybeUndefined::Value(capabilities) => subagent.cancel = capabilities.cancel.is_some(),
        }
        if let MaybeUndefined::Value(state) = &update.state {
            subagent.idle = matches!(state, acp_v1::StateUpdate::Idle(_));
        }
        if !known {
            return vec![Event::ToolCall {
                id,
                title: subagent.card_title(),
                kind: "Delegate".into(),
                // no state yet: announced, its work not confirmed
                status: status.unwrap_or("Pending").into(),
                detail: None,
                diff: None,
                parent: subagent.parent.as_deref().map(subagent_card),
                command: None,
            }];
        }
        if !renamed && status.is_none() {
            return vec![];
        }
        vec![Event::ToolCallUpdate {
            id,
            status: status.map(str::to_string),
            title: renamed.then(|| subagent.card_title()),
            detail: None,
            diff: None,
            command: None,
        }]
    }

    /// The subagent sessions a stop must reach: those still at work that take a cancel. The
    /// open session's cancel is not theirs (an agent may let a background subagent run on).
    fn to_cancel(&self) -> Vec<String> {
        let mut sessions: Vec<String> =
            self.by_session.iter().filter(|(_, subagent)| subagent.cancel && !subagent.idle).map(|(session, _)| session.clone()).collect();
        sessions.sort();
        sessions
    }

    /// A message between sessions, as `session`'s transcript has it (whole: content replaces).
    fn message(&mut self, session: &str, message: acp_v1::SessionMessage) -> Vec<Event> {
        use agent_client_protocol::schema::MaybeUndefined;
        let relayed = self.messages.entry((session.to_string(), message.message_id.to_string())).or_default();
        patch(&mut relayed.sender, message.sender_session_id.map_value(|id| id.to_string()));
        patch(&mut relayed.recipient, message.recipient_session_id.map_value(|id| id.to_string()));
        match message.content {
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => relayed.text.clear(),
            MaybeUndefined::Value(blocks) => relayed.text = blocks.iter().map(text).collect::<Vec<_>>().join("\n"),
        }
        self.show_message(session, &message.message_id.to_string())
    }

    /// One more block of a message in `session`'s transcript.
    fn message_chunk(&mut self, session: &str, chunk: acp_v1::SessionMessageChunk) -> Vec<Event> {
        let relayed = self.messages.entry((session.to_string(), chunk.message_id.to_string())).or_default();
        relayed.text.push_str(&text(&chunk.content));
        self.show_message(session, &chunk.message_id.to_string())
    }

    /// What a message adds to the conversation. Both ends have it in their transcripts; the
    /// sender's is the one shown. To a subagent of the sender it is the brief, the body of that
    /// subagent's card; from a subagent to anyone else it is what the subagent said.
    fn show_message(&mut self, session: &str, id: &str) -> Vec<Event> {
        let Some(relayed) = self.messages.get_mut(&(session.to_string(), id.to_string())) else { return vec![] };
        let (Some(sender), Some(recipient)) = (relayed.sender.as_deref(), relayed.recipient.as_deref()) else { return vec![] };
        if sender != session || relayed.text.len() == relayed.shown {
            return vec![];
        }
        let from_subagent = self.by_session.contains_key(sender);
        let brief = self.by_session.get(recipient).is_some_and(|to| match &to.parent {
            Some(parent) => parent == sender,
            None => !from_subagent,
        });
        let event = if brief {
            Event::ToolCallUpdate { id: subagent_card(recipient), status: None, title: None, detail: Some(relayed.text.clone()), diff: None, command: None }
        } else if from_subagent {
            // content that replaced what was shown starts over
            let from = if relayed.text.is_char_boundary(relayed.shown) && relayed.shown < relayed.text.len() { relayed.shown } else { 0 };
            Event::SubagentText { parent: subagent_card(sender), text: relayed.text[from..].to_string() }
        } else {
            return vec![];
        };
        relayed.shown = relayed.text.len();
        vec![event]
    }

    /// An update of a session that is not the open one. Only an announced subagent's is shown:
    /// its calls and words under its card. Its settings, usage and plan are its own, not the
    /// session's, and are left out.
    fn child_update(&mut self, session: &str, update: SessionUpdate) -> Vec<Event> {
        if !self.by_session.contains_key(session) {
            return vec![];
        }
        match update {
            SessionUpdate::SubagentUpdate(u) => self.announced(Some(session), u),
            SessionUpdate::SessionMessage(m) => self.message(session, m),
            SessionUpdate::SessionMessageChunk(c) => self.message_chunk(session, c),
            SessionUpdate::AgentMessageChunk(c) => vec![match c.content {
                ContentBlock::Image(i) => Event::Image { mime: i.mime_type, data_base64: i.data, parent: Some(subagent_card(session)) },
                content => Event::SubagentText { parent: subagent_card(session), text: text(&content) },
            }],
            call @ SessionUpdate::ToolCall(_) => match to_event(call) {
                Event::ToolCall { id, title, kind, status, detail, diff, command, .. } => {
                    vec![Event::ToolCall { id, title, kind, status, detail, diff, parent: Some(subagent_card(session)), command }]
                }
                _ => vec![],
            },
            update @ SessionUpdate::ToolCallUpdate(_) => vec![to_event(update)],
            _ => vec![],
        }
    }
}

/// How long `session/new` may take before the agent is assumed to be waiting for a login it
/// started on its own (Antigravity opens a browser flow at startup when it has no token and blocks
/// the request until the callback arrives — which never happens inside the jail).
const SESSION_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// [`SESSION_OPEN_TIMEOUT`], or `SILL_SESSION_OPEN_TIMEOUT_MS` (tests: tests/login.rs).
fn session_open_timeout() -> std::time::Duration {
    std::env::var("SILL_SESSION_OPEN_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).map(std::time::Duration::from_millis).unwrap_or(SESSION_OPEN_TIMEOUT)
}

/// `session/new` and then the session loop; a request that doesn't come back in time turns into
/// `auth_required`, so the person gets the login choices instead of a silent hang.
async fn open_fresh(
    cx: &agent_client_protocol::ConnectionTo<Agent>,
    cwd: &Path,
    roots: &[std::path::PathBuf],
    link: SessionLink<'_>,
    auth_methods: &[AuthMethodInfo],
    commands: &async_channel::Receiver<Command>,
    events_loop: &async_channel::Sender<Event>,
) -> Result<(), agent_client_protocol::Error> {
    let opened = std::rc::Rc::new(std::cell::Cell::new(false));
    let session = {
        let opened = opened.clone();
        cx.build_session_from(
            agent_client_protocol::schema::v1::NewSessionRequest::new(cwd).additional_directories(roots.to_vec()).meta(link.source.session_meta()),
        )
        .block_task()
        .run_until(async move |mut s| {
            opened.set(true);
            drive(&mut s, false, link, auth_methods, commands, events_loop).await
        })
    };
    let stall = async {
        let timeout = session_open_timeout();
        async_io::Timer::after(timeout).await;
        if opened.get() {
            std::future::pending::<()>().await;
        }
        let mut e = agent_client_protocol::Error::auth_required();
        let within = if timeout.subsec_millis() == 0 { format!("{} s", timeout.as_secs()) } else { format!("{} ms", timeout.as_millis()) };
        e.message = format!("the agent did not open a session within {within} — it is probably waiting for a login it started itself");
        Err(e)
    };
    match futures::future::select(Box::pin(session), Box::pin(stall)).await {
        futures::future::Either::Left((r, _)) => r,
        futures::future::Either::Right((r, _)) => r,
    }
}

/// Lenient adapters (Gemini CLI, codex-acp) accept any key at `authenticate` and only fail on the
/// first `session/prompt`, sometimes with a provider error message instead of `-32000`.
fn is_prompt_auth_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    let normalized = lower.replace(['_', '-'], " ").replace("apikey", "api key");
    [
        "unauthorized",
        "authentication error",
        "authentication failed",
        "unauthenticated",
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "expired api key",
        "revoked api key",
        "api key not valid",
        "api key is not valid",
        "api key invalid",
        "api key is invalid",
        "api key expired",
        "api key has expired",
        "api key revoked",
        "api key was revoked",
        "api key not provided",
        "api key was not provided",
    ]
    .iter()
    .any(|s| normalized.contains(s))
}

/// Run an open session until `Command::Quit`, the commands channel closes, or the connection
/// fails. Works for fresh and restored sessions alike (the restore's replay arrives as updates).
async fn drive(
    session: &mut ActiveSession<'_, Agent>,
    resumed: bool,
    link: SessionLink<'_>,
    auth_methods: &[AuthMethodInfo],
    commands: &async_channel::Receiver<Command>,
    events_loop: &async_channel::Sender<Event>,
) -> Result<(), agent_client_protocol::Error> {
    // from here every session's notifications come by one queue, in order; those the SDK gave
    // the session before this are read first (below)
    let source = link.source;
    link.open.store(true, std::sync::atomic::Ordering::SeqCst);
    let main_session = session.session_id().to_string();
    let _ = events_loop
        .send(Event::Ready { session_id: session.session_id().to_string(), resumed, controls: controls_from(session.modes(), session.config_options()) })
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
    // turns the agent starts itself, as its `source` tells them (`Event::AgentTurn`)
    let agent_turns = std::cell::RefCell::new(AgentTurns::default());
    // the subagents the agent announced, and what each shows under its card
    let subagents = std::cell::RefCell::new(Subagents::default());
    // update kinds this build can't parse, each noted once
    let mut unknown_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        // wait for a command, an update or the prompt's answer, whichever comes first; the
        // borrowing futures are dropped before `session` is used again
        enum Next {
            Cmd(Option<Command>),
            Update(Result<SessionMessage, agent_client_protocol::Error>),
            Prompt(Result<PromptResponse, agent_client_protocol::Error>),
        }
        // Updates before commands: a set-mode/set-option request is awaited below without reading
        // updates, so the notifications the agent sends while handling it (a config snapshot of
        // that moment) are still queued when it answers. Read them before the next request goes
        // out, or that older snapshot lands after the newer request's response and undoes it.
        // What the SDK queued for the session before `link.notified` took over comes first.
        let next = {
            use futures::future::Either::{Left, Right};
            let cmd_fut = Box::pin(commands.recv());
            let upd_fut = Box::pin(session.read_update());
            let child_fut = Box::pin(link.notified.recv());
            let inner = futures::future::select(upd_fut, futures::future::select(child_fut, cmd_fut));
            let other = |next| match next {
                Left((Ok(dispatch), _)) => Next::Update(Ok(SessionMessage::SessionMessage(dispatch))),
                // the sender lives as long as the connection does
                Left((Err(_), _)) => Next::Cmd(None),
                Right((cmd, _)) => Next::Cmd(Result::ok(cmd)),
            };
            match pending.as_mut() {
                Some(p) => match futures::future::select(inner, p).await {
                    Left((Left((upd, _)), _)) => Next::Update(upd),
                    Left((Right((next, _)), _)) => other(next),
                    Right((r, _)) => Next::Prompt(r),
                },
                None => match inner.await {
                    Left((upd, _)) => Next::Update(upd),
                    Right((next, _)) => other(next),
                },
            }
        };
        match next {
            Next::Prompt(result) => {
                pending = None;
                in_turn = false;
                match result {
                    Ok(resp) => {
                        let _ = events_loop.send(Event::Stopped(stop_reason(resp.stop_reason))).await;
                    }
                    Err(e) if e.code == ErrorCode::AuthRequired || (!auth_methods.is_empty() && is_prompt_auth_error(&e.message)) => {
                        // A failed RPC does not establish cancellation or successful insertion.
                        let _ = events_loop.send(Event::Stopped(StopReason::Other("_sill_prompt_error".into()))).await;
                        let _ = events_loop.send(Event::AuthRequired { message: e.message.clone(), methods: auth_methods.to_vec() }).await;
                    }
                    Err(e) if agent_client_protocol::is_incoming_transport_closed(&e) || agent_client_protocol::is_connection_terminated(&e) => {
                        // A dead connection cannot accept another prompt. End this
                        // worker so the daemon can create a new connection on retry.
                        // Do not replay a request whose execution is uncertain.
                        return Err(e);
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::Other(format!("prompt failed: {e}"))).await;
                        // A failed RPC does not establish cancellation or successful insertion.
                        let _ = events_loop.send(Event::Stopped(StopReason::Other("_sill_prompt_error".into()))).await;
                    }
                }
            }
            // a session is open: a login runs through `authenticate` right here, a retry is a no-op
            Next::Cmd(Some(Command::Authenticate(id))) => {
                let conn = session.connection().clone();
                match conn.send_request(AuthenticateRequest::new(id)).block_task().await {
                    Ok(_) => {
                        let _ = events_loop.send(Event::Other("_sill_authenticated".into())).await;
                        let _ = events_loop.send(Event::Other("logged in; send the message again".into())).await;
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::AuthRequired { message: format!("login failed: {}", e.message), methods: auth_methods.to_vec() }).await;
                    }
                }
            }
            Next::Cmd(Some(Command::AuthenticateKey { method, key_file })) => {
                let conn = session.connection().clone();
                match authenticate_with_key(&conn, &method, &key_file).await {
                    Ok(()) => {
                        let _ = events_loop.send(Event::Other("_sill_authenticated".into())).await;
                        let _ = events_loop.send(Event::Other("logged in; send the message again".into())).await;
                    }
                    Err(reason) => {
                        let _ = events_loop.send(Event::AuthRequired { message: format!("login failed: {reason}"), methods: auth_methods.to_vec() }).await;
                    }
                }
            }
            Next::Cmd(Some(Command::RetrySession)) => {}
            Next::Cmd(Some(Command::Prompt(text, attachments))) => {
                if in_turn {
                    let _ = events_loop.send(Event::Other("busy: prompt dropped".into())).await;
                    continue;
                }
                let ended = agent_turns.borrow_mut().prompt();
                if let Some(step) = ended {
                    let _ = events_loop.send(Event::AgentTurn(step)).await;
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
                pending = Some(Box::pin(async move { conn.send_request(PromptRequest::new(sid, blocks)).block_task().await }));
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
                        let _ = events_loop.send(Event::ControlFailed { setting: "mode".into(), reason: format!("set mode failed: {e}") }).await;
                    }
                }
            }
            Next::Cmd(Some(Command::SetConfig(config_id, value))) => {
                let sid = session.session_id().clone();
                let v = match value {
                    ConfigValue::Id(id) => SessionConfigOptionValue::value_id(id),
                    ConfigValue::Bool(b) => SessionConfigOptionValue::Boolean { value: b },
                };
                let setting = format!("config:{config_id}");
                match session.connection().send_request(SetSessionConfigOptionRequest::new(sid, config_id, v)).block_task().await {
                    Ok(resp) => {
                        last_options = resp.config_options.clone();
                        let _ = events_loop.send(Event::Controls(controls_from(last_modes.as_ref(), Some(&last_options)))).await;
                    }
                    Err(e) => {
                        let _ = events_loop.send(Event::ControlFailed { setting, reason: format!("set option failed: {e}") }).await;
                    }
                }
            }
            Next::Cmd(Some(Command::Cancel)) => {
                let children = subagents.borrow().to_cancel();
                if !in_turn && !agent_turns.borrow().open && children.is_empty() {
                    continue;
                }
                // the session's own, then each subagent's that is still at work
                let sid = session.session_id().clone();
                for sid in std::iter::once(sid).chain(children.into_iter().map(Into::into)) {
                    if let Err(e) = session.connection().send_notification(CancelNotification::new(sid)) {
                        let _ = events_loop.send(Event::Other(format!("cancel failed: {e}"))).await;
                    }
                }
                let stopped = agent_turns.borrow_mut().stopped();
                if let Some(step) = stopped {
                    let _ = events_loop.send(Event::AgentTurn(step)).await;
                }
            }
            Next::Cmd(Some(Command::Quit)) | Next::Cmd(None) => break,
            Next::Update(update) => match update? {
                SessionMessage::StopReason(reason) => {
                    in_turn = false;
                    let _ = events_loop.send(Event::Stopped(stop_reason(reason))).await;
                }
                // another session's: a subagent's, shown under its card
                SessionMessage::SessionMessage(dispatch) if notified_session(&dispatch).is_some_and(|session| session != main_session) => {
                    let shown = std::cell::RefCell::new(Vec::new());
                    let parsed = MatchDispatch::new(dispatch)
                        .if_notification(async |n: SessionNotification| {
                            *shown.borrow_mut() = subagents.borrow_mut().child_update(&n.session_id.to_string(), n.update);
                            Ok(())
                        })
                        .await
                        .otherwise_ignore();
                    match parsed {
                        // one of a kind this build does not know is skipped, as the session's own are
                        Ok(()) => {}
                        Err(e) if e.code == ErrorCode::InvalidParams => {}
                        Err(e) => return Err(e),
                    }
                    let shown = shown.into_inner();
                    // a subagent's call is the agent's work too ([`AgentTurns::output`])
                    let called = shown.iter().any(|event| matches!(event, Event::ToolCall { .. }));
                    let step = called.then(|| agent_turns.borrow_mut().subagent_output(in_turn)).flatten();
                    if let Some(step) = step {
                        let _ = events_loop.send(Event::AgentTurn(step)).await;
                    }
                    for event in shown {
                        let _ = events_loop.send(event).await;
                    }
                }
                SessionMessage::SessionMessage(dispatch) => {
                    if let Some(activity) = source.notification(&dispatch) {
                        let step = agent_turns.borrow_mut().said(activity, in_turn);
                        if let Some(step) = step {
                            let _ = events_loop.send(Event::AgentTurn(step)).await;
                        }
                        continue;
                    }
                    let events = events_loop.clone();
                    let modes_cell = std::cell::RefCell::new(&mut last_modes);
                    let options_cell = std::cell::RefCell::new(&mut last_options);
                    let dispatched = MatchDispatch::new(dispatch)
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
                                SessionUpdate::SubagentUpdate(u) => {
                                    let shown = subagents.borrow_mut().announced(None, u);
                                    for event in shown {
                                        let _ = events.send(event).await;
                                    }
                                }
                                SessionUpdate::SessionMessage(m) => {
                                    let shown = subagents.borrow_mut().message(&main_session, m);
                                    for event in shown {
                                        let _ = events.send(event).await;
                                    }
                                }
                                SessionUpdate::SessionMessageChunk(c) => {
                                    let shown = subagents.borrow_mut().message_chunk(&main_session, c);
                                    for event in shown {
                                        let _ = events.send(event).await;
                                    }
                                }
                                other => {
                                    let said = source.update(&other).and_then(|activity| agent_turns.borrow_mut().said(activity, in_turn));
                                    let event = to_event(other);
                                    let spoke = matches!(event, Event::Text(_) | Event::Thought(_) | Event::ToolCall { .. });
                                    let step = said.or_else(|| spoke.then(|| agent_turns.borrow_mut().output(in_turn)).flatten());
                                    // a start goes before what the update itself shows, an end after
                                    if step == Some(AgentTurn::Started) {
                                        let _ = events.send(Event::AgentTurn(AgentTurn::Started)).await;
                                    }
                                    let _ = events.send(event).await;
                                    if let Some(step) = step.filter(|step| *step != AgentTurn::Started) {
                                        let _ = events.send(Event::AgentTurn(step)).await;
                                    }
                                }
                            }
                            Ok(())
                        })
                        .await
                        .otherwise_ignore();
                    match dispatched {
                        Ok(()) => {}
                        // an update of a kind this build does not know (the protocol grows; the
                        // typed handler can't parse it): skipped, not a dead session — the agent
                        // goes on, and so does the turn (`fake-agent.py` sends one to prove it)
                        Err(e) if e.code == ErrorCode::InvalidParams => {
                            // the kind's name is in the error's data (`unknown variant `x`, expected…`)
                            let detail = e.data.as_ref().and_then(|d| d.get("error")).and_then(|v| v.as_str()).unwrap_or(&e.message);
                            let kind = detail.split('`').nth(1).unwrap_or("?").to_string();
                            if unknown_kinds.insert(kind.clone()) {
                                let _ = events_loop.send(Event::Other(format!("skipped `{kind}` updates: this sill doesn't know them yet"))).await;
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

/// One prompt in a fresh session (M0 CLI).
pub fn run_prompt(stream: UnixStream, cwd: &Path, prompt: &str, auto: AutoPermission, mut on_event: impl FnMut(Event) + Send + 'static) -> Result<()> {
    let (cmd_tx, cmd_rx) = async_channel::bounded(2);
    let (ev_tx, ev_rx) = async_channel::unbounded();
    cmd_tx.send_blocking(Command::Prompt(prompt.to_string(), vec![]))?;
    let opts = SessionOptions { cwd: cwd.to_path_buf(), roots: vec![], auto, resume: None };
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

/// An `Event::Other` that is not for people: a `_sill…` marker, or `to_event`'s `Debug` dump of
/// an update kind it does not render (`SessionInfoUpdate(SessionInfoUpdate { … })`).
pub fn is_diagnostic(text: &str) -> bool {
    if text.starts_with("_sill") {
        return true;
    }
    let name: String = text.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    name.starts_with(|c: char| c.is_ascii_uppercase()) && text[name.len()..].starts_with('(')
}

fn to_event(update: SessionUpdate) -> Event {
    match update {
        SessionUpdate::UserMessageChunk(c) => Event::UserText(text(&c.content)),
        SessionUpdate::AgentMessageChunk(c) => match (parent_tool_use(c.meta.as_ref()), c.content) {
            (parent, ContentBlock::Image(i)) => Event::Image { mime: i.mime_type, data_base64: i.data, parent },
            (Some(parent), content) => Event::SubagentText { parent, text: text(&content) },
            (None, content) => Event::Text(text(&content)),
        },
        SessionUpdate::AgentThoughtChunk(c) => Event::Thought(text(&c.content)),
        SessionUpdate::UsageUpdate(u) => Event::Usage { used: u.used, size: u.size },
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
                command: input_command(t.raw_input.as_ref()),
            }
        }
        SessionUpdate::ToolCallUpdate(u) => Event::ToolCallUpdate {
            id: u.tool_call_id.to_string(),
            status: u.fields.status.map(|s| format!("{s:?}")),
            title: u.fields.title.clone(),
            detail: tool_detail(u.fields.raw_input.as_ref(), u.fields.content.as_deref(), u.fields.raw_output.as_ref()),
            diff: tool_diff(u.fields.content.as_deref()),
            command: input_command(u.fields.raw_input.as_ref()),
        },
        SessionUpdate::Plan(p) => Event::Plan(
            p.entries.iter().map(|e| PlanEntry { content: e.content.clone(), status: acp_string(&e.status), priority: acp_string(&e.priority) }).collect(),
        ),
        SessionUpdate::AvailableCommandsUpdate(u) => Event::Commands(
            u.available_commands
                .iter()
                .map(|c| AgentCommand {
                    name: c.name.clone(),
                    description: c.description.clone(),
                    hint: match &c.input {
                        Some(acp_v1::AvailableCommandInput::Unstructured(i)) => Some(i.hint.clone()),
                        _ => None,
                    },
                })
                .collect(),
        ),
        SessionUpdate::SessionInfoUpdate(u) => match u.title {
            agent_client_protocol::schema::MaybeUndefined::Value(title) if !title.trim().is_empty() => Event::AgentTitle(title),
            // Codex sends one after every turn with only `_meta` (thread status): nothing to show.
            _ => Event::Other(format!("_sill_session_info {:?}", u.updated_at).chars().take(120).collect()),
        },
        SessionUpdate::Notice(n) => Event::Notice { severity: acp_string(&n.severity), title: n.title.clone(), description: n.description.clone() },
        other => Event::Other(format!("{other:?}").chars().take(120).collect()),
    }
}

/// An ACP enum's wire string (`in_progress`, `high`, `warning`…), whatever the variant.
fn acp_string(value: &impl Serialize) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unknown".into())
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
/// The command a tool call's input declares: the text of its `command` field (ACP `rawInput`),
/// as Claude's and Codex's adapters send it for a shell call. Only what the agent declared: a
/// call's title is display text and is never read as a command. Whether the call is a shell
/// call is its kind's to say; the daemon pairs the two (`sfp::Tool::command`).
pub fn input_command(input: Option<&serde_json::Value>) -> Option<String> {
    let command = input?.get("command")?.as_str()?;
    (!command.trim().is_empty()).then(|| command.to_owned())
}

fn is_delegation(name: Option<&str>, title: &str, input: Option<&serde_json::Value>, meta: Option<&Meta>) -> bool {
    if let Some(tool) = meta.and_then(|m| m.get("claudeCode")).and_then(|c| c.get("toolName")).and_then(|v| v.as_str())
        && matches!(tool, "Task" | "Agent")
    {
        return true;
    }
    let name = name.unwrap_or("").to_lowercase();
    if matches!(name.as_str(), "task" | "agent" | "subagent" | "spawn_agent" | "spawnagent" | "delegate" | "run_agent") || name.contains("subagent") {
        return true;
    }
    if let Some(serde_json::Value::Object(m)) = input
        && (m.contains_key("subagent_type") || m.contains_key("agent_type") || (m.contains_key("prompt") && m.contains_key("description")))
    {
        return true;
    }
    let t = title.to_lowercase();
    matches!(t.as_str(), "spawnagent" | "spawn_agent")
        || t.starts_with("start subagent ")
        || t.starts_with("complete subagent ")
        || t.starts_with("interact with subagent ")
        || t.starts_with("interrupt subagent ")
        || t.starts_with("task:")
        || t.starts_with("task(")
        || t.starts_with("subagent")
        || t.starts_with("agent(")
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
            t.push('…');
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

#[cfg(test)]
mod tests {
    #[test]
    fn provider_auth_detection_excludes_quota_and_permission_errors() {
        for text in ["API key not valid", "invalid_api_key", "API key expired", "401 Unauthorized", "authentication_error"] {
            assert!(super::is_prompt_auth_error(text), "{text}");
        }
        for text in ["Quota exceeded for API key", "API key has insufficient permissions", "Rate limit exceeded", "API key billing limit reached"] {
            assert!(!super::is_prompt_auth_error(text), "{text}");
        }
    }

    use super::*;

    fn update(json: serde_json::Value) -> Event {
        to_event(serde_json::from_value(json).unwrap())
    }

    /// A shell call's command travels apart from its title: it is the `command` the agent
    /// declared in the call's input. Claude's adapter reports a Bash call first while its input
    /// streams (no command yet, titled "Terminal") and refines it; Codex declares it at once.
    #[test]
    fn a_tool_calls_declared_command_travels_apart_from_its_title() {
        use serde_json::json;
        let started = update(json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Terminal","kind":"execute","status":"pending","rawInput":{}}));
        assert!(matches!(&started, Event::ToolCall { command: None, title, kind, .. } if title == "Terminal" && kind == "Execute"), "{started:?}");
        let refined = update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"List the files","kind":"execute","rawInput":{"command":"ls; pwd","description":"List the files"}}),
        );
        assert!(matches!(&refined, Event::ToolCallUpdate { command: Some(c), title: Some(t), .. } if c == "ls; pwd" && t == "List the files"), "{refined:?}");
        let codex = update(
            json!({"sessionUpdate":"tool_call","toolCallId":"t2","title":"git status","kind":"execute","status":"in_progress","rawInput":{"command":"git status","cwd":"/tmp"}}),
        );
        assert!(matches!(&codex, Event::ToolCall { command: Some(c), .. } if c == "git status"), "{codex:?}");
        let finished = update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t2","status":"completed"}));
        assert!(matches!(&finished, Event::ToolCallUpdate { command: None, .. }), "{finished:?}");
        // only text is a command line: an argument list, a blank and a bare string declare none
        assert_eq!(input_command(Some(&json!({"command":["bash","-lc","ls"]}))), None);
        assert_eq!(input_command(Some(&json!({"command":"  "}))), None);
        assert_eq!(input_command(Some(&json!("ls"))), None);
        assert_eq!(input_command(None), None);
    }

    #[test]
    fn plans_commands_titles_and_notices_are_typed_not_dumped() {
        let plan = update(serde_json::json!({"sessionUpdate":"plan","entries":[
            {"content":"read","priority":"high","status":"completed"},{"content":"fix","priority":"medium","status":"in_progress"}]}));
        assert!(matches!(&plan, Event::Plan(e) if e.len()==2 && e[1].status=="in_progress" && e[0].priority=="high"), "{plan:?}");
        let commands = update(serde_json::json!({"sessionUpdate":"available_commands_update","availableCommands":[
            {"name":"review","description":"Review changes","input":{"hint":"what to review"}},{"name":"init","description":"Write AGENTS.md"}]}));
        assert!(matches!(&commands, Event::Commands(c) if c[0].hint.as_deref()==Some("what to review") && c[1].hint.is_none()), "{commands:?}");
        assert!(
            matches!(update(serde_json::json!({"sessionUpdate":"session_info_update","title":"Fix the hotkey"})), Event::AgentTitle(t) if t=="Fix the hotkey")
        );
        // Codex: after every turn, only `_meta`; the old dump of this became a visible note
        let codex = update(serde_json::json!({"sessionUpdate":"session_info_update","_meta":{"codex":{"threadStatus":"idle"}}}));
        assert!(matches!(&codex, Event::Other(t) if is_diagnostic(t)), "{codex:?}");
        let notice = update(serde_json::json!({"sessionUpdate":"notice","severity":"warning","title":"Rate limited","description":"retrying"}));
        assert!(
            matches!(&notice, Event::Notice { severity, title, description: Some(d) } if severity=="warning" && title=="Rate limited" && d=="retrying"),
            "{notice:?}"
        );
    }

    #[test]
    fn delegation_adapter_spellings() {
        for title in ["spawnAgent", "Start subagent researcher", "Complete subagent researcher"] {
            assert!(is_delegation(None, title, None, None), "{title}");
        }
        assert!(is_delegation(Some("spawn_agent"), "Research", None, None));
        let meta = serde_json::from_value(serde_json::json!({"claudeCode": {"toolName": "Agent"}})).unwrap();
        assert!(is_delegation(None, "Research", None, Some(&meta)));
        assert!(!is_delegation(None, "Read src/main.rs", None, None));
        let meta = serde_json::from_value(serde_json::json!({"claudeCode": {"parentToolUseId": "task-1"}})).unwrap();
        assert_eq!(parent_tool_use(Some(&meta)).as_deref(), Some("task-1"));
    }

    /// Claude's AskUserQuestion as the adapter sends it: a titled choice per question, an "Other"
    /// text each, a multi-select as an array of titled items.
    #[test]
    fn form_fields_from_the_adapters_schema() {
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "q1": { "type": "string", "title": "Database", "description": "Which one?", "oneOf": [
                    { "const": "Postgres", "title": "Postgres", "description": "the default" },
                    { "const": "SQLite", "title": "SQLite" }
                ]},
                "q1_custom": { "type": "string", "title": "Other", "description": "Type your own answer" },
                "q2": { "type": "array", "title": "Features", "items": { "anyOf": [
                    { "const": "auth", "title": "Auth" }, { "const": "search", "title": "Search" }
                ]}},
                "ok": { "type": "boolean", "title": "Proceed" },
                "n": { "type": "integer", "title": "Workers" }
            }
        }))
        .unwrap();
        let f = form_fields(&schema);
        let keys: Vec<&str> = f.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["n", "ok", "q1", "q1_custom", "q2"]); // the schema map is ordered by key
        let q1 = f.iter().find(|x| x.key == "q1").unwrap();
        assert_eq!((q1.title.as_str(), q1.description.as_str()), ("Database", "Which one?"));
        match &q1.kind {
            FieldKind::Choice(o) => assert_eq!(o[0], ("Postgres".into(), "Postgres".into(), "the default".into())),
            k => panic!("{k:?}"),
        }
        assert!(matches!(f.iter().find(|x| x.key == "q1_custom").unwrap().kind, FieldKind::Text));
        assert!(matches!(&f.iter().find(|x| x.key == "q2").unwrap().kind, FieldKind::Choices(o) if o.len() == 2));
        assert!(matches!(f.iter().find(|x| x.key == "ok").unwrap().kind, FieldKind::Bool));
        assert!(matches!(f.iter().find(|x| x.key == "n").unwrap().kind, FieldKind::Number));
    }

    /// A scripted ACP v1 agent named `name` on a socket pair: after each prompt's answer, and once
    /// the test has seen it, `after(prompt number)` gives the lines it sends on its own. Returns
    /// what the loop reported: turns, stops, agent turns and text. With `stop`, the test cancels
    /// the first agent turn as it starts, and the agent goes on only once the cancel reached it:
    /// the session's, then one for each subagent `after` announced with the `cancel` capability,
    /// which the agent then reports idle as cancelled. A subagent `c2` calls a tool before the
    /// agent itself speaks again.
    fn agent_turns_seen(name: &'static str, prompts: usize, stop: bool, after: fn(usize) -> Vec<serde_json::Value>) -> Vec<String> {
        use std::io::{BufRead, Write};
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let agent = std::thread::spawn(move || {
            let mut out = theirs.try_clone().unwrap();
            let mut send = |v: serde_json::Value| writeln!(out, "{v}").unwrap();
            let mut prompt = 0;
            let mut lines = std::io::BufReader::new(theirs).lines();
            while let Some(line) = lines.next() {
                let m: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                let id = m["id"].clone();
                match m["method"].as_str() {
                    Some("initialize") => send(
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[],"agentInfo":{"name":name,"version":"1"}}}),
                    ),
                    Some("session/new") => {
                        // only Claude is asked for its state messages
                        let asked = m["params"]["_meta"]["claudeCode"]["emitRawSDKMessages"][0]["subtype"] == "session_state_changed";
                        assert_eq!(asked, name.ends_with("claude-agent-acp"), "{}", m["params"]);
                        send(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s1"}}));
                    }
                    Some("session/prompt") => {
                        prompt += 1;
                        send(said(name, "running"));
                        send(text("answer"));
                        send(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                        go_rx.recv().unwrap();
                        // the subagents a stop must cancel: announced with `cancel`, not idle since
                        let mut children = std::collections::BTreeSet::new();
                        for line in after(prompt) {
                            let update = &line["params"]["update"];
                            if let Some(child) = update["sessionId"].as_str() {
                                if update["capabilities"]["cancel"].is_object() {
                                    children.insert(child.to_string());
                                }
                                if update["state"]["state"] == "idle" {
                                    children.remove(child);
                                }
                            }
                            send(line);
                        }
                        if stop {
                            let cancel: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                            assert_eq!(cancel["method"], "session/cancel");
                            assert_eq!(cancel["params"]["sessionId"], "s1");
                            for child in children {
                                let cancel: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                                assert_eq!(cancel["method"], "session/cancel");
                                assert_eq!(cancel["params"]["sessionId"], child);
                                send(sent(
                                    "s1",
                                    serde_json::json!({"sessionUpdate":"subagent_update","sessionId":child,"state":{"state":"idle","stopReason":"cancelled"}}),
                                ));
                            }
                            send(sent("c2", call("late")));
                            send(text("still working"));
                        }
                        send(text("end"));
                    }
                    _ if !id.is_null() => send(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}})),
                    _ => {}
                }
            }
        });
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let opts = SessionOptions { cwd: std::env::temp_dir(), roots: vec![], auto: AutoPermission::Reject, resume: None };
        let worker = std::thread::spawn(move || run_session(ours, opts, cmd_rx, ev_tx));
        let mut seen = Vec::new();
        let cancelled = std::cell::Cell::new(false);
        let until = |seen: &mut Vec<String>, done: &dyn Fn(&Event) -> bool| loop {
            let ev = ev_rx.recv_blocking().unwrap();
            let last = done(&ev);
            match ev {
                Event::TurnStarted => seen.push("turn".into()),
                Event::Stopped(_) => seen.push("stopped".into()),
                Event::AgentTurn(step) => {
                    seen.push(format!("agent {step:?}").to_lowercase());
                    if step == AgentTurn::Started && stop && !cancelled.replace(true) {
                        cmd_tx.send_blocking(Command::Cancel).unwrap();
                    }
                }
                Event::Text(t) if t != "end" => seen.push(t),
                Event::ToolCall { id, title, status, parent, .. } => seen.push(format!("call {id} [{title}] {status} in {}", parent.as_deref().unwrap_or("-"))),
                Event::ToolCallUpdate { id, status, detail, .. } => {
                    seen.push(format!("update {id} {} {}", status.as_deref().unwrap_or("-"), detail.as_deref().unwrap_or("-")));
                }
                Event::SubagentText { parent, text } => seen.push(format!("{parent} said {text}")),
                Event::Controls(_) => seen.push("controls".into()),
                _ => {}
            }
            if last {
                break;
            }
        };
        for _ in 0..prompts {
            cmd_tx.send_blocking(Command::Prompt("hi".into(), vec![])).unwrap();
            until(&mut seen, &|e| matches!(e, Event::Stopped(_)));
            go_tx.send(()).unwrap();
            until(&mut seen, &|e| matches!(e, Event::Text(t) if t == "end"));
        }
        cmd_tx.send_blocking(Command::Quit).unwrap();
        worker.join().unwrap().unwrap();
        drop(agent);
        seen
    }

    fn text(t: &str) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":t}}}})
    }

    /// The agent's own word of its state, in the form its adapter sends it.
    fn said(name: &str, state: &str) -> serde_json::Value {
        if name.ends_with("codex-acp") {
            let status = if state == "running" { serde_json::json!({"type":"active","activeFlags":[]}) } else { serde_json::json!({"type":state}) };
            serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"session_info_update","_meta":{"codex":{"threadStatus":status}}}}})
        } else {
            serde_json::json!({"jsonrpc":"2.0","method":"_claude/sdkMessage","params":{"sessionId":"s1","message":{"type":"system","subtype":"session_state_changed","state":state}}})
        }
    }

    const CLAUDE: &str = "@agentclientprotocol/claude-agent-acp";
    const CODEX: &str = "@agentclientprotocol/codex-acp";

    /// Claude Code's turn after its prompt returned (a background task's notification) is an
    /// `AgentTurn`: from its own `running` to `idle`, or from the model's output when Claude Code
    /// held its idle back (background agents) and so never said `running` again.
    #[test]
    fn a_turn_claude_starts_after_its_prompt_is_shown_until_idle() {
        let seen = agent_turns_seen(CLAUDE, 2, false, |prompt| {
            if prompt == 1 {
                vec![said(CLAUDE, "idle"), said(CLAUDE, "running"), text("the task finished"), said(CLAUDE, "idle")]
            } else {
                vec![text("the agent finished"), said(CLAUDE, "idle")]
            }
        });
        assert_eq!(
            seen,
            [
                "turn",
                "answer",
                "stopped",
                "agent started",
                "the task finished",
                "agent ended",
                "turn",
                "answer",
                "stopped",
                "agent started",
                "the agent finished",
                "agent ended"
            ]
        );
    }

    /// Codex says its thread status: a turn it starts itself (a goal) runs from `active` to `idle`,
    /// and one waiting for an approval (`active` with flags) is still that turn.
    #[test]
    fn a_turn_codex_starts_after_its_prompt_is_shown_until_idle() {
        let seen = agent_turns_seen(CODEX, 1, false, |_| {
            let waiting = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":
                {"sessionUpdate":"session_info_update","_meta":{"codex":{"threadStatus":{"type":"active","activeFlags":["waitingOnApproval"]}}}}}});
            vec![said(CODEX, "idle"), said(CODEX, "running"), text("working on the goal"), waiting, said(CODEX, "running"), said(CODEX, "idle")]
        });
        assert_eq!(seen, ["turn", "answer", "stopped", "agent started", "working on the goal", "agent ended"]);
    }

    /// A session update as `session` sends it.
    /// A tool call, as a session's update.
    fn call(id: &str) -> serde_json::Value {
        serde_json::json!({"sessionUpdate":"tool_call","toolCallId":id,"title":"sleep","kind":"execute","status":"pending"})
    }

    fn sent(session: &str, update: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})
    }

    /// ACP subagent sessions, in claude-agent-acp's words (probed 2026-11-17): the agent
    /// announces a subagent in the session, briefs it with a message both transcripts get, and
    /// the subagent's updates come under its own session id. The conversation gets one card per
    /// subagent, with the brief as its body and the subagent's calls and answer under it; the
    /// subagent's settings are not the session's, and a session nobody announced shows nothing.
    #[test]
    fn a_subagent_session_is_a_card_with_its_calls_and_words_under_it() {
        let seen = agent_turns_seen("opencode", 1, false, |_| {
            let brief = serde_json::json!({"sessionUpdate":"session_message","messageId":"m1","senderSessionId":"s1","recipientSessionId":"c1",
                "content":[{"type":"text","text":"count the words"}]});
            let answer = |extra: (&str, &str)| {
                let mut m = serde_json::json!({"sessionUpdate":"session_message","messageId":"m2","senderSessionId":"c1","recipientSessionId":"s1",
                    "content":[{"type":"text","text":"2"}]});
                m[extra.0] = extra.1.into();
                m
            };
            let call = serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"wc -w a.txt","kind":"execute","status":"pending",
                "_meta":{"claudeCode":{"toolName":"Bash","parentToolUseId":"m1"}}});
            vec![
                sent(
                    "s1",
                    serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c1","title":"Count words","description":"general-purpose",
                    "capabilities":{"cancel":{}},"state":{"state":"running"}}),
                ),
                sent("s1", brief.clone()),
                sent("c1", brief),
                sent("c1", serde_json::json!({"sessionUpdate":"config_option_update","configOptions":[]})),
                sent("c1", call.clone()),
                sent("stray", call),
                sent("c1", serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed"})),
                sent("c1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c2","title":"Nested"})),
                sent("c1", serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"thinking aloud"}})),
                sent("c1", answer(("recipientMessageId", "m2"))),
                sent("s1", answer(("senderMessageId", "m2"))),
                sent("s1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c1","state":{"state":"requires_action"}})),
                sent("s1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c1","state":null})),
                sent("s1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c1","state":{"state":"idle","stopReason":"end_turn"}})),
                sent("s1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c1","state":{"state":"idle","stopReason":"cancelled"}})),
            ]
        });
        assert_eq!(
            seen,
            [
                "turn",
                "answer",
                "stopped",
                "call subagent:c1 [sub-agent · Count words] InProgress in -",
                "update subagent:c1 - count the words",
                "call t1 [wc -w a.txt] Pending in subagent:c1",
                "update t1 Completed -",
                "call subagent:c2 [sub-agent · Nested] Pending in subagent:c1",
                "subagent:c1 said thinking aloud",
                "subagent:c1 said 2",
                "update subagent:c1 RequiresAction -",
                "update subagent:c1 Unknown -",
                "update subagent:c1 Completed -",
                "update subagent:c1 Failed -",
            ]
        );
    }

    /// An agent with no word of its own state is never shown working outside a prompt, whatever it
    /// sends there; and another agent's signals mean nothing from it.
    #[test]
    fn an_agent_without_a_state_of_its_own_starts_no_turn() {
        let seen =
            agent_turns_seen("opencode", 1, false, |_| vec![said(CLAUDE, "idle"), said(CLAUDE, "running"), said(CODEX, "running"), text("woken by a shell")]);
        assert_eq!(seen, ["turn", "answer", "stopped", "woken by a shell"]);
    }

    /// Stop on the agent's own turn sends the cancel and ends the turn at once: no prompt is open
    /// whose answer would end it. An agent that works on regardless is shown working again.
    #[test]
    fn stop_ends_the_agents_own_turn_and_an_agent_that_goes_on_opens_another() {
        let seen = agent_turns_seen(CODEX, 1, true, |_| vec![said(CODEX, "idle"), said(CODEX, "running")]);
        assert_eq!(seen, ["turn", "answer", "stopped", "agent started", "agent cancelled", "agent started", "still working"]);
    }

    /// Stop reaches the subagents too: each one still at work that takes a cancel gets its own
    /// (the session's cancel is not theirs: Claude Code lets a background subagent run on,
    /// probed 2026-11-17). One that takes none works on, and its calls open no turn of the
    /// agent's after the stop; the agent's own word does.
    #[test]
    fn stop_cancels_the_subagents_at_work_and_their_calls_open_no_turn_after_it() {
        let seen = agent_turns_seen(CLAUDE, 1, true, |_| {
            let announce = |session: &str, more: serde_json::Value| {
                let mut update = serde_json::json!({"sessionUpdate":"subagent_update","sessionId":session,"title":session});
                update.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
                sent("s1", update)
            };
            vec![
                announce("c0", serde_json::json!({"capabilities":{"cancel":{}},"state":{"state":"running"}})),
                // done before the stop: no cancel for it
                sent("s1", serde_json::json!({"sessionUpdate":"subagent_update","sessionId":"c0","state":{"state":"idle","stopReason":"end_turn"}})),
                announce("c1", serde_json::json!({"capabilities":{"cancel":{}},"state":{"state":"running"}})),
                announce("c2", serde_json::json!({"state":{"state":"running"}})),
                sent("c1", call("t1")),
            ]
        });
        assert_eq!(
            seen,
            [
                "turn",
                "answer",
                "stopped",
                "call subagent:c0 [sub-agent · c0] InProgress in -",
                "update subagent:c0 Completed -",
                "call subagent:c1 [sub-agent · c1] InProgress in -",
                "call subagent:c2 [sub-agent · c2] InProgress in -",
                "agent started",
                "call t1 [sleep] Pending in subagent:c1",
                "agent cancelled",
                "update subagent:c1 Failed -",
                "call late [sleep] Pending in subagent:c2",
                "agent started",
                "still working",
            ]
        );
    }

    #[test]
    fn the_activity_source_is_chosen_by_the_agents_name() {
        assert_eq!(ActivitySource::of(Some(CLAUDE)), ActivitySource::Claude);
        assert_eq!(ActivitySource::of(Some("@zed-industries/claude-code-acp")), ActivitySource::Claude);
        assert_eq!(ActivitySource::of(Some(CODEX)), ActivitySource::Codex);
        assert_eq!(ActivitySource::of(Some("codex-acp")), ActivitySource::Codex);
        assert_eq!(ActivitySource::of(Some("OpenCode")), ActivitySource::None);
        assert_eq!(ActivitySource::of(None), ActivitySource::None);
    }

    /// A running said while the prompt is open is the prompt's, and stays so when the idle after
    /// it comes late; a prompt sent into the agent's own turn takes that turn over.
    #[test]
    fn the_prompts_own_running_starts_no_agent_turn() {
        let mut turns = AgentTurns::default();
        assert_eq!(turns.said(Activity::Running, true), None);
        assert_eq!(turns.said(Activity::Idle, false), None, "the idle after the prompt returned");
        assert_eq!(turns.said(Activity::Running, false), Some(AgentTurn::Started));
        assert_eq!(turns.said(Activity::Running, false), None, "said again");
        assert_eq!(turns.output(false), None, "its output is that turn's");
        assert_eq!(turns.prompt(), Some(AgentTurn::Ended));
        assert_eq!(turns.said(Activity::Idle, false), None, "the prompt's turn ends at its stop reason");
        assert_eq!(turns.output(false), None, "nothing runs: output alone starts no turn");
        assert_eq!(turns.stopped(), None, "no turn of the agent's to stop");
        assert_eq!(turns.said(Activity::Running, false), Some(AgentTurn::Started));
        assert_eq!(turns.stopped(), Some(AgentTurn::Cancelled));
        assert_eq!(turns.subagent_output(false), None, "a subagent the stop did not reach opens no turn");
        assert_eq!(turns.output(false), Some(AgentTurn::Started), "the agent's own word does");
        assert_eq!(turns.stopped(), Some(AgentTurn::Cancelled));
        assert_eq!(turns.said(Activity::Idle, false), None, "the idle the cancel brings ends nothing twice");
        assert_eq!(turns.said(Activity::Running, false), Some(AgentTurn::Started));
        assert_eq!(turns.subagent_output(false), None, "its subagents' calls are that turn's");
    }
}
