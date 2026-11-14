//! `sill daemon schema` (docs/daemon.md decision 7, docs/protocol.md): the whole protocol as one
//! JSON document — every method with the type of its params and its result, every notification,
//! and the JSON Schema (draft 2020-12) of every type under `$defs` — for a frontend in another
//! language to generate from. [`METHODS`] is also what keeps the method table in docs/protocol.md
//! honest: a method the daemon answers must be listed here.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Value, json};

use crate::method;

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
    /// The capability (and its version) a client must have been answered to call it: every
    /// method added after the freeze has one.
    capability: Option<(&'static str, u32)>,
}

const fn req<P: JsonSchema, R: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: Some(|g| g.subschema_for::<R>()), capability: None }
}

/// A request a client may call only when the daemon answered `capability` at `version` or later.
const fn req_needing<P: JsonSchema, R: JsonSchema>(name: &'static str, capability: &'static str, version: u32, doc: &'static str) -> Method {
    Method { capability: Some((capability, version)), ..req::<P, R>(name, doc) }
}

/// A request answered with `{}`.
const fn req_empty<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Request, doc, params: Some(|g| g.subschema_for::<P>()), result: None, capability: None }
}

const fn notification<P: JsonSchema>(name: &'static str, doc: &'static str) -> Method {
    Method { name, kind: Kind::Notification, doc, params: Some(|g| g.subschema_for::<P>()), result: None, capability: None }
}

/// Every method the daemon answers and every notification it sends, in the order of docs/protocol.md.
pub const METHODS: &[Method] = &[
    req_empty::<crate::memory::TextClose>("_sill/memory.text.close", "Release this connection's immutable memory text snapshot early."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.delete", "Delete one exact session. Inspect existence after uncertainty; never replay against a replacement identity."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.reset", "Explicitly replace a conversation generation. Never blindly retry after uncertainty."),
    req::<crate::sessions::Log,crate::Text>("_sill/sessions.log", "Read a bounded daemon-host session log tail."),
    req::<crate::discovery::Find,crate::discovery::Found>("_sill/files.find", "Read daemon-host file/repository information with exact path bytes and bounded results."),
    req::<crate::discovery::PathKey,crate::discovery::Root>("_sill/repos.root", "Read daemon-host file/repository information with exact path bytes and bounded results."),
    req::<crate::discovery::PathKey,crate::discovery::HeadResult>("_sill/repos.head", "Read daemon-host file/repository information with exact path bytes and bounded results."),
    req::<crate::discovery::PathKey,crate::discovery::SummaryResult>("_sill/repos.summary", "Read daemon-host file/repository information with exact path bytes and bounded results."),
    req::<crate::discovery::ChangedLines,crate::discovery::Lines>("_sill/repos.changed_lines", "Read daemon-host file/repository information with exact path bytes and bounded results."),
    req::<crate::discovery::Status,crate::discovery::StatusPage>("_sill/repos.status", "Read daemon-host file/repository information with exact path bytes and bounded results."),

    req::<crate::Empty,crate::memory::Compacted>("_sill/memory.compact","Apply configured compaction and retention; inspect memory after uncertainty before retrying."),
    req::<crate::Empty,crate::memory::Forgotten>("_sill/memory.forget","Delete ended sessions; do not blindly retry because newly ended sessions may appear."),
    req::<crate::admin::SessionImage,crate::admin::FileBytes>("_sill/files.image","Read an image visible through the session filesystem view in bounded chunks; revision detects concurrent edits."),
    req::<crate::admin::DeniedList,crate::admin::NetworkDenied>("_sill/policy.denied","Read network denials and their acknowledgement state (4 MiB response bound)."),
    req_empty::<crate::admin::NetworkHost>("_sill/policy.remember_denied","Acknowledge a denial; inspect denied state after uncertainty before retrying."),
    req::<crate::admin::NetworkHost,crate::admin::AllowedHost>("_sill/policy.allow_host","Approve a concrete host for this session only; policy limits apply. Inspect policy after uncertainty."),
    req_needing::<crate::admin::DesktopCapture,crate::admin::DesktopFrame>("_sill/desktop.capture","sill.desktop_view",1,"Take a picture of the session's test desktop (Desktop development), on the host, outside the jail. A new one is kept for `_sill/desktop.read`; when it is still the `since` revision nothing is encoded and `changed` is false. `not_found` when the session has no test desktop running."),
    req_needing::<crate::admin::DesktopRead,crate::admin::FileBytes>("_sill/desktop.read","sill.desktop_view",1,"Read the latest test desktop picture (PNG) in chunks of at most 1 MiB; `conflict` when a newer capture replaced that revision."),
    req_needing::<crate::admin::NetworkHost,crate::admin::SavedHost>("_sill/policy.save_host","sill.permission_modes",1,"Save for future sessions: add a host pattern to the session agent's network defaults. Existing sessions and their grants are unchanged."),
    req_needing::<crate::admin::PermissionChoice,crate::admin::PermissionsChanged>("_sill/permissions.set","sill.permission_modes",1,"Remember a contained capability choice for this session within policy limits; blocked widening is refused. Inspect the preview after uncertainty."),
    req_needing::<crate::sessions::SessionKey,crate::admin::PermissionsChanged>("_sill/permissions.apply_development","sill.permission_modes",1,"Apply development permissions: remember the test desktop and the private bus as session choices together; refused as a whole, recording nothing, when a limit or a missing backend blocks either."),
    req_needing::<crate::host_control::Activate,crate::host_control::State>("_sill/host_control.activate","sill.host_control",1,"Activate Host control for a running session with exactly these operations, or change an active one's scope: a new generation each time. Refused for ghosts, a base policy that forbids it, a stopped session, and (exception_required) operations that meet repository restrictions without a recorded exception; never a saved choice."),
    req_needing::<crate::host_control::RecordException,crate::host_control::State>("_sill/host_control.exception","sill.host_control",1,"Record the person's repository exception for the session's next Host control activation: the restrictions of pinned repositories' fragments that these operations can bypass, named exactly as state lists them. Activates nothing; consumed by the next activation, gone with revocation, a scope change, the session stopping or this connection leaving. A base-policy ban has no exception."),
    req_needing::<crate::sessions::SessionKey,crate::host_control::State>("_sill/host_control.revoke","sill.host_control",1,"End the session's Host control activation; a no-op without one. Host processes it launched are stopped, except those marked to keep."),
    req_needing::<crate::host_control::Keep,crate::Empty>("_sill/host_control.keep","sill.host_control",1,"Mark a bridge-launched host process (by its process-list name, host:<id>) to keep running after Host control ends, or not."),
    req_needing::<crate::sessions::SessionKey,crate::host_control::State>("_sill/host_control.state","sill.permission_modes",1,"A session's Host control state: active, generation, operations, displaying frontends, the last end, a base-policy ban, the repository restrictions host operations can bypass and the exception. Session records carry the active brief for every client."),
    req_needing::<crate::sessions::SessionKey,crate::admin::PermissionsChanged>("_sill/permissions.restore_defaults","sill.permission_modes",1,"Restore defaults: remove this session's added capability and host grants, keeping its policy and pins. A running jail keeps its launch state until restarted."),
    notification::<crate::Empty>("_sill/changes.changed", "Filesystem changes changed; refresh bounded public pages."),
    req::<crate::changes::List, crate::changes::Page>("_sill/changes.list", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::Diff, crate::changes::DiffPage>("_sill/changes.diff", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::Keep, crate::changes::Done>("_sill/changes.keep", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::Revert, crate::changes::Done>("_sill/changes.revert", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::Undo, crate::changes::Done>("_sill/changes.undo", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::GitRevert, crate::Text>("_sill/changes.git-revert", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),
    req::<crate::changes::History, crate::changes::HistoryPage>("_sill/changes.history", "Public Changes operation; preserve filesystem preconditions and inspect current state after an uncertain mutation, never blindly retry."),

    req::<crate::memory::List, crate::memory::Page>("_sill/memory.list", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::memory::TextRead, crate::memory::TextPage>("_sill/memory.text", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::memory::Search, crate::memory::Hits>("_sill/memory.search", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::memory::NotesList, crate::memory::Notes>("_sill/memory.notes.list", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::memory::AddNote, crate::Empty>("_sill/memory.notes.add", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::memory::RemoveNote, crate::Empty>("_sill/memory.notes.remove", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),
    req::<crate::Empty, crate::Empty>("_sill/memory.refresh", "Read live bounded memory pages; after uncertain mutations inspect current state before retrying."),

    req::<crate::agent_admin::List, crate::agent_admin::Page>("_sill/agents.list", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Definition>("_sill/agents.get", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::Definition, crate::agent_admin::Saved>("_sill/agents.save", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Removed>("_sill/agents.remove", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Available>("_sill/agents.available", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::AgentKey, crate::wire::AgentSettings>("_sill/agents.defaults.get", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::Defaults, crate::Empty>("_sill/agents.defaults.set", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::Empty, crate::Empty>("_sill/registry.refresh", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::AgentKey, crate::agent_admin::Installation>("_sill/registry.installed", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::EntryKey, crate::Empty>("_sill/registry.uninstall", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),
    req::<crate::agent_admin::EntryKey, crate::agent_admin::Icon>("_sill/registry.icon", "Daemon-owned agent administration; inspect current state before retrying an uncertain mutation."),

    req::<crate::pins::List, crate::pins::Page>("_sill/pins.list", "List global or session pins in bounded live byte-ordered pages."),
    req::<crate::pins::GlobalPin, crate::pins::Pinned>("_sill/pins.add", "Ensure global pin membership; returns the canonical daemon-host path."),
    req_empty::<crate::pins::GlobalPin>("_sill/pins.remove", "Ensure a path is not globally pinned; inspect membership after uncertainty."),
    req_empty::<crate::pins::SessionPin>("_sill/sessions.pin", "Ensure session pin membership; inspect membership after uncertainty."),
    req_empty::<crate::pins::SessionPin>("_sill/sessions.unpin", "Ensure a path is not pinned in a session; inspect membership after uncertainty."),
    req::<crate::host::OperationKey, crate::host::Operation>("_sill/operations.get", "Recover durable login/install status after disconnect or restart. Unknown outcomes must not be replayed."),
    req::<crate::Empty, crate::sessions::HostPath>("_sill/files.home", "Read the daemon host home directory."),
    req::<crate::admin::FileKey, crate::admin::FileMetadata>("_sill/files.metadata", "Read metadata of an exact daemon-host path."),
    req::<crate::admin::FilesList, crate::admin::FilesPage>("_sill/files.list", "List directory children in bounded live byte-ordered pages."),
    req::<crate::admin::FileRead, crate::admin::FileBytes>("_sill/files.read", "Read at most 1 MiB from a regular daemon-host file; chunks are not an immutable snapshot."),
    req::<crate::admin::FileWrite, crate::Empty>("_sill/files.write", "Atomically replace a daemon-host file with at most 1 MiB. Do not blindly retry; read the file after uncertainty."),
    req::<crate::Empty, crate::admin::Settings>("_sill/settings.get", "Read daemon settings."),
    req::<crate::admin::SettingsPatch, crate::Empty>("_sill/settings.patch", "Apply named settings changes. Read settings after an uncertain outcome before retrying."),
    req::<crate::Empty, crate::Text>("_sill/settings.instructions.get", "Read global instructions."),
    req::<crate::admin::Text, crate::Empty>("_sill/settings.instructions.set", "Replace global instructions (at most 1 MiB). Read back after uncertainty before retrying."),
    req::<crate::admin::NamesList, crate::admin::Names>("_sill/policy.list", "List available daemon policy names (4 MiB result bound)."),
    req::<crate::admin::PolicyKey, crate::admin::PolicyEntry>("_sill/policy.get", "Read a named daemon policy."),
    req::<crate::admin::PolicyStart, crate::sessions::Started>("_sill/policy.start", "Start a session with explicit fragment-root exceptions for one jail build only; never blindly retry after uncertainty."),
    req::<crate::sessions::SessionKey, crate::admin::PolicyView>("_sill/policy.preview", "Preview next-build session rules and fragment diagnostics; this view is not enforced by a running jail."),
    req::<crate::sessions::SessionKey, crate::admin::PolicySnapshot>("_sill/policy.snapshot", "Inspect retained launch rules and fragment sources; null means no snapshot was recorded."),
    req::<crate::admin::PolicyCheck, crate::admin::PolicyChecked>("_sill/policy.check", "Validate at most 1 MiB of transferred complete-policy or fragment text without installation or grants."),
    req::<crate::admin::PolicyImport, crate::admin::PolicyEntry>("_sill/policy.import", "Install at most 1 MiB of complete policy text in the daemon host user policy directory. Explicit replace permits overwrite; inspect named policy after uncertain replies."),
    req::<crate::admin::ProcessesList, crate::admin::ProcessesPage>("_sill/procs.list", "List managed session processes in bounded live name-ordered pages."),
    req::<crate::admin::ProcessTail, crate::Text>("_sill/procs.tail", "Read at most 10000 lines and 4 MiB of managed-process output."),
    req::<crate::admin::ProcessKey, crate::Empty>("_sill/procs.stop", "Request stop of one named process. List its state after an uncertain outcome; do not blindly retry."),
    req::<crate::admin::ProcessKey, crate::Empty>("_sill/procs.dismiss", "Dismiss one named process. List after an uncertain outcome; do not blindly retry."),
    req::<crate::sessions::List, crate::sessions::Page>("_sill/sessions.list", "List sessions in bounded live pages ordered by immutable creation time and identity."),
    req::<crate::sessions::SessionKey, crate::sessions::Session>("_sill/sessions.get", "Read one exact session identity."),
    req::<crate::sessions::Create, crate::sessions::SessionKey>("_sill/sessions.create", "Atomically create a session; retry the same create_id and input to recover its identity."),
    req_empty::<crate::sessions::Update>("_sill/sessions.update", "Update one session field. Do not blindly retry after connection loss."),
    req::<crate::sessions::SessionKey, crate::sessions::Started>("_sill/sessions.start", "Start a jail. Do not blindly retry after connection loss."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.stop", "Stop the jail, preserving the session. Do not blindly retry after connection loss."),
    req::<crate::sessions::End, crate::sessions::Ending>("_sill/sessions.end", "End a session, possibly waiting for its summary. Do not blindly retry after connection loss."),
    req_empty::<crate::sessions::SessionKey>("_sill/sessions.checkpoint", "Request a summary. Do not blindly retry after connection loss."),
    req::<crate::host::PortOpen, crate::host::HostStream>("_sill/streams.port", "Connect to a TCP port in a running session jail; close the byte stream to close the connection. Do not blindly retry."),
    req::<crate::host::AgentLogin, crate::host::JobStream>("_sill/agents.login", "Start host login with tagged progress. Disconnect does not cancel; do not blindly retry."),
    req::<crate::host::AgentReset, crate::host::AgentResetDone>("_sill/agents.reset", "Reset an agent as if newly installed: stop its running sessions, then remove its sill-owned home (login, settings, history, memory), ghost copies and saved API keys. The host's own files are untouched."),
    req::<crate::host::RegistryInstall, crate::host::JobStream>("_sill/registry.install", "Install a daemon registry entry with tagged progress. Disconnect does not cancel; do not blindly retry."),
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
    req::<crate::sfp::CancelTurn, crate::sfp::CancellationRequested>("_sill/sessions.cancel", "Request cancellation of one identified active turn."),
    req::<crate::initialize::Initialize, crate::initialize::Initialized>(crate::initialize::METHOD, "Initialize an SFP control connection."),
    req::<crate::sfp::Attach, crate::sfp::Attached>("_sill/sessions.attach", "Subscribe with a durable cursor or immutable paginated snapshot."),
    req::<crate::sfp::SnapshotRequest, crate::sfp::SnapshotPage>("_sill/sessions.snapshot", "Read a connection-bound immutable snapshot page."),
    req_empty::<crate::sfp::Detach>("_sill/sessions.detach", "Detach this subscription without cancelling work."),
    req::<crate::sfp::Submit, crate::sfp::Submission>("_sill/sessions.submit", "Durably accept an idempotent submission; queue during work."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.get", "Recover the outcome of a submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.cancel", "Cancel a knowingly unsent queued submission."),
    req::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.resume", "Resume a held queued submission, retaining acceptance order."),
    req_needing::<crate::sfp::SubmissionKey, crate::sfp::Submission>("_sill/submissions.send_now", "sill.send_now", 1, "Send a queued submission next, stopping the active turn."),
    req::<crate::sfp::AnswerQuestion, crate::sfp::Question>("_sill/questions.answer", "Record the first valid answer with a durable idempotency key."),
    notification::<crate::sfp::SessionEvent>("_sill/session.event", "A committed ordered session event."),
    req::<crate::Empty, Value>(method::DAEMON_SCHEMA, "The Sill Frontend Protocol schema and method capabilities, for remote negotiation."),
    req::<crate::Empty, crate::DaemonStatus>(method::DAEMON_STATUS, "Version, protocol, pid, mode and session counts of the daemon."),
    req_empty::<crate::Empty>(method::DAEMON_SHUTDOWN, "Ask the daemon to leave; sessions keep running and the next daemon adopts them."),
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
                "capabilities": m.capability.map_or(json!({}), |(name, version)| json!({name: version})),
                "params": m.params.map(|f| f(&mut g).to_value()),
                // an empty answer is `{}` today; like every result it may gain fields (clients ignore
                // unknown fields in what the daemon sends)
                "result": m.result.map(|f| f(&mut g).to_value()).unwrap_or(json!({"type": "object"})),
            })
        })
        .collect();
    let progress = g.subschema_for::<crate::host::Progress>().to_value();
    let error = g.subschema_for::<crate::rpc::Error>().to_value();
    let domain_data = g.subschema_for::<crate::rpc::DomainData>().to_value();
    let details: serde_json::Map<String, Value> = crate::rpc::detail_codes().into_iter().map(|(code, always)| {
        let shape = match code {
            "incompatible" => g.subschema_for::<crate::rpc::IncompatibleDetails>(),
            "stale_answer" => g.subschema_for::<crate::rpc::StaleAnswerDetails>(),
            "limit_exceeded" => g.subschema_for::<crate::rpc::LimitDetails>(),
            "unsupported" => g.subschema_for::<crate::rpc::UnsupportedDetails>(),
            "exception_required" => g.subschema_for::<crate::rpc::ExceptionRequiredDetails>(),
            other => unreachable!("detail schema for {other}"),
        }.to_value();
        (code.to_string(), json!({"required": always, "shape": shape}))
    }).collect();
    let capabilities: Vec<Value> = crate::initialize::CAPABILITIES.iter()
        .map(|c| json!({"name": c.name, "version": c.version, "doc": c.doc})).collect();
    let defs = g.take_definitions(true);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Sill Frontend Protocol (SFP)",
        "protocol": crate::initialize::PROTOCOL_ID,
        "limits": crate::initialize::Limits::default(),
        "transport": "JSON-RPC 2.0, one message per line, over the unix socket at $XDG_RUNTIME_DIR/sill/daemon.sock (docs/protocol.md)",
        "capabilities": {"offered": capabilities,
            "rules": "Names are namespace.feature (lowercase ASCII, digits, '_'); versions are cumulative from 1; the daemon answers min(offered, supported) and omits unknown ones. A missing capability disables that feature only; it is never a protocol mismatch."},
        "methods": methods,
        "$defs": defs,
        "errors": {"shape": error, "domain_data": domain_data, "domain_numeric_code": -32000,
            "codes": ["incompatible","not_initialized","not_found","gone","invalid_state","unsupported","busy","id_conflict","stale_answer","invalid_cursor","snapshot_expired","limit_exceeded","permission_denied","operation_failed","exception_required"],
            "details": details,
            "details_rule": "Codes not listed carry no details in sfp/1. Listed shapes may only gain optional fields; clients ignore unknown fields and unknown codes.",
            "retry": "Unknown codes, timeouts and disconnections never authorize automatic mutation retry. Recover durable outcomes or read current state as specified by the method."},
        "streams": {"host_progress": {"framing": "ndjson", "event": progress, "max_text_bytes": crate::host::MAX_PROGRESS_TEXT, "max_jobs": crate::host::MAX_JOBS}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The freeze guard (docs/protocol.md "Freeze guard"). Breaking the recorded document fails
    /// and cannot be accepted; an additive change fails until recorded with
    /// `SILL_SFP_SCHEMA_ACCEPT=1`, so every extension shows up in review as a diff of the file.
    #[test]
    fn the_protocol_only_grows_from_the_recorded_sfp_1_document() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/sfp-1.schema.json");
        let current = document();
        let pretty = serde_json::to_string_pretty(&current).unwrap() + "\n";
        let Ok(text) = std::fs::read_to_string(&path) else {
            std::fs::write(&path, pretty).unwrap();
            panic!("recorded a new baseline at {}; review and commit it", path.display());
        };
        let recorded: Value = serde_json::from_str(&text).unwrap();
        let breaks = crate::schema_compat::breaking_changes(&recorded, &current);
        assert!(breaks.is_empty(), "incompatible with the recorded sfp/1 protocol (that is sfp/2):\n  {}", breaks.join("\n  "));
        if recorded != current {
            if std::env::var_os("SILL_SFP_SCHEMA_ACCEPT").is_some() {
                std::fs::write(&path, pretty).unwrap();
            } else {
                panic!("the protocol changed compatibly; record it with SILL_SFP_SCHEMA_ACCEPT=1 cargo test -p sill-proto and commit {}", path.display());
            }
        }
    }

    /// docs/protocol.md "Unknown fields": clients ignore fields they don't know in what the daemon
    /// sends, so nothing reachable from a result, a notification, an error or a stream may forbid
    /// them in the schema (a strict client validating against it would refuse a later addition).
    #[test]
    fn what_the_daemon_sends_is_open_to_new_fields() {
        let doc = document();
        let defs = doc["$defs"].as_object().unwrap();
        fn walk<'a>(node: &'a Value, defs: &'a serde_json::Map<String, Value>, seen: &mut Vec<String>, closed: &mut Vec<String>, at: &str) {
            match node {
                Value::Object(o) => {
                    if o.get("additionalProperties") == Some(&Value::Bool(false)) {
                        closed.push(at.to_string());
                    }
                    if let Some(name) = o.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/")) {
                        if !seen.iter().any(|s| s == name) {
                            seen.push(name.to_string());
                            walk(&defs[name], defs, seen, closed, name);
                        }
                    }
                    for v in o.values() {
                        walk(v, defs, seen, closed, at);
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| walk(v, defs, seen, closed, at)),
                _ => {}
            }
        }
        let (mut seen, mut closed) = (Vec::new(), Vec::new());
        for m in doc["methods"].as_array().unwrap() {
            let name = m["name"].as_str().unwrap();
            if m["kind"] == "notification" {
                walk(&m["params"], defs, &mut seen, &mut closed, name);
            }
            walk(&m["result"], defs, &mut seen, &mut closed, name);
        }
        walk(&doc["errors"], defs, &mut seen, &mut closed, "errors");
        walk(&doc["streams"], defs, &mut seen, &mut closed, "streams");
        closed.sort();
        closed.dedup();
        assert!(closed.is_empty(), "closed to new fields but sent by the daemon: {closed:?}");
    }

    /// Requests stay strict where the type is open because it is a result too.
    #[test]
    fn strict_params_refuse_unknown_fields() {
        let ok: crate::sfp::Answer = crate::rpc::strict(json!({"type": "permission", "option_id": "yes"})).unwrap();
        assert_eq!(ok, crate::sfp::Answer::Permission { option_id: "yes".into() });
        let e = crate::rpc::strict::<crate::sfp::Answer>(json!({"type": "permission", "option_id": "yes", "extra": 1})).unwrap_err();
        assert!(e.message.contains("extra"), "{}", e.message);
        assert!(crate::rpc::strict::<crate::sfp::Answer>(json!({"type": "cancel"})).is_ok());
        let def = json!({"id": "a", "name": "A", "command": ["a"]});
        assert!(crate::rpc::strict::<crate::agent_admin::Definition>(def.clone()).is_ok());
        let mut extra = def;
        extra["typo"] = json!(true);
        assert!(crate::rpc::strict::<crate::agent_admin::Definition>(extra).is_err());
    }

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
        for t in ["Event", "Answer", "Session", "DaemonStatus", "Submission", "Question", "HostPath"] {
            assert!(defs.contains_key(t), "{t} missing from $defs");
        }
        // a frontend reads the method table: the attach params say what to send
        let attach = doc["methods"].as_array().unwrap().iter().find(|m| m["name"] == "_sill/sessions.attach").unwrap();
        assert!(attach["params"]["$ref"].as_str().unwrap().ends_with("/Attach"), "{attach}");
    }

    #[test]
    fn error_details_and_capabilities_are_exported() {
        let doc = document();
        let details = doc["errors"]["details"].as_object().unwrap();
        for (code, required) in crate::rpc::detail_codes() {
            assert!(doc["errors"]["codes"].as_array().unwrap().contains(&json!(code)), "{code} not a listed code");
            assert_eq!(details[code]["required"], json!(required), "{code}");
        }
        // a method's capability is one the daemon offers, at a version it has
        let offered = doc["capabilities"]["offered"].as_array().unwrap();
        for m in METHODS.iter().filter_map(|m| m.capability) {
            assert!(offered.iter().any(|c| c["name"] == m.0 && c["version"].as_u64() >= Some(m.1 as u64)), "{m:?}");
        }
        assert!(METHODS.iter().all(|m| !m.name.starts_with("_sill/internal/")));
        // a limit error's data reads back as typed details
        let e = crate::rpc::Error::limit(crate::rpc::LimitScope::Result, Some(4), "too big");
        let data: crate::rpc::DomainData = serde_json::from_value(e.data.unwrap()).unwrap();
        assert!(matches!(data.details, Some(crate::rpc::DomainDetails::LimitExceeded(crate::rpc::LimitDetails { scope: crate::rpc::LimitScope::Result, maximum: Some(4) }))));
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
