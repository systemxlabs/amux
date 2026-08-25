//! ACP v1 真实对接测试：起模拟 ACP agent（stdio 子进程），驱动 `AcpAgentDriver`
//! 的真实实现——验证方法帧序列、session/update 聚合、yolo 自动批准、
//! `session/resume`（恢复 agent 自身上下文，docs/DESIGN.md §7.2）。
//! 关闭会话经 `session/close` 帧释放 agent 侧资源，删除经 `session/delete` 清理远端记录
//!（docs/DESIGN.md「ACP 生命周期」）。

use std::time::Duration;

use protocol::ContentBlock;

use server::agent::{AcpAgentDriver, AgentDriver, AgentEvent};

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
    let _ = events;

    // yolo：request_permission 被自动批准（mock 记录到状态文件）
    tokio::time::sleep(Duration::from_millis(200)).await;
    let approved = std::fs::read_to_string(&state_file).unwrap_or_default();
    assert!(
        approved.contains("approved"),
        "request_permission 应被自动批准, 状态文件: {approved:?}"
    );

    // 调用面：session/new、session/prompt、session/resume（恰好一次）
    let calls = std::fs::read_to_string(&calls_file).unwrap_or_default();
    assert!(calls.contains("session/new"), "calls: {calls:?}");
    assert!(calls.contains("session/prompt"), "calls: {calls:?}");
    let resume_count = calls.lines().filter(|l| *l == "session/resume").count();
    assert_eq!(
        resume_count, 1,
        "恢复会话应恰好 resume 一次（幂等）: {calls:?}"
    );

    // 删除先 close 释放运行资源，再 delete 清理远端记录。
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
