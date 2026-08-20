//! 业务类型：agent/会话/对话内容/活动/workspace/应用侧配置形状。
//! 语义依据 docs/DESIGN.md（「Client-Server 通信」协议、普通会话存储、应用各存储）。

use serde::{Deserialize, Serialize};

// ---- 应用侧本地配置形状（协议面单一来源）----

/// 注册机器（docs/DESIGN.md「注册机器存储」）：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineConfig {
    pub name: String,
    /// ws://host:port
    pub url: String,
    pub token: String,
}

/// 技能条目（docs/DESIGN.md「技能存储」）：name 唯一，只存描述。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
}

/// 工作流模板（docs/DESIGN.md「工作流模板存储」）：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowTemplate {
    pub name: String,
    pub plan: String,
}

/// 常用工作目录条目（docs/DESIGN.md「常用工作目录存储」）：(machine, workspace) 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecentWorkspace {
    pub machine: String,
    pub workspace: String,
    pub last_used: u64,
}

/// 快捷指令（docs/DESIGN.md「快捷指令存储」）：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickCommand {
    pub name: String,
    pub prompt: String,
}

/// 内置编排 agent 的 API 配置（docs/DESIGN.md「编排智能体配置存储」）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
        OrchestratorConfig {
            api_format: "chat_completions".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl OrchestratorConfig {
    /// 编排 agent 是否已配置可用：Base URL、API key、模型均非空。
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
    }
}

// ---- 认证（docs/DESIGN.md「认证」）----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthParams {
    pub token: String,
}

// ---- agent（docs/DESIGN.md「agent.list」等）----

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

/// `agent.skills` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkillsResult {
    pub skills: Vec<String>,
}

/// `agent.restart` / `workspace.restore` 通用操作结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---- 会话（docs/DESIGN.md「普通会话存储」「session.*」）----

/// 会话状态：空闲或工作中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Busy,
}

/// 普通会话元数据（docs/DESIGN.md「普通会话存储·元数据」）。
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
}

/// `session.new` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    pub agent: String,
    pub cwd: String,
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

/// `session.configure` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigureParams {
    pub session_id: String,
    pub title: String,
}

/// `session.list` 惰性分页参数。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default)]
    pub limit: Option<usize>,
    /// 独占上界游标：只返回 `last_active_at < before` 的更早一窗
    #[serde(default)]
    pub before: Option<u64>,
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
    pub before: Option<usize>,
}

/// `session.new` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: SessionMeta,
}

/// `session.list` 结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
    pub has_more: bool,
    pub next_before: Option<u64>,
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

/// 会话状态变更通知负载（docs/DESIGN.md 唯一主动推送 `session.state_change`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateChange {
    pub session_id: String,
    pub old_state: SessionState,
    pub new_state: SessionState,
}

// ---- 对话内容（docs/DESIGN.md prompt 输入；普通会话存储·对话历史）----

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

/// 对话历史条目（docs/DESIGN.md「普通会话存储·对话历史」：仅用户输入与 agent 输出，合并后写入）。
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
    pub next_before: usize,
}

// ---- 活动（docs/DESIGN.md「普通会话存储·活动历史」「session.activities」）----

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
    pub next_before: usize,
}

/// `session.ongoing_activity` 结果（进行中的活动；无则 None）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OngoingActivityResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
}

// ---- workspace（docs/DESIGN.md「workspace.diff」「workspace.restore」）----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
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

/// `workspace.restore` 参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRestoreParams {
    pub cwd: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
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

    /// auth 参数与 session 各方法参数可解析。
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
        assert_eq!(cfg.title, "实现登录");
        let p: SessionListParams = serde_json::from_str(r#"{"limit":10,"before":5}"#).unwrap();
        assert_eq!(p.limit, Some(10));
        assert_eq!(p.before, Some(5));
        let page: SessionPageParams = serde_json::from_str(r#"{"sessionId":"s1"}"#).unwrap();
        assert_eq!(page.session_id, "s1");
        assert_eq!(page.before, None);
    }

    /// 会话列表与分页结果序列化为 camelCase。
    #[test]
    fn session_list_result_serialize_camel_case() {
        let res = SessionListResult {
            sessions: Vec::new(),
            has_more: true,
            next_before: Some(42),
        };
        let s = serde_json::to_string(&res).unwrap();
        assert!(s.contains("\"hasMore\":true"), "{s}");
        assert!(s.contains("\"nextBefore\":42"), "{s}");
        assert!(s.contains("\"sessions\":[]"), "{s}");
    }

    /// session.state_change 通知负载序列化。
    #[test]
    fn state_change_payload_serializes() {
        let n = SessionStateChange {
            session_id: "s1".into(),
            old_state: SessionState::Busy,
            new_state: SessionState::Idle,
        };
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains("\"sessionId\":\"s1\""), "{s}");
        assert!(s.contains("\"newState\":\"idle\""), "{s}");
    }

    /// 配置形状往返：MachineConfig/SkillEntry/WorkflowTemplate/QuickCommand/RecentWorkspace/OrchestratorConfig。
    #[test]
    fn config_shapes_roundtrip() {
        let m = MachineConfig {
            name: "localpc".into(),
            url: "ws://127.0.0.1:34567".into(),
            token: "t".into(),
        };
        let back: MachineConfig =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.name, "localpc");

        let w = RecentWorkspace {
            machine: "localpc".into(),
            workspace: "/home/x".into(),
            last_used: 1,
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(s.contains("\"lastUsed\":1"), "{s}");
        let back: RecentWorkspace = serde_json::from_str(&s).unwrap();
        assert_eq!(back.workspace, "/home/x");

        let t = WorkflowTemplate {
            name: "审查".into(),
            plan: "用 codex 实现，claude 审查".into(),
        };
        let back: WorkflowTemplate =
            serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back.plan, "用 codex 实现，claude 审查");

        let q = QuickCommand {
            name: "Commit & Push".into(),
            prompt: "提交并推送".into(),
        };
        let back: QuickCommand = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back.name, "Commit & Push");

        let orch = OrchestratorConfig {
            api_key: "k".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "m".into(),
            ..OrchestratorConfig::default()
        };
        assert!(orch.is_configured());
        let blank = OrchestratorConfig {
            model: "  ".into(),
            ..orch.clone()
        };
        assert!(!blank.is_configured(), "空白模型不算已配置");
    }

    /// Git diff 类型往返。
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
