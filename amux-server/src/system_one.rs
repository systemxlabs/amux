//! 自动化结果分类使用的 System One 请求与响应。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use amux_common::api::{AutomationConfig, AutomationResultLabel};
use serde::{Deserialize, Serialize};
use serde_json::Value;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Serialize)]
struct EvaluationRequest<'a> {
    model: &'a str,
    state: Value,
    questions: BTreeMap<&'static str, Question>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Question {
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
}

#[derive(Deserialize)]
struct EvaluationResponse {
    answers: BTreeMap<String, Answer>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Answer {
    Choice { choice: String },
}

pub async fn evaluate_choice(
    config: &AutomationConfig,
    labels: &[AutomationResultLabel],
    context: &str,
) -> Result<String, String> {
    let request = evaluation_request(config, labels, context);

    let response = CLIENT
        .get_or_init(reqwest::Client::new)
        .post(&config.endpoint)
        .bearer_auth(&config.api_key)
        .json(&request)
        .send()
        .await
        .map_err(|error| format!("决策模型请求失败: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("读取决策模型响应失败: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "决策模型请求失败: HTTP {status}: {}",
            body.chars().take(1_000).collect::<String>()
        ));
    }

    let response: EvaluationResponse =
        serde_json::from_str(&body).map_err(|error| format!("决策模型响应解析失败: {error}"))?;
    let Some(Answer::Choice { choice }) = response.answers.get("result") else {
        return Err("决策模型没有返回选择结果".into());
    };
    if !labels.iter().any(|label| &label.name == choice) {
        return Err(format!("决策模型返回未知分类: {choice}"));
    }
    Ok(choice.clone())
}

fn evaluation_request<'a>(
    config: &'a AutomationConfig,
    labels: &[AutomationResultLabel],
    context: &str,
) -> EvaluationRequest<'a> {
    let criteria = labels
        .iter()
        .map(|label| (label.name.clone(), Some(label.description.clone())))
        .collect();
    let questions = BTreeMap::from([(
        "result",
        Question::Choice {
            instructions: "请告诉我这个任务的结果是什么？".into(),
            criteria,
        },
    )]);
    EvaluationRequest {
        model: &config.model,
        state: serde_json::json!({ "context": context }),
        questions,
    }
}

#[cfg(test)]
mod tests {
    use amux_common::api::{AutomationApiFormat, AutomationConfig, AutomationResultLabel};

    use super::{evaluation_request, EvaluationResponse};

    #[test]
    fn response_ignores_numeric_usage_cost() {
        let body = r#"{
                "model": "jev-latest",
                "answers": {
                    "result": {
                        "type": "choice",
                        "choice": "通过",
                        "probabilities": { "通过": 1.0, "失败": 0.0 },
                        "confidence": 1.0
                    }
                },
                "usage": {
                    "input_tokens": 296,
                    "output_tokens": 20,
                    "cost": 0.042
                }
            }"#;
        let response: EvaluationResponse = serde_json::from_str(body).unwrap();
        let super::Answer::Choice { choice } = response.answers.get("result").unwrap();
        assert_eq!(choice, "通过");
    }

    #[test]
    fn request_uses_documented_question_and_label_descriptions() {
        let config = AutomationConfig {
            api_format: AutomationApiFormat::SystemOne,
            endpoint: "https://example.test".into(),
            api_key: "token".into(),
            model: "model".into(),
        };
        let request = evaluation_request(
            &config,
            &[AutomationResultLabel {
                name: "通过".into(),
                description: "任务完成".into(),
            }],
            "上下文",
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["state"]["context"], "上下文");
        assert_eq!(
            json["questions"]["result"]["instructions"],
            "请告诉我这个任务的结果是什么？"
        );
        assert_eq!(json["questions"]["result"]["criteria"]["通过"], "任务完成");
    }
}
