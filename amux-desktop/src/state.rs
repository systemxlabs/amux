//! 应用状态：连接、会话列表、当前会话视图、设置面板与轮询节拍。

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use amux_common::api::*;
use amux_common::domain::{
    Activity, ContentBlock, FsEntry, GitDiffResult, HistoryItem, SessionConfigOption,
    SessionPlanEntry, SlashCommand,
};

use crate::client::Client;
use crate::config::Connection;

/// 各视图的刷新周期（docs/DESIGN.md「应用」）。
pub const SESSION_LIST_INTERVAL: Duration = Duration::from_secs(10);
pub const HISTORY_INTERVAL: Duration = Duration::from_secs(5);
pub const ONGOING_INTERVAL: Duration = Duration::from_secs(2);
pub const ACTIVITIES_INTERVAL: Duration = Duration::from_secs(10);
pub const PLAN_INTERVAL: Duration = Duration::from_secs(10);
/// 会话选项与斜杠命令：无独立视图，交互视图常驻需要（输入框下方的选项控件与斜杠补全）；
/// 两者由 agent 侧异步推送，取与对话视图相同的周期。
pub const OPTIONS_INTERVAL: Duration = Duration::from_secs(5);
pub const TERMINAL_INTERVAL: Duration = Duration::from_millis(500);

/// 连接状态（决定进入登录页面还是主页面）。
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Unconfigured,
    Connecting,
    Online,
    Failed(String),
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

/// 当前视图：决定「打开时实时获取」该拉取哪一份配置数据（docs/DESIGN.md「应用」各视图小节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewKey {
    /// 新建会话视图
    NewSession,
    /// 会话交互视图
    Interaction(OpenTarget),
    /// 设置浮窗的某个分类
    Settings(SettingsTab),
}

/// 工作目录树节点：目录项不缓存，展开时才拉取子节点
/// （`children` 为 `None` 表示当前无内容，展开时需实时拉取）。
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

/// 终端输出缓冲：按游标增量累积。
///
/// 切换终端、服务端丢弃旧输出、乃至整个会话视图重建时缓冲会整体重建，
/// `generation` 随之取一个新的全局序号，供本地 VT 网格判断是否需要重建
/// （重建一律换新序号，避免「新缓冲恰好回到旧序号」被误判为增量）。
#[derive(Clone)]
pub struct TerminalBuffer {
    bytes: Vec<u8>,
    generation: u64,
}

impl Default for TerminalBuffer {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            generation: next_terminal_generation(),
        }
    }
}

/// 终端缓冲代际序号：全局单调递增，每次缓冲重建取一个新值。
pub fn next_terminal_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl TerminalBuffer {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 追加增量输出。
    pub fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// 整体重建（切换终端、服务端已丢弃旧输出）。
    pub fn reset(&mut self) {
        self.bytes.clear();
        self.generation = next_terminal_generation();
    }
}

/// 列表分页：窗口贴着「最新」一端，随滚动向更早方向按页扩展
/// （docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」「活动列表滚动机制」）。
#[derive(Debug, Clone, PartialEq)]
pub struct Paging {
    /// 页大小：面板可视高度能容纳的条目数，由视图按滚动句柄计算并写入
    pub page_size: usize,
    /// 更早一端是否还有服务端条目
    pub has_older: bool,
    /// 是否有在途的更早一页拉取（避免同一页重复拉取）
    pub loading_older: bool,
    /// 更早一页插入后在窗口头部产生的位移（条目数）：视图据此把原首条目保持在原位置
    pub shift: Option<usize>,
}

impl Default for Paging {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            has_older: false,
            loading_older: false,
            shift: None,
        }
    }
}

/// 默认页大小（尚未布局、拿不到可视高度时使用）。
pub const DEFAULT_PAGE_SIZE: usize = 20;
/// 页大小上限：面板很高时不至于一次拉取过多。
pub const MAX_PAGE_SIZE: usize = 200;

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
    /// 对话历史分页（窗口为最新的若干条）
    pub history_paging: Paging,
    /// 活动历史分页（窗口为最新的若干条）
    pub activities_paging: Paging,
    /// 终端输出字节（按游标增量累积；truncated 时整体替换）
    pub terminal_output: TerminalBuffer,
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
    /// 会话交互视图标题：`agent@机器` 或编排智能体（docs/PRD.md 会话交互视图）。
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
    /// 当前联想请求的目录（实时拉取，不做缓存；应答回来时目录已变则丢弃）
    pub suggestion_dir: Option<String>,
    /// 当前联想请求的前缀（应答回来时按最新前缀过滤）
    pub suggestion_prefix: String,
}

/// 目录条目中名称以 `prefix` 开头的项（工作目录联想项；服务端已只返回目录）。
pub fn matching_prefix(entries: Vec<FsEntry>, prefix: &str) -> Vec<FsEntry> {
    entries
        .into_iter()
        .filter(|entry| entry.name.starts_with(prefix))
        .collect()
}

/// 待发送附件：拖拽或粘贴得到的文件/图片，随消息一并作为内容块发送。
#[derive(Debug, Clone)]
pub struct Attachment {
    pub block: ContentBlock,
    /// 输入区展示用的短标签
    pub label: String,
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

    /// 当前会话类型下可用的面板：工作目录/文件改动/会话计划/终端仅普通会话展示（docs/PRD.md）。
    pub fn for_session(is_workflow: bool) -> Vec<SidePanel> {
        SidePanel::ALL
            .into_iter()
            .filter(|panel| panel.is_available(is_workflow))
            .collect()
    }

    pub fn is_available(self, is_workflow: bool) -> bool {
        !is_workflow || matches!(self, SidePanel::Detail | SidePanel::Activities)
    }

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

    /// 悬浮按钮栏上的短标签。
    pub fn short_label(self) -> &'static str {
        match self {
            SidePanel::Workspace => "目录",
            SidePanel::Diff => "改动",
            SidePanel::Detail => "详情",
            SidePanel::Activities => "活动",
            SidePanel::Plan => "计划",
            SidePanel::Terminal => "终端",
        }
    }

    /// 面板默认宽度（逻辑像素）；改动与终端需要横向空间，故更宽。
    pub fn default_width(self) -> f32 {
        match self {
            SidePanel::Workspace => 520.0,
            SidePanel::Diff => 560.0,
            SidePanel::Detail => 360.0,
            SidePanel::Activities => 400.0,
            SidePanel::Plan => 360.0,
            SidePanel::Terminal => 560.0,
        }
    }
}

/// 提示级别（决定通知颜色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Success,
    Warning,
    Error,
}

/// 后台任务排队的一条提示；由节拍在有窗口时投递为通知。
#[derive(Debug, Clone)]
pub struct Note {
    pub message: String,
    pub level: NoteLevel,
}

/// 后台任务排队的一条结果弹窗（保存成功/失败等需要用户确认的反馈）；
/// 由节拍在有窗口时开出弹窗。
#[derive(Debug, Clone)]
pub struct Alert {
    pub title: String,
    pub message: String,
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
            SettingsTab::Skills => "技能管理",
            SettingsTab::WorkflowPlans => "工作流计划",
        }
    }

    /// 分类导航图标。
    pub fn icon(self) -> gpui_component::IconName {
        use gpui_component::IconName;
        match self {
            SettingsTab::Connection => IconName::Globe,
            SettingsTab::Machines => IconName::HardDrive,
            SettingsTab::Orchestrator => IconName::Bot,
            SettingsTab::QuickCommands => IconName::Play,
            SettingsTab::Skills => IconName::BookOpen,
            SettingsTab::WorkflowPlans => IconName::File,
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
    /// 已拉取过打开时数据的视图；与当前视图不同才重新拉取，不做定时刷新
    pub loaded_view: Option<ViewKey>,
    /// 待投递的提示（后台任务无窗口，只能排队等节拍投递）
    pub notes: VecDeque<Note>,
    /// 待投递的结果弹窗（同上）
    pub alerts: VecDeque<Alert>,
    /// 会话列表分页：每页从普通会话与工作流会话各拉取的条目数
    pub list_loaded: usize,
    /// 会话列表分页状态
    pub list_paging: Paging,
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
    pub options: Option<Instant>,
    pub terminal: Option<Instant>,
    pub terminal_cursor: u64,
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
            loaded_view: None,
            notes: VecDeque::new(),
            alerts: VecDeque::new(),
            list_loaded: DEFAULT_PAGE_SIZE,
            list_paging: Paging::default(),
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

    /// 当前视图：设置浮窗优先，其次是打开的会话，否则为新建会话视图。
    pub fn view_key(&self) -> ViewKey {
        if self.settings_open {
            return ViewKey::Settings(self.settings_tab);
        }
        match &self.open {
            Some(target) => ViewKey::Interaction(target.clone()),
            None => ViewKey::NewSession,
        }
    }

    /// 当前打开的是否为工作流会话。
    /// 按 id 解析列表条目：工作流会话内关联的普通会话也是列表里的一行。
    pub fn entry(&self, id: &str) -> Option<ListEntry> {
        self.entries
            .iter()
            .find(|entry| entry.id() == id)
            .cloned()
            .or_else(|| {
                self.entries.iter().find_map(|entry| match entry {
                    ListEntry::Workflow(workflow) => workflow
                        .linked_sessions
                        .iter()
                        .find(|session| session.id == id)
                        .cloned()
                        .map(ListEntry::Session),
                    ListEntry::Session(_) => None,
                })
            })
    }

    pub fn is_workflow(&self) -> bool {
        matches!(self.open, Some(OpenTarget::Workflow(_)))
    }

    /// 排队一条成功提示。
    pub fn success(&mut self, message: impl Into<String>) {
        self.notes.push_back(Note {
            message: message.into(),
            level: NoteLevel::Success,
        });
    }

    /// 排队一条校验类警示。
    pub fn warning(&mut self, message: impl Into<String>) {
        self.notes.push_back(Note {
            message: message.into(),
            level: NoteLevel::Warning,
        });
    }

    /// 排队一条错误提示。
    pub fn error(&mut self, message: impl Into<String>) {
        self.notes.push_back(Note {
            message: message.into(),
            level: NoteLevel::Error,
        });
    }

    /// 排队一条结果弹窗。
    pub fn alert(&mut self, title: impl Into<String>, message: impl Into<String>) {
        self.alerts.push_back(Alert {
            title: title.into(),
            message: message.into(),
        });
    }
}

/// 共享句柄：UI 线程与后台轮询任务共享的状态。
pub type SharedCore = Arc<parking_lot::Mutex<Core>>;
