//! ACP v1 真实对接测试：起模拟 ACP agent（stdio 子进程），驱动 `AcpAgentDriver`
//! 的真实实现——验证方法帧序列、session/update 聚合、yolo 自动批准、
//! `session/resume`（恢复 agent 自身上下文）。
//! 关闭会话经 `session/close` 帧释放 agent 侧资源，删除经 `session/delete` 清理远端记录。
//! 另覆盖 terminal/* 反向请求全链路（agent 委托客户端执行命令，kimi acp 的路径）。

use std::time::Duration;

use protocol::ContentBlock;

use amux_server::agent::{AcpAgentDriver, AgentEvent};

#[tokio::test]
async fn acp_driver_terminal_flow() {
    // agent 经 terminal/* 反向请求在客户端执行命令（kimi acp 的 shell 执行路径）
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("mock_acp_term");
    let state_file_s = state_file.to_str().unwrap().to_string();

    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let driver =
        AcpAgentDriver::spawn(mock, &[state_file_s.as_str()], &[]).expect("spawn mock acp");

    let (sid, _) = driver.create_session("/tmp/work").expect("create");
    let mut rx = driver.prompt(
        &sid,
        vec![ContentBlock::Text {
            text: "/terminal".into(),
        }],
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut output_chunks: Vec<String> = Vec::new();
    while let Ok(ev) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match ev.expect("事件流关闭") {
            AgentEvent::OutputChunk(text) => output_chunks.push(text),
            AgentEvent::TurnEnded(_) => break,
            _ => {}
        }
    }
    let joined = output_chunks.join("");
    assert!(
        joined.contains("amux-terminal-ok"),
        "agent 应能经 terminal/* 在客户端执行命令: {joined:?}"
    );
    assert!(
        joined.contains("exit=0"),
        "terminal/wait_for_exit 应返回退出状态: {joined:?}"
    );

    driver.close(&sid).expect("close");
}

#[tokio::test]
async fn acp_driver_full_flow() {
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("mock_acp_state");
    let calls_file = state_file.with_extension("calls");
    let state_file_s = state_file.to_str().unwrap().to_string();

    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let driver =
        AcpAgentDriver::spawn(mock, &[state_file_s.as_str()], &[]).expect("spawn mock acp");

    let (sid, options) = driver.create_session("/tmp/work").expect("create");
    assert_eq!(sid, "mock_s_1");
    // mock 声明了 configOptions 能力：new 响应带回初始选项
    assert_eq!(options.len(), 1, "初始选项应含 model: {options:?}");
    assert_eq!(options[0].id, "model");
    let model_current = match &options[0].kind {
        protocol::SessionConfigKind::Select { current_value, .. } => current_value.clone(),
        other => panic!("应为 Select 选项: {other:?}"),
    };
    assert_eq!(model_current, "gpt-4o");
    driver
        .resume_session("mock_s_restored", "/tmp/work")
        .expect("restore");

    // 设置会话选项：返回更新后的完整选项集合
    let updated = driver
        .set_config_option(
            &sid,
            "model",
            protocol::SessionConfigOptionValue::ValueId {
                value: "gpt-5".into(),
            },
        )
        .expect("set_config_option");
    let model = updated
        .iter()
        .find(|o| o.id == "model")
        .expect("model 选项仍在");
    match &model.kind {
        protocol::SessionConfigKind::Select { current_value, .. } => {
            assert_eq!(current_value, "gpt-5")
        }
        other => panic!("应为 Select 选项: {other:?}"),
    }

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
                id, name, title, ..
            } => Some((id.as_str(), name.as_deref(), title.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tools.len(),
        2,
        "ToolCall 与 ToolCallUpdate 各产生一条事件: {tools:?}"
    );
    assert_eq!(tools[0], ("tc1", Some("execute"), Some("运行 cargo test")));
    assert_eq!(
        tools[1],
        ("tc1", None, Some("运行 cargo test 完成")),
        "update 的 kind 缺失时 name 为 None（合并器沿用同 id 名称）"
    );

    // TurnEnded 表示权限请求及其后续事件已经完成，权限批准记录此时可直接读取。
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
