//! Nano 的模型运行与 ACP 活动适配；工具循环由 rig-agent 驱动。

use std::{io, path::PathBuf, sync::Arc};

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

#[cfg(test)]
mod tests;

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
}

impl Tool for Shell {
    const NAME: &'static str = "shell";
    type Args = ShellArgs;
    type Output = ToolOutput;
    type Error = io::Error;

    fn description(&self) -> String {
        "在会话工作目录执行 shell 命令".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: ShellArgs,
    ) -> Result<ToolOutput, io::Error> {
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(args.command)
            .current_dir(&self.cwd)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(ToolOutput::text(format!(
            "exit: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}
