//! JSON-RPC 2.0 基础类型——MCP over stdio 的最小子集。
//!
//! 只覆盖 server 需要的：解析入站请求/通知、构造成功/错误响应。纯 serde/serde_json，无 tokio。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 标准 JSON-RPC 2.0 错误码。
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// 一条入站 JSON-RPC 消息（请求或通知）。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// 请求有 id（string/number）；通知无 id 字段。
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// 通知（无 id）不需要响应。
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// 出站 JSON-RPC 响应。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// 构造一个无 data 的错误。
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
}

impl JsonRpcResponse {
    /// 成功响应：回显请求 id。
    pub fn success(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    /// 错误响应：回显请求 id（解析失败拿不到 id 时用 `Value::Null`）。
    pub fn error(id: Value, err: JsonRpcError) -> Self {
        Self { jsonrpc: "2.0", id, result: None, error: Some(err) }
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
        assert_eq!(v, json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}));
    }

    #[test]
    fn error_response_omits_result_field() {
        let r = JsonRpcResponse::error(json!(2), JsonRpcError::new(METHOD_NOT_FOUND, "no such method"));
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
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn params_absent_deserializes_to_none() {
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).unwrap();
        assert!(req.params.is_none());
    }
}
