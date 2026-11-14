//! `sill daemon schema` (docs/daemon.md decision 7, docs/protocol.md): the whole protocol as one
//! JSON document — every method with the type of its params and its result, every notification,
//! and the JSON Schema (draft 2020-12) of every type under `$defs` — for a frontend in another
//! language to generate from. [`METHODS`] is also what keeps the method table in docs/protocol.md
//! honest: a method the daemon answers must be listed here.

use std::path::PathBuf;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Value, json};

use crate::{method, wire};

/// Request (answered) or notification (no id, no answer; `from` says who sends it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Request,
    /// Daemon → every attached frontend.
    Notification,
}

pub struct Method {
    pub name: &'static str,
    pub kind: Kind,
    pub doc: &'static str,
    /// The params type's schema, or None when the method takes none.
    params: Option<fn(&mut SchemaGenerator) -> Schema>,
    /// The result type's schema; None for a notification or an empty `{}` answer.
    result: Option<fn(&mut SchemaGenerator) -> Schema>,
}

const fn req<P: JsonSchema, R: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: Some(|g| g.subschema_for::<R>()) }
}

/// A request answered with `{}`.
const fn req_empty<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: None }
}

const fn notification<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Notification, doc, params: Some(|g| g.subschema_for::<P>()), result: None }
}

/// Every method the daemon answers and every notification it sends, in the order of docs/protocol.md.
pub const METHODS: &[Method] = &[
    req::<crate::sessions::List, crate::sessions::Page>("_sill/sessions.list", "List sessions in bounded live pages ordered by immutable creation time and identity."),
    req::<crate::sessions::SessionKey, crate::sessions::Session>("_sill/sessions.get", "Read one exact session identity."),
    req::<crate::sessions::Create, crate::sessions::SessionKey>("_sill/sessions.create", "Atomically create a session; retry the same create_id and input to recover its identity."),
    req_empty::<crate::sessions::Update>("_sill/sessions.update", "Update one session field. Do not blindly retry after connection loss."),
    req::<crate::sessions::SessionKey, crate::sessions::Started>("_sill/sessions.start", "Start a jail. Do not blindly retry after connection loss."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.stop", "Stop the jail, preserving the session. Do not blindly retry after connection loss."),
    req::<crate::sessions::End, crate::sessions::Ending>("_sill/sessions.end", "End a session, possibly waiting for its summary. Do not blindly retry after connection loss."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.checkpoint", "Request a summary. Do not blindly retry after connection loss."),
    req::<crate::host::PortOpen, crate::host::HostStream>("_sill/streams.port", "Connect to a TCP port in a running session jail; close the byte stream to close the connection. Do not blindly retry."),
    req::<crate::host::AgentLogin, crate::host::HostStream>("_sill/agents.login", "Start host login with tagged progress. Disconnect does not cancel; do not blindly retry."),
    req::<crate::host::AgentLogout, crate::host::LoggedOut>("_sill/agents.logout", "Remove an agent's login from sill after stopping its running sessions; the host's own login is untouched. `unsupported` when sill does not know where the agent keeps it."),
    req::<crate::host::RegistryInstall, crate::host::HostStream>("_sill/registry.install", "Install a daemon registry entry with tagged progress. Disconnect does not cancel; do not blindly retry."),
    req::<crate::host::RegistryList, crate::host::RegistryPage>("_sill/registry.list", "List the daemon's registry in bounded live pages, including install selectors and trust source."),
    req::<crate::terminals::Open, crate::terminals::Terminal>("_sill/terminals.open", "Open a daemon-owned terminal in a session's jail or on the daemon host. Do not blindly retry after connection loss: list first."),
    req::<crate::terminals::List, crate::terminals::Terminals>("_sill/terminals.list", "Running and recently exited terminals, at most 64."),
    req::<crate::terminals::TerminalKey, crate::terminals::Terminal>("_sill/terminals.get", "Read one terminal, including its exit."),
    req::<crate::terminals::TerminalKey, crate::terminals::Stream>("_sill/terminals.attach", "A byte-stream token: recent output first, then live; closing the stream detaches without ending the terminal."),
    req_empty::<crate::terminals::Resize>("_sill/terminals.resize", "Set the terminal's window size."),
    req::<crate::terminals::TerminalKey, crate::terminals::Cwd>("_sill/terminals.cwd", "A host terminal's current working directory."),
    req_empty::<crate::terminals::TerminalKey>("_sill/terminals.close", "Hang up the terminal and forget it; attached streams end."),
    req::<crate::sfp::UploadCreate, crate::sfp::UploadCreated>("_sill/uploads.create", "Create an immutable session upload with an idempotency key."),
    req::<crate::sfp::UploadGet, crate::sfp::Upload>("_sill/uploads.get", "Read an upload's immutable bytes (attachments are referenced, never embedded)."),
    req_empty::<crate::sfp::SessionControl>("_sill/sessions.control", "Change adapter controls; do not blindly retry an uncertain login or retry-session action."),
    req::<crate::sfp::CancelTurn, Value>("_sill/sessions.cancel", "Request cancellation of one identified active turn."),
    req::<crate::initialize::Initialize, crate::initialize::Initialized>(crate::initialize::METHOD, "Initialize an SFP control connection."),
    req::<crate::sfp::Attach, crate::sfp::Attached>("_sill/sessions.attach", "Subscribe with a durable cursor or immutable paginated snapshot."),
    req::<crate::sfp::SnapshotRequest, crate::sfp::SnapshotPage>("_sill/sessions.snapshot", "Read a connection-bound immutable snapshot page."),
    req_empty::<crate::sfp::Detach>("_sill/sessions.detach", "Detach this subscription without cancelling work."),
    req::<crate::sfp::Submit, crate::sfp::Submission>("_sill/sessions.submit", "Durably accept an idempotent submission; queue during work."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.get", "Recover the outcome of a submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.cancel", "Cancel a knowingly unsent queued submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.resume", "Resume a held queued submission, retaining acceptance order."),
    req::<crate::sfp::AnswerQuestion, crate::sfp::Question>("_sill/questions.answer", "Record the first valid answer with a durable idempotency key."),
    notification::<crate::sfp::SessionEvent>("_sill/session.event", "A committed ordered session event."),
    notification::<Value>(method::SESSION_REPLAY, "Atomic stored transcript tail before attach live state; core transcript entries are opaque in this schema."),
    req::<Value, Value>(method::FRONTEND_REQUEST, "Host data operations for the rendering frontend; sill_core::frontend::Request is typed in Rust, opaque in this schema."),
    req::<crate::Empty, Value>(method::DAEMON_SCHEMA, "The daemon protocol schema and method capabilities, for remote negotiation."),
    req::<crate::Empty, crate::DaemonStatus>(method::DAEMON_STATUS, "Version, protocol, pid, mode and session counts of the daemon."),
    req_empty::<crate::Empty>(method::DAEMON_SHUTDOWN, "Ask the daemon to leave; sessions keep running and the next daemon adopts them."),
    req::<crate::Empty, Vec<crate::Session>>(method::SESSIONS_LIST, "Every session in the store, newest first."),
    req::<crate::Key, crate::Session>(method::SESSIONS_GET, "One session by id, id prefix or exact title."),
    req::<crate::NewSession, crate::Key>(method::SESSIONS_NEW, "Create a session row; answers its id."),
    req_empty::<crate::SessionSet>(method::SESSIONS_SET, "Change one field of a session's row."),
    req_empty::<crate::Key>(method::SESSIONS_DELETE, "Stop the jail, remove the session's state and its row."),
    req_empty::<crate::Pin>(method::SESSIONS_PIN, "Pin a path in a session (bound at its next start)."),
    req_empty::<crate::Pin>(method::SESSIONS_UNPIN, "Unpin a path."),
    req::<crate::Key, crate::Started>(method::SESSIONS_START, "Start a session's jail, or find it running."),
    req_empty::<crate::Key>(method::SESSIONS_STOP, "Stop a session's jail; the row stays."),
    req::<crate::LogRequest, crate::Text>(method::SESSIONS_LOG, "The tail of a session's log."),
    req_empty::<crate::Attach>(method::SESSIONS_ATTACH, "Start jail and agent when needed and subscribe this connection to the session's events; its current state comes first."),
    req_empty::<crate::Key>(method::SESSIONS_DETACH, "Unsubscribe; the agent stays the daemon's."),
    req_empty::<crate::SessionCommand>(method::SESSIONS_COMMAND, "A prompt, cancel, mode, option, login or quit for a running session; a prompt during a turn is refused."),
    req_empty::<crate::SessionAnswer>(method::SESSIONS_ANSWER, "The answer to an agent's question (permission or form); the first answer counts."),
    notification::<crate::SessionEvent>(method::SESSION_EVENT, "One event of a session, to every connection attached to it."),
    req::<crate::End, crate::Ending>(method::SESSIONS_END, "End a session as closing its tab would: the summary turn when the agent can be asked, the row written or deleted, the jail stopped."),
    req_empty::<crate::Key>(method::SESSIONS_CHECKPOINT, "Summarize now, without ending; the summary comes as a Summarized event."),
    req::<crate::Key, crate::Count>(method::SESSIONS_CHANGES, "How many unkept writes a session has outside its pins."),
    req::<crate::Key, crate::JailView>(method::SESSIONS_JAIL_VIEW, "A session's jail layout, for showing files as the agent sees them."),
    req::<crate::Empty, Vec<PathBuf>>(method::PINS_LIST, "The global pins (docs/pins.md)."),
    req_empty::<crate::GlobalPin>(method::PINS_ADD, "Add a global pin (canonicalized; refused inside another or in sill's state)."),
    req_empty::<crate::GlobalPin>(method::PINS_REMOVE, "Remove a global pin."),
    req::<crate::Empty, wire::ChangesList>(method::CHANGES_LIST, "Every session's overlay changes and undoable keeps, and the pinned repositories' git status."),
    req::<wire::DiffOf, crate::Text>(method::CHANGES_DIFF, "One change's diff."),
    req::<crate::ChangesKeep, wire::Done>(method::CHANGES_KEEP, "Copy changed paths to the host; each checked against the hash it was listed with."),
    req::<crate::ChangesPaths, wire::Done>(method::CHANGES_REVERT, "Put the host's version back in the session."),
    req::<crate::ChangesUndo, wire::Done>(method::CHANGES_UNDO, "Undo keeps."),
    req::<crate::GitRevert, crate::Text>(method::CHANGES_GIT_REVERT, "A pinned repository's path back to HEAD."),
    req::<Value, Value>(method::CHANGES_HISTORY, "The ledger across time (SPEC §3.6); params and result are sill_core::history's types, not in this schema yet."),
    notification::<crate::Empty>(method::CHANGES_CHANGED, "The overlays changed (a sweep, a keep, a turn, a deletion): whatever shows them rescans."),
    req::<crate::Empty, Vec<crate::MemorySession>>(method::MEMORY_LIST, "Ended sessions, newest first."),
    req::<crate::Key, crate::Text>(method::MEMORY_SHOW, "An ended session's summary and notes, as the CLI prints them."),
    req::<crate::MemorySearch, crate::Text>(method::MEMORY_SEARCH, "Full-text search over summaries and transcripts."),
    req::<crate::Transcript, crate::Text>(method::MEMORY_TRANSCRIPT, "A session's transcript."),
    req_empty::<crate::Note>(method::MEMORY_NOTE_ADD, "Add a note to a session's memory."),
    req_empty::<crate::Note>(method::MEMORY_NOTE_REMOVE, "Remove a note by its time."),
    req::<crate::Empty, crate::Compacted>(method::MEMORY_COMPACT, "Compaction and retention now."),
    req::<crate::Empty, crate::Count>(method::MEMORY_FORGET, "Delete every ended session."),
    req_empty::<crate::AgentSettingsSet>(method::AGENTS_SET_SETTINGS, "An agent type's default settings."),
    req::<crate::Empty, Value>(method::SETTINGS_GET, "sill's settings (sill.toml); the type is sill_core::config's, not in this schema yet."),
];

/// The protocol as one document: `methods`, each with `params` and `result` as `$ref`s into
/// `$defs`, where every wire type's schema is.
pub fn document() -> Value {
    let mut g = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    let methods: Vec<Value> = METHODS
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "kind": match m.kind { Kind::Request => "request", Kind::Notification => "notification" },
                "doc": m.doc,
                "x-sill-internal": m.name.starts_with("_sill/internal/"),
                "capabilities": if m.name.starts_with("_sill/internal/") { json!({"sill.internal":1}) } else { json!({}) },
                "params": m.params.map(|f| f(&mut g).to_value()),
                "result": m.result.map(|f| f(&mut g).to_value()).unwrap_or(json!({"type": "object", "additionalProperties": false})),
            })
        })
        .collect();
    let progress = g.subschema_for::<crate::host::Progress>().to_value();
    let defs = g.take_definitions(true);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "sill daemon protocol",
        "protocol": crate::initialize::PROTOCOL_ID,
        "limits": crate::initialize::Limits::default(),
        "transport": "JSON-RPC 2.0, one message per line, over the unix socket at $XDG_RUNTIME_DIR/sill/daemon.sock (docs/protocol.md)",
        "methods": methods,
        "$defs": defs,
        "streams": {"host_progress": {"framing": "ndjson", "event": progress, "max_text_bytes": crate::host::MAX_PROGRESS_TEXT, "max_jobs": crate::host::MAX_JOBS}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_listed_once_and_the_document_builds() {
        let mut names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "a method twice");
        let doc = document();
        assert_eq!(doc["protocol"], json!(crate::initialize::PROTOCOL_ID));
        assert_eq!(doc["methods"].as_array().unwrap().len(), METHODS.len());
        let defs = doc["$defs"].as_object().unwrap();
        for t in ["Event", "Command", "Answer", "Session", "DaemonStatus", "ChangesList", "StopReason"] {
            assert!(defs.contains_key(t), "{t} missing from $defs");
        }
        // a frontend reads the method table: the attach params say what to send
        let attach = doc["methods"].as_array().unwrap().iter().find(|m| m["name"] == "_sill/sessions.attach").unwrap();
        assert!(attach["params"]["$ref"].as_str().unwrap().ends_with("/Attach"), "{attach}");
    }

    /// docs/protocol.md is the human reading of this table: a method the daemon answers is in it.
    #[test]
    fn the_protocol_doc_names_every_method() {
        let doc = include_str!("../../../docs/protocol.md");
        let missing: Vec<&str> = METHODS.iter().map(|m| m.name).filter(|n| !doc.contains(&format!("`{n}`"))).collect();
        assert!(missing.is_empty(), "not in docs/protocol.md: {missing:?}");
        assert!(doc.contains(crate::initialize::PROTOCOL_ID));
    }
}
