//! 应用状态：连接、会话列表、当前会话视图、设置面板与轮询节拍。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use amux_common::api::*;
use amux_common::domain::{
    Activity, FsEntry, GitDiffResult, HistoryItem, SessionConfigOption, SessionPlanEntry,
    SlashCommand,
};

use crate::client::Client;
use crate::config::Connection;

/// 各视图的刷新周期（docs/DESIGN.md「应用」）。
pub const SESSION_LIST_INTERVAL: Duration = Duration::from_secs(10);
pub const HISTORY_INTERVAL: Duration = Duration::from_secs(5);
pub const ONGOING_INTERVAL: Duration = Duration::from_secs(2);
pub const ACTIVITIES_INTERVAL: Duration = Duration::from_secs(10);
pub const PLAN_INTERVAL: Duration = Duration::from_secs(10);
pub const TERMINAL_INTERVAL: Duration = Duration::from_millis(500);
/// 机器/agent 列表与编排智能体配置：新建会话视图常驻需要，按设置浮窗的周期刷新。
pub const SETTINGS_INTERVAL: Duration = Duration::from_secs(10);

/// 连接状态（设置面板展示）。
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Unconfigured,
    Connecting,
    Online,
    Failed(String),
}

impl ConnectionStatus {
    pub fn label(&self) -> String {
        match self {
            ConnectionStatus::Unconfigured => "未配置".to_string(),
            ConnectionStatus::Connecting => "连接中…".to_string(),
            ConnectionStatus::Online => "已连接".to_string(),
            ConnectionStatus::Failed(error) => format!("连接失败：{error}"),
        }
    }
}

/// 会话列表条目：普通会话或工作流会话（含其关联普通会话）。
#[derive(Debug, Clone)]
pub enum ListEntry {
    Session(Session),
    Workflow(Workflow),
}

impl ListEntry {
    pub fn id(&self) -> &str {
        match self {
            ListEntry::Session(session) => &session.id,
            ListEntry::Workflow(workflow) => &workflow.id,
        }
    }

    pub fn updated_at(&self) -> u64 {
        match self {
            ListEntry::Session(session) => session.updated_at,
            ListEntry::Workflow(workflow) => workflow.updated_at,
        }
    }

    pub fn title(&self) -> String {
        match self {
            ListEntry::Session(session) => session.title.clone(),
            ListEntry::Workflow(workflow) => workflow.title.clone(),
        }
    }
}

/// 当前打开的会话（普通或工作流）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    Session(String),
    Workflow(String),
}

/// 工作目录树节点：目录的子节点按需加载（`children` 为 `None` 表示尚未加载）。
#[derive(Debug, Clone)]
pub struct WorkspaceNode {
    pub entry: FsEntry,
    pub expanded: bool,
    pub children: Option<Vec<WorkspaceNode>>,
}

impl WorkspaceNode {
    pub fn new(entry: FsEntry) -> Self {
        Self {
            entry,
            expanded: false,
            children: None,
        }
    }

    /// 按绝对路径查找节点，用于把异步加载结果回填到树上。
    pub fn find_mut<'a>(nodes: &'a mut [WorkspaceNode], path: &str) -> Option<&'a mut Self> {
        for node in nodes {
            if node.entry.path == path {
                return Some(node);
            }
            if let Some(children) = node.children.as_mut() {
                if let Some(found) = Self::find_mut(children, path) {
                    return Some(found);
                }
            }
        }
        None
    }
}

/// 打开的会话明细视图数据（普通会话与工作流会话共用）。
#[derive(Default, Clone)]
pub struct SessionView {
    pub history: Vec<HistoryItem>,
    pub activities: Vec<Activity>,
    pub plan: Vec<SessionPlanEntry>,
    pub config_options: Vec<SessionConfigOption>,
    pub slash_commands: Vec<SlashCommand>,
    pub ongoing: Option<Activity>,
    pub context_size: u64,
    pub context_window_size: u64,
    pub diff: Option<GitDiffResult>,
    pub terminals: Vec<Terminal>,
    pub active_terminal: Option<String>,
    pub history_has_more: bool,
    /// 终端输出字节（按游标增量累积；truncated 时整体替换）
    pub terminal_output: Vec<u8>,
    /// 工作目录树的根节点（懒加载子目录）
    pub workspace_tree: Vec<WorkspaceNode>,
    /// 最近查看的文件内容
    pub file_content: Option<String>,
}

/// 打开的会话视图数据（普通会话或工作流会话）。
#[derive(Default, Clone)]
pub struct OpenView {
    pub session: Option<Session>,
    pub workflow: Option<Workflow>,
    pub detail: SessionView,
}

impl OpenView {
    pub fn title(&self) -> String {
        if let Some(session) = &self.session {
            session.title.clone()
        } else if let Some(workflow) = &self.workflow {
            workflow.title.clone()
        } else {
            String::new()
        }
    }

    pub fn subtitle(&self) -> String {
        if let Some(session) = &self.session {
            format!("{}@{}", session.agent, session.machine)
        } else {
            "编排智能体".to_string()
        }
    }
}

/// 新建会话视图状态。
#[derive(Default, Clone)]
pub struct NewSessionForm {
    pub workflow_mode: bool,
    pub machine: Option<String>,
    pub agent: Option<String>,
    pub use_worktree: bool,
    /// 工作目录输入框的前缀匹配目录项
    pub suggestions: Vec<FsEntry>,
    /// 前缀联想的目录缓存：同一目录只拉取一次，前缀变化时在本地过滤
    pub suggestion_cache: Option<DirectoryCache>,
}

/// 已拉取的目录条目，用于工作目录输入框的本地前缀过滤。
#[derive(Debug, Clone)]
pub struct DirectoryCache {
    pub machine: String,
    pub dir: String,
    pub entries: Vec<FsEntry>,
}

impl DirectoryCache {
    /// 目录中名称以 `prefix` 开头的子目录。
    pub fn matching(&self, prefix: &str) -> Vec<FsEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.is_dir && entry.name.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// 右侧面板分类（PRD 右侧面板）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidePanel {
    Workspace,
    Diff,
    Detail,
    Activities,
    Plan,
    Terminal,
}

impl SidePanel {
    pub const ALL: [SidePanel; 6] = [
        SidePanel::Workspace,
        SidePanel::Diff,
        SidePanel::Detail,
        SidePanel::Activities,
        SidePanel::Plan,
        SidePanel::Terminal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SidePanel::Workspace => "工作目录",
            SidePanel::Diff => "文件改动",
            SidePanel::Detail => "会话详情",
            SidePanel::Activities => "会话活动",
            SidePanel::Plan => "会话计划",
            SidePanel::Terminal => "终端",
        }
    }
}

/// 设置面板分类（PRD 设置页面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Connection,
    Machines,
    Orchestrator,
    QuickCommands,
    Skills,
    WorkflowPlans,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 6] = [
        SettingsTab::Connection,
        SettingsTab::Machines,
        SettingsTab::Orchestrator,
        SettingsTab::QuickCommands,
        SettingsTab::Skills,
        SettingsTab::WorkflowPlans,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Connection => "连接设置",
            SettingsTab::Machines => "机器管理",
            SettingsTab::Orchestrator => "编排智能体",
            SettingsTab::QuickCommands => "快捷指令",
            SettingsTab::Skills => "技能",
            SettingsTab::WorkflowPlans => "工作流计划",
        }
    }
}

/// 设置面板数据（按需加载）。
#[derive(Default, Clone)]
pub struct SettingsData {
    pub machines: Vec<Machine>,
    pub agents: Vec<(String, Vec<Agent>)>,
    pub skills: Vec<Skill>,
    pub plans: Vec<WorkflowPlanItem>,
    pub quick_commands: Vec<QuickCommand>,
    pub orchestrator: Option<OrchestratorConfig>,
}

/// 非 UI 状态：连接、客户端与缓存。
#[derive(Clone)]
pub struct Core {
    pub connection: Connection,
    pub client: Option<Client>,
    pub status: ConnectionStatus,
    pub entries: Vec<ListEntry>,
    pub new_session: NewSessionForm,
    pub recent_workspaces: Vec<RecentWorkspace>,
    pub open: Option<OpenTarget>,
    pub view: OpenView,
    pub settings_open: bool,
    pub settings_tab: SettingsTab,
    pub settings: SettingsData,
    pub toast: Option<String>,
    pub list_limit: usize,
    /// 当前打开的右侧面板
    pub side_panel: Option<SidePanel>,
    /// 工作流会话展开的关联普通会话列表
    pub expanded_workflows: HashSet<String>,
    /// 各视图上次刷新时间（轮询节流）
    pub last: Ticks,
}

#[derive(Default, Clone, Copy)]
pub struct Ticks {
    pub list: Option<Instant>,
    pub history: Option<Instant>,
    pub ongoing: Option<Instant>,
    pub activities: Option<Instant>,
    pub plan: Option<Instant>,
    pub terminal: Option<Instant>,
    pub terminal_cursor: u64,
    pub settings: Option<Instant>,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            connection: Connection::default(),
            client: None,
            status: ConnectionStatus::Unconfigured,
            entries: Vec::new(),
            new_session: NewSessionForm::default(),
            recent_workspaces: Vec::new(),
            open: None,
            view: OpenView::default(),
            settings_open: false,
            settings_tab: SettingsTab::Connection,
            settings: SettingsData::default(),
            toast: None,
            list_limit: 20,
            side_panel: None,
            expanded_workflows: HashSet::new(),
            last: Ticks::default(),
        }
    }
}

impl Core {
    pub fn new(connection: Connection) -> Self {
        let mut core = Core::default();
        core.apply_connection(connection);
        core
    }

    /// 应用连接配置：重建客户端与状态。
    pub fn apply_connection(&mut self, connection: Connection) {
        self.status = if connection.is_configured() {
            ConnectionStatus::Connecting
        } else {
            ConnectionStatus::Unconfigured
        };
        self.client = connection.is_configured().then(|| Client::new(&connection));
        self.connection = connection;
    }

    pub fn due(&self, last: Option<Instant>, interval: Duration) -> bool {
        last.map(|at| at.elapsed() >= interval).unwrap_or(true)
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.toast = Some(message.into());
    }
}

/// 共享句柄：UI 线程与后台轮询任务共享的状态。
pub type SharedCore = Arc<parking_lot::Mutex<Core>>;
