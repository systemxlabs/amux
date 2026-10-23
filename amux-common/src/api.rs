//! Client API（HTTPS）的请求与响应类型。
//!
//! 端点路径与 DESIGN「Client-Server 通信」一节一一对应；Server 实现、Web 应用消费这些类型。

use serde::{Deserialize, Serialize};

use crate::domain::{
    Activity, ContentBlock, GitBranchListResult, GitDiffResult, HistoryItem, SessionConfigOption,
    SessionConfigOptionValue, SessionPlanEntry, SessionState, SlashCommand, WorktreeBase,
};

/// Daemon 内置智能体的发现名称。
pub const NANO_AGENT: &str = "nano";

/// 端点路径片段（应用侧请求共用，避免字面量漂移）。
pub mod path {
    pub const MACHINES: &str = "/machines";
    pub const SESSIONS: &str = "/sessions";
    pub const WORKFLOWS: &str = "/workflows";
    pub const GROUPS: &str = "/groups";
    pub const CONFIG_SKILLS: &str = "/config/skills/";
    pub const CONFIG_WORKFLOWS: &str = "/config/workflows/";
    pub const CONFIG_RECENT_WORKSPACES: &str = "/config/recent_workspaces/";
    pub const CONFIG_QUICK_COMMANDS: &str = "/config/quick_commands/";
    pub const CONFIG_WORKFLOW_AGENT: &str = "/config/workflow_agent/";
    pub const CONFIG_AUTOMATION: &str = "/config/automation";
    pub const CONFIG_PROJECTS: &str = "/config/projects/";
    pub const AUTOMATIONS: &str = "/automations";
    pub const GITHUB_WEBHOOK: &str = "/webhooks/github";
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

/// 某机器上的一个 agent。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub name: String,
    /// 可用 = agent 已启动且 ACP 已初始化。
    pub available: bool,
    /// 已成功创建或恢复到该 agent 的打开会话数。
    pub opened_sessions: usize,
}

/// Agent 本地配置中的模型候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModel {
    /// ACP `session/set_config_option` 使用的模型值。
    pub value: String,
    /// 展示名称。
    pub name: String,
}

/// 普通会话元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    /// 所属工作流会话 ID；None = 非关联普通会话
    #[serde(skip)]
    pub workflow_id: Option<String>,
    /// 所属群聊会话 ID；None = 非群聊成员会话
    #[serde(skip)]
    pub group_id: Option<String>,
    /// 所属机器
    pub machine: String,
    /// 所属 agent
    pub agent: String,
    /// 模型偏好；None = 使用 Agent 默认模型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 会话标题：默认取首条指令截断，用户可修改
    pub title: String,
    pub state: SessionState,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 是否置顶
    pub pinned: bool,
    /// 是否未读
    pub unread: bool,
    /// 用户指定的工作目录
    pub workspace: String,
    /// worktree 目录；空串 = 未启用 worktree
    #[serde(default)]
    pub worktree_dir: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Session {
    /// 会话实际执行目录：启用 worktree 时为 worktree 目录，否则为工作目录。
    pub fn exec_dir(&self) -> &str {
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
    /// 模型偏好；None = 使用 Agent 默认模型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub workspace: String,
    #[serde(default)]
    pub use_worktree: bool,
    /// worktree 基点；开启 worktree 时必填
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base: Option<WorktreeBase>,
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

/// `POST /sessions/<id>/configure` 请求：全量配置标题、项目、置顶状态和可选会话选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureSessionRequest {
    pub title: String,
    pub config: Option<SessionConfigSetting>,
    /// 所属项目；None = 未归属项目
    pub project: Option<String>,
    pub pinned: bool,
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

/// 会话用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// 当前上下文中已使用的 token 数。
    pub used: u64,
    /// 上下文窗口总 token 数。
    pub size: u64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.used == 0 && self.size == 0
    }
}

/// `GET /sessions/<id>/diff` 响应。
pub type DiffResponse = GitDiffResult;
pub type BranchesResponse = GitBranchListResult;

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

/// 会话附件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// Server 存储文件名（`<uuid>.<后缀>`）
    pub name: String,
    pub size: u64,
    pub created_at: u64,
}

/// 附件分页列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentList {
    pub attachments: Vec<Attachment>,
    pub has_more: bool,
}

/// 工作流会话。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub id: String,
    pub title: String,
    pub state: SessionState,
    pub plan: String,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 是否置顶
    pub pinned: bool,
    /// 是否未读
    pub unread: bool,
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

/// 群聊预设成员（`~/.amux/config/group_members.json`；name 可重复）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Eq)]
pub struct GroupMemberTemplate {
    pub name: String,
    pub description: String,
    pub machine: String,
    pub agent: String,
    /// 模型偏好；None = 使用 Agent 默认模型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub workspace: String,
    pub use_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_base: Option<WorktreeBase>,
}

/// 群聊成员：每个 agent 成员背后对应一个成员普通会话。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMember {
    pub id: String,
    pub name: String,
    pub description: String,
    pub machine: String,
    pub agent: String,
    pub session_id: String,
    pub pinned: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 群聊会话。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub title: String,
    pub state: SessionState,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub pinned: bool,
    /// 是否未读
    pub unread: bool,
    pub created_at: u64,
    pub updated_at: u64,
    /// 成员普通会话对应的成员信息
    pub members: Vec<GroupMember>,
    /// 成员普通会话（供会话列表展开与成员状态展示）
    pub sessions: Vec<Session>,
}

/// `GET /groups` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupList {
    pub groups: Vec<Group>,
    pub has_more: bool,
}

/// `POST /groups` 请求：成员列表（名称在群聊内必须唯一）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupRequest {
    pub members: Vec<GroupMemberTemplate>,
    /// 所属项目；None = 未归属项目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `POST /groups/<id>/configure` 请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureGroupRequest {
    pub title: String,
    /// 所属项目；None = 未归属项目
    pub project: Option<String>,
    pub pinned: bool,
}

/// 群聊历史消息：服务端已把成员会话引用解析为完整内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMessage {
    pub id: String,
    /// 发送成员 ID；None = 用户消息
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    /// 发送者展示名（用户为 `user`）
    pub name: String,
    /// 成员所属 agent（`<agent>@<machine>`）；用户消息为空串
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// 成员角色描述；用户消息为空串
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

/// `GET /groups/<id>/history` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupHistoryPage {
    pub items: Vec<GroupMessage>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
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
    pub title: String,
    /// 所属项目；None = 未归属项目
    pub project: Option<String>,
    pub pinned: bool,
}

/// 自动化触发方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationTrigger {
    /// 按 cron 表达式定时触发。
    Schedule { cron: String },
    /// 按 GitHub Webhook 触发。
    GithubWebhook {
        /// `owner/repository`
        repository: String,
        /// 订阅的 GitHub 事件类型；空数组表示全部事件
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<String>,
    },
}

/// 自动化运行任务时用于创建普通会话的选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AutomationSessionOptions {
    /// 使用已有普通会话。
    ExistingSession { session_id: String },
    /// 使用已有工作流会话。
    ExistingWorkflow { workflow_id: String },
    /// 使用已有群聊会话。
    ExistingGroup { group_id: String },
    /// 新建普通会话并执行任务。
    NewSession {
        machine: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        workspace: String,
        #[serde(default)]
        use_worktree: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_base: Option<WorktreeBase>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// 新建工作流会话并执行任务。
    NewWorkflow {
        plan: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// 新建群聊会话并执行任务。
    NewGroup {
        members: Vec<GroupMemberTemplate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
}

/// 自动化运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunState {
    Queued,
    Running,
    Finished,
    Failed,
}

impl AutomationRunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

pub fn parse_automation_run_state(value: &str) -> Option<AutomationRunState> {
    match value {
        "queued" => Some(AutomationRunState::Queued),
        "running" => Some(AutomationRunState::Running),
        "finished" => Some(AutomationRunState::Finished),
        "failed" => Some(AutomationRunState::Failed),
        _ => None,
    }
}

/// 自动化的一次运行记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRun {
    pub id: String,
    pub automation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub state: AutomationRunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

/// 自动化结果分类项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationResultLabel {
    pub name: String,
    pub description: String,
}

/// 自动化定义与最近运行状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Automation {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub task: String,
    pub trigger: AutomationTrigger,
    pub session_options: AutomationSessionOptions,
    #[serde(default)]
    pub result_labels: Vec<AutomationResultLabel>,
    pub created_at: u64,
    pub updated_at: u64,
    /// 下一次定时运行时间；Webhook 触发时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<AutomationRun>,
}

/// 新建或全量更新自动化的请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationInput {
    pub title: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub task: String,
    pub trigger: AutomationTrigger,
    pub session_options: AutomationSessionOptions,
    #[serde(default)]
    pub result_labels: Vec<AutomationResultLabel>,
}

/// `GET /automations/<id>/runs` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationRunList {
    pub runs: Vec<AutomationRun>,
    pub has_more: bool,
}

/// 自动化决策模型 API 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationApiFormat {
    SystemOne,
}

/// 自动化决策模型配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationConfig {
    pub api_format: AutomationApiFormat,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

impl AutomationConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.endpoint.trim().is_empty()
            || self.api_key.trim().is_empty()
            || self.model.trim().is_empty()
        {
            return Err("请配置 Endpoint、API Key 和模型".into());
        }
        Ok(())
    }
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
    /// 模型上下文窗口 token 数
    pub context_window: u64,
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
        if self.context_window == 0 {
            return Err("上下文窗口必须大于 0".into());
        }
        Ok(())
    }

    pub fn auth_meta(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({
            "amuxApiFormat": self.api_format,
            "amuxBaseUrl": self.base_url,
            "amuxApiKey": self.api_key,
            "amuxModel": self.model,
            "amuxEffort": self.effort,
            "amuxContextWindow": self.context_window
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
            "effort": meta.get("amuxEffort"),
            "contextWindow": meta.get("amuxContextWindow")
        }))
        .map_err(|_| "模型配置格式不正确".to_string())?;
        config.validate()?;
        Ok(config)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_serializes_machine_and_worktree() {
        let session = Session {
            id: "s1".into(),
            workflow_id: None,
            group_id: None,
            machine: "localpc".into(),
            agent: "codex".into(),
            model: None,
            title: "标题".into(),
            state: SessionState::Idle,
            project: None,
            pinned: true,
            unread: false,
            workspace: "/w".into(),
            worktree_dir: String::new(),
            created_at: 1,
            updated_at: 2,
        };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["machine"], "localpc");
        assert_eq!(json["state"], "idle");
        assert!(json.get("workflowId").is_none());
        assert_eq!(json["pinned"], true);
        assert_eq!(json["worktreeDir"], "");
    }

    #[test]
    fn create_session_request_serializes_worktree_base() {
        let request = CreateSessionRequest {
            machine: "pc".into(),
            agent: "codex".into(),
            model: None,
            workspace: "/repo".into(),
            use_worktree: true,
            worktree_base: Some(WorktreeBase::parse("refs/remotes/origin/main").unwrap()),
            project: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["useWorktree"], true);
        assert_eq!(json["worktreeBase"], "refs/remotes/origin/main");
    }

    #[test]
    fn group_member_template_requires_worktree_choice() {
        let json = serde_json::json!({
            "name": "coder",
            "description": "负责实现",
            "machine": "pc",
            "agent": "codex",
            "workspace": "/repo"
        });
        assert!(serde_json::from_value::<GroupMemberTemplate>(json).is_err());
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
    fn configure_request_serializes_full_session_state() {
        let json = serde_json::to_value(ConfigureSessionRequest {
            title: "标题".into(),
            config: None,
            project: None,
            pinned: true,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "title": "标题",
                "config": null,
                "project": null,
                "pinned": true
            })
        );
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
            context_window: 1_000_000,
        };
        let meta = config.auth_meta();
        assert_eq!(meta["amuxApiFormat"], "responses");
        assert_eq!(meta["amuxModel"], "deepseek-v4-flash");
        assert_eq!(meta["amuxContextWindow"], 1_000_000);
        assert_eq!(OrchestratorConfig::from_auth_meta(meta).unwrap(), config);
    }

    #[test]
    fn agent_config_rejects_missing_required_fields() {
        let mut meta = serde_json::Map::new();
        meta.insert("amuxModel".into(), "m".into());
        assert!(OrchestratorConfig::from_auth_meta(meta).is_err());
    }

    #[test]
    fn automation_session_options_use_mode_tag() {
        let options = AutomationSessionOptions::NewWorkflow {
            plan: "先实现再审查".into(),
            project: Some("p".into()),
        };
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json["mode"], "new_workflow");
        assert_eq!(json["plan"], "先实现再审查");
        assert_eq!(json["project"], "p");
    }

    #[test]
    fn automation_session_options_use_camel_case_fields() {
        let existing = serde_json::to_value(AutomationSessionOptions::ExistingSession {
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(existing["mode"], "existing_session");
        assert_eq!(existing["sessionId"], "s1");
        assert!(existing.get("session_id").is_none());
        assert_eq!(
            serde_json::from_value::<AutomationSessionOptions>(existing).unwrap(),
            AutomationSessionOptions::ExistingSession {
                session_id: "s1".into(),
            }
        );

        let new_session = serde_json::to_value(AutomationSessionOptions::NewSession {
            machine: "pc".into(),
            agent: "codex".into(),
            model: Some("gpt-5".into()),
            workspace: "/repo".into(),
            use_worktree: true,
            worktree_base: Some(WorktreeBase::parse("refs/heads/main").unwrap()),
            project: Some("p".into()),
        })
        .unwrap();
        assert_eq!(new_session["mode"], "new_session");
        assert_eq!(new_session["model"], "gpt-5");
        assert_eq!(new_session["useWorktree"], true);
        assert_eq!(new_session["worktreeBase"], "refs/heads/main");
    }

    #[test]
    fn automation_config_uses_endpoint_field() {
        let config = AutomationConfig {
            api_format: AutomationApiFormat::SystemOne,
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            api_key: "token".into(),
            model: "jev-latest".into(),
        };
        let json = serde_json::to_value(config).unwrap();
        assert_eq!(json["apiFormat"], "system_one");
        assert_eq!(json["endpoint"], "https://api.typesafe.ai/v1/systemone");
        assert!(json.get("baseUrl").is_none());
        assert!(json.get("base_url").is_none());
    }
}
