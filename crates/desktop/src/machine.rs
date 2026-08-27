//! 机器连接视图模型：MachineStatus 状态机与每机器的 UI 聚合状态（MachineView）。

use std::collections::{HashMap, HashSet};

use protocol::{AgentInfo, SessionMeta};

use crate::aggregate::SessionView;
use crate::config::{machine_ws_url, MachineConfig};
use crate::ws::WsClient;

use protocol::GitDiffFile;

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

pub(crate) struct MachineView {
    pub(crate) config: MachineConfig,
    pub(crate) client: WsClient,
    pub(crate) connection_epoch: u64,
    pub(crate) status: MachineStatus,
    pub(crate) notice: Option<String>,
    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) sessions: Vec<SessionMeta>,
    pub(crate) sessions_has_more: bool,
    pub(crate) sessions_next_before: Option<String>,
    pub(crate) views: std::collections::HashMap<String, SessionView>,
    pub(crate) diff_files: Vec<GitDiffFile>,
    pub(crate) diff_not_repo: bool,
    pub(crate) diff_selection: HashSet<(String, Option<usize>)>,
    pub(crate) workspace_directories: HashMap<String, WorkspaceDirectory>,
    pub(crate) workspace_expanded: HashSet<String>,
    pub(crate) workspace_loading: HashSet<String>,
    pub(crate) workspace_list_request_id: u64,
    pub(crate) workspace_read_request_id: u64,
    pub(crate) workspace_file: Option<String>,
    pub(crate) workspace_content: String,
    pub(crate) workspace_error: Option<String>,
    pub(crate) workspace_read_loading: bool,
    pub(crate) workspace_read_has_more: bool,
    pub(crate) workspace_read_next_offset: usize,
    pub(crate) diff_request_id: u64,
    pub(crate) diff_loading: bool,
    pub(crate) diff_error: Option<String>,
    pub(crate) diff_tree_collapsed: bool,
    pub(crate) diff_changes_collapsed: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct WorkspaceDirectory {
    pub(crate) entries: Vec<protocol::WorkspaceEntry>,
    pub(crate) has_more: bool,
    pub(crate) next_offset: usize,
}

impl MachineView {
    pub(crate) fn new(config: MachineConfig) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            client: WsClient::connect_with_token(url, config.token.clone()),
            config,
            connection_epoch: 1,
            status: MachineStatus::Connecting,
            notice: None,
            agents: Vec::new(),
            sessions: Vec::new(),
            sessions_has_more: false,
            sessions_next_before: None,
            views: std::collections::HashMap::new(),
            diff_files: Vec::new(),
            diff_not_repo: false,
            diff_selection: HashSet::new(),
            workspace_directories: HashMap::new(),
            workspace_expanded: HashSet::new(),
            workspace_loading: HashSet::new(),
            workspace_list_request_id: 0,
            workspace_read_request_id: 0,
            workspace_file: None,
            workspace_content: String::new(),
            workspace_error: None,
            workspace_read_loading: false,
            workspace_read_has_more: false,
            workspace_read_next_offset: 0,
            diff_request_id: 0,
            diff_loading: false,
            diff_error: None,
            diff_tree_collapsed: false,
            diff_changes_collapsed: false,
        }
    }
}
