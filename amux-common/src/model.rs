//! 内置智能体共享的非流式模型工具循环。

use std::future::Future;
use std::pin::Pin;

use crate::api::{ApiFormat, OrchestratorConfig};
use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    Reasoning, ReasoningContent, ToolCall, ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, CompletionModel, Message, ToolDefinition};

/// 单轮工作流推进最多允许的工具轮次。
const MAX_TOOL_TURNS: usize = 32;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// 工作流智能体执行面：工具清单、工具执行与活动记录。
pub trait Tools: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn dispatch<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> ToolFuture<'a>;
    /// 记录模型响应中的可读推理文本。
    fn record_thinking(&self, text: &str);
    /// 记录一次工具调用，在 [`Tools::dispatch`] 之前调用。
    fn record_tool_call(&self, call: &ToolCall);
    fn record_text(&self, _text: &str) {}
}

/// 运行一轮编排：模型循环调用工具直到输出纯文本。
pub async fn run(
    config: &OrchestratorConfig,
    preamble: &str,
    history: &mut Vec<Message>,
    tools: &dyn Tools,
) -> Result<String, String> {
    let additional_params = effort_params(config);
    match config.api_format {
        ApiFormat::ChatCompletions => {
            let client = openai(config)?;
            tool_loop(
                client
                    .completions_api()
                    .completion_model(config.model.clone()),
                preamble,
                history,
                tools,
                additional_params,
            )
            .await
        }
        ApiFormat::Responses => {
            let client = openai(config)?;
            tool_loop(
                client.completion_model(config.model.clone()),
                preamble,
                history,
                tools,
                additional_params,
            )
            .await
        }
        ApiFormat::Messages => {
            let client = rig_core::providers::anthropic::Client::builder()
                .api_key(config.api_key.clone())
                .base_url(config.base_url.clone())
                .build()
                .map_err(|error| format!("构建 Anthropic client 失败: {error}"))?;
            tool_loop(
                client.completion_model(config.model.clone()),
                preamble,
                history,
                tools,
                additional_params,
            )
            .await
        }
    }
}

/// 推理级别映射到各 API 线缆上的字段：chat_completions 为 `reasoning_effort`，
/// responses 为 `reasoning.effort`，messages 为 `output_config.effort`；空串则不携带。
fn effort_params(config: &OrchestratorConfig) -> Option<serde_json::Value> {
    let effort = config.effort.trim();
    if effort.is_empty() {
        return None;
    }
    Some(match config.api_format {
        ApiFormat::ChatCompletions => serde_json::json!({ "reasoning_effort": effort }),
        ApiFormat::Responses => serde_json::json!({ "reasoning": { "effort": effort } }),
        ApiFormat::Messages => serde_json::json!({ "output_config": { "effort": effort } }),
    })
}

fn openai(config: &OrchestratorConfig) -> Result<rig_core::providers::openai::Client, String> {
    rig_core::providers::openai::Client::builder()
        .api_key(config.api_key.clone())
        .base_url(config.base_url.clone())
        .build()
        .map_err(|error| format!("构建 OpenAI client 失败: {error}"))
}

async fn tool_loop<M>(
    model: M,
    preamble: &str,
    history: &mut Vec<Message>,
    tools: &dyn Tools,
    additional_params: Option<serde_json::Value>,
) -> Result<String, String>
where
    M: CompletionModel + Clone + 'static,
{
    let definitions = tools.definitions();
    for _ in 0..MAX_TOOL_TURNS {
        let prompt = history.pop().ok_or("对话历史为空")?;
        let request = model
            .completion_request(prompt.clone())
            .preamble(preamble.to_string())
            .messages(history.iter().cloned())
            .tools(definitions.clone())
            .additional_params_opt(additional_params.clone())
            .build();
        let response = model
            .completion(request)
            .await
            .map_err(|error| format!("模型调用失败: {error}"))?;
        history.push(prompt);

        let mut text = String::new();
        let mut calls = Vec::new();
        for item in &response.choice {
            match item {
                AssistantContent::Text(content) => text.push_str(&content.text),
                AssistantContent::Reasoning(reasoning) => {
                    let thinking = reasoning_text(reasoning);
                    if !thinking.trim().is_empty() {
                        tools.record_thinking(&thinking);
                    }
                }
                AssistantContent::ToolCall(call) => calls.push(call.clone()),
                _ => {}
            }
        }
        if !text.is_empty() {
            tools.record_text(&text);
        }
        history.push(Message::Assistant {
            id: response.message_id.clone(),
            content: response.choice.clone(),
        });
        if calls.is_empty() {
            return Ok(text);
        }

        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            tools.record_tool_call(&call);
            let outcome = tools
                .dispatch(&call.function.name, call.function.arguments.clone())
                .await
                .unwrap_or_else(|error| format!("工具执行失败：{error}"));
            results.push(UserContent::tool_result(
                call.id.clone(),
                &call.function.name,
                vec![ToolResultContent::text(outcome)],
            ));
        }
        history.push(Message::User { content: results });
    }
    Ok(format!(
        "（本轮工具调度已达 {MAX_TOOL_TURNS} 次上限，已暂停；可继续输入消息推进）"
    ))
}

/// reasoning 块中的可读文本；加密与脱敏载荷不是可读思考，不落盘。
fn reasoning_text(reasoning: &Reasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|item| match item {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            ReasoningContent::Summary(text) => Some(text.as_str()),
            ReasoningContent::Encrypted(_) | ReasoningContent::Redacted { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rig_core::completion::message::ToolFunction;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    use super::*;

    /// 记录调用顺序的假执行面。
    #[derive(Default)]
    struct RecordingTools {
        seen: Mutex<Vec<String>>,
    }

    impl Tools for RecordingTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }

        fn dispatch<'a>(&'a self, name: &'a str, _arguments: serde_json::Value) -> ToolFuture<'a> {
            Box::pin(async move { Ok(format!("{name} 结果")) })
        }

        fn record_thinking(&self, text: &str) {
            self.seen.lock().unwrap().push(format!("thinking:{text}"));
        }

        fn record_tool_call(&self, call: &ToolCall) {
            self.seen
                .lock()
                .unwrap()
                .push(format!("tool_call:{}", call.function.name));
        }
    }

    #[tokio::test]
    async fn tool_loop_records_thinking_and_tool_call_before_dispatch() {
        let model = MockCompletionModel::new([
            MockTurn::from_contents([
                AssistantContent::Reasoning(Reasoning::new("先看关联会话")),
                AssistantContent::ToolCall(ToolCall::from_wire(
                    "call_001",
                    ToolFunction::new("list_sessions".to_string(), serde_json::json!({})),
                )),
            ]),
            MockTurn::text("完成"),
        ]);
        let tools = RecordingTools::default();
        let text = tool_loop(
            model,
            "系统提示词",
            &mut vec![Message::user("开始")],
            &tools,
            None,
        )
        .await
        .unwrap();
        assert_eq!(text, "完成");
        assert_eq!(
            tools.seen.lock().unwrap().as_slice(),
            ["thinking:先看关联会话", "tool_call:list_sessions"]
        );
    }

    #[test]
    fn effort_maps_to_each_api_format() {
        let config = |api_format, effort: &str| OrchestratorConfig {
            api_format,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            effort: effort.to_string(),
        };
        assert_eq!(
            effort_params(&config(ApiFormat::ChatCompletions, "high")),
            Some(serde_json::json!({ "reasoning_effort": "high" }))
        );
        assert_eq!(
            effort_params(&config(ApiFormat::Responses, "high")),
            Some(serde_json::json!({ "reasoning": { "effort": "high" } }))
        );
        assert_eq!(
            effort_params(&config(ApiFormat::Messages, "high")),
            Some(serde_json::json!({ "output_config": { "effort": "high" } }))
        );
        assert_eq!(effort_params(&config(ApiFormat::Responses, "")), None);
        assert_eq!(effort_params(&config(ApiFormat::Responses, "  ")), None);
    }
}
