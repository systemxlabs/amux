//! ACP v1 真实对接测试：起模拟 ACP agent（stdio 子进程），驱动 `AcpAgentDriver`
//! 的真实实现——验证方法帧序列、session/update 聚合、yolo 自动批准、
//! `session/resume`（恢复 agent 自身上下文）。
//! 关闭会话经 `session/close` 帧释放 agent 侧资源，删除经 `session/delete` 清理远端记录。

use std::time::Duration;

use protocol::ContentBlock;

use amux_server::agent::{AcpAgentDriver, AgentDriver, AgentEvent};

#[tokio::test]
async fn acp_driver_full_flow() {
    let state_file = std::env::temp_dir().join(format!("mock_acp_state_{}", std::process::id()));
    let _ = std::fs::remove_file(&state_file);
    let calls_file = state_file.with_extension("calls");
    let _ = std::fs::remove_file(&calls_file);
    let state_file_s = state_file.to_str().unwrap().to_string();

    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let driver =
        AcpAgentDriver::spawn(mock, &[state_file_s.as_str()], &[]).expect("spawn mock acp");

    let sid = driver.create_session("/tmp/work").expect("create");
    assert_eq!(sid, "mock_s_1");
    driver
        .resume_session("mock_s_restored", "/tmp/work")
        .expect("restore");

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
        if matches!(ev, AgentEvent::TurnEnded(_)) {
            break;
        }
    }
    let usage = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::UsageUpdate { used, size } => Some((*used, *size)),
            _ => None,
        })
        .expect("prompt 事件流应收到 usage_update");
    assert_eq!(usage, (53_000, 200_000));

    // tool_call 与同 id 的 tool_call_update 都应路由（合并发生在 TurnMerger）。
    let tools = events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::ToolCall {
                id,
                name,
                title,
                ..
            } => Some((id.as_str(), name.as_deref(), title.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2, "ToolCall 与 ToolCallUpdate 各产生一条事件: {tools:?}");
    assert_eq!(tools[0], ("tc1", Some("execute"), Some("运行 cargo test")));
    assert_eq!(
        tools[1],
        ("tc1", None, Some("运行 cargo test 完成")),
        "update 的 kind 缺失时 name 为 None（合并器沿用同 id 名称）"
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    let approved = std::fs::read_to_string(&state_file).unwrap_or_default();
    assert!(
        approved.contains("approved"),
        "request_permission 应被自动批准, 状态文件: {approved:?}"
    );

    let calls = std::fs::read_to_string(&calls_file).unwrap_or_default();
    assert!(calls.contains("session/new"), "calls: {calls:?}");
    assert!(calls.contains("session/prompt"), "calls: {calls:?}");
    let resume_count = calls.lines().filter(|l| *l == "session/resume").count();
    assert_eq!(
        resume_count, 1,
        "恢复会话应恰好 resume 一次（幂等）: {calls:?}"
    );

    driver.close(&sid).expect("close");
    driver.delete_session(&sid).expect("delete");
    let calls = std::fs::read_to_string(&calls_file).unwrap_or_default();
    assert!(
        calls.contains("session/close"),
        "关闭应触发 session/close: {calls:?}"
    );
    assert!(
        calls.contains("session/delete"),
        "删除应触发 session/delete: {calls:?}"
    );
}
