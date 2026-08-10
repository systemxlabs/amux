//! 模拟 ACP v1 agent（stdio JSON-RPC）：按 ACP v1 帧应答 server 的方法调用。
//! 用于驱动 `AcpAgentDriver` 的对接测试（crates/server/tests/acp.rs）。
//! 用法：mock_acp <状态文件>——把收到的权限批准记录追加到状态文件。

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let state_file = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/mock_acp_state".into());
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = String::new();
    let mut reader = stdin.lock();

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
            eprintln!("[mock] 收到请求: {method}");
            let id = v.get("id").cloned();
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let sid = params
                .get("sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            match method {
                "session/new" => respond(&mut stdout, id, Some(json!({ "sessionId": "mock_s_1" }))),
                "session/load" => {
                    // 全量重放：用户消息 + agent 输出
                    notify(
                        &mut stdout,
                        &sid,
                        "user_message_chunk",
                        json!({ "messageId": "u1", "content": { "type": "text", "text": "你好" } }),
                    );
                    notify(
                        &mut stdout,
                        &sid,
                        "agent_message_chunk",
                        json!({ "messageId": "a1", "content": { "type": "text", "text": "历史回复" } }),
                    );
                    respond(&mut stdout, id, Some(json!({})));
                }
                "session/prompt" => {
                    // 先请求权限（期望 server yolo 自动批准）
                    let req_id = json!(9000);
                    let frame = json!({
                        "jsonrpc": "2.0", "id": req_id, "method": "session/request_permission",
                        "params": {
                            "sessionId": sid, "title": "运行命令？",
                            "options": [{ "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" }]
                        }
                    });
                    let _ = stdout.write_all(format!("{frame}\n").as_bytes());
                    // 事件流
                    notify(
                        &mut stdout,
                        &sid,
                        "agent_thought_chunk",
                        json!({ "messageId": "t1", "content": { "type": "text", "text": "思考中" } }),
                    );
                    notify(
                        &mut stdout,
                        &sid,
                        "tool_call",
                        json!({ "toolCallId": "tc1", "kind": "shell", "title": "运行 cargo test", "status": "pending" }),
                    );
                    notify(
                        &mut stdout,
                        &sid,
                        "agent_message_chunk",
                        json!({ "messageId": "a2", "content": { "type": "text", "text": "完成！" } }),
                    );
                    respond(&mut stdout, id, Some(json!({ "stopReason": "end_turn" })));
                }
                "session/cancel" | "session/close" | "session/delete" | "session/resume" => {
                    respond(&mut stdout, id, Some(json!({})))
                }
                "session/list" => respond(
                    &mut stdout,
                    id,
                    Some(json!({ "sessions": [{ "id": "mock_s_1" }] })),
                ),
                _ => respond(&mut stdout, id, None),
            }
        } else if v.get("id").is_some() && v.get("result").is_some() {
            // client 的响应：权限批准（outcome.selected）→ 记录到状态文件
            if v["result"]["outcome"]["outcome"].as_str() == Some("selected") {
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&state_file)
                    .map(|mut f| {
                        let _ = writeln!(f, "approved");
                    });
            }
        }
    }
}

fn notify(w: &mut io::Stdout, sid: &str, kind: &str, payload: Value) {
    let mut params = payload.clone();
    params["sessionUpdate"] = json!(kind);
    params["sessionId"] = json!(sid);
    let frame = json!({ "jsonrpc": "2.0", "method": "session/update", "params": params });
    let _ = w.write_all(format!("{frame}\n").as_bytes());
    let _ = w.flush();
}

fn respond(w: &mut io::Stdout, id: Option<Value>, result: Option<Value>) {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let _ = w.write_all(format!("{frame}\n").as_bytes());
    let _ = w.flush();
}
