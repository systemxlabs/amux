//! 业务类型：机器/会话/对话内容/活动/git。
//! 语义依据 docs/DESIGN.md（§5 会话数据、§6 会话、§7 GUI）与 docs/PRD.md。

use serde::{Deserialize, Serialize};

// ---- 机器与 harness ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub name: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    pub server_version: String,
    pub harnesses: Vec<HarnessInfo>,
}

// ---- 会话 ----

/// 会话状态（GUI 展示）：忙 = agent 正在工作，空闲 = 可接收新输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub harness: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: SessionState,
    /// server 崩溃恢复标记（非 ACP 状态）
    pub interrupted: bool,
    /// 已关闭（历史保留、可恢复）
    pub closed: bool,
    pub created_at: u64,
    pub last_event_at: u64,
}

// ---- prompt 输入 ----

/// prompt 输入内容块（docs/DESIGN.md §6：文本 / 内嵌资源 / 资源引用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Resource {
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

// ---- 对话内容（非流式交付，docs/DESIGN.md §5）----

/// 对话内容条目：用户消息或 agent 完整输出。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DialogItem {
    UserMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    AgentOutput {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
}

// ---- 会话活动（activities，docs/DESIGN.md §5.3）----

/// 会话活动：turn 过程中的详细活动（PRD §4.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    Thinking {
        timestamp: u64,
        content: String,
    },
    ToolCall {
        timestamp: u64,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    Compaction {
        timestamp: u64,
        detail: String,
    },
}

// ---- 通知负载 ----

/// turn 完成：agent 完整输出（非流式，docs/DESIGN.md §5.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompleted {
    pub session_id: String,
    pub output: Vec<ContentBlock>,
    pub timestamp: u64,
}

/// 会话状态通知（turn 边界）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateNotify {
    pub session_id: String,
    pub state: SessionState,
}

/// 用户消息通知（GUI 同步"我"的气泡）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessageNotify {
    pub session_id: String,
    pub content: Vec<ContentBlock>,
    pub timestamp: u64,
}

// ---- git（server 直连，docs/DESIGN.md §6）----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
    pub path: String,
    pub status: GitChangeStatus,
    pub staged: bool,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResult {
    pub branch: String,
    pub changes: Vec<GitChange>,
    /// cwd 不是 git 仓库（GUI 不提供 diff 按钮）
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_repo: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOpResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---- 方法参数 ----

#[derive(Debug, Deserialize)]
pub struct CreateSessionParams {
    pub harness: String,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    pub input: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetActivitiesParams {
    pub session_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct GitDiffParams {
    pub cwd: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitRevertParams {
    pub cwd: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

// ---- 方法结果 ----

#[derive(Debug, Serialize)]
pub struct SessionsResult {
    pub sessions: Vec<SessionMeta>,
}

#[derive(Debug, Serialize)]
pub struct SessionResult {
    pub session: SessionMeta,
}

#[derive(Debug, Serialize)]
pub struct OpenSessionResult {
    /// 对话内容全量（用户消息 + agent 输出，非流式）
    pub items: Vec<DialogItem>,
}

#[derive(Debug, Serialize)]
pub struct ActivitiesResult {
    pub activities: Vec<Activity>,
}
