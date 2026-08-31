//! JSON-RPC 2.0 over a line of stdin at a time.
//!
//! MCP's stdio transport is newline-delimited JSON — one object per line, no
//! framing header — so the whole transport is a `BufRead::read_line` and a
//! `serde_json::from_str`. It is written out here rather than taken from an SDK
//! because the rest of this workspace is synchronous and dependency-light, and
//! an SDK would bring an async runtime along for a protocol this size.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One request from the client. `id` is absent for a notification, which is a
/// message the client does not expect an answer to.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// The JSON-RPC codes this server returns. A tool that fails does not appear
/// here: that comes back as a result with `isError`, because the model can act
/// on it and cannot act on a protocol failure.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// A response to a request whose id could not be read. The spec asks for a
    /// null id here rather than silence.
    pub fn anonymous_err(code: i32, message: impl Into<String>) -> Self {
        Response::err(json!(null), code, message)
    }
}
