//! SFP control-connection initialization. Identity is independent of private relay versions.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::rpc::Error;

pub const METHOD: &str = "_sill/initialize";
/// Development implementation; the complete sfp/1-draft.1 contract is not frozen or advertised.
pub const PROTOCOL_ID: &str = "sfp/1-dev.3";

/// A negotiable optional feature. Names are `namespace.feature` in lowercase ASCII (`a-z`,
/// `0-9`, `_`, `.`); the `sill.` namespace is the daemon's own. Versions start at 1 and are
/// cumulative: version N keeps every guarantee of N−1. A client offers the highest version it
/// implements; the daemon answers min(offered, its own), and omits a capability it does not have
/// or that was offered as 0. Only answered capabilities may be used on the connection.
pub struct Capability { pub name: &'static str, pub version: u32, pub doc: &'static str }

/// Every capability this daemon offers. The baseline sfp/1 contract needs none of them; later
/// compatible additions arrive here (docs/protocol.md, Compatibility and errors).
pub const CAPABILITIES: &[Capability] = &[
    Capability { name: "sill.send_now", version: 1, doc: "_sill/submissions.send_now: a queued submission goes next, the active turn stopped." },
    Capability { name: "sill.permission_modes", version: 1, doc: "The frontend shows session permission state: it may change session capability choices (_sill/permissions.set, apply_development, restore_defaults) and operate sessions using Desktop development or Host control capabilities." },
    Capability { name: "sill.desktop_view", version: 1, doc: "_sill/desktop.capture and _sill/desktop.read: pictures of a session's test desktop, for a live view." },
    Capability { name: "sill.agent_setup", version: 1, doc: "_sill/agents.discover and _sill/agents.discovered: a first start in a throwaway jail proposes an installed agent's home_paths; the person approves them on the Agents page (saved with _sill/agents.save)." },
    Capability { name: "sill.workspaces", version: 1, doc: "_sill/workspaces.*: a session's own clone and branch of a repository; the person merges (fast-forward) or discards the result. _sill/workspaces.changed goes only to connections answered this capability." },
    Capability { name: "sill.network_quiet", version: 1, doc: "_sill/policy.quiet_host: stop showing a host's refusal banner in every session, without granting access." },
    Capability { name: "sill.usage", version: 1, doc: "_sill/usage.active_time: the time agents worked per hour, over all sessions, for a running-time chart." },
    Capability { name: "sill.login_browser", version: 1, doc: "Host logins hand their pages to the frontend (`open_url` login progress) instead of opening a browser on the daemon host; _sill/streams.login_callback relays the page's loopback callback from a frontend on another machine; _sill/operations.cancel stops a running login." },
    Capability { name: "sill.host_control", version: 1, doc: "The frontend displays Host control: the permanent session caption and the window-level line naming every session with active control. It may activate and revoke (_sill/host_control.activate, revoke); attached to a session, it counts as displaying that session's control, which ends when the last such connection goes." },
];

/// Capabilities that state a frontend's own user-facing contract. The shared client offers
/// them only when the program declares it implements them, never for every program linking it.
pub const FRONTEND_DECLARED: &[&str] = &["sill.permission_modes", "sill.host_control"];

/// Whether `name` is a well-formed capability name.
pub fn valid_capability_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() >= 2 && name.len() <= 64
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
}

/// The capabilities answered for an offer: min(offered, supported) for each known one.
pub fn negotiate(offer: &BTreeMap<String, u32>) -> BTreeMap<String, u32> {
    negotiate_supported(offer, CAPABILITIES)
}

fn negotiate_supported(offer: &BTreeMap<String, u32>, supported: &[Capability]) -> BTreeMap<String, u32> {
    supported.iter()
        .filter_map(|c| offer.get(c.name).map(|&v| (c.name.to_string(), v.min(c.version))))
        .filter(|(_, v)| *v > 0)
        .collect()
}
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Decoded image bytes per submission and bytes per uploaded object.
pub const MAX_INLINE_BYTES: usize = 10 * 1024 * 1024;
pub const INITIALIZE_TIMEOUT_SECONDS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Peer {
    pub name: String,
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Initialize {
    pub protocols: Vec<String>,
    pub client: Peer,
    #[serde(default)]
    pub capabilities: BTreeMap<String, u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    pub max_message_bytes: usize,
    pub max_inline_bytes: usize,
    pub max_text_bytes: usize,
    pub snapshot_ttl_seconds: u64,
    pub max_snapshot_record_bytes: usize,
    pub max_snapshot_bytes: usize,
    pub max_snapshot_page_records: usize,
    pub retained_events: usize,
    pub retained_event_bytes: usize,
    pub max_connection_queue_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self { max_message_bytes: MAX_MESSAGE_BYTES, max_inline_bytes: MAX_INLINE_BYTES, max_text_bytes: 1024 * 1024, snapshot_ttl_seconds: 60, max_snapshot_record_bytes: 64*1024*1024, max_snapshot_bytes: 1024*1024*1024, max_snapshot_page_records: 128, retained_events: 4096, retained_event_bytes: 16*1024*1024, max_connection_queue_bytes: 16*1024*1024 }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Initialized {
    pub protocol: String,
    pub daemon: Peer,
    pub capabilities: BTreeMap<String, u32>,
    pub limits: Limits,
}

#[derive(Default)]
pub struct ConnectionState { initialized: bool }
impl ConnectionState {
    pub fn is_initialized(&self) -> bool { self.initialized }
    pub fn require_initialized(&self) -> Result<(), Error> {
        if self.initialized { Ok(()) } else { Err(Error::domain("not_initialized", "send _sill/initialize first")) }
    }
    pub fn initialize(&mut self, request: Initialize, version: &str) -> Result<Initialized, Error> {
        if self.initialized { return Err(Error::domain("invalid_state", "this connection is already initialized")); }
        let Some(protocol) = request.protocols.into_iter().find(|p| p == PROTOCOL_ID) else {
            let mut error = Error::domain("incompatible", "the client and daemon have no supported SFP identity in common; explicitly update/restart the daemon to change it");
            error.data.as_mut().unwrap()["details"] = serde_json::json!({"supported_protocols": [PROTOCOL_ID]});
            return Err(error);
        };
        self.initialized = true;
        let capabilities = negotiate(&request.capabilities);
        Ok(Initialized { protocol, daemon: Peer { name: "sill".into(), version: version.into() }, capabilities, limits: Limits::default() })
    }
}

/// Read one newline-terminated frame without allowing an unbounded allocation. The limit
/// includes the newline; EOF after a partial frame is an error rather than a truncated message.
pub fn read_frame(reader: &mut impl std::io::BufRead, limit: usize) -> std::io::Result<Option<String>> {
    use std::io::{Error, ErrorKind};
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() { Ok(None) } else { Err(Error::new(ErrorKind::UnexpectedEof, "partial SFP frame")) };
        }
        let end = available.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let count = end.unwrap_or(available.len());
        if bytes.len().saturating_add(count) > limit { return Err(Error::new(ErrorKind::InvalidData, "SFP message exceeds max_message_bytes")); }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return String::from_utf8(bytes).map(Some).map_err(|e| Error::new(ErrorKind::InvalidData, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn offer(protocols: &[&str]) -> Initialize { Initialize { protocols: protocols.iter().map(|s| s.to_string()).collect(), client: Peer { name: "test".into(), version: "0".into() }, capabilities: BTreeMap::from([("unknown/1".into(), 1)]) } }
    #[test]
    fn failure_leaves_connection_uninitialized_and_success_is_once_only() {
        let mut state = ConnectionState::default();
        assert_eq!(state.require_initialized().unwrap_err().data.unwrap()["code"], "not_initialized");
        assert_eq!(state.initialize(offer(&["6"]), "test").unwrap_err().data.unwrap()["code"], "incompatible");
        assert!(!state.is_initialized());
        let response = state.initialize(offer(&["unknown", PROTOCOL_ID]), "test").unwrap();
        assert_eq!(response.protocol, PROTOCOL_ID);
        assert!(response.capabilities.is_empty());
        assert!(state.require_initialized().is_ok());
        assert_eq!(state.initialize(offer(&[PROTOCOL_ID]), "test").unwrap_err().data.unwrap()["code"], "invalid_state");
    }
    #[test]
    fn capabilities_negotiate_to_the_lower_version_and_ignore_unknown_offers() {
        let offer = |pairs: &[(&str, u32)]| pairs.iter().map(|(n, v)| (n.to_string(), *v)).collect::<BTreeMap<_, _>>();
        assert!(negotiate(&offer(&[("unknown.feature", 3)])).is_empty());
        let supported = [Capability { name: "test.feature", version: 2, doc: "test" }];
        assert_eq!(negotiate_supported(&offer(&[("test.feature", 7)]), &supported), offer(&[("test.feature", 2)]));
        assert_eq!(negotiate_supported(&offer(&[("test.feature", 1)]), &supported), offer(&[("test.feature", 1)]));
        assert!(negotiate_supported(&offer(&[("test.feature", 0)]), &supported).is_empty());
        assert!(negotiate(&offer(&[("sill.internal", 1)])).is_empty());
        for c in CAPABILITIES { assert!(valid_capability_name(c.name), "{}", c.name); assert!(c.version >= 1); }
        assert!(!valid_capability_name("nodot") && !valid_capability_name("Upper.case") && !valid_capability_name("a..b"));
    }
    #[test]
    fn framing_bounds_include_newline_and_reject_partial_eof() {
        assert_eq!(read_frame(&mut &b"abc\n"[..], 4).unwrap(), Some("abc\n".into()));
        assert!(read_frame(&mut &b"abcd\n"[..], 4).is_err());
        assert!(read_frame(&mut &b"abc"[..], 4).is_err());
        assert!(read_frame(&mut &b"\xff\n"[..], 4).is_err());
        assert_eq!(read_frame(&mut &b""[..], 4).unwrap(), None);
    }
}
