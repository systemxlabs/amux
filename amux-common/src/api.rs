//! Client API（HTTPS）的请求与响应类型。
//!
//! 端点路径与 DESIGN「Client-Server 通信」一节一一对应；Server 与桌面应用共用这些类型。

use serde::{Deserialize, Serialize};

use crate::domain::{
    Activity, ContentBlock, GitDiffResult, HistoryItem, SessionConfigOption,
    SessionConfigOptionValue, SessionPlanEntry, SessionState, SlashCommand,
};

/// Daemon 内置智能体的发现名称。
pub const NANO_AGENT: &str = "nano";

/// 端点路径片段（应用侧请求共用，避免字面量漂移）。
pub mod path {
    pub const MACHINES: &str = "/machines";
    pub const SESSIONS: &str = "/sessions";
    pub const WORKFLOWS: &str = "/workflows";
    pub const CONFIG_SKILLS: &str = "/config/skills/";
    pub const CONFIG_WORKFLOWS: &str = "/config/workflows/";
    pub const CONFIG_RECENT_WORKSPACES: &str = "/config/recent_workspaces/";
    pub const CONFIG_QUICK_COMMANDS: &str = "/config/quick_commands/";
    pub const CONFIG_AGENT: &str = "/config/agent/";
    pub const CONFIG_PROJECTS: &str = "/config/projects/";
}

/// 一台已连接的机器（`machine.info` 透传）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    /// 该机器的系统临时目录（技能操作以它作为会话工作目录）
    pub temp_dir: String,
    pub version: String,
}

/// 某机器上的一个 agent：名称与可用性（可用 = agent 已启动且 ACP 已初始化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub name: String,
    pub available: bool,
}

/// 普通会话元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    /// 所属机器
    pub machine: String,
    /// 所属 agent
    pub agent: String,
    /// 会话标题：默认取首条指令截断，用户可修改
    pub title: String,
    pub state: SessionState,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 用户指定的工作目录
    pub workspace: String,
    /// worktree 目录；空串 = 未启用 worktree
    #[serde(default)]
    pub worktree_dir: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Session {
    /// 会话实际使用的根目录：启用 worktree 时为 worktree 目录，否则为工作目录。
    pub fn root_dir(&self) -> &str {
        if self.worktree_dir.is_empty() {
            &self.workspace
        } else {
            &self.worktree_dir
        }
    }
}

/// `GET /sessions` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionList {
    pub sessions: Vec<Session>,
    pub has_more: bool,
}

/// `POST /sessions` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub machine: String,
    pub agent: String,
    pub workspace: String,
    #[serde(default)]
    pub use_worktree: bool,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `POST /sessions/<id>` 请求：发送指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub input: Vec<ContentBlock>,
}

/// `POST /sessions/<id>/configure` 中的会话选项设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSetting {
    pub config_id: String,
    #[serde(flatten)]
    pub value: SessionConfigOptionValue,
}

/// `POST /sessions/<id>/configure` 请求：标题与选项均可选，至少设置一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SessionConfigSetting>,
    /// 所属项目；None 不修改，Some(None) = 未归属
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub project: Option<Option<String>>,
}

/// `GET /sessions/<id>/history` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub items: Vec<HistoryItem>,
    pub has_more: bool,
    /// 下一页偏移；无更早时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// `GET /sessions/<id>/activities` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitiesPage {
    pub activities: Vec<Activity>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

/// `GET /sessions/<id>/ongoing_activity` 响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OngoingActivity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
}

/// `GET /sessions/<id>/config_options` 响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptions {
    pub options: Vec<SessionConfigOption>,
}

/// `GET /sessions/<id>/slash_commands` 响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommands {
    pub commands: Vec<SlashCommand>,
}

/// `GET /sessions/<id>/plan` 响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub entries: Vec<SessionPlanEntry>,
}

/// `GET /sessions/<id>/context` 响应。尚未收到 ACP `usage_update` 时两者为 0。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextInfo {
    pub context_size: u64,
    pub context_window_size: u64,
}

/// `GET /sessions/<id>/diff` 响应。
pub type DiffResponse = GitDiffResult;

/// 终端状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Running,
    Exited,
}

/// `GET /sessions/<id>/terminals` 的条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    pub id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub state: TerminalState,
}

/// `POST /sessions/<id>/terminals` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminalRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// `POST /sessions/<id>/terminals/<id>` 请求：输入（base64 原始字节）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputRequest {
    pub data: String,
}

/// `POST /sessions/<id>/terminals/<id>/resize` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

/// `GET /sessions/<id>/terminals/<id>` SSE 流的输出事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutput {
    /// base64 编码的输出字节
    pub data: String,
    /// 该事件之后终端输出流的字节偏移
    pub next_cursor: u64,
    /// 客户端应重建本地输出缓冲（首个事件或流中发生断点）
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// 工作流会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub id: String,
    pub title: String,
    pub state: SessionState,
    pub plan: String,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// 关联普通会话
    pub linked_sessions: Vec<Session>,
}

/// `GET /workflows` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowList {
    pub workflows: Vec<Workflow>,
    pub has_more: bool,
}

/// `POST /workflows` 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkflowRequest {
    pub plan: String,
    /// 缺省取计划截断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `POST /workflows/<id>/configure` 请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureWorkflowRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 所属项目；None 不修改，Some(None) = 未归属
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub project: Option<Option<String>>,
}

/// 技能配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
}

/// 工作流计划配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPlanItem {
    pub name: String,
    pub plan: String,
    /// 最近一次使用该计划时选择的项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_project: Option<String>,
}

/// 项目配置项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub name: String,
    pub description: String,
}

/// 项目排序请求（`POST /config/projects/order`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectOrderRequest {
    pub names: Vec<String>,
}

/// 项目更新请求（`PUT /config/projects/<name>`）：项目名称不可修改。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectRequest {
    pub description: String,
}

/// 最近工作目录配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentWorkspace {
    pub machine: String,
    pub workspace: String,
    /// 最近一次使用该工作目录时选择的项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_project: Option<String>,
    pub last_used: u64,
}

/// 快捷指令配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickCommand {
    /// 所属项目；None = 通用快捷指令
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub name: String,
    pub prompt: String,
}

/// 编排智能体的 API 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    ChatCompletions,
    Responses,
    Messages,
}

/// 编排智能体配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorConfig {
    pub api_format: ApiFormat,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 推理级别
    pub effort: String,
}

pub const AMUX_AUTH_METHOD: &str = "amux-config";
/// Nano 无法归类为标准 ACP stop reason 时的自定义停止原因。
pub const NANO_ERROR_STOP_REASON: &str = "_nano_error";
/// `IdleStateUpdate._meta` 中承载 Nano 错误详情的键。
pub const NANO_ERROR_META_KEY: &str = "_nanoError";

impl OrchestratorConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.base_url.trim().is_empty()
            || self.api_key.trim().is_empty()
            || self.model.trim().is_empty()
        {
            return Err("请配置 Base URL、API Key 和模型".into());
        }
        Ok(())
    }

    pub fn auth_meta(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({
            "amuxApiFormat": self.api_format,
            "amuxBaseUrl": self.base_url,
            "amuxApiKey": self.api_key,
            "amuxModel": self.model,
            "amuxEffort": self.effort
        })
        .as_object()
        .unwrap()
        .clone()
    }

    pub fn from_auth_meta(
        meta: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, String> {
        let config: Self = serde_json::from_value(serde_json::json!({
            "apiFormat": meta.get("amuxApiFormat"),
            "baseUrl": meta.get("amuxBaseUrl"),
            "apiKey": meta.get("amuxApiKey"),
            "model": meta.get("amuxModel"),
            "effort": meta.get("amuxEffort")
        }))
        .map_err(|_| "模型配置格式不正确".to_string())?;
        config.validate()?;
        Ok(config)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `null` 表示显式设置为未归属；字段缺失时由 `default` 保留为 `None`。
fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_serializes_machine_and_worktree() {
        let session = Session {
            id: "s1".into(),
            machine: "localpc".into(),
            agent: "codex".into(),
            title: "标题".into(),
            state: SessionState::Idle,
            project: None,
            workspace: "/w".into(),
            worktree_dir: String::new(),
            created_at: 1,
            updated_at: 2,
        };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["machine"], "localpc");
        assert_eq!(json["state"], "idle");
        assert_eq!(json["worktreeDir"], "");
    }

    #[test]
    fn terminal_output_omits_false_truncated() {
        let output = TerminalOutput {
            data: "aGk=".into(),
            next_cursor: 3,
            truncated: false,
        };
        let json = serde_json::to_value(&output).unwrap();
        assert!(json.get("truncated").is_none());
        assert_eq!(json["nextCursor"], 3);
    }

    #[test]
    fn configure_project_distinguishes_omitted_null_and_assigned() {
        let omitted: ConfigureSessionRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(omitted.project, None);

        let unassigned: ConfigureSessionRequest =
            serde_json::from_value(serde_json::json!({ "project": null })).unwrap();
        assert_eq!(unassigned.project, Some(None));
        let unassigned_json = serde_json::to_value(ConfigureSessionRequest {
            title: None,
            config: None,
            project: Some(None),
        })
        .unwrap();
        assert_eq!(unassigned_json["project"], serde_json::Value::Null);

        let assigned: ConfigureWorkflowRequest =
            serde_json::from_value(serde_json::json!({ "project": "项目 A" })).unwrap();
        assert_eq!(assigned.project, Some(Some("项目 A".to_string())));
        assert_eq!(serde_json::to_value(assigned).unwrap()["project"], "项目 A");
    }

    /// Nano 认证的 _meta 载荷：编码后能无损还原，字段名按 docs/DESIGN.md「ACP 认证」。
    #[test]
    fn agent_config_round_trips_through_auth_meta() {
        let config = OrchestratorConfig {
            api_format: ApiFormat::Responses,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: "sk-xxx".into(),
            model: "deepseek-v4-flash".into(),
            effort: "high".into(),
        };
        let meta = config.auth_meta();
        assert_eq!(meta["amuxApiFormat"], "responses");
        assert_eq!(meta["amuxModel"], "deepseek-v4-flash");
        assert_eq!(OrchestratorConfig::from_auth_meta(meta).unwrap(), config);
    }

    #[test]
    fn agent_config_rejects_missing_required_fields() {
        let mut meta = serde_json::Map::new();
        meta.insert("amuxModel".into(), "m".into());
        assert!(OrchestratorConfig::from_auth_meta(meta).is_err());
    }
}
