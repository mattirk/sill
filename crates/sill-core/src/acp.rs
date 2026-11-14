//! ACP client over the shim's agent stream (SPEC §4.1). Runtime-neutral: the SDK is driven with
//! `futures::executor::block_on`; the socket is wrapped by `async-io`.

use std::os::unix::net::UnixStream;
use std::path::Path;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, InitializeRequest, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification,
    SessionUpdate, StopReason,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{ByteStreams, Client, SessionMessage};
use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt;

/// What the caller sees while a prompt runs.
#[derive(Debug)]
pub enum Event {
    Text(String),
    Thought(String),
    ToolCall { title: String, status: String },
    ToolCallUpdate { id: String, status: Option<String>, title: Option<String> },
    Permission { title: String, decided: String },
    Other(String),
    Stopped(String),
}

/// How permission requests are answered without a human (M0 CLI). The shelf will route them to the
/// person instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPermission {
    /// Pick the first `AllowOnce` option, else `AllowAlways`, else cancel.
    AllowOnce,
    /// Pick a reject option, else cancel.
    Reject,
}

/// Run one prompt in a fresh ACP session over `stream` and report events through `on_event`.
pub fn run_prompt(
    stream: UnixStream,
    cwd: &Path,
    prompt: &str,
    auto: AutoPermission,
    on_event: impl FnMut(Event) + Send + 'static,
) -> Result<StopReason> {
    stream.set_nonblocking(true)?;
    let stream = async_io::Async::new(stream).context("async socket")?;
    let (reader, writer) = stream.split();
    let transport = ByteStreams::new(writer, reader);
    let cwd = cwd.to_path_buf();
    let prompt = prompt.to_string();

    let result: Result<StopReason, agent_client_protocol::Error> = futures::executor::block_on(async move {
        let stop = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop2 = stop.clone();
        let events = std::sync::Arc::new(std::sync::Mutex::new(on_event));
        let events_perm = events.clone();
        Client
            .builder()
            .name("sill")
            .on_receive_request(
                async move |req: RequestPermissionRequest, responder, _cx| {
                    let events = events_perm.clone();
                    {
                        let pick = |kinds: &[PermissionOptionKind]| {
                            kinds.iter().find_map(|k| req.options.iter().find(|o| &o.kind == k)).cloned()
                        };
                        let option = match auto {
                            AutoPermission::AllowOnce => {
                                pick(&[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways])
                            }
                            AutoPermission::Reject => {
                                pick(&[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways])
                            }
                        };
                        let title = req
                            .tool_call
                            .fields
                            .title
                            .clone()
                            .unwrap_or_else(|| req.tool_call.tool_call_id.to_string());
                        let (outcome, decided) = match option {
                            Some(o) => (
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(o.option_id.clone())),
                                o.name.clone(),
                            ),
                            None => (RequestPermissionOutcome::Cancelled, "cancelled".to_string()),
                        };
                        (events.lock().unwrap())(Event::Permission { title, decided });
                        responder.respond(RequestPermissionResponse::new(outcome))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async move |cx| {
                cx.send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(ClientCapabilities::new()),
                )
                .block_task()
                .await?;
                cx.build_session(&cwd)
                    .block_task()
                    .run_until(async |mut session| {
                        session.send_prompt(prompt.as_str())?;
                        loop {
                            match session.read_update().await? {
                                SessionMessage::StopReason(reason) => {
                                    *stop2.lock().unwrap() = Some(reason);
                                    (events.lock().unwrap())(Event::Stopped(format!("{reason:?}")));
                                    break;
                                }
                                SessionMessage::SessionMessage(dispatch) => {
                                    let events = events.clone();
                                    MatchDispatch::new(dispatch)
                                        .if_notification(async |n: SessionNotification| {
                                            (events.lock().unwrap())(to_event(n.update));
                                            Ok(())
                                        })
                                        .await
                                        .otherwise_ignore()?;
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    })
                    .await?;
                Ok(())
            })
            .await?;
        let r = stop.lock().unwrap().take();
        r.ok_or_else(|| agent_client_protocol::Error::internal_error())
    });
    result.map_err(|e| anyhow!("acp: {e}"))
}

fn to_event(update: SessionUpdate) -> Event {
    fn text(block: &ContentBlock) -> String {
        match block {
            ContentBlock::Text(t) => t.text.clone(),
            other => format!("[{}]", kind_name(other)),
        }
    }
    match update {
        SessionUpdate::AgentMessageChunk(c) => Event::Text(text(&c.content)),
        SessionUpdate::AgentThoughtChunk(c) => Event::Thought(text(&c.content)),
        SessionUpdate::ToolCall(t) => Event::ToolCall { title: t.title.clone(), status: format!("{:?}", t.status) },
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
