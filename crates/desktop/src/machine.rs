//! 机器连接视图模型：MachineStatus 状态机与每机器的 UI 聚合状态（MachineView）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::AppContext as _;
use protocol::{AgentInfo, SessionMeta};

use crate::aggregate::SessionView;
use crate::config::{machine_ws_url, MachineConfig};
use crate::ws::WsClient;

static NEXT_CONNECTION_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_connection_generation() -> u64 {
    NEXT_CONNECTION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// 机器连接状态：强类型状态机。曾用中文字符串
/// 前缀匹配充当状态机——任何文案改动都会静默破坏在线判断。
#[derive(Debug, Clone, PartialEq)]
pub enum MachineStatus {
    /// WS 建连/认证进行中（初始态）
    Connecting,
    /// 认证通过、可用
    Online,
    /// 认证被拒（token 错误；等待手动重连）
    AuthFailed(String),
    /// 曾在线后断开或连不上。WS 层不做自动重连，等待用户在设置里手动重连
    Offline,
}

impl MachineStatus {
    pub fn label(&self) -> String {
        match self {
            MachineStatus::Connecting => "连接中…".into(),
            MachineStatus::Online => "已连接".into(),
            MachineStatus::AuthFailed(e) => format!("认证失败（{e}）"),
            MachineStatus::Offline => "离线".into(),
        }
    }

    pub fn online(&self) -> bool {
        matches!(self, MachineStatus::Online)
    }
}

pub struct MachineView {
    pub(crate) config: MachineConfig,
    pub(crate) client: WsClient,
    pub status: MachineStatus,
    pub(crate) notice: Option<String>,
    pub(crate) agents: Vec<AgentInfo>,
    pub sessions: Vec<SessionMeta>,
    pub(crate) sessions_has_more: bool,
    /// 当前列表窗口内、批量 `session.info` 未返回的工作流关联会话。
    /// 这些会话仍需在工作流下展示，但不能作为可操作的普通会话打开。
    pub(crate) unavailable_workflow_sessions: HashSet<String>,
    /// 每机器独立的改动审查状态（含陈旧响应防乱的 request_id）
    pub diff: gpui::Entity<crate::diff_review::DiffReviewState>,
    /// 本连接的代次；重连后递增，所有异步回调必须匹配该代次才能回写。
    pub(crate) connection_generation: u64,
    /// 会话列表请求序号，用于丢弃乱序响应。
    pub(crate) sessions_request_id: u64,
    pub views: std::collections::HashMap<String, SessionView>,
    pub(crate) workspace_directories: HashMap<String, WorkspaceDirectory>,
    pub(crate) workspace_expanded: HashSet<String>,
    pub(crate) workspace_tree_collapsed: bool,
    pub(crate) workspace_loading: HashMap<String, u64>,
    pub(crate) workspace_list_request_id: u64,
    pub(crate) workspace_read_request_id: u64,
    pub(crate) workspace_file: Option<String>,
    pub(crate) workspace_content: String,
    pub(crate) workspace_error: Option<String>,
    pub(crate) workspace_read_loading: bool,
    pub(crate) workspace_read_has_more: bool,
    pub(crate) workspace_read_next_offset: usize,
    /// 本连接打开的终端；连接断开时一并清理。
    pub(crate) terminals: Vec<crate::terminal::TerminalEntry>,
    pub(crate) active_terminal: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct WorkspaceDirectory {
    pub(crate) entries: Vec<protocol::WorkspaceEntry>,
    pub(crate) has_more: bool,
    pub(crate) next_offset: usize,
}

impl MachineView {
    pub fn new(config: MachineConfig, cx: &mut gpui::App) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            diff: cx.new(|_| crate::diff_review::DiffReviewState::default()),
            client: WsClient::connect_with_token(url, config.token.clone()),
            config,
            status: MachineStatus::Connecting,
            notice: None,
            agents: Vec::new(),
            sessions: Vec::new(),
            sessions_has_more: false,
            unavailable_workflow_sessions: HashSet::new(),
            connection_generation: next_connection_generation(),
            sessions_request_id: 0,
            views: std::collections::HashMap::new(),
            workspace_directories: HashMap::new(),
            workspace_expanded: HashSet::new(),
            workspace_tree_collapsed: false,
            workspace_loading: HashMap::new(),
            workspace_list_request_id: 0,
            workspace_read_request_id: 0,
            workspace_file: None,
            workspace_content: String::new(),
            workspace_error: None,
            workspace_read_loading: false,
            workspace_read_has_more: false,
            workspace_read_next_offset: 0,
            terminals: Vec::new(),
            active_terminal: None,
        }
    }
}
