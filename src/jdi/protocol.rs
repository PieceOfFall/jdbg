//! JSON payload types for the JDI sidecar protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SIDECAR_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarMessage {
    Request {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<SidecarErrorPayload>,
    },
    Event {
        session: String,
        seq: u64,
        event: String,
        #[serde(default)]
        payload: Value,
    },
    Heartbeat {
        seq: u64,
    },
}

impl SidecarMessage {
    pub fn matches_response_id(&self, expected: &str) -> bool {
        matches!(self, SidecarMessage::Response { id, .. } if id == expected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeRequest {
    pub protocol_version: u32,
    pub server_version: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarProtocolError {
    #[error("invalid sidecar auth token")]
    InvalidToken,
    #[error("unsupported sidecar protocol version {got}; expected {expected}")]
    UnsupportedProtocolVersion { got: u32, expected: u32 },
}

pub fn validate_handshake(
    request: &HandshakeRequest,
    expected_token: &str,
) -> Result<HandshakeResponse, SidecarProtocolError> {
    if request.token != expected_token {
        return Err(SidecarProtocolError::InvalidToken);
    }
    if request.protocol_version != SIDECAR_PROTOCOL_VERSION {
        return Err(SidecarProtocolError::UnsupportedProtocolVersion {
            got: request.protocol_version,
            expected: SIDECAR_PROTOCOL_VERSION,
        });
    }
    Ok(HandshakeResponse {
        protocol_version: SIDECAR_PROTOCOL_VERSION,
        capabilities: vec![
            "ping".into(),
            "shutdown".into(),
            "attach".into(),
            "detach".into(),
            "threads".into(),
            "stack".into(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_response_event_and_heartbeat_have_stable_json_shape() {
        let request = SidecarMessage::Request {
            id: "r1".into(),
            method: "ping".into(),
            params: json!({}),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"type": "request", "id": "r1", "method": "ping", "params": {}})
        );

        let response = SidecarMessage::Response {
            id: "r1".into(),
            result: Some(json!({"ok": true})),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"type": "response", "id": "r1", "result": {"ok": true}})
        );

        let event = SidecarMessage::Event {
            session: "s1".into(),
            seq: 7,
            event: "vmDisconnected".into(),
            payload: json!({"reason": "target exit"}),
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"type": "event", "session": "s1", "seq": 7, "event": "vmDisconnected", "payload": {"reason": "target exit"}})
        );

        let heartbeat = SidecarMessage::Heartbeat { seq: 8 };
        assert_eq!(
            serde_json::to_value(heartbeat).unwrap(),
            json!({"type": "heartbeat", "seq": 8})
        );
    }

    #[test]
    fn response_id_matching_is_explicit() {
        let response = SidecarMessage::Response {
            id: "r2".into(),
            result: Some(json!({})),
            error: None,
        };

        assert!(response.matches_response_id("r2"));
        assert!(!response.matches_response_id("r3"));
        assert!(!SidecarMessage::Heartbeat { seq: 1 }.matches_response_id("r2"));
    }

    #[test]
    fn handshake_success_returns_capabilities() {
        let req = HandshakeRequest {
            protocol_version: SIDECAR_PROTOCOL_VERSION,
            server_version: "1.0.0".into(),
            token: "secret".into(),
        };

        let response = validate_handshake(&req, "secret").unwrap();

        assert_eq!(response.protocol_version, SIDECAR_PROTOCOL_VERSION);
        assert!(response.capabilities.contains(&"ping".to_string()));
        assert!(response.capabilities.contains(&"attach".to_string()));
    }

    #[test]
    fn handshake_rejects_bad_token() {
        let req = HandshakeRequest {
            protocol_version: SIDECAR_PROTOCOL_VERSION,
            server_version: "1.0.0".into(),
            token: "wrong".into(),
        };

        assert_eq!(
            validate_handshake(&req, "secret").unwrap_err(),
            SidecarProtocolError::InvalidToken
        );
    }

    #[test]
    fn handshake_rejects_unsupported_protocol_version() {
        let req = HandshakeRequest {
            protocol_version: SIDECAR_PROTOCOL_VERSION + 1,
            server_version: "1.0.0".into(),
            token: "secret".into(),
        };

        assert_eq!(
            validate_handshake(&req, "secret").unwrap_err(),
            SidecarProtocolError::UnsupportedProtocolVersion {
                got: SIDECAR_PROTOCOL_VERSION + 1,
                expected: SIDECAR_PROTOCOL_VERSION,
            }
        );
    }
}
