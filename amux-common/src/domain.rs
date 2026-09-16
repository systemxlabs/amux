//! Daemon 与 Server 两侧共享的领域类型。

use serde::{Deserialize, Serialize};

/// 会话状态：空闲或工作中（普通会话与工作流会话共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Busy,
}

impl SessionState {
    /// 线上/持久化的 snake_case 表示（与 [`parse_session_state`] 互为逆）。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Busy => "busy",
        }
    }
}

/// 会话状态的线上表示解析（snake_case）。
pub fn parse_session_state(s: &str) -> Option<SessionState> {
    match s {
        "idle" => Some(SessionState::Idle),
        "busy" => Some(SessionState::Busy),
        _ => None,
    }
}

/// 状态变更原因，源自 ACP `state_update` 的 `stopReason`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateChangeReason {
    /// 正常结束（ACP end_turn）
    #[default]
    Completed,
    /// 客户端取消（ACP cancelled）
    Cancelled,
    /// 达到 token 上限（ACP max_tokens）
    MaxTokens,
    /// turn 内请求次数上限（ACP max_turn_requests）
    MaxTurnRequests,
    /// agent 拒绝继续（ACP refusal）
    Refusal,
    /// 异常终止：连接中断 / ACP 调用失败，无 stopReason 可取
    Aborted,
}

/// prompt 输入内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// 对话历史条目：用户输入与 agent 输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum HistoryItem {
    #[serde(rename = "user")]
    UserMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    #[serde(rename = "agent")]
    AgentMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
}

/// 会话活动：turn 过程中的详细活动（thinking / tool_call / error）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    Thinking {
        timestamp: u64,
        thinking: String,
    },
    ToolCall {
        timestamp: u64,
        tool_call_id: String,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<String>,
    },
    Error {
        timestamp: u64,
        error: String,
    },
}

/// 会话配置选项（ACP `configOptions` 的投影）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    /// 选项唯一标识（ACP `session/set_config_option` 的 configId）
    pub id: String,
    /// 展示名（如「模型」「推理级别」）
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 语义分类（ACP category，如 model / thought_level；仅供 UX）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(flatten)]
    pub kind: SessionConfigKind,
}

/// 会话选项的类型化负载（ACP `type` 判别）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigKind {
    /// 单值选择器（下拉）：当前选中值 + 可选值（分组已展平）
    Select {
        current_value: String,
        options: Vec<SessionConfigSelectEntry>,
    },
    /// 布尔开关
    Boolean { current_value: bool },
}

/// select 选项的一个可选值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectEntry {
    pub value: String,
    pub name: String,
}

/// 会话选项的取值（ACP `SessionConfigOptionValue`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigOptionValue {
    ValueId { value: String },
    Boolean { value: bool },
}

/// 会话斜杠命令（ACP `AvailableCommand` 的投影）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// 命令名（如 `goal`，用户输入 `/goal` 调用）
    pub name: String,
    pub description: String,
    /// 命令名之后的输入提示；无需输入则缺省
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// 会话计划条目的相对重要度（ACP `PlanEntryPriority` 的投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanPriority {
    High,
    Medium,
    Low,
}

/// 会话计划条目的执行状态（ACP `PlanEntryStatus` 的投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// 会话计划条目（ACP `PlanEntry` 的投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlanEntry {
    pub content: String,
    pub priority: SessionPlanPriority,
    pub status: SessionPlanStatus,
}

/// git 改动状态。status 检测关闭重命名跟踪，重命名呈现为删除+新增。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeStatus {
    Added,
    Modified,
    Deleted,
}

/// 单个 diff hunk（改动审查中可选中的最小单位）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffHunk {
    pub header: String,
    /// 完整 patch 文本（含文件头 + 该 hunk），作为选中内容发送给 agent
    pub patch: String,
    /// hunk 行，供 inline 展示
    pub lines: Vec<GitDiffLine>,
}

/// diff 行类别（统一 diff 的前缀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffLineKind {
    Context,
    Add,
    Remove,
}

impl GitDiffLineKind {
    /// 统一 diff 的行前缀。
    pub fn prefix(self) -> char {
        match self {
            GitDiffLineKind::Context => ' ',
            GitDiffLineKind::Add => '+',
            GitDiffLineKind::Remove => '-',
        }
    }
}

/// 一行 diff 内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffLine {
    pub kind: GitDiffLineKind,
    /// 行内容（不含前导前缀与行尾换行）
    pub text: String,
}

/// 单文件 diff。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffFile {
    pub path: String,
    pub status: GitChangeStatus,
    pub additions: u32,
    pub deletions: u32,
    /// 完整 patch 文本（含全部 hunk），作为选中内容发送给 agent
    pub patch: String,
    pub hunks: Vec<GitDiffHunk>,
}

/// 仓库改动 diff。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub files: Vec<GitDiffFile>,
    /// 目标目录不是 git 仓库
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_repo: bool,
}

/// 目录浏览/联想共用的目录项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    /// 条目名称（最后一段）
    pub name: String,
    /// 条目绝对路径；客户端直接用它加载子目录/读文件，无需拼接
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// `fs.list` 参数。`path` 为目标目录绝对路径，缺省按 daemon 当前目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default = "default_fs_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

/// `fs.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsListResult {
    pub path: String,
    pub entries: Vec<FsEntry>,
    pub has_more: bool,
    pub next_offset: usize,
}

/// `fs.read` 参数。按 UTF-8 文本行分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadParams {
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_fs_read_limit")]
    pub limit: usize,
}

/// `fs.read` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadResult {
    pub path: String,
    pub content: String,
    pub has_more: bool,
    pub next_offset: usize,
}

/// `terminal.open` 参数：cwd 与初始行列（避免全屏程序以默认尺寸先渲染）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenParams {
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

/// `terminal.open` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenResult {
    pub terminal_id: String,
}

/// `terminal.resize` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeParams {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

/// `terminal.input` 参数。data 为 base64 编码的原始按键字节。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputParams {
    pub terminal_id: String,
    pub data: String,
}

/// `terminal.close` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalIdParams {
    pub terminal_id: String,
}

/// `terminal.output` 通知负载。data 为 base64 编码的 PTY 输出字节。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputNotification {
    pub terminal_id: String,
    pub data: String,
}

/// `terminal.exit` 通知负载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExitNotification {
    pub terminal_id: String,
}

/// 管理类操作的通用结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl OpResult {
    pub fn ok() -> Self {
        Self {
            ok: true,
            message: None,
        }
    }
}

/// 分页/窗口默认大小（Server 兜底值与客户端请求值共用，避免字面量漂移）。
pub const SESSION_LIST_DEFAULT_LIMIT: usize = 50;
pub const SESSION_PAGE_DEFAULT_LIMIT: usize = 200;
pub const FS_LIST_PAGE_LIMIT: usize = 200;
pub const FS_READ_PAGE_LIMIT: usize = 400;

fn default_fs_list_limit() -> usize {
    FS_LIST_PAGE_LIMIT
}

fn default_fs_read_limit() -> usize {
    FS_READ_PAGE_LIMIT
}

/// 会话标题：取首条提示词首行、压缩空白、截断到 40 字符（超出追加省略号）。
pub fn generate_title(input: &str) -> String {
    const MAX_CHARS: usize = 40;
    let line = input.lines().next().unwrap_or("").trim();
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(MAX_CHARS).collect();
    if collapsed.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        for state in [SessionState::Idle, SessionState::Busy] {
            assert_eq!(parse_session_state(state.as_str()), Some(state));
        }
        assert_eq!(parse_session_state("running"), None);
    }

    #[test]
    fn content_block_is_tagged_by_type() {
        let text = serde_json::to_string(&ContentBlock::Text { text: "hi".into() }).unwrap();
        assert_eq!(text, r#"{"type":"text","text":"hi"}"#);
    }

    #[test]
    fn title_from_first_line_collapses_whitespace_and_truncates() {
        assert_eq!(generate_title("实现登录功能\n然后写测试"), "实现登录功能");
        assert_eq!(generate_title("  多  个   空格  \n第二行"), "多 个 空格");
        assert_eq!(generate_title(""), "");
        let long = "这".repeat(50);
        let title = generate_title(&long);
        assert_eq!(title.chars().count(), 41);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn terminal_output_notification_shape() {
        let notification = TerminalOutputNotification {
            terminal_id: "t1".into(),
            data: "aGk=".into(),
        };
        let json = serde_json::to_value(&notification).unwrap();
        assert_eq!(json["terminalId"], "t1");
        assert_eq!(json["data"], "aGk=");
    }
}
