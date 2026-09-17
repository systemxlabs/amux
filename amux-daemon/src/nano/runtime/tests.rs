use super::*;
use agent_client_protocol::{schema::ProtocolVersion, Agent};
use rig_core::{
    completion::message::{Reasoning, ToolCall, ToolFunction, UserContent},
    test_utils::{MockCompletionModel, MockTurn},
};
use tokio::sync::{mpsc, oneshot};

fn tool_turn(id: &str, command: &str) -> MockTurn {
    MockTurn::from_contents([
        AssistantContent::Reasoning(Reasoning::new("检查工作目录")),
        AssistantContent::ToolCall(ToolCall::from_wire(
            id,
            ToolFunction::new("shell".into(), serde_json::json!({"command": command})),
        )),
    ])
}

fn has_tool_result(messages: &[Message], expected: &str) -> bool {
    messages.iter().any(|message| match message {
        Message::User { content } => content.iter().any(|item| match item {
            UserContent::ToolResult(result) => {
                serde_json::to_string(result).unwrap().contains(expected)
            }
            _ => false,
        }),
        _ => false,
    })
}

#[tokio::test]
async fn runtime_preserves_tool_history_and_maps_acp_events() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let model = MockCompletionModel::new([
        tool_turn("call-one", "printf hello > result; printf hello"),
        MockTurn::text("完成"),
        MockTurn::text("后续回答"),
    ]);
    let recorded = model.clone();
    let history = Arc::new(Mutex::new(vec![
        Message::user("之前的问题"),
        Message::assistant("之前的回答"),
    ]));
    let saved = history.clone();
    let (done_tx, done_rx) = oneshot::channel();
    let done_tx = Mutex::new(Some(done_tx));
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    let agent = Agent.v2().on_receive_request(
        async move |request: InitializeRequest, responder, cx: V2ConnectionTo<Client>| {
            responder.respond(InitializeResponse::new(
                request.protocol_version,
                Implementation::new("nano-test", "0"),
            ))?;
            let model = model.clone();
            let history = history.clone();
            let cwd = cwd.clone();
            let done = done_tx.lock().take().unwrap();
            cx.clone().spawn(async move {
                let events = Events {
                    cx,
                    id: SessionId::new("session"),
                };
                run(
                    AgentBuilder::new(model.clone()),
                    cwd.clone(),
                    "执行".into(),
                    history.clone(),
                    events.clone(),
                )
                .await
                .unwrap();
                run(
                    AgentBuilder::new(model),
                    cwd,
                    "继续".into(),
                    history,
                    events,
                )
                .await
                .unwrap();
                done.send(()).unwrap();
                Ok(())
            })
        },
        agent_client_protocol::on_receive_request!(),
    );
    Client
        .v2()
        .on_receive_notification(
            async move |notification: UpdateSessionNotification, _cx| {
                updates_tx.send(notification.update).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |cx| {
            cx.send_request(InitializeRequest::new(
                ProtocolVersion::V2,
                Implementation::new("test", "0"),
            ))
            .block_task()
            .await?;
            done_rx.await.unwrap();
            let mut updates = Vec::new();
            while updates.len() < 5 {
                updates.push(updates_rx.recv().await.unwrap());
            }
            assert!(matches!(&updates[0], SessionUpdate::AgentThoughtChunk(_)));
            let SessionUpdate::ToolCallUpdate(start) = &updates[1] else {
                panic!("{updates:?}")
            };
            let SessionUpdate::ToolCallUpdate(end) = &updates[2] else {
                panic!("{updates:?}")
            };
            assert_eq!(start.tool_call_id, end.tool_call_id);
            assert_eq!(end.tool_call_id.to_string(), "call-one");
            assert!(serde_json::to_value(end).unwrap()["content"]
                .to_string()
                .contains("hello"));
            assert_eq!(serde_json::to_value(end).unwrap()["status"], "completed");
            assert!(matches!(updates[3], SessionUpdate::AgentMessage(_)));
            assert!(matches!(updates[4], SessionUpdate::AgentMessage(_)));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result")).unwrap(),
        "hello"
    );
    assert_eq!(recorded.request_count(), 3);
    assert!(has_tool_result(
        &recorded.requests()[1].chat_history,
        "hello"
    ));
    assert!(has_tool_result(
        &recorded.requests()[2].chat_history,
        "hello"
    ));
    assert!(has_tool_result(&saved.lock(), "hello"));
    assert_eq!(saved.lock().len(), 8, "历史不能重复添加旧消息");
}

#[tokio::test]
async fn shell_spawn_failure_is_a_tool_result_not_a_failed_run() {
    let model = MockCompletionModel::new([
        tool_turn("failed-call", "printf hello"),
        MockTurn::text("目录不可用"),
    ]);
    let captured = model.clone();
    let dir = tempfile::tempdir().unwrap();
    let agent = AgentBuilder::new(model)
        .tool(Shell {
            cwd: dir.path().join("missing"),
        })
        .default_max_turns(32)
        .build();
    let response = agent.runner("开始").run().await.unwrap();
    assert_eq!(response.output, "目录不可用");
    assert_eq!(captured.request_count(), 2);
    assert!(has_tool_result(&response.messages.unwrap(), "failed-call"));
}
