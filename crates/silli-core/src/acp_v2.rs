//! Explicitly selected experimental native ACP v2 client. Acceptance and foreground completion
//! are separate; native update identities/patches cross the relay without a v1 translation.
use crate::acp::{
    Attachment, AttachmentKind, AuthMethodInfo, Command, ConfigKind, ConfigOption, ConfigValue, Controls, Event, PermissionOption, PermissionOptionKind,
    SessionOptions,
};
use agent_client_protocol::schema::{ProtocolVersion, v2};
use agent_client_protocol::{Agent, ByteStreams, Client, Error, ErrorCode, V2ConnectionTo};
use anyhow::{Context, Result};
use futures::{AsyncReadExt, channel::oneshot};
use serde_json::{Value, json};
use std::os::unix::net::UnixStream;

fn protocol<T: serde::de::DeserializeOwned>(value: Value) -> std::result::Result<T, Error> {
    serde_json::from_value(value).map_err(Error::into_internal_error)
}
fn json<T: serde::Serialize>(value: T) -> std::result::Result<Value, Error> {
    serde_json::to_value(value).map_err(Error::into_internal_error)
}
fn auth_methods(methods: &[v2::AuthMethod]) -> Vec<AuthMethodInfo> {
    methods
        .iter()
        .filter_map(|m| {
            let terminal = match m {
                v2::AuthMethod::Agent(_) => None,
                v2::AuthMethod::Terminal(t) => Some((t.args.clone(), t.env.iter().map(|v| (v.name.clone(), v.value.clone())).collect())),
                _ => return None,
            };
            Some(AuthMethodInfo {
                id: m.method_id().to_string(),
                name: m.name().into(),
                description: m.description().map(str::to_string),
                terminal,
                api_key: m.meta().and_then(|m| m.get("api-key")).and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}
fn controls(value: &Value) -> Controls {
    let mut out = Controls::default();
    for o in value.as_array().into_iter().flatten() {
        let (Some(id), Some(name), Some(kind)) = (o["configId"].as_str(), o["name"].as_str(), o["type"].as_str()) else {
            continue;
        };
        let kind = match kind {
            "boolean" => match o["currentValue"].as_bool() {
                Some(v) => ConfigKind::Boolean(v),
                None => continue,
            },
            "select" => {
                let Some(current) = o["currentValue"].as_str() else {
                    continue;
                };
                let mut options = Vec::new();
                for entry in o["options"].as_array().into_iter().flatten() {
                    let entries = entry.get("options").and_then(Value::as_array).map(|v| v.as_slice()).unwrap_or_else(|| std::slice::from_ref(entry));
                    for e in entries {
                        if let (Some(v), Some(name)) = (e["value"].as_str(), e["name"].as_str()) {
                            options.push((v.into(), name.into()));
                        }
                    }
                }
                ConfigKind::Select(current.into(), options)
            }
            _ => continue,
        };
        out.options.push(ConfigOption { id: id.into(), name: name.into(), category: o["category"].as_str().unwrap_or("").into(), kind });
    }
    out
}
async fn login(cx: &V2ConnectionTo<Agent>, methods: &[v2::AuthMethod], id: &str, key_file: Option<&std::path::Path>) -> std::result::Result<(), Error> {
    if !methods.iter().any(|m| matches!(m,v2::AuthMethod::Agent(a) if a.method_id.to_string()==id)) {
        return Err(Error::invalid_params().data("method is not an advertised agent login"));
    }
    let mut req = v2::LoginAuthRequest::new(id.to_string());
    if let Some(file) = key_file {
        let secret = std::fs::read_to_string(file).map_err(Error::into_internal_error)?;
        req.meta = Some(serde_json::Map::from_iter([("api-key".into(), json!(secret.trim()))]));
    }
    cx.send_request(req).block_task().await?;
    Ok(())
}
fn prompt(id: &str, text: String, attachments: Vec<Attachment>) -> std::result::Result<v2::PromptRequest, Error> {
    let mut blocks = vec![json!({"type":"text","text":text})];
    for a in attachments {
        blocks.push(match a.kind {
            AttachmentKind::Image { data_base64 } => json!({"type":"image","data":data_base64,"mimeType":a.mime}),
            AttachmentKind::File { jail_path } => {
                let path = jail_path.to_str().ok_or_else(|| Error::invalid_params().data("ACP resource paths require UTF-8"))?;
                json!({"type":"resource_link","uri":format!("file://{path}"),"name":a.name,"mimeType":a.mime})
            }
        });
    }
    protocol(json!({"sessionId":id,"prompt":blocks}))
}

#[expect(clippy::large_enum_variant, reason = "one message at a time through the session's loop")]
enum Inbound {
    Update(v2::UpdateSessionNotification),
    Event { session_id: String, event: Event },
}

pub fn run_session(stream: UnixStream, opts: SessionOptions, commands: async_channel::Receiver<Command>, events: async_channel::Sender<Event>) -> Result<()> {
    stream.set_nonblocking(true)?;
    let stream = async_io::Async::new(stream).context("async v2 socket")?;
    let (reader, writer) = stream.split();
    let (updates, update_rx) = async_channel::bounded::<Inbound>(256);
    let permission_updates = updates.clone();
    let cancellations = std::sync::Arc::new(std::sync::Mutex::new(Vec::<oneshot::Sender<()>>::new()));
    let permission_cancellations = cancellations.clone();
    let result = futures::executor::block_on(async {
        Client
            .v2()
            .name("silli")
            .on_receive_notification(
                async move |notification: v2::UpdateSessionNotification, _cx: V2ConnectionTo<Agent>| {
                    updates.try_send(Inbound::Update(notification)).map_err(Error::into_internal_error)
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: v2::RequestPermissionRequest, responder, cx: V2ConnectionTo<Agent>| {
                    let options = req
                        .options
                        .iter()
                        .map(|o| PermissionOption {
                            id: o.option_id.to_string(),
                            name: o.name.clone(),
                            kind: serde_json::from_value(serde_json::to_value(&o.kind).unwrap_or(Value::Null))
                                .unwrap_or_else(|_| PermissionOptionKind::Other("unknown".into())),
                        })
                        .collect::<Vec<_>>();
                    let subject = req.subject.as_ref().map(serde_json::to_value).transpose().map_err(Error::into_internal_error)?;
                    let tool_call_id =
                        subject.as_ref().and_then(|s| s["toolCall"]["toolCallId"].as_str().or_else(|| s["toolCallId"].as_str())).map(str::to_string);
                    let (tx, rx) = oneshot::channel();
                    let title = match req.description {
                        Some(d) => format!("{}\n{}", req.title, d),
                        None => req.title,
                    };
                    let (cancel, cancelled) = oneshot::channel();
                    {
                        let mut pending = permission_cancellations.lock().unwrap();
                        pending.retain(|s| !s.is_canceled());
                        // Bound suspended SDK handlers independently from the event queue.
                        if pending.len() >= 128 {
                            return responder.respond(protocol::<v2::RequestPermissionResponse>(json!({"outcome":{"outcome":"cancelled"}}))?);
                        }
                        pending.push(cancel);
                    }
                    permission_updates
                        .try_send(Inbound::Event {
                            session_id: req.session_id.to_string(),
                            event: Event::V2PermissionRequest { tool_call_id, title, subject, options: options.clone(), reply: tx },
                        })
                        .map_err(Error::into_internal_error)?;
                    cx.spawn(async move {
                        let selected = match futures::future::select(rx, cancelled).await {
                            futures::future::Either::Left((answer, _)) => answer.unwrap_or(None),
                            _ => None,
                        };
                        let chosen = selected.filter(|id| options.iter().any(|o| &o.id == id));
                        let response = match chosen {
                            Some(id) => json!({"outcome":{"outcome":"selected","optionId":id}}),
                            None => json!({"outcome":{"outcome":"cancelled"}}),
                        };
                        responder.respond(protocol::<v2::RequestPermissionResponse>(response)?)
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(ByteStreams::new(writer, reader), async |cx| {
                let init = cx
                    .send_request(v2::InitializeRequest::new(ProtocolVersion::V2, v2::Implementation::new("silli", env!("CARGO_PKG_VERSION"))))
                    .block_task()
                    .await?;
                if init.protocol_version != ProtocolVersion::V2 || init.capabilities.session.is_none() {
                    return Err(Error::invalid_params().data("experimental adapter requires ACP v2 session capability"));
                }
                let methods = auth_methods(&init.auth_methods);
                let images = init.capabilities.session.as_ref().and_then(|s| s.prompt.as_ref()).is_some_and(|p| p.image.is_some());
                let _ = events.send(Event::Capabilities { images }).await;
                let cwd = opts.cwd.to_str().ok_or_else(|| Error::invalid_params().data("ACP cwd requires UTF-8"))?;
                // the pinned folders, for an agent that advertises `additionalDirectories`
                let roots: Vec<&str> = if init.capabilities.session.as_ref().is_some_and(|s| s.additional_directories.is_some()) {
                    opts.roots.iter().filter_map(|p| p.to_str()).collect()
                } else {
                    vec![]
                };
                let (session_id, resumed, config) = loop {
                    let opened = if let Some(id) = &opts.resume {
                        let req: v2::ResumeSessionRequest = protocol(json!({"sessionId":id,"cwd":cwd,"additionalDirectories":roots}))?;
                        cx.send_request(req).block_task().await.and_then(|r| Ok((id.clone(), true, json(r)?)))
                    } else {
                        let req: v2::NewSessionRequest = protocol(json!({"cwd":cwd,"additionalDirectories":roots}))?;
                        cx.send_request(req).block_task().await.and_then(|r| Ok((r.session_id.to_string(), false, json(r)?)))
                    };
                    match opened {
                        Ok(v) => break v,
                        Err(e) if e.code == ErrorCode::AuthRequired => {
                            let _ = events.send(Event::AuthRequired { message: e.message, methods: methods.clone() }).await;
                            loop {
                                match commands.recv().await {
                                    Ok(Command::Authenticate(id)) => match login(&cx, &init.auth_methods, &id, None).await {
                                        Ok(()) => break,
                                        Err(e) => {
                                            let _ = events.send(Event::AuthRequired { message: e.message, methods: methods.clone() }).await;
                                        }
                                    },
                                    Ok(Command::AuthenticateKey { method, key_file }) => match login(&cx, &init.auth_methods, &method, Some(&key_file)).await {
                                        Ok(()) => break,
                                        Err(e) => {
                                            let _ = events.send(Event::AuthRequired { message: e.message, methods: methods.clone() }).await;
                                        }
                                    },
                                    Ok(Command::RetrySession) => break,
                                    Ok(Command::Quit) | Err(_) => return Ok(()),
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => return Err(e),
                    }
                };
                let _ = events.send(Event::V2Ready { session_id: session_id.clone(), resumed, controls: controls(&config["configOptions"]) }).await;
                drive(&cx, &session_id, &init.auth_methods, &commands, &update_rx, &events, &cancellations).await
            })
            .await
    });
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    let _ = events.send_blocking(Event::Ended(error));
    result.map_err(|e| anyhow::anyhow!("ACP v2: {e:#}"))
}

async fn drive(
    cx: &V2ConnectionTo<Agent>,
    id: &str,
    methods: &[v2::AuthMethod],
    commands: &async_channel::Receiver<Command>,
    updates: &async_channel::Receiver<Inbound>,
    events: &async_channel::Sender<Event>,
    cancellations: &std::sync::Mutex<Vec<oneshot::Sender<()>>>,
) -> std::result::Result<(), Error> {
    type Pending = std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<v2::PromptResponse, Error>>>>;
    let mut pending: Option<Pending> = None;
    let mut active = false;
    let mut observed_active = false;
    let mut deferred_idle = None;
    loop {
        #[expect(clippy::large_enum_variant, reason = "one per turn of the loop, matched at once")]
        enum Next {
            Command(Option<Command>),
            Update(Option<Inbound>),
            Accepted(std::result::Result<v2::PromptResponse, Error>),
        }
        let next = {
            let input = futures::future::select(Box::pin(updates.recv()), Box::pin(commands.recv()));
            match pending.as_mut() {
                Some(p) => match futures::future::select(input, p).await {
                    futures::future::Either::Right((r, _)) => Next::Accepted(r),
                    futures::future::Either::Left((futures::future::Either::Left((v, _)), _)) => Next::Update(v.ok()),
                    futures::future::Either::Left((futures::future::Either::Right((v, _)), _)) => Next::Command(v.ok()),
                },
                None => match input.await {
                    futures::future::Either::Left((v, _)) => Next::Update(v.ok()),
                    futures::future::Either::Right((v, _)) => Next::Command(v.ok()),
                },
            }
        };
        match next {
            Next::Accepted(result) => {
                pending = None;
                match result {
                    Ok(response) => {
                        let _ = events.send(Event::PromptAccepted { message_id: response.message_id.to_string() }).await;
                        if let Some(update) = deferred_idle.take() {
                            let _ = events.send(Event::V2Update { update }).await;
                            active = false;
                            observed_active = false;
                        }
                    }
                    Err(e) => {
                        let _ = events.send(Event::V2PromptFailed { code: format!("{:?}", e.code), message: e.message.clone() }).await;
                        if e.code == ErrorCode::AuthRequired {
                            let _ = events.send(Event::AuthRequired { message: e.message, methods: auth_methods(methods) }).await;
                        }
                        active = false;
                        observed_active = false;
                        deferred_idle = None;
                    }
                }
            }
            Next::Update(Some(Inbound::Event { session_id, event })) => {
                if session_id == id {
                    let _ = events.send(event).await;
                }
            }
            Next::Update(Some(Inbound::Update(notification))) => {
                if notification.session_id.to_string() != id {
                    continue;
                }
                let update = json(notification.update)?;
                if update["sessionUpdate"] == "usage_update"
                    && let (Some(used), Some(size)) = (update["used"].as_u64(), update["size"].as_u64())
                {
                    let _ = events.send(Event::Usage { used, size }).await;
                }
                if update["sessionUpdate"] == "config_option_update" {
                    let _ = events.send(Event::Controls(controls(&update["configOptions"]))).await;
                }
                if update["sessionUpdate"] == "state_update" {
                    match update["state"].as_str() {
                        Some("running" | "requires_action") => {
                            observed_active = true;
                            active = true;
                        }
                        Some("idle") if observed_active => {
                            if pending.is_some() {
                                deferred_idle = Some(update);
                                continue;
                            }
                            active = false;
                            observed_active = false;
                        }
                        Some("idle") if pending.is_none() => {
                            active = false;
                        }
                        Some("idle") => {}
                        _ => {
                            active = true;
                        }
                    }
                }
                let _ = events.send(Event::V2Update { update }).await;
            }
            Next::Command(Some(Command::Prompt(text, attachments))) => {
                if active || pending.is_some() {
                    let _ = events
                        .send(Event::V2PromptFailed {
                            code: "not_dispatched".into(),
                            message: "ACP v2 became active before this prompt could be dispatched".into(),
                        })
                        .await;
                    continue;
                }
                let request = prompt(id, text, attachments)?;
                let connection = cx.clone();
                let _ = events.send(Event::TurnStarted).await;
                active = true;
                observed_active = false;
                pending = Some(Box::pin(async move { connection.send_request(request).block_task().await }));
            }
            Next::Command(Some(Command::Cancel)) => {
                for pending in cancellations.lock().unwrap().drain(..) {
                    let _ = pending.send(());
                }
                let _ = events.send(Event::V2PermissionsCancelled).await;
                if active {
                    cx.send_notification(v2::CancelSessionNotification::new(id.to_string()))?;
                }
            }
            Next::Command(Some(Command::Authenticate(method))) => match login(cx, methods, &method, None).await {
                Ok(()) => {
                    let _ = events.send(Event::Other("_silli_authenticated".into())).await;
                }
                Err(e) => {
                    let _ = events.send(Event::AuthRequired { message: e.message, methods: auth_methods(methods) }).await;
                }
            },
            Next::Command(Some(Command::AuthenticateKey { method, key_file })) => match login(cx, methods, &method, Some(&key_file)).await {
                Ok(()) => {
                    let _ = events.send(Event::Other("_silli_authenticated".into())).await;
                }
                Err(e) => {
                    let _ = events.send(Event::AuthRequired { message: e.message, methods: auth_methods(methods) }).await;
                }
            },
            Next::Command(Some(Command::RetrySession)) => {
                let _ = events.send(Event::Other("_silli_authenticated".into())).await;
            }
            Next::Command(Some(Command::SetConfig(config_id, value))) => {
                let value = match value {
                    ConfigValue::Id(value) => json!({"type":"id","value":value}),
                    ConfigValue::Bool(value) => json!({"type":"boolean","value":value}),
                };
                let mut request = value;
                request["sessionId"] = json!(id);
                request["configId"] = json!(config_id);
                let setting = format!("config:{config_id}");
                let request: v2::SetSessionConfigOptionRequest = protocol(request)?;
                match cx.send_request(request).block_task().await {
                    Ok(response) => {
                        let _ = events.send(Event::Controls(controls(&json(response)?["configOptions"]))).await;
                    }
                    Err(e) => {
                        let _ = events.send(Event::ControlFailed { setting, reason: format!("set option failed: {e}") }).await;
                    }
                }
            }
            Next::Command(Some(Command::SetMode(_))) => {
                let _ = events.send(Event::Other("ACP v2 uses configuration options instead of session modes".into())).await;
            }
            Next::Command(Some(Command::Quit)) | Next::Command(None) => {
                for pending in cancellations.lock().unwrap().drain(..) {
                    let _ = pending.send(());
                }
                let _ = events.send(Event::V2PermissionsCancelled).await;
                cx.send_request(v2::CloseSessionRequest::new(id.to_string())).block_task().await?;
                return Ok(());
            }
            Next::Update(None) => {
                return Err(Error::internal_error().data("native v2 update channel ended"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Responder;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn update(cx: &V2ConnectionTo<Client>, body: Value) -> std::result::Result<(), Error> {
        cx.send_notification(protocol::<v2::UpdateSessionNotification>(json!({"sessionId":"native","update":body}))?)
    }
    fn recv(events: &async_channel::Receiver<Event>) -> Event {
        futures::executor::block_on(async {
            match futures::future::select(Box::pin(events.recv()), Box::pin(async_io::Timer::after(std::time::Duration::from_secs(5)))).await {
                futures::future::Either::Left((event, _)) => event.unwrap(),
                _ => panic!("native v2 event deadline"),
            }
        })
    }
    #[test]
    fn sdk_v2_peer_acceptance_upserts_permission_and_idle_are_distinct() {
        sdk_scenario(false, false);
    }
    #[test]
    fn sdk_v2_cancellation_resolves_native_permission_without_a_human_answer() {
        sdk_scenario(true, false);
    }
    #[test]
    fn sdk_v2_resume_receives_state_before_dispatch_and_avoids_history_replay() {
        sdk_scenario(false, true);
    }
    fn sdk_scenario(cancel: bool, resume: bool) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (commands, cmd_rx) = async_channel::unbounded();
        let (events, event_rx) = async_channel::bounded(256);
        let accepted = Arc::new(AtomicUsize::new(0));
        let seen = accepted.clone();
        let peer = std::thread::spawn(move || {
            theirs.set_nonblocking(true).unwrap();
            let socket = async_io::Async::new(theirs).unwrap();
            let (r, w) = socket.split();
            futures::executor::block_on(Agent.v2().name("silli-v2-fixture")
                .on_receive_request(async |req:v2::InitializeRequest,responder:Responder<v2::InitializeResponse>,_|{
                    assert_eq!(req.protocol_version,ProtocolVersion::V2);
                    responder.respond(v2::InitializeResponse::new(ProtocolVersion::V2,v2::Implementation::new("fixture","1")).capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new())))
                },agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_:v2::NewSessionRequest,responder:Responder<v2::NewSessionResponse>,cx:V2ConnectionTo<Client>|{
                    update(&cx,json!({"sessionUpdate":"state_update","state":"idle"}))?;
                    responder.respond(v2::NewSessionResponse::new("native"))
                },agent_client_protocol::on_receive_request!())
                .on_receive_request(async |req:v2::ResumeSessionRequest,responder:Responder<v2::ResumeSessionResponse>,cx:V2ConnectionTo<Client>|{
                    assert!(req.replay_from.is_none());assert_eq!(req.session_id.to_string(),"native");
                    update(&cx,json!({"sessionUpdate":"state_update","state":"idle"}))?;
                    responder.respond(v2::ResumeSessionResponse::new())
                },agent_client_protocol::on_receive_request!())
                .on_receive_notification(async |_:v2::CancelSessionNotification,_:V2ConnectionTo<Client>|Ok(()),agent_client_protocol::on_receive_notification!())
                .on_receive_request(async move |_:v2::PromptRequest,responder:Responder<v2::PromptResponse>,cx:V2ConnectionTo<Client>|{
                    seen.fetch_add(1,Ordering::SeqCst);
                    let task_cx=cx.clone();
                    cx.spawn(async move { let cx=task_cx;
                    // Deliberately deliver native completion before the acceptance response.
                    // The adapter must preserve the evidence distinction and delay terminal idle.
                    update(&cx,json!({"sessionUpdate":"state_update","state":"running"}))?;
                    update(&cx,json!({"sessionUpdate":"agent_message_chunk","messageId":"answer","content":{"type":"text","text":"partial"}}))?;
                    update(&cx,json!({"sessionUpdate":"agent_message","messageId":"answer","content":null}))?;
                    update(&cx,json!({"sessionUpdate":"agent_message","messageId":"answer","content":[{"type":"text","text":"replacement"}]}))?;
                    let request:v2::RequestPermissionRequest=protocol(json!({"sessionId":"native","title":"Allow command?","subject":{"type":"command","command":"echo fixture","cwd":"/tmp"},"options":[{"optionId":"yes","name":"Allow once","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}))?;
                    let response=cx.send_request(request).block_task().await?;
                    assert_eq!(json(response)?["outcome"]["outcome"],if cancel{"cancelled"}else{"selected"});
                    update(&cx,json!({"sessionUpdate":"state_update","state":"idle","stopReason":if cancel{"cancelled"}else{"end_turn"}}))?;
                    responder.respond(v2::PromptResponse::new("user-1"))
                    })
                },agent_client_protocol::on_receive_request!())
                .on_receive_request(async |_:v2::CloseSessionRequest,responder:Responder<v2::CloseSessionResponse>,_|responder.respond(v2::CloseSessionResponse::new()),agent_client_protocol::on_receive_request!())
                .connect_to(ByteStreams::new(w,r)))
        });
        let client = std::thread::spawn(move || {
            run_session(
                ours,
                SessionOptions { cwd: "/tmp".into(), roots: vec![], auto: crate::acp::AutoPermission::Ask, resume: resume.then(|| "native".into()) },
                cmd_rx,
                events,
            )
        });
        loop {
            match recv(&event_rx) {
                Event::V2Ready { resumed, .. } => {
                    assert_eq!(resumed, resume);
                    break;
                }
                Event::Ended(e) => panic!("open failed: {e:?}"),
                _ => {}
            }
        }
        commands.send_blocking(Command::Prompt("hello".into(), vec![])).unwrap();
        let mut turn = false;
        let mut accepted = false;
        let mut message_updates = 0;
        let mut permission = false;
        let mut held_replies = Vec::new();
        loop {
            match recv(&event_rx) {
                Event::TurnStarted => turn = true,
                Event::PromptAccepted { message_id } => {
                    assert_eq!(message_id, "user-1");
                    accepted = true;
                }
                Event::V2PermissionRequest { tool_call_id, subject, reply, .. } => {
                    assert!(tool_call_id.is_none());
                    assert_eq!(subject.unwrap()["type"], "command");
                    permission = true;
                    if cancel {
                        commands.send_blocking(Command::Cancel).unwrap();
                        held_replies.push(reply);
                    } else {
                        reply.send(Some("yes".into())).unwrap();
                    }
                }
                Event::V2Update { update } => {
                    if update["messageId"] == "answer" {
                        message_updates += 1;
                    }
                    if update["state"] == "idle" && turn && update.get("stopReason").is_some() {
                        assert!(accepted, "idle cannot overtake acceptance");
                        break;
                    }
                }
                Event::Ended(e) => panic!("ended before native completion: {e:?}"),
                _ => {}
            }
        }
        assert!(permission);
        assert_eq!(message_updates, 3);
        drop(held_replies);
        commands.send_blocking(Command::Quit).unwrap();
        assert!(client.join().unwrap().is_ok());
        let _ = peer.join().unwrap();
    }
}
