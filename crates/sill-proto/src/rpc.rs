//! The frame: JSON-RPC 2.0, one object per line. Shared by the client here and the daemon's loop.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    /// Absent on a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: Option<u64>, method: &str, params: Value) -> Self {
        Request { jsonrpc: "2.0", id, method: method.to_string(), params: Some(params) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Response { jsonrpc: "2.0", id: Some(id), result: Some(result), error: None }
    }
    pub fn err(id: Option<u64>, error: Error) -> Self {
        Response { jsonrpc: "2.0", id, result: None, error: Some(error) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<DomainData>")]
    pub data: Option<Value>,
}

impl Error {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// Something sill-side went wrong: the message is for the person.
    pub const FAILED: i64 = -32000;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), data: None }
    }
    pub fn domain(code: &str, message: impl Into<String>) -> Self {
        Self { code: Self::FAILED, message: message.into(), data: Some(serde_json::json!({"code": code})) }
    }
    /// `limit_exceeded` with its required details.
    pub fn limit(scope: LimitScope, maximum: Option<u64>, message: impl Into<String>) -> Self {
        let mut e = Self::domain("limit_exceeded", message);
        e.data.as_mut().unwrap()["details"] = serde_json::to_value(LimitDetails { scope, maximum }).unwrap();
        e
    }
    /// `unsupported` because the connection did not negotiate `capability`.
    pub fn needs_capability(capability: &str, message: impl Into<String>) -> Self {
        let mut e = Self::domain("unsupported", message);
        e.data.as_mut().unwrap()["details"] = serde_json::to_value(UnsupportedDetails { capability: capability.into() }).unwrap();
        e
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("unknown method {method}"))
    }
    pub fn invalid_params(e: impl std::fmt::Display) -> Self {
        Self::new(Self::INVALID_PARAMS, format!("invalid params: {e}"))
    }
    pub fn failed(e: impl std::fmt::Display) -> Self {
        Self::new(Self::FAILED, e.to_string())
    }
}

/// Parse request params whose type is open in the schema (a result carries it too, and clients
/// ignore unknown fields in results) while still refusing fields the type doesn't have: requests
/// are strict. Top-level keys only; the type must serialize every field it has.
pub fn strict<T: serde::de::DeserializeOwned + Serialize>(v: serde_json::Value) -> Result<T, Error> {
    let parsed: T = serde_json::from_value(v.clone()).map_err(Error::invalid_params)?;
    if let (Some(given), Ok(serde_json::Value::Object(known))) = (v.as_object(), serde_json::to_value(&parsed))
        && let Some(k) = given.keys().find(|k| !known.contains_key(*k))
    {
        return Err(Error::invalid_params(format!("unknown field `{k}`")));
    }
    Ok(parsed)
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// Anything that came in on a line: a request (with id), a notification (without), or a response.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Error>,
}

/// Public domain errors preserve unknown codes. Clients must not retry mutations on an
/// unfamiliar error, timeout or disconnection.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DomainData {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<DomainDetails>,
}
/// `details` by code. Only the codes listed in [`detail_codes`] carry details; for those, the
/// record shape is fixed by sfp/1 and may only gain optional fields. Clients ignore unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[expect(clippy::large_enum_variant, reason = "made once for a failed request; a Box would only add an allocation")]
#[serde(untagged)]
pub enum DomainDetails {
    Incompatible(IncompatibleDetails),
    StaleAnswer(StaleAnswerDetails),
    LimitExceeded(LimitDetails),
    Unsupported(UnsupportedDetails),
}
/// `incompatible`: the protocol identities the daemon supports. Always present.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IncompatibleDetails {
    pub supported_protocols: Vec<String>,
}
/// `stale_answer`: the question as currently resolved. Always present.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StaleAnswerDetails {
    pub question: crate::sfp::Question,
}
/// `limit_exceeded`: which kind of bound was hit, so a client knows what could succeed instead.
/// Always present.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LimitDetails {
    pub scope: LimitScope,
    /// The bound, in the unit the message names (bytes, items, objects…), where it is a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u64>,
}
/// What a client can do about a `limit_exceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    /// The request itself is too large or names too many items: send less.
    Request,
    /// The response would be too large: ask for a smaller page.
    Result,
    /// One stored record is too large for this method; paging cannot help.
    Record,
    /// A daemon resource is full (connections, jobs, snapshots, receipts, pins…): release one
    /// or wait. Repeating the same request unchanged fails again until something is released.
    Capacity,
}
/// `unsupported`: present when the method or feature needs a capability the connection did not
/// negotiate. Absent when the refusal is about the target (platform, policy, agent).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UnsupportedDetails {
    pub capability: String,
}

/// `invalid_state` (with `sill.workspaces`): why a workspace operation cannot run, as a stable
/// reason (`stale_revision`, `source_overlap`, `submodules`…), and the files involved.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InvalidStateDetails {
    pub reason: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Codes whose `details` sfp/1 defines, with whether every occurrence carries them.
pub fn detail_codes() -> [(&'static str, bool); 5] {
    [("incompatible", true), ("stale_answer", true), ("limit_exceeded", true), ("unsupported", false), ("invalid_state", false)]
}
