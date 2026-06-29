//! Basic JSON-RPC 2.0 types: the minimal subset needed for MCP over stdio.
//!
//! Covers only what the server needs: parsing inbound requests/notifications and building success/error responses.
//! Pure serde/serde_json, no tokio.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Standard JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// One inbound JSON-RPC message, either a request or a notification.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Requests have an id (string/number); notifications omit the id field.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Notifications have no id and do not require a response.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Outbound JSON-RPC response.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// Build an error without data.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl JsonRpcResponse {
    /// Success response: echo the request id.
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response: echo the request id, or use `Value::Null` when parse failure prevents reading an id.
    pub fn error(id: Value, err: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn success_response_omits_error_field() {
        let r = JsonRpcResponse::success(json!(1), json!({"ok": true}));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})
        );
    }

    #[test]
    fn error_response_omits_result_field() {
        let r = JsonRpcResponse::error(
            json!(2),
            JsonRpcError::new(METHOD_NOT_FOUND, "no such method"),
        );
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32601, "message": "no such method"}})
        );
    }

    #[test]
    fn error_with_null_id_serializes_id_null() {
        let r = JsonRpcResponse::error(Value::Null, JsonRpcError::new(PARSE_ERROR, "bad json"));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], json!(-32700));
    }

    #[test]
    fn request_with_id_is_not_notification() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert!(!req.is_notification());
        assert_eq!(req.method, "ping");
    }

    #[test]
    fn message_without_id_is_notification() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn params_absent_deserializes_to_none() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).unwrap();
        assert!(req.params.is_none());
    }
}
