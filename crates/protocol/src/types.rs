//! 业务类型：agent/会话/对话内容/活动/workspace。
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthParams {
    pub token: String,
}

/// 某机器上的一个 agent：名称与可用性。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub name: String,
    pub available: bool,
}

/// `session.new` / `session.prompt` 等标识 agent 的名字。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentParams {
    pub agent: String,
}

/// `agent.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListResult {
    pub agents: Vec<AgentInfo>,
}

/// `agent.restart` / `workspace.restore` 通用操作结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 会话状态：空闲或工作中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Busy,
}

impl SessionState {
    /// 线上/持久化的 snake_case 表示（与 `parse_session_state` 互为逆）。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Busy => "busy",
        }
    }
}

/// 普通会话元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    /// 所属 agent（agent.list 里的名字）
    pub agent: String,
    /// 工作目录
    pub cwd: String,
    pub state: SessionState,
    /// 会话标题：默认由首条指令自动生成，用户可随时修改；空串 = 尚无首条指令。
    #[serde(default)]
    pub title: String,
    pub created_at: u64,
    /// 最近活跃时间（会话列表按它排序）
    pub last_active_at: u64,
    /// git worktree 目录：非空时 agent 实际工作在
    /// 该目录，磁盘上的工作树由 `session.new` 立即创建（`use_worktree=true`）；
    /// 空串 = 未启用 worktree。
    #[serde(default)]
    pub worktree_dir: String,
    /// 当前上下文大小（token，ACP `usage_update` 的 used）；0 = 尚未收到通知。
    #[serde(default)]
    pub context_size: u64,
    /// 上下文窗口总大小（token，ACP `usage_update` 的 size）；0 = 尚未收到通知。
    #[serde(default)]
    pub context_window_size: u64,
}

/// 会话配置选项（ACP `configOptions` 的投影；类型与协议对齐）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    /// 选项唯一标识（ACP `session/set_config_option` 的 configId）
    pub id: String,
    /// 展示名（如「模型」「推理级别」）
    pub name: String,
    /// 可选描述
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 语义分类（ACP category，如 model / thought_level；仅供 UX，可为空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// 选项类型与当前值
    #[serde(flatten)]
    pub kind: SessionConfigKind,
}

/// 会话选项的类型化负载（ACP `type` 判别）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// select 选项的一个可选值（ACP 分组展平为扁平的 value + name）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectEntry {
    pub value: String,
    pub name: String,
}

/// 会话选项的取值（ACP `SessionConfigOptionValue`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigOptionValue {
    /// select 类选项：选项值 id
    ValueId { value: String },
    /// boolean 类选项：开关值
    Boolean { value: bool },
}

/// `session.new` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    pub agent: String,
    pub cwd: String,
    /// 是否以 git worktree 方式工作：`session.new` 即在工作树根目录下创建独立
    /// worktree 并写入元数据；agent 侧会话仍延后到首条指令时懒创建
    #[serde(default)]
    pub use_worktree: bool,
}

/// `session.prompt` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub session_id: String,
    pub input: Vec<ContentBlock>,
}

/// `session.configure` / `session.cancel` / `session.delete` / `session.ongoing_activity` 通用：仅含会话 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdParams {
    pub session_id: String,
}

/// `session.configure` 中设置的会话选项（Server 转为 ACP
/// `session/set_config_option` 请求；选项集合以 Agent 侧为权威）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSetting {
    pub config_id: String,
    #[serde(flatten)]
    pub value: SessionConfigOptionValue,
}

/// `session.configure` 参数：会话标题与会话选项均可选，至少设置一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigureParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SessionConfigSetting>,
}

/// `session.list` 惰性分页参数。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `session.history` / `session.activities` 惰性分页参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPageParams {
    pub session_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
    /// 独占上界游标：只返回该下标之前的条目（None = 从最新一窗开始）
    #[serde(default)]
    pub before: Option<u64>,
}

/// `session.new` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: SessionMeta,
}

/// `session.config_options` 结果。会话未打开 agent 侧会话或 agent 不支持时为空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOptionsResult {
    pub options: Vec<SessionConfigOption>,
}

/// 会话斜杠命令（ACP `AvailableCommand` 的投影）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// 命令名（如 `goal`，用户输入 `/goal` 调用）
    pub name: String,
    /// 命令功能描述
    pub description: String,
    /// 命令名之后的输入提示（ACP `UnstructuredCommandInput.hint`；无需输入则缺省）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// `session.slash_commands` 结果。尚无 agent 侧会话或 agent 未下发时为空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSlashCommandsResult {
    pub commands: Vec<SlashCommand>,
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

/// 会话计划条目（agent 完成用户复杂指令的一个步骤；ACP `PlanEntry` 的投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlanEntry {
    /// 条目描述
    pub content: String,
    pub priority: SessionPlanPriority,
    pub status: SessionPlanStatus,
}

/// `session.plan` 结果。尚无 agent 侧会话或 agent 未下发时为空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlanResult {
    pub entries: Vec<SessionPlanEntry>,
}

/// `session.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
    pub has_more: bool,
}

/// `session.info` 参数：批量查询指定会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoParams {
    pub session_ids: Vec<String>,
}

/// `session.info` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoResult {
    pub sessions: Vec<SessionMeta>,
}

/// 会话状态变更原因，源自 ACP `session/prompt` 响应的 stopReason。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateChangeReason {
    /// 正常结束（ACP end_turn）
    #[default]
    Completed,
    /// 客户端取消（ACP cancelled；规范要求 agent 收到 session/cancel 后必须返回）
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

/// 会话状态变更通知负载。
/// reason 仅在变更为 Idle（turn 结束）时有意义；Busy 侧恒为 completed。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateChange {
    pub session_id: String,
    pub old_state: SessionState,
    pub new_state: SessionState,
    pub reason: StateChangeReason,
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

/// 对话历史条目：仅包含用户输入与合并后的 agent 输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryItem {
    UserMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    AgentMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
}

/// `session.history` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryResult {
    pub items: Vec<HistoryItem>,
    pub has_more: bool,
    /// 更早一窗的独占上界游标；无更早时为 None。
    pub next_before: Option<u64>,
}

/// 会话活动：turn 过程中的详细活动。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    Error {
        timestamp: u64,
        detail: String,
    },
}

/// `session.activities` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitiesResult {
    pub activities: Vec<Activity>,
    pub has_more: bool,
    /// 更早一窗的独占上界游标；无更早时为 None。
    pub next_before: Option<u64>,
}

/// `session.ongoing_activity` 结果（进行中的活动；无则 None）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OngoingActivityResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
}

/// git 改动状态。server 侧 status 检测关闭了重命名跟踪，重命名呈现为删除+新增，
/// 故无 Renamed/Untracked 变体（Untracked 与 Added 语义重叠）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeStatus {
    Added,
    Modified,
    Deleted,
}

/// 单个 diff hunk（可独立反向应用撤销）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffHunk {
    pub header: String,
    /// 完整可应用的 patch（含文件头 + 该 hunk），可直接用于 `git apply --reverse`
    pub patch: String,
}

/// 单文件 diff。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffFile {
    pub path: String,
    pub status: GitChangeStatus,
    pub additions: u32,
    pub deletions: u32,
    pub patch: String,
    pub hunks: Vec<GitDiffHunk>,
}

/// `workspace.diff` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffResult {
    pub files: Vec<GitDiffFile>,
    /// cwd 不是 git 仓库
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_repo: bool,
}

/// `workspace.diff` 参数（与 restore 不同：无 patch 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffParams {
    /// 普通会话 ID；Server 从注册表解析其绑定的工作目录。
    pub session_id: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// `workspace.restore` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRestoreParams {
    /// 普通会话 ID；Server 从注册表解析其绑定的工作目录。
    pub session_id: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

/// `workspace.list` 参数。path 始终是相对 cwd 的目录路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListParams {
    /// 普通会话 ID；工作目录不可由调用方任意指定。
    pub session_id: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "workspace_page_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

/// 工作目录中的一个目录项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// `workspace.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListResult {
    pub path: String,
    pub entries: Vec<WorkspaceEntry>,
    pub has_more: bool,
    pub next_offset: usize,
}

/// `workspace.read` 参数。offset/limit 按 UTF-8 文本行分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReadParams {
    /// 普通会话 ID；工作目录不可由调用方任意指定。
    pub session_id: String,
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "workspace_read_limit")]
    pub limit: usize,
}

/// `workspace.read` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReadResult {
    pub path: String,
    pub content: String,
    pub has_more: bool,
    pub next_offset: usize,
}

/// 分页/窗口默认大小（server 兜底值与 GUI 请求值共用，避免两侧字面量漂移）。
pub const SESSION_LIST_DEFAULT_LIMIT: usize = 50;
pub const SESSION_PAGE_DEFAULT_LIMIT: usize = 200;
pub const WORKSPACE_LIST_PAGE_LIMIT: usize = 200;
pub const WORKSPACE_READ_PAGE_LIMIT: usize = 400;

fn workspace_page_limit() -> usize {
    WORKSPACE_LIST_PAGE_LIMIT
}

fn workspace_read_limit() -> usize {
    WORKSPACE_READ_PAGE_LIMIT
}

/// `terminal.open` 参数。
/// 终端不跨应用共享、与连接绑定；cwd 由应用指定（普通会话场景为工作目录或
/// worktree 目录）。size 为初始行列——避免
/// 「先 80×24 再 resize」竞态导致 vim/htop 等全屏程序初始渲染错乱。
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

/// `terminal.close` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalIdParams {
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

/// `terminal.input` 参数。data 为 base64 编码的原始字节流
/// （终端输入是按键字节而非 UTF-8 命令文本，经 base64 走 JSON-RPC 文本消息）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInputParams {
    pub terminal_id: String,
    pub data: String,
}

/// `terminal.output` 通知负载。data 为 base64 编码的 PTY 输出字节流。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputNotification {
    pub terminal_id: String,
    pub data: String,
}

/// `terminal.exit` 通知负载（用于客户端感知 shell 退出并清理）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExitNotification {
    pub terminal_id: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 会话状态的线上表示解析（snake_case）。GUI 与 server 之外的持久化层（工作流 sqlite）
/// 也存该表示，解析统一收口在此，避免各处手写 match 漂移。
pub fn parse_session_state(s: &str) -> Option<SessionState> {
    match s {
        "idle" => Some(SessionState::Idle),
        "busy" => Some(SessionState::Busy),
        _ => None,
    }
}

/// 会话标题生成（协议面共享的纯逻辑）：取首行、压缩空白、截断到 40 字符
/// （超出追加省略号）。server 在首条 prompt 时用它生成默认标题；
/// GUI 在创建工作流会话时用它生成本地标题。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_first_line_collapses_whitespace() {
        assert_eq!(generate_title("实现登录功能\n然后写测试"), "实现登录功能");
        assert_eq!(generate_title("  多  个   空格  \n第二行"), "多 个 空格");
        assert_eq!(generate_title(""), "");
    }

    #[test]
    fn title_truncates_with_ellipsis() {
        let long = "这".repeat(50);
        let t = generate_title(&long);
        assert_eq!(t.chars().count(), 41);
        assert!(t.ends_with('…'));

        let short = "a".repeat(40);
        assert_eq!(generate_title(&short).chars().count(), 40);
        assert!(!generate_title(&short).ends_with('…'));
    }

    #[test]
    fn auth_and_session_params_deserialize() {
        let a: AuthParams = serde_json::from_str(r#"{"token":"t"}"#).unwrap();
        assert_eq!(a.token, "t");
        let s: SessionNewParams =
            serde_json::from_str(r#"{"agent":"codex","cwd":"/tmp"}"#).unwrap();
        assert_eq!(s.agent, "codex");
        let id: SessionIdParams = serde_json::from_str(r#"{"sessionId":"s1"}"#).unwrap();
        assert_eq!(id.session_id, "s1");
        let cfg: SessionConfigureParams =
            serde_json::from_str(r#"{"sessionId":"s1","title":"实现登录"}"#).unwrap();
        assert_eq!(cfg.title.as_deref(), Some("实现登录"));
        assert!(cfg.config.is_none(), "仅设置标题时 config 缺省");
        let cfg: SessionConfigureParams = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "config": {"configId": "model", "type": "value_id", "value": "gpt-5"}
        }))
        .unwrap();
        let set = cfg.config.expect("应带会话选项设置");
        assert_eq!(set.config_id, "model");
        assert_eq!(
            set.value,
            SessionConfigOptionValue::ValueId {
                value: "gpt-5".into()
            }
        );
        let p: SessionListParams = serde_json::from_str(r#"{"limit":10}"#).unwrap();
        assert_eq!(p.limit, Some(10));
        let page: SessionPageParams = serde_json::from_str(r#"{"sessionId":"s1"}"#).unwrap();
        assert_eq!(page.session_id, "s1");
        assert_eq!(page.before, None);
    }

    #[test]
    fn session_list_result_serialize_camel_case() {
        let res = SessionListResult {
            sessions: Vec::new(),
            has_more: true,
        };
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("\"hasMore\":true"), "{s}");
        assert!(s.contains("\"sessions\":[]"), "{s}");
    }

    #[test]
    fn state_change_payload_serializes() {
        let n = SessionStateChange {
            session_id: "s1".into(),
            old_state: SessionState::Busy,
            new_state: SessionState::Idle,
            reason: StateChangeReason::Cancelled,
        };
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains("\"sessionId\":\"s1\""), "{s}");
        assert!(s.contains("\"newState\":\"idle\""), "{s}");
        assert!(s.contains("\"reason\":\"cancelled\""), "{s}");
    }

    #[test]
    fn workspace_params_and_results_use_documented_json_shape() {
        let list: WorkspaceListParams =
            serde_json::from_str(r#"{"sessionId":"s1","path":"src"}"#).unwrap();
        assert_eq!(list.session_id, "s1");
        assert_eq!(list.path.as_deref(), Some("src"));
        assert_eq!(list.limit, 200);
        assert_eq!(list.offset, 0);

        let read: WorkspaceReadParams =
            serde_json::from_str(r#"{"sessionId":"s1","path":"README.md"}"#).unwrap();
        assert_eq!(read.limit, 400);

        let result = WorkspaceReadResult {
            path: "README.md".into(),
            content: "hello\n".into(),
            has_more: false,
            next_offset: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"hasMore\":false"), "{json}");
        assert!(json.contains("\"nextOffset\":1"), "{json}");
    }

    #[test]
    fn session_state_parses_snake_case() {
        assert_eq!(parse_session_state("idle"), Some(SessionState::Idle));
        assert_eq!(parse_session_state("busy"), Some(SessionState::Busy));
        assert_eq!(parse_session_state("Idle"), None);
        assert_eq!(parse_session_state(""), None);
    }

    /// as_str / parse_session_state / serde 序列化三个表示互为一致：
    /// 任一侧漂移（手写 match 与 rename_all 不同步）都会静默破坏线上与持久化数据。
    #[test]
    fn session_state_representations_agree() {
        for state in [SessionState::Idle, SessionState::Busy] {
            assert_eq!(parse_session_state(state.as_str()), Some(state));
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(json.as_str(), Some(state.as_str()));
            let parsed: SessionState = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn git_diff_types_roundtrip() {
        let hunk = GitDiffHunk {
            header: "@@ -1,2 +1,3 @@".into(),
            patch: "diff --git a/x b/x\n@@ -1,2 +1,3 @@\n+new\n".into(),
        };
        let file = GitDiffFile {
            path: "x".into(),
            status: GitChangeStatus::Modified,
            additions: 1,
            deletions: 0,
            patch: "diff --git a/x b/x\n@@ -1,2 +1,3 @@\n+new\n".into(),
            hunks: vec![hunk],
        };
        let res = WorkspaceDiffResult {
            files: vec![file],
            not_repo: false,
        };
        let s = serde_json::to_string(&res).unwrap();
        let back: WorkspaceDiffResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].hunks[0].header, "@@ -1,2 +1,3 @@");
        assert!(!back.not_repo);
    }
}
