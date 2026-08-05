//! Wire types for the buyer's local JSON-RPC surface.
//!
//! One request and one response per line (newline-delimited JSON) over the
//! [`crate::buyer`] Unix socket. Deliberately small: a shell for the thin-client
//! boundary, not the buyer lifecycle (later phases add the trade methods).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A client call: a method name, opaque params, and an echo id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub id: Value,
}

/// The daemon's reply: exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// A structured failure. `code` follows JSON-RPC conventions loosely; the daemon
/// uses [`CODE_NOT_IMPLEMENTED`] for methods that are recognized but deferred to a
/// later phase, and [`CODE_METHOD_NOT_FOUND`] for unknown methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Unknown method name.
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Recognized method, not yet built (step-1 daemon shell).
pub const CODE_NOT_IMPLEMENTED: i64 = -32001;
/// The daemon failed to service a recognized method (actor/store error).
pub const CODE_INTERNAL: i64 = -32603;

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}
