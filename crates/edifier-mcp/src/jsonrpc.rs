//! Minimal, dependency-free JSON-RPC 2.0 layer for the MCP stdio transport.
//!
//! MCP over stdio is newline-delimited JSON: one complete JSON-RPC message per line, no framing
//! header. The types here cover exactly what a tools-only server needs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC error codes used by this server.
pub mod code {
    /// Invalid JSON or an invalid request object.
    pub const PARSE_ERROR: i64 = -32700;
    /// The request object is not a valid JSON-RPC 2.0 request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad parameters for an existing method.
    pub const INVALID_PARAMS: i64 = -32602;
}

/// An incoming request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Whether the peer expects an answer (`false` for notifications).
    #[must_use]
    pub fn expects_reply(&self) -> bool {
        self.id.is_some()
    }

    /// Decode the whole line, returning `None` for blank input.
    ///
    /// # Errors
    ///
    /// Propagates the `serde_json` error so the caller can answer with [`code::PARSE_ERROR`].
    pub fn parse(line: &str) -> Result<Option<Request>, serde_json::Error> {
        if line.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(line).map(Some)
    }
}

/// A successful reply.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub result: Value,
}

/// A failed reply.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub error: ErrorObject,
}

/// JSON-RPC error body.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    /// Build a success reply for `id`.
    #[must_use]
    pub fn new(id: Value, result: Value) -> Self {
        Response {
            jsonrpc: "2.0",
            id,
            result,
        }
    }

    /// Render the reply as a single line without a trailing newline.
    ///
    /// # Errors
    ///
    /// Fails only if `result` cannot be serialized, which cannot happen for `Value`.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl ErrorResponse {
    /// Build an error reply for `id`.
    #[must_use]
    pub fn new(id: Value, code: i64, message: impl Into<String>) -> Self {
        ErrorResponse {
            jsonrpc: "2.0",
            id,
            error: ErrorObject {
                code,
                message: message.into(),
                data: None,
            },
        }
    }

    /// Convert a tool failure into a JSON-RPC error.
    ///
    /// # Errors
    ///
    /// See [`ErrorResponse::to_line`].
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications() {
        let request = Request::parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(request.method, "tools/list");
        assert!(request.expects_reply());
        assert_eq!(request.id.unwrap(), Value::from(7));

        let notification =
            Request::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap()
                .unwrap();
        assert!(!notification.expects_reply());

        assert!(Request::parse("   ").unwrap().is_none());
        assert!(Request::parse("{not json").is_err());
    }

    #[test]
    fn responses_are_single_line_json() {
        let line = Response::new(Value::from(1), serde_json::json!({"ok": true}))
            .to_line()
            .unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["id"],
            Value::from(1)
        );

        let error = ErrorResponse::new(Value::Null, code::METHOD_NOT_FOUND, "nope")
            .to_line()
            .unwrap();
        let parsed: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(parsed["error"]["code"], Value::from(code::METHOD_NOT_FOUND));
    }
}
