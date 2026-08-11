//! JSON-RPC 2.0 信封类型与错误码（docs/DESIGN.md §4）。

use serde::{Deserialize, Serialize};

pub type JsonRpcId = serde_json::Value;

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
    pub const SESSION_NOT_FOUND: i32 = -32001;
    pub const HARNESS_UNAVAILABLE: i32 = -32002;
    pub const SESSION_BUSY: i32 = -32003;
    pub const INVALID_INPUT: i32 = -32005;
    /// steer 失败：agent 不支持进行中注入（docs/DESIGN.md §9）
    pub const STEER_UNSUPPORTED: i32 = -32006;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            method: "get_info".into(),
            params: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "get_info");
        assert_eq!(back.id, serde_json::json!(1));
    }

    #[test]
    fn response_with_error_roundtrip() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::json!("x"),
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
