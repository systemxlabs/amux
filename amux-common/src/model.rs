//! 内置智能体共享的模型构建与推理文本提取，不负责工具循环。

use crate::api::{ApiFormat, OrchestratorConfig};
use rig_agent::AgentBuilder;
use rig_core::{
    client::CompletionClient,
    completion::message::{Reasoning, ReasoningContent},
};

pub fn builder(config: &OrchestratorConfig) -> Result<AgentBuilder, String> {
    let builder = match config.api_format {
        ApiFormat::ChatCompletions | ApiFormat::Responses => {
            let client = rig_core::providers::openai::Client::builder()
                .api_key(config.api_key.clone())
                .base_url(config.base_url.clone())
                .build()
                .map_err(|error| format!("构建 OpenAI client 失败: {error}"))?;
            if config.api_format == ApiFormat::ChatCompletions {
                AgentBuilder::new(
                    client
                        .completions_api()
                        .completion_model(config.model.clone()),
                )
            } else {
                AgentBuilder::new(client.completion_model(config.model.clone()))
            }
        }
        ApiFormat::Messages => {
            let client = rig_core::providers::anthropic::Client::builder()
                .api_key(config.api_key.clone())
                .base_url(config.base_url.clone())
                .build()
                .map_err(|error| format!("构建 Anthropic client 失败: {error}"))?;
            AgentBuilder::new(client.completion_model(config.model.clone()))
        }
    };
    Ok(match effort_params(config) {
        Some(params) => builder.additional_params(params),
        None => builder,
    })
}

fn effort_params(config: &OrchestratorConfig) -> Option<serde_json::Value> {
    let effort = config.effort.trim();
    if effort.is_empty() {
        return None;
    }
    Some(match config.api_format {
        ApiFormat::ChatCompletions => serde_json::json!({"reasoning_effort": effort}),
        ApiFormat::Responses => serde_json::json!({"reasoning": {"effort": effort}}),
        ApiFormat::Messages => serde_json::json!({"output_config": {"effort": effort}}),
    })
}

/// 加密或脱敏载荷不是可读思考，不进入活动记录。
pub fn reasoning_text(reasoning: &Reasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|content| match content {
            ReasoningContent::Text { text, .. } | ReasoningContent::Summary(text) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_maps_to_each_api_format() {
        let config = |api_format, effort: &str| OrchestratorConfig {
            api_format,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            effort: effort.into(),
        };
        assert_eq!(
            effort_params(&config(ApiFormat::ChatCompletions, "high")),
            Some(serde_json::json!({"reasoning_effort":"high"}))
        );
        assert_eq!(
            effort_params(&config(ApiFormat::Responses, "high")),
            Some(serde_json::json!({"reasoning":{"effort":"high"}}))
        );
        assert_eq!(
            effort_params(&config(ApiFormat::Messages, "high")),
            Some(serde_json::json!({"output_config":{"effort":"high"}}))
        );
        assert_eq!(effort_params(&config(ApiFormat::Responses, "")), None);
        assert_eq!(effort_params(&config(ApiFormat::Responses, "  ")), None);
    }
}
