//! ACP v2 真实对接测试：起模拟 ACP agent（stdio 子进程），连接 `AcpConnection`
//! 的真实实现——验证方法帧序列、`session/update` 的 upsert 事件路由、权限自动批准、
//! `session/resume`（恢复 agent 自身上下文，不带 replayFrom）。
//! 关闭会话经 `session/close` 帧释放 agent 侧资源，删除经 `session/delete` 清理远端记录。

use std::time::Duration;

use protocol::{ContentBlock, SessionState};

use amux_server::agent::{AcpConnection, AgentEvent};

#[tokio::test]
async fn acp_client_full_flow() {
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("mock_acp_state");
    let calls_file = state_file.with_extension("calls");
    let state_file_s = state_file.to_str().unwrap().to_string();

    let mock = env!("CARGO_BIN_EXE_mock_acp");
    let connection =
        AcpConnection::spawn(mock, &[state_file_s.as_str()], &[]).expect("spawn mock acp");

    let (sid, options) = connection.create_session("/tmp/work").expect("create");
    assert_eq!(sid, "mock_s_1");
    assert_eq!(options.len(), 1, "初始选项应含 model: {options:?}");
    assert_eq!(options[0].id, "model");
    let model_current = match &options[0].kind {
        protocol::SessionConfigKind::Select { current_value, .. } => current_value.clone(),
        other => panic!("应为 Select 选项: {other:?}"),
    };
    assert_eq!(model_current, "gpt-4o");
    connection
        .resume_session("mock_s_restored", "/tmp/work")
        .expect("restore");

    // 设置会话选项：返回更新后的完整选项集合
    let updated = connection
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

    let mut rx = connection.prompt(
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
        // v2：前台结束由 state_update(idle) 报告
        if matches!(
            ev,
            AgentEvent::StateUpdate {
                state: SessionState::Idle,
                ..
            }
        ) {
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

    // 思考与输出都带 messageId（upsert 语义的键）
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            AgentEvent::ThinkingChunk { message_id, .. } if message_id == "t1"
        )),
        "应收到带 messageId 的思考事件: {events:?}"
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            AgentEvent::AgentMessageChunk { message_id, .. } if message_id == "a1"
        )),
        "应收到带 messageId 的 agent 消息事件: {events:?}"
    );

    // tool_call_update 按 toolCallId 下发：两次 update 各产生一条事件，缺省字段保持不变
    let tools = events
        .iter()
        .filter_map(|ev| match ev {
            AgentEvent::ToolCall {
                id, name, title, ..
            } => Some((id.as_str(), name.as_deref(), title.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2, "两次 tool_call_update: {tools:?}");
    assert_eq!(tools[0], ("tc1", Some("execute"), Some("运行 cargo test")));
    assert_eq!(
        tools[1],
        ("tc1", None, Some("运行 cargo test 完成")),
        "未携带 kind 的 update 不再重复给出名称"
    );

    // 事件流结束（idle）时权限请求已完成，批准记录可直接读取。
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

    connection.close(&sid).expect("close");
    connection.delete_session(&sid).expect("delete");
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
