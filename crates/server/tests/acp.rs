//! ACP v1 真实对接测试：起模拟 ACP agent（stdio 子进程），驱动 `AcpAgentDriver`
//! 的真实实现——验证方法帧序列、session/update 聚合、yolo 自动批准、会话列表。

use std::time::Duration;

use protocol::ContentBlock;

// server 是 bin crate：测试用 #[path] 引入 agent 模块的真实实现
#[path = "../src/agent.rs"]
mod agent;

use agent::{AcpAgentDriver, AgentDriver, AgentEvent, DialogRecord};

/// 隔离实验：直接 tokio spawn mock + 手动读写，定位管道/runtime 问题。
#[tokio::test]
async fn debug_mock_stdio() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let mut child = tokio::process::Command::new(mock)
        .arg("/tmp/mock_state_dbg")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\",\"mcpServers\":[]}}\n")
        .await
        .expect("write");
    stdin.flush().await.expect("flush");
    let line = tokio::time::timeout(std::time::Duration::from_secs(3), reader.next_line())
        .await
        .expect("timeout")
        .expect("read")
        .expect("line");
    println!("DEBUG mock 响应: {line}");
    assert!(line.contains("mock_s_1"));
}

#[tokio::test]
async fn acp_driver_full_flow() {
    let state_file = std::env::temp_dir().join(format!("mock_acp_state_{}", std::process::id()));
    let _ = std::fs::remove_file(&state_file);

    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let driver =
        AcpAgentDriver::spawn(mock, &[state_file.to_str().unwrap()]).expect("spawn mock acp");

    // create_session → session/new → mock_s_1
    let sid = driver.create_session("/tmp/work", None).expect("create");
    assert_eq!(sid, "mock_s_1");

    // prompt → session/prompt → 事件流聚合（thinking / tool_call / 输出 / 完成）
    let mut rx = driver.prompt(
        &sid,
        vec![ContentBlock::Text {
            text: "你好".into(),
        }],
    );
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(ev) = tokio::time::timeout_at(deadline, rx.recv()).await {
        let ev = ev.expect("事件流关闭");
        events.push(ev.clone());
        if matches!(ev, AgentEvent::TurnEnded) {
            break;
        }
    }
    let has_thinking = events.iter().any(|e| matches!(e, AgentEvent::Thinking(_)));
    let has_tool = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCall { .. }));
    let has_output = events
        .iter()
        .any(|e| matches!(e, AgentEvent::OutputChunk(s) if s == "完成！"));
    assert!(has_thinking, "应聚合 thinking 事件: {events:?}");
    assert!(has_tool, "应聚合 tool_call 事件: {events:?}");
    assert!(has_output, "应聚合 agent 输出: {events:?}");

    // yolo：request_permission 被自动批准（mock 记录到状态文件）
    tokio::time::sleep(Duration::from_millis(200)).await;
    let approved = std::fs::read_to_string(&state_file).unwrap_or_default();
    assert!(
        approved.contains("approved"),
        "request_permission 应被自动批准，状态文件: {approved:?}"
    );

    // load_session → session/load 重放 → 对话内容
    let records = driver.load_session(&sid).expect("load");
    let output_texts: Vec<String> = records
        .iter()
        .filter_map(|r| match r {
            DialogRecord::AgentOutput(c) => Some(
                c.iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect();
    assert!(
        output_texts.iter().any(|t| t.contains("历史回复")),
        "load 重放应含 agent 输出: {output_texts:?}"
    );

    // list_sessions → 恢复会话列表
    let sessions = driver.list_sessions();
    assert!(
        sessions.contains(&"mock_s_1".to_string()),
        "list 应含 mock_s_1: {sessions:?}"
    );

    // resume / cancel / delete 帧正常
    driver.resume_session(&sid).expect("resume");
    driver.cancel(&sid).expect("cancel");
    driver.delete(&sid).expect("delete");
}
