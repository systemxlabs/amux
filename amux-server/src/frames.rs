//! 出站帧构造：JSON-RPC 消息序列化为单帧文本（WebSocket 一个文本帧一条消息）。

use amux_common::jsonrpc::JsonRpcNotification;
use serde::Serialize;

/// 通知帧。
pub fn notification(method: &str, params: &impl Serialize) -> String {
    serde_json::to_string(&JsonRpcNotification::new(method, params)).expect("通知序列化失败")
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
