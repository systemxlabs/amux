//! JSON-RPC 2.0 信封类型与错误码。

use serde::{Deserialize, Serialize};

/// JSON-RPC id：本协议只产生整数 id（GUI 自增计数器），但按规范兼容字符串；
/// `Null` 仅用于无法确定 id 的错误响应（如 Parse error）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
    Null,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
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
    pub const AUTH_FAILED: i32 = -32000;
    pub const SESSION_NOT_FOUND: i32 = -32001;
    pub const AGENT_UNAVAILABLE: i32 = -32002;
    pub const SESSION_BUSY: i32 = -32003;
    pub const INVALID_INPUT: i32 = -32005;
    pub const TERMINAL_NOT_FOUND: i32 = -32006;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Number(1),
            method: "session.list".into(),
            params: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "session.list");
        assert_eq!(back.id, JsonRpcId::Number(1));
        assert!(
            serde_json::from_str::<JsonRpcRequest>(r#"{"jsonrpc":"2.0","method":"auth"}"#).is_err()
        );
    }

    #[test]
    fn id_accepts_string_and_null() {
        assert_eq!(
            serde_json::from_str::<JsonRpcId>(r#""abc""#).unwrap(),
            JsonRpcId::String("abc".into())
        );
        assert_eq!(
            serde_json::from_str::<JsonRpcId>("null").unwrap(),
            JsonRpcId::Null
        );
    }

    #[test]
    fn response_with_error_roundtrip() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::String("x".into()),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "方法不存在".into(),
                data: None,
            }),
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"error\""));
        let back: JsonRpcResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.error.unwrap().code, -32601);
    }

    #[test]
    fn content_block_roundtrip() {
        let block = crate::types::ContentBlock::Text { text: "hi".into() };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains("\"type\":\"text\""));
        let back: crate::types::ContentBlock = serde_json::from_str(&s).unwrap();
        match back {
            crate::types::ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("type mismatch"),
        }
    }
}
