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
    message::{ToolCall, ToolFunction, UserContent},
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
    /// 把一个对话回合追加到内存中的模型上下文（工具结果不入盘）。
    fn record_context(&self, message: Message);
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
        // 整个回合原样进内存上下文（含思考）：思考模型要求 assistant 回合回放推理内容
        if !event.content.is_empty() {
            self.tools.record_context(Message::Assistant {
                id: event.message_id.map(str::to_string),
                content: event.content.clone(),
            });
        }
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

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: hook::ToolResultEvent<'_>,
    ) -> hook::ToolResultAction {
        self.tools.record_context(Message::User {
            content: vec![UserContent::tool_result(
                event.tool_call_id.unwrap_or(event.internal_call_id),
                event.tool_name,
                event.presentation.as_content().to_vec(),
            )],
        });
        hook::ToolResultAction::Keep
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
        "你是一个工作流智能体，你的职责是按照用户制定的工作流计划，调度一个或多个不同机器上的不同执行智能体，在用户和执行智能体之间传达消息，最终协调执行智能体来完成用户的任务。\n\n\
         调度是通过创建关联普通会话（用户与单个执行智能体的会话），然后通过工具往关联普通会话以用户角色注入消息，驱动执行智能体工作。\n\n\
         你会在以下情况被调用：收到用户新指令，或关联普通会话状态发生变化（执行智能体执行完毕或异常）。除此之外不存在需要主动行动的时刻。\n\n\
         行为规范：\n\
         1. 只做任务调度，不做任务拆解、任务执行、任务决策，不得替用户或执行智能体决定怎么做、做到什么程度\n\
         2. 如实传达信息：把用户指令和执行智能体输出完整如实传递，不增删、不改写、不代入自己的解读、不补建议、不总结你的主张\n\
         3. 只执行计划中明确写出的条件分支：不得创造计划之外的步骤，不得自主变更目标\n\
         4. 未在工作流计划与用户指令中指定的事项交由用户决定；需要人类判断时，输出结论并停下等待用户指令，不得擅自继续\n\
         5. 工作流计划描述的是会话如何创建、任务如何调度，不要把工作流计划内容传递给执行智能体\n\
         6. 创建或配置会话所需的机器、智能体、工作目录等参数，优先取自用户指令和工作流计划；两者都没有时向用户询问，不得编造\n\
         7. 用户也可直接向关联普通会话发送指令进行介入，此时关联会话状态发生变化会通知到你，发现用户已直接下发的消息时，不得重复或冲突地下发指令\n\n\
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
        fn record_context(&self, message: Message) {
            self.events.lock().push(format!(
                "context:{}",
                serde_json::to_string(&message).unwrap()
            ));
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
        // 内存上下文按回合累积：assistant 回合（含思考）、工具结果、最终输出
        let events = tools.events.lock().clone();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event.split(':').next().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["context", "thinking", "call", "dispatch", "context", "context"]
        );
        assert!(events[0].contains("call-1") && events[0].contains("先检查"));
        assert!(events[1].contains("先检查"));
        assert!(events[4].contains("session-result"));
        assert!(events[5].contains("按新指令暂停"));
    }
}
