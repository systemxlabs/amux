//! Nano 的模型运行与 ACP 活动适配；工具循环由 rig-agent 驱动。

use std::{io, path::PathBuf, sync::Arc, time::Duration};

use agent_client_protocol::{schema::v2::*, Client, V2ConnectionTo};
pub(super) use amux_common::model::builder;
use amux_common::model::reasoning_text;
use parking_lot::Mutex;
use rig_agent::{
    agent::hook::{self, AgentHook, HookContext},
    completion::PromptError,
    tool::{Tool, ToolContext, ToolOutput},
    AgentBuilder,
};
use rig_core::completion::{AssistantContent, Message};
use serde::Deserialize;

const MAX_MODEL_CALLS: usize = 32;

#[derive(Clone)]
pub(super) struct Events {
    pub cx: V2ConnectionTo<Client>,
    pub id: SessionId,
}

impl Events {
    pub fn update(&self, update: SessionUpdate) {
        if let Err(error) = self
            .cx
            .send_notification(UpdateSessionNotification::new(self.id.clone(), update))
        {
            log::warn!("Nano 通知发送失败: {error}");
        }
    }

    pub fn text(&self, text: &str) {
        self.update(SessionUpdate::AgentMessage(
            AgentMessage::new(uuid::Uuid::new_v4().to_string())
                .content(vec![text.to_string().into()]),
        ));
    }
}

pub(super) async fn run(
    builder: AgentBuilder,
    cwd: PathBuf,
    text: String,
    history: Arc<Mutex<Vec<Message>>>,
    events: Events,
) -> Result<(), String> {
    let previous = history.lock().clone();
    history.lock().push(Message::user(text.clone()));
    let agent = builder
        .preamble("你是 Nano，使用 shell 工具在指定工作目录中完成用户任务。")
        .default_max_turns(MAX_MODEL_CALLS)
        .tool(Shell { cwd })
        .add_hook(AcpHook {
            events,
            history: history.clone(),
        })
        .build();
    // 显式串行，避免同一批 shell 命令在工作目录中相互竞争。
    match agent
        .runner(text)
        .history(previous.clone())
        .tool_concurrency(1)
        .run()
        .await
    {
        Ok(response) => {
            if let Some(messages) = response.messages {
                // rig 返回本次 run 的增量消息，不包含传入的旧历史。
                *history.lock() = previous.into_iter().chain(messages).collect();
            }
            Ok(())
        }
        Err(PromptError::MaxTurnsError { chat_history, .. }) => {
            *history.lock() = *chat_history;
            // 保留原 Nano 达到预算后结束本轮的行为，而不是报告模型错误。
            Ok(())
        }
        Err(error) => Err(format!("模型调用失败: {error}")),
    }
}

struct AcpHook {
    events: Events,
    history: Arc<Mutex<Vec<Message>>>,
}

impl AgentHook for AcpHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: hook::CompletionCall<'_>,
    ) -> hook::CompletionCallAction {
        // 只保存模型调用边界上的完整历史。取消工具批次时不留下缺少 result 的 tool call。
        *self.history.lock() = event
            .history
            .iter()
            .cloned()
            .chain([event.prompt.clone()])
            .collect();
        hook::CompletionCallAction::Continue
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: hook::CompletionResponse<'_>,
    ) -> hook::ObservationAction {
        let mut text = String::new();
        for content in event.content {
            match content {
                AssistantContent::Text(content) => text.push_str(&content.text),
                AssistantContent::Reasoning(reasoning) => {
                    let thinking = reasoning_text(reasoning);
                    if !thinking.trim().is_empty() {
                        self.events
                            .update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                                thinking.into(),
                                MessageId::new(uuid::Uuid::new_v4().to_string()),
                            )));
                    }
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            self.events.text(&text);
        }
        hook::ObservationAction::Continue
    }

    async fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: hook::ToolCall<'_>,
    ) -> hook::ToolCallAction {
        self.events.update(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(event.tool_call_id.unwrap_or(event.internal_call_id))
                .title("shell")
                .kind(ToolKind::Execute)
                .status(ToolCallStatus::InProgress)
                .raw_input(
                    serde_json::from_str::<serde_json::Value>(event.args).unwrap_or_default(),
                ),
        ));
        hook::ToolCallAction::Run
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: hook::ToolResultEvent<'_>,
    ) -> hook::ToolResultAction {
        self.events.update(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(event.tool_call_id.unwrap_or(event.internal_call_id))
                .title("shell")
                .kind(ToolKind::Execute)
                .status(if event.raw_result.is_error() {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .content(vec![ToolCallContent::Content(Box::new(Content::new(
                    ContentBlock::Text(TextContent::new(event.presentation.render())),
                )))]),
        ));
        hook::ToolResultAction::Keep
    }

    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &hook::InvalidToolCallContext,
    ) -> Option<hook::InvalidToolCallAction> {
        Some(hook::InvalidToolCallAction::Skip {
            reason: format!("工具执行失败：未知工具: {}", event.tool_name),
        })
    }
}

struct Shell {
    cwd: PathBuf,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    /// 超时秒数（可选，见 `shell::MAX_TIMEOUT_SECONDS`）。
    timeout: Option<u64>,
}

impl Tool for Shell {
    const NAME: &'static str = "shell";
    type Args = ShellArgs;
    type Output = ToolOutput;
    type Error = io::Error;

    fn description(&self) -> String {
        "在会话工作目录通过 sh -c 执行命令。返回退出状态、stdout 和 stderr；\
         每个输出流最多保留末尾 2000 行或 50 KiB，超出部分丢弃，完整输出写入临时文件并在结果里给出路径。\
         可选 timeout（秒），超时会终止整个进程组。"
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的 shell 命令" },
                "timeout": {
                    "type": "integer",
                    "description": format!("超时秒数（1-{}），缺省不超时", super::shell::MAX_TIMEOUT_SECONDS)
                }
            },
            "required": ["command"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: ShellArgs,
    ) -> Result<ToolOutput, io::Error> {
        let timeout = match args.timeout {
            None => None,
            Some(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "timeout 必须大于 0 秒",
                ))
            }
            Some(seconds) if seconds > super::shell::MAX_TIMEOUT_SECONDS => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("timeout 最大为 {} 秒", super::shell::MAX_TIMEOUT_SECONDS),
                ))
            }
            Some(seconds) => Some(Duration::from_secs(seconds)),
        };
        Ok(ToolOutput::text(
            super::shell::execute(&self.cwd, &args.command, timeout).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
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

    #[cfg(unix)]
    #[test]
    fn shell_child_fixture() {
        use std::io::{Read, Write};
        let Ok(address) = std::env::var("AMUX_NANO_TEST_SOCKET") else {
            return;
        };
        let mut socket = std::os::unix::net::UnixStream::connect(address).unwrap();
        socket.write_all(b"ready").unwrap();
        let _ = socket.read(&mut [0]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_shell_runner_kills_descendants() {
        use std::time::Duration;
        use tokio::{io::AsyncReadExt, net::UnixListener};

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("ready.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let executable = std::env::current_exe().unwrap();
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        // 子进程持有 socket；父 shell 必须继续 wait，不能被 exec 优化替换。
        let command = format!(
            "AMUX_NANO_TEST_SOCKET={} {} --exact nano::runtime::tests::shell_child_fixture --nocapture & wait",
            quote(&socket_path), quote(&executable),
        );
        let agent = AgentBuilder::new(MockCompletionModel::new([tool_turn("child", &command)]))
            .tool(Shell {
                cwd: dir.path().into(),
            })
            .build();
        let task = tokio::spawn(async move { agent.runner("开始").run().await });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut ready = [0; 5];
        socket.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // EOF 证明子进程关闭了描述符，不依赖 sleep 或 PID 回收时机。
        // 失败时 socket 也会关闭，fixture 因 EOF 退出，不遗留测试进程。
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), socket.read(&mut [0]))
                .await
                .expect("取消后 shell 子进程仍在运行")
                .unwrap(),
            0,
        );
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
}
