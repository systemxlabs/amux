//! 编排智能体：rig 工具循环。
//!
//! 按 docs/DESIGN.md「编排智能体」实现：系统提示词（角色/工作方式/行为约束 + 工作流计划）
//! 与工具清单，模型通过工具调度关联普通会话。流式与合并在本实现里简化为一次性
//! completion（拿到完整条目后落盘），steer 由每轮请求前注入用户消息实现。

use std::future::Future;
use std::pin::Pin;

use amux_common::api::{ApiFormat, OrchestratorConfig};
use rig_core::client::CompletionClient;
use rig_core::completion::message::{ToolResultContent, UserContent};
use rig_core::completion::{AssistantContent, CompletionModel, Message, ToolDefinition};

/// 单轮工作流推进最多允许的工具轮次。
const MAX_TOOL_TURNS: usize = 32;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// 工具执行面：编排智能体可用的调度动作。
pub trait Tools: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn dispatch<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> ToolFuture<'a>;
}

/// 运行一轮编排：模型循环调用工具直到输出纯文本。
pub async fn run(
    config: &OrchestratorConfig,
    preamble: &str,
    history: Vec<Message>,
    tools: &dyn Tools,
) -> Result<String, String> {
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
            )
            .await
        }
    }
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
    mut history: Vec<Message>,
    tools: &dyn Tools,
) -> Result<String, String>
where
    M: CompletionModel + Clone + 'static,
{
    let definitions = tools.definitions();
    for _ in 0..MAX_TOOL_TURNS {
        let prompt = history.pop().ok_or("编排对话历史为空")?;
        let request = model
            .completion_request(prompt.clone())
            .preamble(preamble.to_string())
            .messages(history.iter().cloned())
            .tools(definitions.clone())
            .build();
        let response = model
            .completion(request)
            .await
            .map_err(|error| format!("编排智能体调用失败: {error}"))?;
        history.push(prompt);

        let mut text = String::new();
        let mut calls = Vec::new();
        for item in &response.choice {
            match item {
                AssistantContent::Text(content) => text.push_str(&content.text),
                AssistantContent::ToolCall(call) => calls.push(call.clone()),
                _ => {}
            }
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

/// 系统提示词：角色、工作方式、行为约束 + 工作流计划。
pub fn preamble(plan: &str) -> String {
    format!(
        "你是 amux 的编排智能体。按工作流计划与用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在计划与用户指令中指定的事项交由用户决定；需要人类判断时输出结论并等待用户指令。\n\
         只能操作本工作流的关联普通会话。\n\n\
         工作流计划：\n{plan}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_carries_plan_and_constraints() {
        let text = preamble("使用远程 codex 实现功能");
        assert!(text.contains("使用远程 codex 实现功能"));
        assert!(text.contains("编排智能体"));
        assert!(text.contains("关联普通会话"));
    }
}
