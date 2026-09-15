//! JSON-RPC 2.0 信封类型与错误码（Server-Daemon WebSocket 传输）。

use serde::{Deserialize, Serialize};

/// JSON-RPC id。本协议只产生整数 id（自增计数器），但按规范兼容字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
}

impl From<u64> for JsonRpcId {
    fn from(n: u64) -> Self {
        JsonRpcId::Number(n)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Number(id),
            method: method.into(),
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: JsonRpcId, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }

    pub fn error(id: JsonRpcId, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// JSON-RPC 2.0 标准错误码。
pub mod rpc_error {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// 业务错误码（-32000..-32099）。
pub mod server_error {
    pub const AGENT_UNAVAILABLE: i32 = -32000;
    pub const INVALID_INPUT: i32 = -32001;
    pub const TERMINAL_NOT_FOUND: i32 = -32002;
    pub const GIT_FAILED: i32 = -32003;
    pub const FS_FAILED: i32 = -32004;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_requires_id() {
        let req = JsonRpcRequest::new(1, "agent.list", serde_json::json!({}));
        let text = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(back.method, "agent.list");
        assert_eq!(back.id, JsonRpcId::Number(1));
        assert!(
            serde_json::from_str::<JsonRpcRequest>(r#"{"jsonrpc":"2.0","method":"agent.list"}"#)
                .is_err(),
            "请求必须带 id"
        );
    }

    #[test]
    fn error_response_roundtrip() {
        let resp = JsonRpcResponse::error(JsonRpcId::Number(7), server_error::GIT_FAILED, "boom");
        let text = serde_json::to_string(&resp).unwrap();
        assert!(!text.contains("result"));
        let back: JsonRpcResponse = serde_json::from_str(&text).unwrap();
        assert_eq!(back.error.unwrap().code, server_error::GIT_FAILED);
    }
}
