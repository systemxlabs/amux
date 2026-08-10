//! 模拟 ACP v1 agent（stdio JSON-RPC）：按 ACP v1 帧应答 server 的方法调用。
//! 用于驱动 `AcpAgentDriver` 的对接测试（crates/server/tests/acp.rs）与
//! test-server 的端到端测试。
//!
//! 行为要点：
//! - `session/new` 返回自增的唯一 sessionId（mock_s_1、mock_s_2、…），支持多会话
//! - `session/prompt` 记录该会话的用户指令与 agent 输出（内存），`session/load`
//!   全量重放记录的历史（更接近真实 agent 的持久化语义）
//! - `session/prompt` 先请求权限（期望 server yolo 自动批准），随后 sleep
//!   `AMUX_MOCK_DELAY_MS`（默认 300ms）再发事件流与响应——保证忙时 prompt
//!   （-32006）测试有确定性的 busy 窗口
//! - `skill/list` 返回固定的 skills 列表（PRD §3.3）
//! - 把收到的权限批准记录追加到状态文件（第二个参数，或 `AMUX_MOCK_STATE`）

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 每会话的历史（用户指令 + agent 输出 content block 列表）。
fn history() -> &'static std::sync::Mutex<HashMap<String, Vec<Value>>> {
    use std::sync::OnceLock;
    static H: OnceLock<std::sync::Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn delay_ms() -> u64 {
    std::env::var("AMUX_MOCK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

fn main() {
    let state_file = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AMUX_MOCK_STATE").ok())
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
                "session/new" => {
                    let n = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                    respond(
                        &mut stdout,
                        id,
                        Some(json!({ "sessionId": format!("mock_s_{n}") })),
                    );
                }
                "session/load" => {
                    // 全量重放：该会话记录的历史（用户指令 + agent 输出）
                    let hist = history().lock().unwrap().get(&sid).cloned().unwrap_or_else(|| {
                        vec![
                            json!({ "messageId": "u1", "kind": "user", "content": { "type": "text", "text": "你好" } }),
                            json!({ "messageId": "a1", "kind": "agent", "content": { "type": "text", "text": "历史回复" } }),
                        ]
                    });
                    for (i, item) in hist.iter().enumerate() {
                        let kind = if item["kind"] == "user" {
                            "user_message_chunk"
                        } else {
                            "agent_message_chunk"
                        };
                        let mid =
                            format!("{}{}", if item["kind"] == "user" { "u" } else { "a" }, i);
                        notify(
                            &mut stdout,
                            &sid,
                            kind,
                            json!({ "messageId": mid, "content": item["content"].clone() }),
                        );
                    }
                    respond(&mut stdout, id, Some(json!({})));
                }
                "session/prompt" => {
                    // 记录用户指令（先取 id 再锁，避免 json! 内再次锁同一 mutex 死锁）
                    let user_text: String = params
                        .get("prompt")
                        .and_then(|p| p.as_array())
                        .and_then(|arr| {
                            arr.iter().find_map(|b| {
                                b.get("text").and_then(|t| t.as_str()).map(str::to_string)
                            })
                        })
                        .unwrap_or_default();
                    let user_mid = format!("u{}", history_len(&sid));
                    history()
                        .lock()
                        .unwrap()
                        .entry(sid.clone())
                        .or_default()
                        .push(json!({
                            "messageId": user_mid,
                            "kind": "user",
                            "content": { "type": "text", "text": user_text }
                        }));
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
                    let _ = stdout.flush();
                    // busy 窗口：让 server 有确定的时间观察 Busy 状态（-32006 测试）
                    let ms = delay_ms();
                    if ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                    }
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
                    let agent_mid = format!("a{}", history_len(&sid));
                    notify(
                        &mut stdout,
                        &sid,
                        "agent_message_chunk",
                        json!({ "messageId": agent_mid, "content": { "type": "text", "text": "完成！" } }),
                    );
                    history()
                        .lock()
                        .unwrap()
                        .entry(sid.clone())
                        .or_default()
                        .push(json!({
                            "messageId": agent_mid,
                            "kind": "agent",
                            "content": { "type": "text", "text": "完成！" }
                        }));
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
                "skill/list" => respond(
                    &mut stdout,
                    id,
                    Some(json!({
                        "skills": [
                            { "name": "web-browser" },
                            { "name": "docs-search" },
                            { "name": "code-analysis" }
                        ]
                    })),
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

fn history_len(sid: &str) -> usize {
    history()
        .lock()
        .unwrap()
        .get(sid)
        .map(|h| h.len())
        .unwrap_or(0)
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
