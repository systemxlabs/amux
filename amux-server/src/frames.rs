//! 出站帧构造：JSON-RPC 消息序列化为单帧文本（WebSocket 一个文本帧一条消息）。

use amux_common::jsonrpc::{JsonRpcNotification, JsonRpcResponse};
use serde::Serialize;

/// 通知帧。
pub fn notification(method: &str, params: &impl Serialize) -> String {
    serde_json::to_string(&JsonRpcNotification::new(method, params)).expect("通知序列化失败")
}

/// 成功响应帧（保留给 Server 未来作为请求服务方的场景）。
#[allow(dead_code)]
pub fn response(id: amux_common::jsonrpc::JsonRpcId, result: &impl Serialize) -> String {
    serde_json::to_string(&JsonRpcResponse::ok(id, result)).expect("响应序列化失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_frame_is_single_jsonrpc_message() {
        let frame = notification("acp", &serde_json::json!({"agent": "codex", "raw": "{}"}));
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "acp");
        assert_eq!(value["params"]["agent"], "codex");
    }
}
