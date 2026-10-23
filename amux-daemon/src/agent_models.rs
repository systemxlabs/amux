//! Agent 本地配置中的模型候选。
//!
//! 这里读取的是新建群聊成员时的候选列表，不代表 ACP 会话当前实际可用的模型。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use amux_common::api::AgentModel;
use serde::Deserialize;

#[derive(Deserialize)]
struct CodexConfig {
    model: Option<String>,
    model_catalog_json: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CodexCatalog {
    #[serde(default)]
    models: Vec<CodexCatalogModel>,
}

#[derive(Deserialize)]
struct CodexCatalogModel {
    slug: String,
    display_name: Option<String>,
    visibility: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSettings {
    model: Option<String>,
    model_picker: Option<ClaudeModelPicker>,
    env: Option<ClaudeEnv>,
}

#[derive(Deserialize)]
struct ClaudeModelPicker {
    #[serde(default)]
    options: Vec<ClaudePickerOption>,
}

#[derive(Deserialize)]
struct ClaudePickerOption {
    model: String,
    label: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeEnv {
    #[serde(rename = "ANTHROPIC_DEFAULT_HAIKU_MODEL")]
    haiku_model: Option<String>,
    #[serde(rename = "ANTHROPIC_DEFAULT_OPUS_MODEL")]
    opus_model: Option<String>,
    #[serde(rename = "ANTHROPIC_DEFAULT_SONNET_MODEL")]
    sonnet_model: Option<String>,
    #[serde(rename = "ANTHROPIC_SMALL_FAST_MODEL")]
    small_fast_model: Option<String>,
}

pub fn agent_models(agent: &str, nano_model: Option<&str>) -> Result<Vec<AgentModel>, String> {
    match agent {
        amux_common::api::NANO_AGENT => {
            let model = nano_model.ok_or("Nano 尚未完成认证")?;
            Ok(vec![agent_model(model, model)])
        }
        "codex" => {
            let path = config_path("CODEX_HOME", ".codex", "config.toml");
            codex_models(&path)
        }
        "claude" => {
            let path = config_path("CLAUDE_CONFIG_DIR", ".claude", "settings.json");
            claude_models(&path)
        }
        _ => Err(format!("未知 agent: {agent}")),
    }
}

fn config_path(env: &str, home_dir: &str, file: &str) -> PathBuf {
    std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(home_dir)
        })
        .join(file)
}

fn codex_models(config_path: &Path) -> Result<Vec<AgentModel>, String> {
    let Some(text) = read_optional(config_path)? else {
        return Ok(Vec::new());
    };
    let config: CodexConfig = toml::from_str(&text)
        .map_err(|error| format!("解析 {} 失败: {error}", config_path.display()))?;
    let mut models = Vec::new();
    if let Some(catalog_path) = config.model_catalog_json {
        let catalog_path = if catalog_path.is_absolute() {
            catalog_path
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(catalog_path)
        };
        if let Some(text) = read_optional(&catalog_path)? {
            let catalog: CodexCatalog = serde_json::from_str(&text)
                .map_err(|error| format!("解析 {} 失败: {error}", catalog_path.display()))?;
            models.extend(
                catalog
                    .models
                    .into_iter()
                    .filter(|model| {
                        model
                            .visibility
                            .as_deref()
                            .is_none_or(|value| value == "list")
                    })
                    .map(|model| {
                        agent_model(
                            &model.slug,
                            model.display_name.as_deref().unwrap_or(&model.slug),
                        )
                    }),
            );
        }
    }
    if let Some(current) = config.model.filter(|model| !model.trim().is_empty()) {
        if !models.iter().any(|model| model.value == current) {
            models.insert(0, agent_model(&current, &current));
        }
    }
    Ok(dedup(models))
}

fn claude_models(settings_path: &Path) -> Result<Vec<AgentModel>, String> {
    let Some(text) = read_optional(settings_path)? else {
        return Ok(Vec::new());
    };
    let settings: ClaudeSettings = serde_json::from_str(&text)
        .map_err(|error| format!("解析 {} 失败: {error}", settings_path.display()))?;
    let mut models = settings
        .model_picker
        .map(|picker| {
            picker
                .options
                .into_iter()
                .filter(|option| !option.model.trim().is_empty())
                .map(|option| {
                    agent_model(
                        &option.model,
                        option.label.as_deref().unwrap_or(&option.model),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(current) = settings.model.filter(|model| !model.trim().is_empty()) {
        if !models.iter().any(|model| model.value == current) {
            models.insert(0, agent_model(&current, &current));
        }
    }
    if let Some(env) = settings.env {
        for value in [
            env.haiku_model,
            env.opus_model,
            env.sonnet_model,
            env.small_fast_model,
        ]
        .into_iter()
        .flatten()
        {
            if !value.trim().is_empty() && !models.iter().any(|model| model.value == value) {
                models.push(agent_model(&value, &value));
            }
        }
    }
    Ok(dedup(models))
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 {} 失败: {error}", path.display())),
    }
}

fn agent_model(value: &str, name: &str) -> AgentModel {
    AgentModel {
        value: value.to_string(),
        name: name.to_string(),
    }
}

fn dedup(models: Vec<AgentModel>) -> Vec<AgentModel> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert(model.value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_models_use_catalog_and_keep_current_model() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("models.json");
        std::fs::write(
            &catalog,
            r#"{"models":[
                {"slug":"gpt-5","display_name":"GPT-5","visibility":"list"},
                {"slug":"hidden","display_name":"Hidden","visibility":"hide"}
            ]}"#,
        )
        .unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "model = \"custom\"\nmodel_catalog_json = \"{}\"\n",
                catalog.display()
            ),
        )
        .unwrap();

        assert_eq!(
            codex_models(&config).unwrap(),
            vec![
                agent_model("custom", "custom"),
                agent_model("gpt-5", "GPT-5")
            ]
        );
    }

    #[test]
    fn claude_models_read_picker_and_default_models() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
                "model":"sonnet",
                "modelPicker":{"options":[
                    {"model":"sonnet","label":"Sonnet"},
                    {"model":"opus","label":"Opus"}
                ]},
                "env":{"ANTHROPIC_DEFAULT_HAIKU_MODEL":"haiku"}
            }"#,
        )
        .unwrap();

        assert_eq!(
            claude_models(&settings).unwrap(),
            vec![
                agent_model("sonnet", "Sonnet"),
                agent_model("opus", "Opus"),
                agent_model("haiku", "haiku")
            ]
        );
    }

    #[test]
    fn missing_config_has_no_models() {
        assert!(codex_models(Path::new("/definitely/missing/config.toml"))
            .unwrap()
            .is_empty());
    }
}
