//! 工作流智能体：rig-agent 非流式运行、工具注册和活动 hooks。

use amux_common::{api::OrchestratorConfig, model};
use parking_lot::Mutex;
use rig_agent::{
    agent::hook::{self, AgentHook, HookContext},
    completion::PromptError,
    tool::{DynamicTool, ToolOutput},
    AgentBuilder,
};
use rig_core::completion::{
    message::{ToolCall, ToolFunction},
    AssistantContent, Message, ToolDefinition,
};
use std::{future::Future, pin::Pin, sync::Arc};

const MAX_MODEL_CALLS: usize = 32;
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

pub trait Tools: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn dispatch<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> ToolFuture<'a>;
    fn record_thinking(&self, text: &str);
    fn record_tool_call(&self, call: &ToolCall);
    fn take_steers(&self) -> Vec<Message>;
}

pub async fn run(
    config: &OrchestratorConfig,
    preamble: &str,
    history: Vec<Message>,
    tools: Arc<dyn Tools>,
) -> Result<String, String> {
    run_agent(model::builder(config)?, preamble, history, tools).await
}

async fn run_agent(
    builder: AgentBuilder,
    preamble: &str,
    mut history: Vec<Message>,
    tools: Arc<dyn Tools>,
) -> Result<String, String> {
    let prompt = history.pop().ok_or("对话历史为空")?;
    let registered = tools
        .definitions()
        .into_iter()
        .map(|definition| {
            let tools = tools.clone();
            let name = definition.name.clone();
            DynamicTool::new(
                definition.name,
                definition.description,
                definition.parameters,
                move |_context, arguments| {
                    let tools = tools.clone();
                    let name = name.clone();
                    Box::pin(async move {
                        let result = tools
                            .dispatch(&name, arguments)
                            .await
                            .unwrap_or_else(|error| format!("工具执行失败：{error}"));
                        Ok(ToolOutput::text(result))
                    })
                },
            )
        })
        .collect();
    let agent = builder
        .preamble(preamble)
        .default_max_turns(MAX_MODEL_CALLS)
        .dynamic_tools(registered)
        .add_hook(WorkflowHook {
            tools,
            injected: Mutex::new(Vec::new()),
        })
        .build();
    match agent
        .runner(prompt)
        .history(history)
        .tool_concurrency(1)
        .run()
        .await
    {
        Ok(response) => Ok(response.output),
        Err(PromptError::MaxTurnsError { .. }) => Ok(format!(
            "（本轮工具调度已达 {MAX_MODEL_CALLS} 次上限，已暂停；可继续输入消息推进）"
        )),
        Err(error) => Err(format!("模型调用失败: {error}")),
    }
}

struct WorkflowHook {
    tools: Arc<dyn Tools>,
    injected: Mutex<Vec<Message>>,
}

impl AgentHook for WorkflowHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: hook::CompletionCall<'_>,
    ) -> hook::CompletionCallAction {
        let mut injected = self.injected.lock();
        injected.extend(self.tools.take_steers());
        if injected.is_empty() {
            return hook::CompletionCallAction::Continue;
        }
        // RequestPatch 只改变本次请求，不写入 rig transcript，后续请求需再次携带已注入消息。
        let mut history = event.history.to_vec();
        // 不把用户指令插在 assistant tool call 与当前 tool result 之间。
        let position = if matches!(history.last(), Some(Message::Assistant { content, .. }) if content.iter().any(|item| matches!(item, AssistantContent::ToolCall(_))))
        {
            history.len() - 1
        } else {
            history.len()
        };
        history.splice(position..position, injected.iter().cloned());
        hook::CompletionCallAction::patch(hook::RequestPatch::default().history(history))
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: hook::CompletionResponse<'_>,
    ) -> hook::ObservationAction {
        for content in event.content {
            if let AssistantContent::Reasoning(reasoning) = content {
                let text = model::reasoning_text(reasoning);
                if !text.trim().is_empty() {
                    self.tools.record_thinking(&text);
                }
            }
        }
        hook::ObservationAction::Continue
    }

    async fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: hook::ToolCall<'_>,
    ) -> hook::ToolCallAction {
        self.tools.record_tool_call(&ToolCall::from_wire(
            event.tool_call_id.unwrap_or(event.internal_call_id),
            ToolFunction::new(
                event.tool_name.into(),
                serde_json::from_str(event.args).unwrap_or_default(),
            ),
        ));
        hook::ToolCallAction::Run
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

pub fn preamble(plan: &str) -> String {
    format!(
        "你是 amux 的工作流智能体。按工作流计划与用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在计划与用户指令中指定的事项交由用户决定；需要人类判断时输出结论并等待用户指令。\n\
         只能操作本工作流的关联普通会话。\n\n\
         工作流计划：\n{plan}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::{
        completion::message::Reasoning,
        test_utils::{MockCompletionModel, MockTurn},
    };
    use tokio::sync::{oneshot, Notify};

    struct ControlledTools {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: Notify,
        steers: Mutex<Vec<Message>>,
        events: Mutex<Vec<String>>,
    }

    impl Tools for ControlledTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "list_sessions".into(),
                description: "读取关联会话".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }]
        }
        fn dispatch<'a>(&'a self, name: &'a str, _arguments: serde_json::Value) -> ToolFuture<'a> {
            Box::pin(async move {
                self.events.lock().push(format!("dispatch:{name}"));
                if let Some(tx) = self.entered.lock().take() {
                    tx.send(()).unwrap();
                }
                self.release.notified().await;
                Ok("session-result".into())
            })
        }
        fn record_thinking(&self, text: &str) {
            self.events.lock().push(format!("thinking:{text}"));
        }
        fn record_tool_call(&self, call: &ToolCall) {
            self.events.lock().push(format!("call:{}", call.id));
        }
        fn take_steers(&self) -> Vec<Message> {
            std::mem::take(&mut *self.steers.lock())
        }
    }

    #[tokio::test]
    async fn steer_arriving_during_tool_execution_reaches_next_model_request() {
        let model = MockCompletionModel::new([
            MockTurn::from_contents([
                AssistantContent::Reasoning(Reasoning::new("先检查")),
                AssistantContent::ToolCall(ToolCall::from_wire(
                    "call-1",
                    ToolFunction::new("list_sessions".into(), serde_json::json!({})),
                )),
            ]),
            MockTurn::text("按新指令暂停"),
        ]);
        let recorded = model.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let tools = Arc::new(ControlledTools {
            entered: Mutex::new(Some(entered_tx)),
            release: Notify::new(),
            steers: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        });
        let runner = tokio::spawn({
            let tools = tools.clone();
            async move {
                run_agent(
                    AgentBuilder::new(model),
                    &preamble("按计划运行"),
                    vec![Message::user("开始")],
                    tools,
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        tools
            .steers
            .lock()
            .push(Message::user("现在暂停，不再启动新会话"));
        tools.release.notify_one();
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(output, "按新指令暂停");
        let requests = recorded.requests();
        assert_eq!(requests.len(), 2);
        let first = serde_json::to_string(&requests[0].chat_history).unwrap();
        let second = serde_json::to_string(&requests[1].chat_history).unwrap();
        assert!(!first.contains("现在暂停"));
        assert_eq!(second.matches("现在暂停，不再启动新会话").count(), 1);
        assert!(
            second.contains("session-result"),
            "工具结果未回填: {second}"
        );
        assert!(tools.steers.lock().is_empty());
        assert_eq!(
            *tools.events.lock(),
            ["thinking:先检查", "call:call-1", "dispatch:list_sessions"]
        );
    }
}
