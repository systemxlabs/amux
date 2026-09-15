//! Client API（HTTPS）的请求与响应类型。
//!
//! 端点路径与 DESIGN「Client-Server 通信」一节一一对应；Server 与桌面应用共用这些类型。

use serde::{Deserialize, Serialize};

use crate::domain::{
    Activity, ContentBlock, GitDiffResult, HistoryItem, SessionConfigOption,
    SessionConfigOptionValue, SessionPlanEntry, SessionState, SlashCommand,
};

/// 端点路径片段（Server 路由与应用侧请求共用，避免字面量漂移）。
pub mod path {
    pub const MACHINES: &str = "/machines";
    pub const SESSIONS: &str = "/sessions";
    pub const WORKFLOWS: &str = "/workflows";
    pub const CONFIG_SKILLS: &str = "/config/skills/";
    pub const CONFIG_WORKFLOWS: &str = "/config/workflows/";
    pub const CONFIG_RECENT_WORKSPACES: &str = "/config/recent_workspaces/";
    pub const CONFIG_QUICK_COMMANDS: &str = "/config/quick_commands/";
    pub const CONFIG_AGENT: &str = "/config/agent/";
}

/// 一台已连接的机器（`machine.info` 透传）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
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
    /// 用户指定的工作目录
    pub workspace: String,
    /// worktree 目录；空串 = 未启用 worktree
    #[serde(default)]
    pub worktree_dir: String,
    pub created_at: u64,
    pub updated_at: u64,
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
}

/// `POST /sessions/<id>` 请求：发送指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub input: Vec<ContentBlock>,
}

/// `session/configure` 中的会话选项设置。
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

/// `POST /sessions/<id>/restore` 请求：给 patch 按块撤销，否则整文件撤销。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
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

/// `GET /sessions/<id>/terminals/<id>` 响应：游标之后的增量输出。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutput {
    /// base64 编码的输出字节
    pub data: String,
    /// 下次请求携带的游标
    pub next_cursor: u64,
    /// 请求的游标早于缓存起点（旧输出已被丢弃），本次返回缓存全量
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
}

/// `POST /workflows/<id>/configure` 请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureWorkflowRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
}

/// 常用工作目录配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentWorkspace {
    pub machine: String,
    pub workspace: String,
    pub last_used: u64,
}

/// 快捷指令配置项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickCommand {
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

fn is_false(b: &bool) -> bool {
    !*b
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
}
