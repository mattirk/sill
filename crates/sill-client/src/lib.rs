//! What any Sill frontend needs beyond raw protocol calls, without a GUI toolkit
//! (docs/sill-gui.md §4.1). sill-gui adopts it first; the GTK shelf keeps its own client until these
//! interfaces settle.
//!
//! - [`conversation`]: one session's public SFP view, rebuilt from a snapshot and sequenced events,
//!   reporting what each event changed.
//! - [`lane`]: a connection of its own per open session that stays subscribed across connection
//!   loss and daemon restarts, resuming from its cursor. Dropping it ends the subscription only.
//! - [`actions`]: the person's requests (send, stop, answer, a control) on a connection apart from
//!   the lanes, each with an outcome that says whether it happened, was refused, never left, or
//!   may have happened.
//! - [`outbox`]: a prompt between Send and the daemon having it, so a refusal never loses text and
//!   a lost connection never sends it twice.
//! - [`supervision`]: what needs the person's attention in a session.
//! - [`files`]: session pins, directories, repositories and file reading, and the per-session state
//!   of a file drawer, with late replies kept to their own session.
//! - [`terminal`]: a session's jail shell over the daemon's byte stream, detached when dropped.
//!
//! Everything blocks on threads of its own and reports through callbacks; the frontend decides how
//! its main loop is woken. Nothing here is Linux-only: the client core is meant to build for macOS
//! and Windows frontends too (`scripts/check-portable.sh`).

pub mod actions;
pub mod agents;
pub mod changes;
pub mod conversation;
pub mod files;
pub mod lane;
pub mod login;
pub mod outbox;
pub mod sessions;
pub mod settings;
pub mod supervision;
pub mod terminal;

use std::sync::Arc;

pub use conversation::{Change, Conversation};

/// Makes a connection to the daemon, initialized and ready for requests. Production frontends pass
/// [`connect`]; tests pass a socket to a fake daemon.
pub type Connector = Arc<dyn Fn() -> anyhow::Result<sill_proto::Client> + Send + Sync>;

/// The daemon the protocol client's transport selection names (the local socket, or a saved box
/// over SSH). Never starts one: a frontend's executable is not `sill daemon`.
pub fn connect() -> Connector {
    Arc::new(sill_proto::Client::connect)
}

/// A fresh identity for a submission, an answer or a creation: 1–128 bytes, unique for this
/// daemon, so a retry with the same identity is recognised instead of acted on twice.
pub fn request_id(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "gui-{kind}-{}-{nanos:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The connection went with this request: it may or may not have reached the daemon. Reads the
/// whole error chain — a connection lost after sending arrives wrapped in the method's context,
/// which `Client::is_closed` (outermost message only) does not see.
pub fn connection_lost(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    [
        "closed the connection",
        "went away",
        "writing to the daemon",
        "did not answer",
    ]
    .iter()
    .any(|m| message.contains(m))
}

/// The domain code of a daemon refusal (`not_found`, `stale_answer`, `limit_exceeded`…), if the
/// error is one.
pub fn refusal_code(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<sill_proto::rpc::Error>()
        .and_then(|e| e.data.as_ref())
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
pub(crate) mod fake;
