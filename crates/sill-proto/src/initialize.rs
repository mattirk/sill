//! SFP control-connection initialization. Identity is independent of private relay versions.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::rpc::Error;

pub const METHOD: &str = "_sill/initialize";
/// Development implementation; the complete sfp/1-draft.1 contract is not frozen or advertised.
pub const PROTOCOL_ID: &str = "sfp/1-dev.1";
pub const INTERNAL_CAPABILITY: &str = "sill.internal";
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
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
}
impl Default for Limits {
    fn default() -> Self {
        Self { max_message_bytes: MAX_MESSAGE_BYTES, max_inline_bytes: 1024 * 1024, max_text_bytes: 1024 * 1024, snapshot_ttl_seconds: 60 }
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
        let capabilities = if request.capabilities.get(INTERNAL_CAPABILITY) == Some(&1) { BTreeMap::from([(INTERNAL_CAPABILITY.into(), 1)]) } else { BTreeMap::new() };
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
    fn framing_bounds_include_newline_and_reject_partial_eof() {
        assert_eq!(read_frame(&mut &b"abc\n"[..], 4).unwrap(), Some("abc\n".into()));
        assert!(read_frame(&mut &b"abcd\n"[..], 4).is_err());
        assert!(read_frame(&mut &b"abc"[..], 4).is_err());
        assert!(read_frame(&mut &b"\xff\n"[..], 4).is_err());
        assert_eq!(read_frame(&mut &b""[..], 4).unwrap(), None);
    }
}
