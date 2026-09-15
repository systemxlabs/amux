//! 出站帧构造：JSON-RPC 消息序列化为单帧文本（WebSocket 一个文本帧一条消息）。

use amux_common::jsonrpc::{JsonRpcId, JsonRpcNotification, JsonRpcResponse};
use serde::Serialize;

/// 通知帧。
pub fn notification(method: &str, params: &impl Serialize) -> String {
    serde_json::to_string(&JsonRpcNotification::new(method, params)).expect("通知序列化失败")
}

/// 成功响应帧。
pub fn response(id: JsonRpcId, result: &impl Serialize) -> String {
    serde_json::to_string(&JsonRpcResponse::ok(id, result)).expect("响应序列化失败")
}

/// 错误响应帧。
pub fn error_response(id: JsonRpcId, code: i32, message: &str) -> String {
    serde_json::to_string(&JsonRpcResponse::error(id, code, message)).expect("错误响应序列化失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_common::daemon::{notify, AcpForward};

    #[test]
    fn acp_notification_frame_shape() {
        let frame = notification(
            notify::ACP,
            &AcpForward {
                agent: "codex".into(),
                raw: r#"{"jsonrpc":"2.0","id":1}"#.into(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "acp");
        assert_eq!(value["params"]["agent"], "codex");
        assert!(value.get("id").is_none(), "通知不带 id");
    }
}
