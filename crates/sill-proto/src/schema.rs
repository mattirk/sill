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
                "params": m.params.map(|f| f(&mut g).to_value()),
                "result": m.result.map(|f| f(&mut g).to_value()).unwrap_or(json!({"type": "object", "additionalProperties": false})),
            })
        })
        .collect();
    let defs = g.take_definitions(true);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "sill daemon protocol",
        "protocol": crate::PROTOCOL,
        "transport": "JSON-RPC 2.0, one message per line, over the unix socket at $XDG_RUNTIME_DIR/sill/daemon.sock (docs/protocol.md)",
        "methods": methods,
        "$defs": defs,
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
        assert_eq!(doc["protocol"], json!(crate::PROTOCOL));
        assert_eq!(doc["methods"].as_array().unwrap().len(), METHODS.len());
        let defs = doc["$defs"].as_object().unwrap();
        for t in ["Event", "Command", "Answer", "Session", "DaemonStatus", "ChangesList", "StopReason"] {
            assert!(defs.contains_key(t), "{t} missing from $defs");
        }
        // a frontend reads the method table: the attach params say what to send
        let attach = doc["methods"].as_array().unwrap().iter().find(|m| m["name"] == method::SESSIONS_ATTACH).unwrap();
        assert!(attach["params"]["$ref"].as_str().unwrap().ends_with("/Attach"), "{attach}");
    }

    /// docs/protocol.md is the human reading of this table: a method the daemon answers is in it.
    #[test]
    fn the_protocol_doc_names_every_method() {
        let doc = include_str!("../../../docs/protocol.md");
        let missing: Vec<&str> = METHODS.iter().map(|m| m.name).filter(|n| !doc.contains(&format!("`{n}`"))).collect();
        assert!(missing.is_empty(), "not in docs/protocol.md: {missing:?}");
        assert!(doc.contains(&format!("protocol **{}**", crate::PROTOCOL)), "docs/protocol.md names another protocol number");
    }
}
