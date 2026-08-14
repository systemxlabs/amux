//! 业务类型：机器/会话/对话内容/活动/git。
//! 语义依据 docs/DESIGN.md（§5 会话数据、§6 会话、§7 GUI）与 docs/PRD.md。

use serde::{Deserialize, Serialize};

// ---- 机器与 harness ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

// ---- GUI 本地配置形状（协议面单一来源，docs/DESIGN.md §5.5）----

/// 快捷指令（PRD §3.4）：每条即一段发给 agent 的提示词。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuickCommand {
    pub id: String,
    pub name: String,
    pub prompt: String,
}

/// Skills 注册表条目（PRD §3.6）：只存一段描述（仓库/资源 URL 或下载安装方法说明）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// 工作流模板（PRD §3.7）：名称 + 自然语言描述（可复用的工作流）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// 内置编排 agent 的 API 配置（PRD §4.3「编排 agent」分类）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorConfig {
    /// API format：`chat_completions`（OpenAI Chat Completions）
    /// | `responses`（OpenAI Responses）| `messages`（Anthropic Messages）
    pub api_format: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        // api_format 默认 chat_completions；Base URL / API key / 模型均由用户显式填写
        OrchestratorConfig {
            api_format: "chat_completions".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl OrchestratorConfig {
    /// 编排 agent 是否已配置可用（PRD §4.3）：Base URL、API key、模型均非空。
    /// 未配置时创建工作流会话应给出提示并引导到设置页（docs/DESIGN.md §10）。
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
    }
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
    /// 会话标题：默认由首条指令/目标自动生成（简短摘要），用户可随时修改
    /// （docs/PRD.md §3.1）。空字符串 = 尚无首条指令，GUI 显示占位文案。
    #[serde(default)]
    pub title: String,
    pub created_at: u64,
    pub last_event_at: u64,
}

/// 会话标题生成（协议面共享的纯逻辑）：取首行、压缩空白、截断到 max_chars。
/// server 在首条 prompt 时用它生成默认标题；GUI 在创建工作流会话时用它生成本地标题。
pub fn generate_title(input: &str) -> String {
    generate_title_max(input, 40)
}

/// 带长度上限的标题生成（可单测）。
pub fn generate_title_max(input: &str, max_chars: usize) -> String {
    let line = input.lines().next().unwrap_or("").trim();
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// ---- prompt 输入 ----

/// prompt 输入内容块（docs/DESIGN.md §6：文本 / 内嵌资源 / 资源引用）。
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

// ---- 对话内容（非流式交付，docs/DESIGN.md §5）----

/// 对话内容条目：用户消息或 agent 完整输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 系统/错误活动（如编排 agent 调用失败）
    Error {
        timestamp: u64,
        detail: String,
    },
}

// ---- 通知负载 ----

/// server → GUI 透传事件（docs/DESIGN.md §5.1）：普通会话事件逐条透传，由 GUI 应用聚合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PassthroughEvent {
    /// 用户消息（prompt 回显 / load 重放）
    UserMessage {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    /// agent 输出片段（GUI 应用按消息收敛拼接为完整输出）
    OutputChunk {
        text: String,
        timestamp: u64,
    },
    /// thinking 片段（GUI 应用逐块累积为一条活动）
    ThinkingChunk {
        content: String,
        timestamp: u64,
    },
    /// 工具调用（GUI 应用合并为一条活动）
    ToolCall {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        timestamp: u64,
    },
    /// 上下文压缩
    Compaction {
        detail: String,
        timestamp: u64,
    },
    /// agent 自报状态（ACP `session_info_update` 透传；ACP 未携带状态时为 None）
    SessionInfo {
        state: Option<SessionState>,
        timestamp: u64,
    },
    /// turn 边界（server 从 prompt 请求生命周期反射：发出请求 = 开始，收到 result = 结束）
    TurnStarted {
        timestamp: u64,
    },
    TurnEnded {
        timestamp: u64,
    },
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

/// 单个 diff hunk（PRD §3.5：单 hunk revert）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffHunk {
    /// `@@ -1,3 +1,4 @@` 头部
    pub header: String,
    /// 完整可应用的 patch（含文件头 + 该 hunk），可直接用于 `git apply --reverse`
    pub patch: String,
}

/// 单文件 diff（PRD §3.5：文件列表 + 增减行数 + side-by-side/inline 渲染 + revert）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffFile {
    pub path: String,
    pub status: GitChangeStatus,
    pub additions: u32,
    pub deletions: u32,
    /// 该文件完整 patch（`git diff HEAD -- path`），渲染与"全部变更"revert 用
    pub patch: String,
    pub hunks: Vec<GitDiffHunk>,
}

/// git_diff 的结构化结果（取代旧实现返回的裸字符串）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub files: Vec<GitDiffFile>,
    /// cwd 不是 git 仓库（GUI 不提供 diff 按钮）
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_repo: bool,
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

/// 打开会话（惰性加载：默认只取最新一窗，`before` 游标向上取更早历史）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionParams {
    pub session_id: String,
    /// 返回的最大条目数（默认 200）
    #[serde(default)]
    pub limit: Option<usize>,
    /// 独占上界游标：只返回该下标之前的条目（None = 从最新一窗开始）
    #[serde(default)]
    pub before: Option<usize>,
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

/// 修改会话标题（用户可随时修改，PRD §3.1）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionTitleParams {
    pub session_id: String,
    pub title: String,
}

/// 配置 agent 默认模型（PRD §3.3，server 侧持久化）。
#[derive(Debug, Deserialize)]
pub struct SetDefaultModelParams {
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// 查询某 agent 安装的 skills 列表（PRD §3.3）。
#[derive(Debug, Deserialize)]
pub struct ListAgentSkillsParams {
    pub harness: String,
}

/// 手动重新拉起不可用的 agent（PRD §3.3/§4.3，无需重启 server）。
#[derive(Debug, Deserialize)]
pub struct RetryHarnessParams {
    pub harness: String,
}

// ---- 方法结果 ----

/// 会话列表惰性分页参数（docs/DESIGN.md §3.2 / PRD §4.1.1：
/// 首次只取最近活跃会话，滚动加载更早）。
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsParams {
    /// 窗口大小（默认 50，实现决策）
    #[serde(default)]
    pub limit: Option<usize>,
    /// 独占上界游标：只返回 `last_event_at < before` 的更早一窗（None = 从最近活跃开始）
    #[serde(default)]
    pub before: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsResult {
    /// 一窗会话（按最近活跃降序）
    pub sessions: Vec<SessionMeta>,
    /// 是否还有更早的会话（GUI 显示"加载更早"）
    pub has_more: bool,
    /// 下一次"加载更早"应传的 before 游标
    pub next_before: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SessionResult {
    pub session: SessionMeta,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionResult {
    /// 重放事件（一窗，docs/DESIGN.md §5.2 按需拉取；GUI 应用聚合为对话内容与活动）
    pub events: Vec<PassthroughEvent>,
    /// 是否还有更早的历史（GUI 显示"加载更早消息"）
    pub has_more: bool,
    /// 下一次"加载更早"应传的 before 游标
    pub next_before: usize,
}

/// agent 安装的 skills 列表（PRD §3.3）。
#[derive(Debug, Serialize)]
pub struct ListAgentSkillsResult {
    pub skills: Vec<String>,
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
        assert_eq!(t.chars().count(), 41); // 40 字符 + …
        assert!(t.ends_with('…'));

        let short = "a".repeat(40);
        assert_eq!(generate_title(&short).chars().count(), 40);
        assert!(!generate_title(&short).ends_with('…'));
    }

    #[test]
    fn session_meta_title_roundtrip() {
        let meta = SessionMeta {
            id: "s_1".into(),
            harness: "codex".into(),
            cwd: "/tmp".into(),
            model: None,
            state: SessionState::Idle,
            interrupted: false,
            title: "实现登录".into(),
            created_at: 1,
            last_event_at: 1,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"title\":\"实现登录\""));
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title, "实现登录");
        // 旧数据缺 title 字段时回退空串（beta 规则之外仍稳妥）
        let legacy = r#"{"id":"s","harness":"h","cwd":"/","state":"idle","interrupted":false,"closed":false,"createdAt":1,"lastEventAt":1}"#;
        let meta: SessionMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.title, "");
    }

    #[test]
    fn config_shapes_roundtrip() {
        let cmd = QuickCommand {
            id: "q1".into(),
            name: "Commit & Push".into(),
            prompt: "提交并推送".into(),
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: QuickCommand = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "Commit & Push");

        let skill = SkillEntry {
            id: "k1".into(),
            name: "web".into(),
            description: "https://github.com/x/web".into(),
        };
        let back: SkillEntry =
            serde_json::from_str(&serde_json::to_string(&skill).unwrap()).unwrap();
        assert_eq!(back.description, "https://github.com/x/web");

        let tpl = WorkflowTemplate {
            id: "t1".into(),
            name: "实现并审查".into(),
            description: "用 codex 实现，claude 审查".into(),
        };
        let back: WorkflowTemplate =
            serde_json::from_str(&serde_json::to_string(&tpl).unwrap()).unwrap();
        assert_eq!(back.name, "实现并审查");

        let orch = OrchestratorConfig::default();
        let s = serde_json::to_string(&orch).unwrap();
        assert!(s.contains("\"apiFormat\":\"chat_completions\""));
        let back: OrchestratorConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.model, orch.model);
        // 默认配置（全部留空）视为未配置；Base URL / API key / 模型都填上才视为已配置
        assert!(!OrchestratorConfig::default().is_configured());
        let cfg = OrchestratorConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "some-model".into(),
            ..OrchestratorConfig::default()
        };
        assert!(cfg.is_configured());
        let blank = OrchestratorConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "  ".into(),
            ..OrchestratorConfig::default()
        };
        assert!(!blank.is_configured(), "空白模型不算已配置");
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
        let res = GitDiffResult {
            files: vec![file],
            not_repo: false,
        };
        let s = serde_json::to_string(&res).unwrap();
        let back: GitDiffResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].hunks[0].header, "@@ -1,2 +1,3 @@");
        assert!(!back.not_repo);
    }

    #[test]
    fn new_method_params_deserialize() {
        let p: SetSessionTitleParams =
            serde_json::from_str(r#"{"sessionId":"s1","title":"t"}"#).unwrap();
        assert_eq!(p.title, "t");
        let p: SetDefaultModelParams = serde_json::from_str(r#"{"harness":"codex"}"#).unwrap();
        assert_eq!(p.model, None);
        let p: SetDefaultModelParams =
            serde_json::from_str(r#"{"harness":"codex","model":"gpt-4o"}"#).unwrap();
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
        let p: ListAgentSkillsParams = serde_json::from_str(r#"{"harness":"codex"}"#).unwrap();
        assert_eq!(p.harness, "codex");
    }

    /// 会话列表惰性分页参数（docs/DESIGN.md §3.2 / PRD §4.1.1）：
    /// 缺省（首次取最近活跃一窗）、limit、before 游标均可解析。
    #[test]
    fn list_sessions_params_deserialize() {
        let p: ListSessionsParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.limit, None);
        assert_eq!(p.before, None);

        let p: ListSessionsParams = serde_json::from_str(r#"{"limit":20}"#).unwrap();
        assert_eq!(p.limit, Some(20));
        assert_eq!(p.before, None);

        let p: ListSessionsParams = serde_json::from_str(r#"{"before":1786512000000}"#).unwrap();
        assert_eq!(p.before, Some(1786512000000));
        assert_eq!(p.limit, None);

        let p: ListSessionsParams = serde_json::from_str(r#"{"limit":10,"before":100}"#).unwrap();
        assert_eq!(p.limit, Some(10));
        assert_eq!(p.before, Some(100));
    }

    /// 分页结果序列化为 camelCase（`hasMore`/`nextBefore`，与 SessionMeta 及
    /// GUI 读取约定一致）；open_session 结果同约定。
    #[test]
    fn paging_results_serialize_camel_case() {
        let res = SessionsResult {
            sessions: Vec::new(),
            has_more: true,
            next_before: Some(42),
        };
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("\"hasMore\":true"), "{s}");
        assert!(s.contains("\"nextBefore\":42"), "{s}");

        let open = OpenSessionResult {
            events: Vec::new(),
            has_more: true,
            next_before: 7,
        };
        let s = serde_json::to_string(&open).unwrap();
        assert!(s.contains("\"hasMore\":true"), "{s}");
        assert!(s.contains("\"nextBefore\":7"), "{s}");
    }
}
