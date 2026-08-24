//! amux 主视图（docs/DESIGN.md §7 / PRD §「桌面 GUI 设计」）。
//! 三栏布局（左侧边栏 / 中间交互 / 右侧 Panel）+ 多机器 + 工作流编排 + 设置浮窗（五分类）。
//!
//! - 左侧：会话列表（普通会话 + 工作流会话统一按最近活跃排序；工作流挂载的关联普通会话
//!   默认折叠、可展开下钻）+ 顶部「+」新建会话入口 + 底部设置入口
//! - 中间：未选中会话显示新建会话视图（机器/Agent/工作目录/常用目录一屏并列、创建会话按钮）；
//!   选中会话显示对话历史气泡 + 实时活动条 + 快捷按钮栏 + 输入区（含取消）+ 右侧竖向悬浮按钮
//! - 右侧：代码审查 / 会话详情 / 会话活动，默认折叠，点悬浮按钮展开
//! - 设置浮窗：半透明遮罩 + 分类导航侧边栏（机器管理 / 编排智能体 / 快捷指令 / 技能管理 /
//!   工作流模板）+ 右侧内容；设置项为竖向堆叠卡片式，新增入口位于卡片区右上角，编辑/新增
//!   使用弹窗，删除/机器操作需要确认；skills 操作弹窗选择目标机器和 agent）
//!
//! 数据为拉取式：会话列表 `session.list` 定时 10s + 主动；对话 `session.history` 打开才刷 10s；
//! 活动 `session.activities` 打开才刷 10s；实时 `session.ongoing_activity` 5s；`session.state_change`
//! 通知用于工作流驱动。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    collapsible::Collapsible,
    dialog::DialogButtonProps,
    input::{Input, InputState},
    label::Label,
    notification::Notification as UiNotification,
    radio::RadioGroup,
    scroll::ScrollableElement,
    spinner::Spinner,
    text::{TextView, TextViewStyle},
    WindowExt, *,
};

use serde_json::json;

use protocol::{
    ActivitiesResult, Activity, AgentListResult, AgentParams, AgentSkillsResult,
    ContentBlock, GitChangeStatus, HistoryItem, HistoryResult, OpResult, OngoingActivityResult,
    SessionConfigureParams, SessionIdParams, SessionListResult, SessionMeta, SessionNewParams,
    SessionPageParams, SessionPromptParams, SessionResult, SessionState, SessionStateChange,
    WorkspaceDiffParams, WorkspaceDiffResult, WorkspaceListResult, WorkspaceReadParams,
    WorkspaceReadResult, WorkspaceRestoreParams,
};

use crate::config::{
    machine_ws_url, ApiFormat, ConfigStore, OrchestratorConfig, QuickCommand, SkillEntry,
    WorkflowTemplate,
};
use crate::diff::{diff_lines, DiffLineKind};
use crate::display::{activity_display, info_row, machine_status_badge, short_cwd};
use crate::machine::{MachineStatus, MachineView, WorkspaceDirectory};
use crate::logic::{
    compose_prompt, compose_workflow_text, external_path_attachment, merge_session_window,
    parse_at_references, path_attachment, read_path_context, DialogMsg, InputAttachment,
};
use crate::text::{block_text, one_line, truncate};
use crate::workflow::{now_ts, AgentSlot, MachineSummary, OrcBackend, RigBackend, WorkflowEngine};
use crate::ws::{Notification as WsNotification, WsClient};

/// 会话列表惰性分页窗口大小（PRD §4.1.1：首次只取最近活跃一窗）。
const PAGE_LIMIT: usize = 50;

/// 右侧面板（默认折叠，悬浮按钮展开）。
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Workspace,
    Diff,
    Detail,
    Activities,
}

/// 设置浮窗分类（PRD §4.3 五分类）。
#[derive(Clone, Copy, PartialEq)]
enum SettingsCategory {
    Machines,
    Orchestrator,
    QuickCommands,
    Skills,
    Templates,
}

/// 新会话创建模式（普通 / 工作流）。
#[derive(Clone, Copy, PartialEq)]
enum NewSessionMode {
    Direct,
    Workflow,
}

/// 当前选中的会话。
#[derive(Clone, PartialEq)]
enum Selected {
    Session { machine: usize, id: String },
    Workflow { engine: usize },
}

/// 右键菜单目标。
#[derive(Clone)]
enum ContextMenuTarget {
    Session { machine: usize, session_id: String },
    Workflow { engine: usize },
}

/// 右键弹出菜单。
struct SessionContextMenu {
    target: ContextMenuTarget,
    x: f32,
    y: f32,
}

#[derive(Clone, Copy)]
enum SkillAction {
    Install,
    Update,
    Uninstall,
}

impl SkillAction {
    fn prompt(self, skill: &SkillEntry) -> String {
        let operation = match self {
            SkillAction::Install => "安装",
            SkillAction::Update => "更新",
            SkillAction::Uninstall => "卸载",
        };
        format!(
            "请在当前环境{}技能「{}」。技能说明：{}。完成后报告实际执行结果。",
            operation, skill.name, skill.description
        )
    }

    fn label(self) -> &'static str {
        match self {
            SkillAction::Install => "安装",
            SkillAction::Update => "更新",
            SkillAction::Uninstall => "卸载",
        }
    }
}

pub struct AmuxApp {
    store: Arc<ConfigStore>,
    machines: Vec<MachineView>,
    workflows: Vec<WorkflowEngine>,
    session_dir: PathBuf,
    selected: Option<Selected>,
    panel: Option<Panel>,
    sidebar_width_px: f32,
    sidebar_resize_origin: Option<f32>,
    sidebar_resize_initial: f32,
    panel_delta_px: f32,
    panel_resize_origin: Option<f32>,
    panel_resize_initial: f32,
    show_settings: bool,
    show_add_machine_form: bool,
    machine_form_error: Option<String>,
    settings_category: SettingsCategory,
    new_session_mode: NewSessionMode,
    input_state: Entity<InputState>,
    input_attachments: Vec<InputAttachment>,
    session_cwd_input: Entity<InputState>,
    workflow_input: Entity<InputState>,
    machine_name_input: Entity<InputState>,
    machine_url_input: Entity<InputState>,
    machine_token_input: Entity<InputState>,
    qc_name_input: Entity<InputState>,
    qc_prompt_input: Entity<InputState>,
    skill_name_input: Entity<InputState>,
    skill_desc_input: Entity<InputState>,
    tpl_name_input: Entity<InputState>,
    tpl_desc_input: Entity<InputState>,
    orch_api_format: ApiFormat,
    orch_base_input: Entity<InputState>,
    orch_key_input: Entity<InputState>,
    orch_model_input: Entity<InputState>,
    orchestrator_form_error: Option<String>,
    orchestrator_form_status: Option<String>,
    settings_form_error: Option<String>,
    title_input: Entity<InputState>,
    qc_edit_target: Option<String>,
    skill_edit_target: Option<String>,
    tpl_edit_target: Option<String>,
    show_quick_command_form: bool,
    show_skill_form: bool,
    show_template_form: bool,
    skill_action_dialog: Option<(SkillEntry, SkillAction)>,
    context_menu: Option<SessionContextMenu>,
    renaming_session: Option<(usize, String)>,
    renaming_workflow: Option<usize>,
    new_session_machine: Option<usize>,
    new_session_agent: Option<String>,
    new_session_error: Option<String>,
    show_workspace_dropdown: bool,
    workflow_error: Option<String>,
    workflow_template: Option<WorkflowTemplate>,
    dialog_scroll: ScrollHandle,
    activities_scroll: ScrollHandle,
    diff_scroll: ScrollHandle,
    workflow_dialog_limit: usize,
    activities_limit: usize,
    expanded_activities: std::collections::HashSet<String>,
    expanded_workflows: std::collections::HashSet<usize>,
    _tasks: Vec<Task<()>>,
}

impl AmuxApp {
    pub fn new(store: Arc<ConfigStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入消息，Enter 发送；Ctrl+Enter 换行；@ 引用文件/目录作为上下文")
                .auto_grow(3, 8)
                .submit_on_enter(false)
        });
        let session_cwd_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("工作目录（如 ~/projects/api-server）")
        });
        let workflow_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("用自然语言描述完整执行计划（支持 @ 引用上下文）…")
                .multi_line(true)
        });
        let machine_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("名称，如 localpc"));
        let machine_url_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("连接地址 ws://host:port"));
        let machine_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Token")
                .masked(true)
        });
        let qc_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("指令名")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let qc_prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("提示词（发给 agent 的一段话）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let skill_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("名称")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let skill_desc_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("描述（仓库/资源 URL 或安装方法）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let tpl_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("模板名")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let tpl_desc_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("执行计划（自然语言描述）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let orch_base_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Base URL（如 https://api…/v1）"));
        let orch_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("API Key")
                .masked(true)
        });
        let orch_model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("模型名（如 gpt-4.1）"));
        let title_input = cx.new(|cx| InputState::new(window, cx));

        let mut app = AmuxApp {
            store,
            machines: Vec::new(),
            workflows: Vec::new(),
            session_dir: PathBuf::new(),
            selected: None,
            panel: None,
            sidebar_width_px: crate::theme::SIDEBAR_WIDTH * window.scale_factor(),
            sidebar_resize_origin: None,
            sidebar_resize_initial: crate::theme::SIDEBAR_WIDTH * window.scale_factor(),
            panel_delta_px: 0.0,
            panel_resize_origin: None,
            panel_resize_initial: 0.0,
            show_settings: false,
            show_add_machine_form: false,
            machine_form_error: None,
            settings_category: SettingsCategory::Machines,
            new_session_mode: NewSessionMode::Direct,
            input_state,
            input_attachments: Vec::new(),
            session_cwd_input,
            workflow_input,
            machine_name_input,
            machine_url_input,
            machine_token_input,
            qc_name_input,
            qc_prompt_input,
            skill_name_input,
            skill_desc_input,
            tpl_name_input,
            tpl_desc_input,
            orch_base_input,
            orch_key_input,
            orch_model_input,
            orch_api_format: ApiFormat::ChatCompletions,
            orchestrator_form_error: None,
            orchestrator_form_status: None,
            settings_form_error: None,
            title_input,
            qc_edit_target: None,
            skill_edit_target: None,
            tpl_edit_target: None,
            show_quick_command_form: false,
            show_skill_form: false,
            show_template_form: false,
            skill_action_dialog: None,
            context_menu: None,
            renaming_session: None,
            renaming_workflow: None,
            new_session_machine: None,
            new_session_agent: None,
            new_session_error: None,
            show_workspace_dropdown: false,
            workflow_error: None,
            workflow_template: None,
            dialog_scroll: ScrollHandle::new(),
            activities_scroll: ScrollHandle::new(),
            diff_scroll: ScrollHandle::new(),
            workflow_dialog_limit: 50,
            activities_limit: 100,
            expanded_activities: std::collections::HashSet::new(),
            expanded_workflows: std::collections::HashSet::new(),
            _tasks: Vec::new(),
        };
        app.session_dir = app.store.session_dir();
        for m in app.store.list_machines() {
            app.machines.push(MachineView::new(m));
        }
        for i in 0..app.machines.len() {
            let client = app.machines[i].client.clone();
            let t = app.spawn_machine_tasks(window, cx, i, client);
            app._tasks.push(t);
            app.refresh_sessions(i, window, cx);
            app.fetch_agents(i, window, cx);
        }
        app.restore_workflows(window, cx);
        app.spawn_polling(window, cx);
        app._setup_orch_inputs(window, cx);
        app
    }

    fn _setup_orch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.store.orchestrator();
        self.orch_api_format = cfg.api_format;
        self.orch_base_input
            .update(cx, |s, cx| s.set_value(&cfg.base_url, window, cx));
        self.orch_key_input
            .update(cx, |s, cx| s.set_value(&cfg.api_key, window, cx));
        self.orch_model_input
            .update(cx, |s, cx| s.set_value(&cfg.model, window, cx));
    }

    fn save_orchestrator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // api_format 为枚举单选，无需字符串校验
        let api_format = self.orch_api_format;
        let base_url = self.orch_base_input.read(cx).value().trim().to_owned();
        let api_key = self.orch_key_input.read(cx).value().trim().to_owned();
        let model = self.orch_model_input.read(cx).value().trim().to_owned();
        let error = if base_url.is_empty() {
            Some("请输入 Base URL。")
        } else if api_key.is_empty() {
            Some("请输入 API Key。")
        } else if model.is_empty() {
            Some("请输入模型名称。")
        } else {
            None
        };
        if let Some(error) = error {
            self.orchestrator_form_error = Some(error.into());
            self.orchestrator_form_status = None;
            window.push_notification(
                UiNotification::error(error).title("编排智能体设置保存失败"),
                cx,
            );
        } else {
            let result = self.store.save_orchestrator(&OrchestratorConfig {
                api_format,
                base_url,
                api_key,
                model,
            });
            match result {
                Ok(()) => {
                    self.orchestrator_form_error = None;
                    self.orchestrator_form_status = Some("已保存。".into());
                    window.push_notification(
                        UiNotification::success("编排智能体设置已保存").title("保存成功"),
                        cx,
                    );
                }
                Err(error) => {
                    let message = format!("保存失败：{error}");
                    self.orchestrator_form_error = Some(message.clone());
                    self.orchestrator_form_status = None;
                    window.push_notification(
                        UiNotification::error(message).title("编排智能体设置保存失败"),
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    fn machine(&self, i: usize) -> Option<&MachineView> {
        self.machines.get(i)
    }
    fn machine_mut(&mut self, i: usize) -> Option<&mut MachineView> {
        self.machines.get_mut(i)
    }

    /// 当前被选中的普通会话（machine 下标 + id）。
    fn open_session_target(&self) -> Option<(usize, String)> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => Some((*machine, id.clone())),
            _ => None,
        }
    }

    /// 默认机器下标：有选中会话则用它，否则第一台。
    fn active_machine(&self) -> Option<usize> {
        match &self.selected {
            Some(Selected::Session { machine, .. }) => Some(*machine),
            _ => (!self.machines.is_empty()).then_some(0),
        }
    }

    fn selected_workspace(&self) -> Option<(usize, String)> {
        let Selected::Session { machine, id } = self.selected.as_ref()? else {
            return None;
        };
        let cwd = self
            .machine(*machine)?
            .sessions
            .iter()
            .find(|session| session.id == *id)?
            .cwd
            .clone();
        Some((*machine, cwd))
    }

    // ---- 通知路由（connected/disconnected/auth + session.state_change 工作流驱动）----

    fn on_notify(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &WsNotification,
    ) {
        match n.method.as_str() {
            "connected" => {
                // WS 已建连但尚未认证：保持 Connecting，由 auth_ok 驱动后续
            }
            "auth_ok" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = MachineStatus::Online;
                }
                // 认证通过后再拉取初始数据（此前启动即发请求会被未认证拒绝，
                // 产生「连接失败」误报闪烁）
                this.fetch_agents(idx, window, cx);
                this.refresh_sessions(idx, window, cx);
            }
            "auth_failed" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    let msg = n
                        .params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("认证失败")
                        .to_string();
                    m.status = MachineStatus::AuthFailed(msg);
                }
            }
            "connect_failed" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    let err = n
                        .params
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("无法连接")
                        .to_string();
                    m.status = MachineStatus::ConnectFailed(err);
                }
            }
            "disconnected" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = MachineStatus::Offline;
                }
            }
            // 唯一主动推送 `session.state_change`：
            // 1) 更新普通会话本地状态；2) 若属于某工作流的关联普通会话则驱动工作流。
            _ if n.method == protocol::notify::SESSION_STATE_CHANGE => {
                Self::on_state_change(this, window, cx, idx, n);
            }
            _ => {}
        }
        cx.notify();
    }

    fn on_state_change(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &WsNotification,
    ) {
        // 通知负载按协议强类型解析（SessionStateChange，camelCase）
        let Ok(change) = serde_json::from_value::<SessionStateChange>(n.params.clone()) else {
            protocol::log::warn("gui.ws", "state_change 通知负载解析失败");
            return;
        };
        let SessionStateChange {
            session_id: sid,
            old_state,
            new_state,
        } = change;
        let idle = new_state == SessionState::Idle;
        // 更新普通会话元数据状态与聚合视图 busy（若已加载）
        if let Some(m) = this.machines.get_mut(idx) {
            if let Some(s) = m.sessions.iter_mut().find(|s| s.id == sid) {
                s.state = new_state;
            }
            if let Some(v) = m.views.get_mut(&sid) {
                v.set_busy(!idle);
            }
        }
        this.refresh_sessions(idx, window, cx);

        // 工作流驱动：查找挂载了该关联普通会话的工作流
        let Some(wi) = this
            .workflows
            .iter()
            .position(|wf| wf.session.read().unwrap().children.iter().any(|c| c.id == sid))
        else {
            return;
        };
        // 用户取消工作流会话导致的子会话状态变更不注入（docs/DESIGN.md §工作流会话驱动）
        if this
            .workflows
            .get(wi)
            .map(|wf| wf.session.read().unwrap().cancelled)
            .unwrap_or(false)
        {
            if let Some(wf) = this.workflows.get_mut(wi) {
                wf.on_child_state_local(&sid, new_state);
            }
            return;
        }
        // 同步更新子会话本地状态（busy/idle 视觉）
        if let Some(wf) = this.workflows.get_mut(wi) {
            wf.on_child_state_local(&sid, new_state);
        }
        if !idle {
            return;
        }
        // 子会话变 idle：抽取其最新输出并异步推进该工作流。
        // 会话从未在 GUI 打开过时 views 无条目（后台创建的子会话），
        // 先拉一次对话历史尾窗再抽取，避免向编排智能体注入空输出。
        let opened_output = this
            .machines
            .get(idx)
            .and_then(|m| m.views.get(&sid))
            .and_then(|v| {
                v.dialog.iter().rev().find_map(|d| match d {
                    DialogMsg::AgentMessage { content, .. } => Some(block_text(content)),
                    _ => None,
                })
            });
        let client_for_tail = this.machines.get(idx).map(|m| m.client.clone());
        let wf = match this.workflows.get(wi) {
            Some(wf) => wf.clone(),
            None => return,
        };
        let sid_for_tail = sid.clone();
        wf.on_child_state_local(&sid, new_state);
        let session_dir = this.session_dir.clone();
        // 状态共享于引擎内部（Arc<RwLock>），任务结束无需整引擎回写
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            run_engine_on_tokio(async move {
                let output = match opened_output {
                    Some(o) => o,
                    None => {
                        let tail = match client_for_tail {
                            Some(c) => c
                                .request::<_, HistoryResult>(
                                    protocol::method::SESSION_HISTORY,
                                    Some(SessionPageParams {
                                        session_id: sid_for_tail.clone(),
                                        limit: Some(10),
                                        before: None,
                                    }),
                                )
                                .await
                                .map(|r| r.items)
                                .unwrap_or_default(),
                            None => Vec::new(),
                        };
                        tail.iter()
                            .rev()
                            .find_map(|item| match item {
                                HistoryItem::AgentMessage { content, .. } => {
                                    Some(block_text(content))
                                }
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                };
                if let Err(e) = wf
                    .on_child_state(&sid, old_state, new_state, Some(output))
                    .await
                {
                    protocol::log::error("gui.workflow", format!("推进工作流失败：{e}"));
                }
                if let Err(e) = wf.persist(&session_dir) {
                    protocol::log::error(
                        "gui.workflow",
                        format!("工作流状态持久化失败 {}: {e}", wf.session.read().unwrap().id),
                    );
                }
            })
            .await;
            let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
        });
        this._tasks.push(t);
    }

    // ---- 数据拉取：会话列表（session.list）主动刷新 ----

    fn refresh_sessions(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = json!({ "limit": PAGE_LIMIT });
            if let Ok(res) = client
                .request::<_, SessionListResult>(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                let sessions = res.sessions;
                let has_more = res.has_more;
                let next_before = res.next_before;
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        let (list, hm, nb) = if has_more {
                            merge_session_window(
                                &m.sessions,
                                sessions.clone(),
                                has_more,
                                next_before,
                            )
                        } else {
                            (sessions.clone(), false, None)
                        };
                        m.sessions = list;
                        m.sessions_has_more = hm;
                        m.sessions_next_before = nb;
                        crate::logic::sort_sessions_recent(&mut m.sessions);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// agent.list：某机器 agents。
    fn fetch_agents(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| match client
            .request::<_, AgentListResult>(protocol::method::AGENT_LIST, None::<serde_json::Value>)
            .await
        {
            Ok(result) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    // 连接状态由 ws 认证通知驱动；此处只更新 agent 列表
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.agents = result.agents;
                    }
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        // 连接状态机之外的操作级提示
                        m.notice = Some(format!("agent 列表获取失败：{e}"));
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 对话历史（session.history · 打开才刷）。
    fn refresh_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: session_id.clone(),
                limit: Some(PAGE_LIMIT),
                before: None,
            };
            if let Ok(res) = client
                .request::<_, HistoryResult>(protocol::method::SESSION_HISTORY, Some(params))
                .await
            {
                let items = res.items;
                let has_more = res.has_more;
                let next_before = res.next_before.map(|v| v as usize);
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(machine) {
                        if let Some(v) = m.views.get_mut(&session_id) {
                            v.set_history_page(&items, has_more, next_before);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 活动历史（session.activities · 打开才刷）。
    fn refresh_activities(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        // docs/DESIGN.md「活动视图」：面板未打开时不主动刷新
        if self.panel != Some(Panel::Activities) {
            return;
        }
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: session_id.clone(),
                limit: Some(PAGE_LIMIT),
                before: None,
            };
            if let Ok(res) = client
                .request::<_, ActivitiesResult>(protocol::method::SESSION_ACTIVITIES, Some(params))
                .await
            {
                let acts = res.activities;
                let has_more = res.has_more;
                let next_before = res.next_before.map(|v| v as usize);
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(machine) {
                        if let Some(v) = m.views.get_mut(&session_id) {
                            v.set_activities_page(acts, has_more, next_before);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_more_activities(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selected::Session { machine, id }) = self.selected.clone() else {
            return;
        };
        let Some(view) = self.machines.get(machine).and_then(|m| m.views.get(&id)) else {
            return;
        };
        let Some(before) = view.activities_next_before else {
            return;
        };
        let client = self.machines[machine].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: id.clone(),
                limit: Some(PAGE_LIMIT),
                before: Some(before as u64),
            };
            if let Ok(res) = client
                .request::<_, ActivitiesResult>(protocol::method::SESSION_ACTIVITIES, Some(params))
                .await
            {
                let acts = res.activities;
                let has_more = res.has_more;
                let next_before = res.next_before.map(|v| v as usize);
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(view) = this
                        .machines
                        .get_mut(machine)
                        .and_then(|m| m.views.get_mut(&id))
                    {
                        view.prepend_activities_page(acts, has_more, next_before);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_more_history(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selected::Session { machine, id }) = self.selected.clone() else {
            return;
        };
        let Some(view) = self.machines.get(machine).and_then(|m| m.views.get(&id)) else {
            return;
        };
        let Some(before) = view.history_next_before else {
            return;
        };
        let client = self.machines[machine].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: id.clone(),
                limit: Some(PAGE_LIMIT),
                before: Some(before as u64),
            };
            if let Ok(res) = client
                .request::<_, HistoryResult>(protocol::method::SESSION_HISTORY, Some(params))
                .await
            {
                let items = res.items;
                let has_more = res.has_more;
                let next_before = res.next_before.map(|v| v as usize);
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(view) = this
                        .machines
                        .get_mut(machine)
                        .and_then(|m| m.views.get_mut(&id))
                    {
                        view.prepend_history_page(&items, has_more, next_before);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 实时活动（session.ongoing_activity · 5s）。
    fn refresh_ongoing(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: session_id.clone(),
            };
            if let Ok(res) = client
                .request::<_, OngoingActivityResult>(protocol::method::SESSION_ONGOING_ACTIVITY, Some(params))
                .await
            {
                let act = res.activity;
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(machine) {
                        if let Some(v) = m.views.get_mut(&session_id) {
                            v.set_live(act);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 加载更早一窗会话（滚动到底部）。
    fn load_more_sessions(&self, window: &mut Window, cx: &mut Context<Self>, machine: usize) {
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let Some(before) = m.sessions_next_before.clone() else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = json!({ "limit": PAGE_LIMIT, "before": before });
            if let Ok(res) = client
                .request::<_, SessionListResult>(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                let sessions = res.sessions;
                let has_more = res.has_more;
                let next_before = res.next_before;
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(machine) {
                        let (list, hm, nb) =
                            merge_session_window(&m.sessions, sessions, has_more, next_before);
                        m.sessions = list;
                        m.sessions_has_more = hm;
                        m.sessions_next_before = nb;
                        crate::logic::sort_sessions_recent(&mut m.sessions);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ---- 轮询：会话列表 10s（全机器）；打开会话的对话/活动 10s；实时 5s ----

    fn spawn_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.machines.len();
        // 会话列表 10s（每机器）
        for i in 0..n {
            let client = self.machines[i].client.clone();
            let machine_name = self.machines[i].config.name.clone();
            let epoch = self.machines[i].connection_epoch;
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| loop {
                if let Ok(res) = client
                    .request::<_, SessionListResult>(protocol::method::SESSION_LIST, Some(json!({ "limit": PAGE_LIMIT })))
                    .await
                {
                    let sessions = res.sessions;
                    let has_more = res.has_more;
                    let next_before = res.next_before;
                    let _ = this.update_in(cx, |this, _w, cx| {
                        let Some(i) = this.machines.iter().position(|m| {
                            m.config.name == machine_name && m.connection_epoch == epoch
                        }) else {
                            return;
                        };
                        if let Some(m) = this.machines.get_mut(i) {
                            let (list, hm, nb) = if has_more {
                                merge_session_window(&m.sessions, sessions, has_more, next_before)
                            } else {
                                (sessions, false, None)
                            };
                            m.sessions = list;
                            m.sessions_has_more = hm;
                            m.sessions_next_before = nb;
                            crate::logic::sort_sessions_recent(&mut m.sessions);
                        }
                        cx.notify();
                    });
                }
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
            });
            self._tasks.push(t);
        }

        // 打开会话的对话 + 活动 10s
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| loop {
            let target = this
                .update_in(cx, |this, _w, _cx| this.open_session_target())
                .ok()
                .flatten();
            if let Some((machine, id)) = target {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.refresh_dialog(window, cx, machine, id.clone());
                    this.refresh_activities(window, cx, machine, id);
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_secs(10))
                .await;
        });
        self._tasks.push(t);

        // 打开会话的实时活动 5s
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| loop {
            let target = this
                .update_in(cx, |this, _w, _cx| this.open_session_target())
                .ok()
                .flatten();
            if let Some((machine, id)) = target {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.refresh_ongoing(window, cx, machine, id);
                    cx.notify();
                });
            }
            cx.background_executor().timer(Duration::from_secs(5)).await;
        });
        self._tasks.push(t);
    }

    /// 单机器后台任务（通知订阅）。
    fn spawn_machine_tasks(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        _client: WsClient,
    ) -> Task<()> {
        let mut notify_rx = self.machines[idx].client.subscribe();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            while let Ok(n) = notify_rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    Self::on_notify(this, window, cx, idx, &n);
                });
            }
        })
    }

    // ---- 打开会话 / 工作流 ----

    fn open_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        self.selected = Some(Selected::Session {
            machine,
            id: session_id.clone(),
        });
        self.set_panel(window, cx, None);
        if let Some(m) = self.machines.get_mut(machine) {
            m.views.entry(session_id.clone()).or_default();
            m.diff_files.clear();
            m.diff_not_repo = false;
            m.diff_selection.clear();
            m.diff_request_id = m.diff_request_id.saturating_add(1);
            m.diff_loading = false;
            m.diff_error = None;
            m.workspace_directories.clear();
            m.workspace_expanded.clear();
            m.workspace_loading.clear();
            m.workspace_list_request_id = m.workspace_list_request_id.saturating_add(1);
            m.workspace_read_request_id = m.workspace_read_request_id.saturating_add(1);
            m.workspace_file = None;
            m.workspace_content.clear();
            m.workspace_error = None;
            m.workspace_read_loading = false;
            m.workspace_read_has_more = false;
            m.workspace_read_next_offset = 0;
        }
        self.refresh_dialog(window, cx, machine, session_id.clone());
        self.refresh_activities(window, cx, machine, session_id.clone());
        self.refresh_ongoing(window, cx, machine, session_id);
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn open_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, wi: usize) {
        self.selected = Some(Selected::Workflow { engine: wi });
        self.set_panel(window, cx, None);
        self.workflow_dialog_limit = 50;
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    // ---- 动作：创建会话 / 发送 / 取消 / 删除 / 重命名 ----

    fn create_session_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let cwd = self.session_cwd_input.read(cx).value().trim().to_owned();
        if cwd.is_empty() {
            self.new_session_error = Some("请输入工作目录，或选择一个常用工作目录。".into());
            cx.notify();
            return;
        }
        self.new_session_error = None;
        let agent = match self.new_session_agent.clone() {
            Some(a) => a,
            None => match self.available_agent(machine) {
                Some(a) => a,
                None => {
                    if let Some(m) = self.machine_mut(machine) {
                        m.notice = Some("无可用 agent".into());
                    }
                    cx.notify();
                    return;
                }
            },
        };
        let params = SessionNewParams {
            agent: agent.clone(),
            cwd: cwd.clone(),
        };
        let client = self.machines[machine].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request::<_, SessionResult>(protocol::method::SESSION_NEW, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match &res {
                    Ok(res) => {
                        let new_id = res.session.id.clone();
                        if !new_id.is_empty() {
                            this.store
                                .record_recent_workspace(&machine_name, &cwd, now_ts());
                            this.refresh_sessions(machine, w, cx);
                            this.open_session(w, cx, machine, new_id);
                        } else {
                            w.push_notification(
                                UiNotification::error("服务器返回了无效的会话信息")
                                    .title("创建会话失败"),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        w.push_notification(
                            UiNotification::error(format!("无法创建会话：{error}"))
                                .title("创建会话失败"),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        let attachments = self.input_attachments.clone();
        if text.trim().is_empty() && attachments.is_empty() {
            return;
        }
        let (clean_text, refs) = parse_at_references(&text);
        let mut all = attachments;
        for r in refs {
            all.push(path_attachment(&r));
        }
        let blocks = compose_prompt(&clean_text, &all);
        let Some(target) = self.selected.clone() else {
            cx.notify();
            return;
        };
        match target {
            Selected::Session { machine, id } => {
                let Some(m) = self.machine(machine) else {
                    return;
                };
                let client = m.client.clone();
                let params = SessionPromptParams {
                    session_id: id.clone(),
                    input: blocks.clone(),
                };
                // 本地立即渲染用户消息
                if let Some(m) = self.machine_mut(machine) {
                    let v = m.views.entry(id.clone()).or_default();
                    v.dialog.push(DialogMsg::UserMessage {
                        content: blocks.clone(),
                        timestamp: now_ts(),
                    });
                }
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    let result = client
                        .request_ok(protocol::method::SESSION_PROMPT, Some(serde_json::to_value(&params).unwrap()))
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
                        if let Err(error) = &result {
                            w.push_notification(
                                UiNotification::error(format!("发送失败：{error}"))
                                    .title("消息未发送"),
                                cx,
                            );
                        }
                        this.refresh_dialog(w, cx, machine, id);
                        cx.notify();
                    });
                })
                .detach();
            }
            Selected::Workflow { engine } => {
                let session_dir = self.session_dir.clone();
                let workflow_text = compose_workflow_text(&clean_text, &all);
                let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
                    let should_advance = wf.record_user(&workflow_text);
                    if should_advance {
                        wf.begin_busy();
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        protocol::log::error(
                            "gui.workflow",
                            format!("工作流用户消息持久化失败 {}: {e}", wf.session.read().unwrap().id),
                        );
                    }
                    should_advance
                } else {
                    false
                };
                if should_advance {
                    let wf = self.workflows[engine].clone();
                    let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                        run_engine_on_tokio(async move {
                            if let Err(e) = wf.advance().await {
                                protocol::log::error(
                                    "gui.workflow",
                                    format!("推进工作流失败 {}: {e}", wf.session.read().unwrap().id),
                                );
                            }
                            if let Err(e) = wf.persist(&session_dir) {
                                protocol::log::error(
                                    "gui.workflow",
                                    format!("工作流状态持久化失败 {}: {e}", wf.session.read().unwrap().id),
                                );
                            }
                        })
                        .await;
                        let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
                    });
                    self._tasks.push(t);
                }
            }
        }
        self.input_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.input_attachments.clear();
    }

    fn quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>, cmd: &QuickCommand) {
        match self.selected.clone() {
            Some(Selected::Session { machine, id }) => {
                let Some(m) = self.machine(machine) else {
                    return;
                };
                let client = m.client.clone();
                let params = SessionPromptParams {
                    session_id: id.clone(),
                    input: vec![ContentBlock::Text {
                        text: cmd.prompt.clone(),
                    }],
                };
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    let result = client
                        .request_ok(protocol::method::SESSION_PROMPT, Some(serde_json::to_value(&params).unwrap()))
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
                        if let Err(error) = &result {
                            w.push_notification(
                                UiNotification::error(format!("发送失败：{error}"))
                                    .title("快捷指令未发送"),
                                cx,
                            );
                        }
                        this.refresh_dialog(w, cx, machine, id);
                        cx.notify();
                    });
                })
                .detach();
            }
            Some(Selected::Workflow { .. }) => {
                // 快捷指令作为用户输入进入工作流会话（PRD §快捷指令）
                self.input_state
                    .update(cx, |s, cx| s.set_value(&cmd.prompt, window, cx));
                self.send_prompt(window, cx);
            }
            None => {}
        }
    }

    /// 当前选中项是否正在工作中，可被取消。
    fn can_cancel(&self) -> bool {
        match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.sessions.iter().find(|s| s.id == *id))
                .is_some_and(|s| s.state == SessionState::Busy),
            Some(Selected::Workflow { engine }) => self
                .workflows
                .get(*engine)
                .is_some_and(|wf| wf.session.read().unwrap().state == SessionState::Busy && !wf.session.read().unwrap().done),
            None => false,
        }
    }

    fn cancel_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Selected::Workflow { engine }) = self.selected.clone() {
            self.cancel_workflow(window, cx, engine);
            cx.notify();
            return;
        }
        let Some(Selected::Session { machine, id }) = self.selected.clone() else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let sid = id.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: sid.clone(),
            };
            let res = client
                .request_ok(protocol::method::SESSION_CANCEL, Some(serde_json::to_value(&params).unwrap()))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if let Err(error) = &res {
                    if let Some(m) = this.machines.get_mut(machine) {
                        m.notice = Some(format!("取消失败（{error}）"));
                    }
                }
                this.refresh_dialog(w, cx, machine, sid);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn delete_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let sid = session_id.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: sid.clone(),
            };
            let res = client
                .request_ok(protocol::method::SESSION_DELETE, Some(serde_json::to_value(&params).unwrap()))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match res {
                    Ok(_) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.sessions.retain(|s| s.id != sid);
                            m.views.remove(&sid);
                        }
                        if let Some(Selected::Session { id, .. }) = this.selected.clone() {
                            if id == sid {
                                this.selected = None;
                            }
                        }
                        this.refresh_sessions(machine, w, cx);
                    }
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("删除会话失败（{error}）"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_delete_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let sid = session_id.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("删除会话")
                .description(format!(
                    "确定删除会话 {session_id} 吗？删除后历史一并移除，不可恢复。"
                ))
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    let sid = sid.clone();
                    this.update(cx, |this, cx| {
                        this.delete_session(window, cx, machine, sid);
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn rename_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
        title: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let title_trim = title.trim().to_string();
        let params = SessionConfigureParams {
            session_id,
            title: title_trim,
        };
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request_ok(protocol::method::SESSION_CONFIGURE, Some(serde_json::to_value(&params).unwrap()))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match res {
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("重命名失败（{error}）"));
                        }
                    }
                    Ok(()) => {
                        this.renaming_session = None;
                        // 主动刷新会话列表以体现新标题（修复 M1：重命名后不主动刷新）
                        this.refresh_sessions(machine, w, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rename_workflow(&mut self, cx: &mut Context<Self>, wi: usize, title: String) {
        if let Some(wf) = self.workflows.get_mut(wi) {
            wf.session.write().unwrap().title = title.trim().to_string();
            let _ = wf.persist(&self.session_dir);
        }
        self.renaming_workflow = None;
        cx.notify();
    }

    // ---- 工作流 ----

    fn restore_workflows(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let sessions = match WorkflowEngine::load_all(&self.session_dir) {
            Ok(sessions) => sessions,
            Err(e) => {
                protocol::log::error("gui.workflow", format!("加载工作流失败：{e}"));
                return;
            }
        };
        if sessions.is_empty() {
            return;
        }
        let clients: Vec<WsClient> = self.machines.iter().map(|m| m.client.clone()).collect();
        let summaries = self.machine_summaries();
        let backend = self.orchestrator_backend();
        for s in sessions {
            self.workflows.push(WorkflowEngine::restore(
                s,
                backend.clone(),
                clients.clone(),
                summaries.clone(),
            ));
        }
        cx.notify();
    }

    fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.workflow_input.read(cx).value().to_string();
        let template = self.workflow_template.take();
        let description = if goal.trim().is_empty() {
            template
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default()
        } else {
            goal.trim().to_string()
        };
        if template.is_none() && description.is_empty() {
            self.workflow_error = Some("请先用自然语言描述执行计划".into());
            cx.notify();
            return;
        }
        let preamble = template.map(|t| t.plan);
        self.create_workflow_with(window, cx, description, preamble);
        self.workflow_input
            .update(cx, |s, cx| s.set_value("", window, cx));
    }

    fn create_workflow_with(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        description: String,
        preamble: Option<String>,
    ) {
        if !self.store.orchestrator().is_configured() {
            self.workflow_error = Some(
                "编排 agent 未配置 API（Base URL / API key / 模型）。请先在 设置 → 编排 agent 中配置。"
                    .into(),
            );
            cx.notify();
            return;
        }
        self.workflow_error = None;
        let (clean, refs) = parse_at_references(&description);
        let context = refs
            .iter()
            .map(|r| read_path_context(r))
            .collect::<Vec<_>>()
            .join("\n");
        let clients: Vec<WsClient> = self.machines.iter().map(|m| m.client.clone()).collect();
        let summaries = self.machine_summaries();
        let backend = self.orchestrator_backend();
        let engine = WorkflowEngine::new(
            &clean,
            &context,
            preamble.as_deref().unwrap_or(""),
            backend,
            clients.clone(),
            summaries,
        );
        let wi = self.workflows.len();
        let session_dir = self.session_dir.clone();
        self.workflows.push(engine);
        self.selected = Some(Selected::Workflow { engine: wi });
        let should_advance =
            !clean.trim().is_empty() || preamble.as_deref().is_some_and(|p| !p.trim().is_empty());
        if should_advance {
            if let Some(wf) = self.workflows.get_mut(wi) {
                wf.begin_busy();
            }
        }
        if let Some(wf) = self.workflows.get(wi) {
            if let Err(e) = wf.persist(&session_dir) {
                protocol::log::error(
                    "gui.workflow",
                    format!("工作流创建后持久化失败 {}: {e}", wf.session.read().unwrap().id),
                );
            }
        }
        if should_advance {
            let wf = self.workflows[wi].clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                run_engine_on_tokio(async move {
                    if let Err(e) = wf.advance().await {
                        protocol::log::error(
                            "gui.workflow",
                            format!("推进工作流失败 {}: {e}", wf.session.read().unwrap().id),
                        );
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        protocol::log::error(
                            "gui.workflow",
                            format!("工作流状态持久化失败 {}: {e}", wf.session.read().unwrap().id),
                        );
                    }
                })
                .await;
                let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
            });
            self._tasks.push(t);
        }
        cx.notify();
    }

    fn cancel_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        if let Some(wf) = self.workflows.get_mut(idx) {
            wf.mark_cancelled();
        }
        let session_dir = self.session_dir.clone();
        let Some(wf) = self.workflows.get(idx).cloned() else {
            return;
        };
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            run_engine_on_tokio(async move {
                if let Err(e) = wf.send_cancel_to_children().await {
                    protocol::log::error("gui.workflow", format!("取消关联会话失败：{e}"));
                }
                if let Err(e) = wf.persist(&session_dir) {
                    protocol::log::error(
                        "gui.workflow",
                        format!("取消后工作流持久化失败 {}: {e}", wf.session.read().unwrap().id),
                    );
                }
            })
            .await;
            let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
        });
        self._tasks.push(t);
        let _ = cx;
    }

    fn confirm_delete_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        let child_count = self
            .workflows
            .get(idx)
            .map(|w| w.session.read().unwrap().children.len())
            .unwrap_or(0);
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("删除工作流会话")
                .description(format!(
                    "确定删除该工作流会话吗？将同时删除其 {child_count} 个关联普通会话，不可恢复。"
                ))
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    this.update(cx, |this, cx| {
                        this.delete_workflow(window, cx, idx);
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn delete_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        let children: Vec<(usize, String)> = self
            .workflows
            .get(idx)
            .map(|w| {
                let sg = w.session.read().unwrap();
                sg.children
                    .iter()
                    .map(|c| (c.machine_idx, c.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let Some(wf_id) = self.workflows.get(idx).map(|wf| wf.session.read().unwrap().id.clone()) else {
            return;
        };
        if self
            .workflows
            .get(idx)
            .is_some_and(|workflow| workflow.session.read().unwrap().state == SessionState::Busy)
        {
            self.workflow_error = Some("请先取消正在执行的工作流，再删除工作流会话。".into());
            cx.notify();
            return;
        }
        let targets: Vec<(usize, WsClient, String)> = children
            .iter()
            .filter_map(|(machine, sid)| {
                self.machine(*machine)
                    .map(|m| (*machine, m.client.clone(), sid.clone()))
            })
            .collect();
        let missing_machine = targets.len() != children.len();
        let remote_targets = targets.clone();
        let session_dir = self.session_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                if missing_machine {
                    return Err("关联普通会话所属机器已移除，无法完成远端删除".to_string());
                }
                for (_, client, sid) in &remote_targets {
                    let params = SessionIdParams {
                        session_id: sid.clone(),
                    };
                    client
                        .request_ok(
                            protocol::method::SESSION_DELETE,
                            Some(serde_json::to_value(&params).unwrap()),
                        )
                        .await
                        .map_err(|error| format!("删除关联普通会话 {sid} 失败：{error}"))?;
                }
                Ok(())
            })
            .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                match result {
                    Some(Ok(())) => {
                        for (machine, _, sid) in &targets {
                            if let Some(m) = this.machine_mut(*machine) {
                                m.sessions.retain(|session| session.id != *sid);
                                m.views.remove(sid);
                            }
                        }
                        match WorkflowEngine::remove(&session_dir, &wf_id) {
                            Ok(()) => {
                                if let Some(current_idx) = this
                                    .workflows
                                    .iter()
                                    .position(|workflow| workflow.session.read().unwrap().id == wf_id)
                                {
                                    this.workflows.remove(current_idx);
                                    this.selected = match this.selected.clone() {
                                        Some(Selected::Workflow { engine })
                                            if engine == current_idx =>
                                        {
                                            None
                                        }
                                        Some(Selected::Workflow { engine })
                                            if engine > current_idx =>
                                        {
                                            Some(Selected::Workflow { engine: engine - 1 })
                                        }
                                        other => other,
                                    };
                                }
                            }
                            Err(error) => {
                                this.workflow_error =
                                    Some(format!("删除工作流持久化记录失败：{error}"));
                            }
                        }
                    }
                    Some(Err(error)) => {
                        this.workflow_error = Some(format!("删除工作流失败：{error}"));
                    }
                    None => {
                        this.workflow_error = Some("删除工作流任务未能执行".into());
                    }
                }
                cx.notify();
            });
        });
        self._tasks.push(t);
    }

    fn orchestrator_backend(&self) -> Arc<dyn OrcBackend> {
        let cfg = self.store.orchestrator();
        Arc::new(RigBackend::new(cfg))
    }

    fn machine_summaries(&self) -> Vec<MachineSummary> {
        // 全量透传（含不可用 agent 的真实 available）：编排 LLM 需要看到
        // 「某 agent 不可用」才能避让或上报，预先过滤会让该事实消失
        self.machines
            .iter()
            .map(|m| MachineSummary {
                name: m.config.name.clone(),
                online: m.status.online(),
                agents: m
                    .agents
                    .iter()
                    .map(|a| AgentSlot {
                        name: a.name.clone(),
                        available: a.available,
                    })
                    .collect(),
            })
            .collect()
    }

    fn available_agent(&self, idx: usize) -> Option<String> {
        self.machine(idx).and_then(|m| {
            m.agents
                .iter()
                .find(|a| a.available)
                .map(|a| a.name.clone())
        })
    }

    fn selected_meta(&self) -> Option<SessionMeta> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.sessions.iter().find(|s| s.id == *id))
                .cloned(),
            Some(Selected::Workflow { engine }) => {
                let wf = self.workflows.get(*engine)?;
                let sg = wf.session.read().unwrap();
                Some(SessionMeta {
                    id: sg.id.clone(),
                    agent: "编排".into(),
                    cwd: String::new(),
                    state: sg.state,
                    title: sg.title.clone(),
                    created_at: sg.created_at,
                    last_active_at: sg.updated_at,
                })
            }
            None => None,
        }
    }

    // ---- workspace.diff（代码审查面板）----

    fn load_diff(&mut self, window: &mut Window, cx: &mut Context<Self>, machine: usize) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine && m.sessions.iter().any(|s| s.id == *id) => {
                id.clone()
            }
            _ => return,
        };
        let client = m.client.clone();
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.diff_request_id = m.diff_request_id.saturating_add(1);
                m.diff_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff_loading = true;
            m.diff_error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceDiffParams {
                session_id: session_id.clone(),
                path: None,
            };
            let res = client
                .request::<_, WorkspaceDiffResult>(protocol::method::WORKSPACE_DIFF, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                let is_current = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                ) && this
                    .machines
                    .get(machine)
                    .is_some_and(|m| m.diff_request_id == request_id);
                if !is_current {
                    return;
                }
                let mut error_message = None;
                if let Some(m) = this.machines.get_mut(machine) {
                    match &res {
                        Ok(r) => {
                            m.diff_files = r.files.clone();
                            m.diff_not_repo = r.not_repo;
                        }
                        Err(error) => {
                            m.diff_files.clear();
                            m.diff_not_repo = false;
                            error_message = Some(format!("加载改动失败：{error}"));
                        }
                    }
                    m.diff_loading = false;
                    m.diff_error = error_message.clone();
                }
                if let Some(error) = error_message {
                    w.push_notification(
                        UiNotification::error(error.clone()).title("无法加载改动"),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_workspace_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
        offset: usize,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine => id.clone(),
            _ => return,
        };
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.workspace_list_request_id = m.workspace_list_request_id.saturating_add(1);
                m.workspace_list_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.workspace_loading.insert(path.clone());
        }
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let directory_path = path.clone();
            let params = json!({
                "sessionId": session_id.clone(),
                "path": if path.is_empty() { None } else { Some(path.clone()) },
                "offset": offset,
                "limit": 200,
            });
            let res = client
                .request::<_, WorkspaceListResult>(protocol::method::WORKSPACE_LIST, Some(params))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                let selected_session_matches = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                );
                let Some(m) = this.machines.get_mut(machine) else {
                    return;
                };
                if !selected_session_matches || m.workspace_list_request_id != request_id {
                    return;
                }
                m.workspace_loading.remove(&directory_path);
                match res {
                    Ok(result) => {
                        if offset == 0 {
                            m.workspace_directories.insert(
                                directory_path.clone(),
                                WorkspaceDirectory {
                                    entries: result.entries,
                                    has_more: result.has_more,
                                    next_offset: result.next_offset,
                                },
                            );
                        } else {
                            let directory = m
                                .workspace_directories
                                .entry(directory_path.clone())
                                .or_default();
                            directory.entries.extend(result.entries);
                            directory.has_more = result.has_more;
                            directory.next_offset = result.next_offset;
                        }
                        m.workspace_error = None;
                    }
                    Err(error) => {
                        m.workspace_error = Some(format!("工作目录列表失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_workspace_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
        offset: usize,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine => id.clone(),
            _ => return,
        };
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.workspace_read_request_id = m.workspace_read_request_id.saturating_add(1);
                m.workspace_read_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.workspace_read_loading = true;
            m.workspace_error = None;
            if offset == 0 {
                m.workspace_file = Some(path.clone());
                m.workspace_content.clear();
                m.workspace_read_has_more = false;
                m.workspace_read_next_offset = 0;
            }
        }
        cx.notify();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceReadParams {
                session_id: session_id.clone(),
                path,
                offset,
                limit: 400,
            };
            let res = client
                .request::<_, WorkspaceReadResult>(protocol::method::WORKSPACE_READ, Some(params))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                let selected_session_matches = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                );
                let Some(m) = this.machines.get_mut(machine) else {
                    return;
                };
                if !selected_session_matches || m.workspace_read_request_id != request_id {
                    return;
                }
                m.workspace_read_loading = false;
                match res {
                    Ok(result) => {
                        if offset == 0 {
                            m.workspace_content = result.content;
                        } else {
                            m.workspace_content.push_str(&result.content);
                        }
                        m.workspace_file = Some(result.path);
                        m.workspace_error = None;
                        m.workspace_read_has_more = result.has_more;
                        m.workspace_read_next_offset = result.next_offset;
                    }
                    Err(error) => {
                        m.workspace_error = Some(format!("读取文件失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn restore_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: Option<String>,
        patch: Option<String>,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let Some(session_id) = self.selected.as_ref().and_then(|selected| match selected {
            Selected::Session { id, .. } => Some(id.clone()),
            Selected::Workflow { .. } => None,
        }) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceRestoreParams {
                session_id: session_id.clone(),
                path,
                patch,
            };
            let res = client
                .request::<_, OpResult>(protocol::method::WORKSPACE_RESTORE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match res {
                    Ok(result) if result.ok => this.load_diff(w, cx, machine),
                    Ok(result) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.workspace_error =
                                Some(result.message.unwrap_or_else(|| "恢复改动失败".into()));
                        }
                    }
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.workspace_error = Some(format!("恢复改动失败：{error}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 切换 diff 面板中某个文件或 hunk 的选中状态。
    fn toggle_diff_selection(
        &mut self,
        machine: usize,
        path: String,
        hunk: Option<usize>,
        _cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let key = (path, hunk);
        if !m.diff_selection.remove(&key) {
            m.diff_selection.insert(key);
        }
    }

    fn is_diff_selected(&self, machine: usize, path: &str, hunk: Option<usize>) -> bool {
        self.machine(machine)
            .is_some_and(|m| m.diff_selection.contains(&(path.to_string(), hunk)))
    }

    fn clear_diff_selection(&mut self, machine: usize, _cx: &mut Context<Self>) {
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff_selection.clear();
        }
    }

    /// 把 diff 面板选中的 patch 作为用户消息发送到当前普通会话。
    fn send_selected_diff(&mut self, window: &mut Window, cx: &mut Context<Self>, machine: usize) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let selection: HashSet<(String, Option<usize>)> = m.diff_selection.clone();
        let files = m.diff_files.clone();
        let Some(Selected::Session { id, .. }) = self.selected.clone() else {
            return;
        };
        let session_id = id.clone();

        let mut patches: Vec<String> = Vec::new();
        for f in &files {
            let file_selected = selection.contains(&(f.path.clone(), None));
            for (i, h) in f.hunks.iter().enumerate() {
                if file_selected || selection.contains(&(f.path.clone(), Some(i))) {
                    patches.push(format!("// {}\n{}", f.path, h.patch));
                }
            }
            // 没有拆分 hunk 时（如新增/删除整文件），整文件选中用完整 patch。
            if file_selected && f.hunks.is_empty() {
                patches.push(format!("// {}\n{}", f.path, f.patch));
            }
        }
        if patches.is_empty() {
            return;
        }
        let prompt = format!(
            "请审查以下选中的代码改动并给出意见或执行所需修改：\n```diff\n{}\n```",
            patches.join("\n")
        );
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPromptParams {
                session_id: session_id.clone(),
                input: vec![ContentBlock::Text { text: prompt }],
            };
            let _ = client
                .request_ok(protocol::method::SESSION_PROMPT, Some(serde_json::to_value(&params).unwrap()))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                this.refresh_dialog(w, cx, machine, session_id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- agent 操作（skills / 重启）----

    fn fetch_agent_skills(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let a = agent.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = AgentParams { agent: a.clone() };
            let skills = client
                .request::<_, AgentSkillsResult>(protocol::method::AGENT_SKILLS, Some(params))
                .await
                .map(|r| r.skills)
                .unwrap_or_default();
            let _ = this.update_in(cx, |this, _w, cx| {
                if let Some(m) = this.machines.get_mut(machine) {
                    m.skills = skills;
                    m.skills_agent = Some(a);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 通过普通会话执行技能操作，保留完整会话供用户继续干预。
    fn manage_skill_on_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
        skill: SkillEntry,
        action: SkillAction,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let cwd = self
            .store
            .recent_workspaces_for_machine(&machine_name)
            .into_iter()
            .next()
            .unwrap_or_else(|| ".".into());
        let operation_prompt = action.prompt(&skill);
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = async {
                let session = client
                    .request::<_, SessionResult>(
                        protocol::method::SESSION_NEW,
                        Some(SessionNewParams {
                            agent: agent.clone(),
                            cwd: cwd.clone(),
                        }),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let session_id = session.session.id;
                if session_id.is_empty() {
                    return Err("创建技能操作会话响应缺少 session.id".to_string());
                }
                let input = SessionPromptParams {
                    session_id: session_id.clone(),
                    input: vec![ContentBlock::Text { text: operation_prompt }],
                };
                client
                    .request_ok(protocol::method::SESSION_PROMPT, Some(serde_json::to_value(&input).unwrap()))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<String, String>(session_id)
            }
            .await;

            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(session_id) => {
                    this.refresh_sessions(machine, window, cx);
                    this.open_session(window, cx, machine, session_id);
                }
                Err(error) => {
                    if let Some(m) = this.machines.get_mut(machine) {
                        m.notice = Some(format!("技能{}失败：{error}", action.label()));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn restart_agent(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let params = AgentParams { agent };
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request_ok(protocol::method::AGENT_RESTART, Some(serde_json::to_value(&params).unwrap()))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let _ = res;
                this.fetch_agents(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_restart_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let agent2 = agent.clone();
            let agent_ok = agent.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("重启")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("重启 agent")
                .description(format!("确定重启 agent「{agent2}」吗？"))
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    let agent = agent_ok.clone();
                    this.update(cx, |this, cx| {
                        this.restart_agent(window, cx, machine, agent);
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn confirm_reconnect_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        name: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("重连")
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("重连机器")
                .description(format!("确定重连机器「{name}」吗？"))
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    this.update(cx, |this, cx| {
                        this.reconnect_machine(window, cx, idx);
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    // ---- 设置：机器管理 ----

    fn close_add_machine_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_add_machine_form = false;
        self.machine_form_error = None;
        self.machine_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.machine_url_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.machine_token_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    fn add_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
        url: String,
        token: String,
    ) -> bool {
        let validation_error = if name.trim().is_empty() {
            Some("请输入机器名称。")
        } else if !url.trim().starts_with("ws://") {
            Some("连接地址必须以 ws:// 开头。")
        } else if token.trim().is_empty() {
            Some("请输入连接 Token。")
        } else {
            None
        };
        if let Some(error) = validation_error {
            self.machine_form_error = Some(error.into());
            cx.notify();
            return false;
        }
        let machine = self
            .store
            .add_machine(name.trim(), url.trim(), token.trim());
        let view = MachineView::new(machine);
        // 初始 Connecting：状态由 ws 认证通知驱动，不伪造「已连接」
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        let t = self.spawn_machine_tasks(window, cx, idx, client);
        self._tasks.push(t);
        self.fetch_agents(idx, window, cx);
        self.refresh_sessions(idx, window, cx);
        self.machine_form_error = None;
        cx.notify();
        true
    }

    fn remove_machine(&mut self, _window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        if idx >= self.machines.len() {
            return;
        }
        if self.workflows.iter().any(|workflow| {
            workflow
                .session
                .read()
                .unwrap()
                .children
                .iter()
                .any(|child| child.machine_idx == idx)
        }) {
            self.machine_form_error = Some("请先删除关联工作流会话，再移除该机器。".into());
            cx.notify();
            return;
        }
        let name = self.machines[idx].config.name.clone();
        self.store.remove_machine(&name);
        self.machines.remove(idx);
        self.selected = match self.selected.clone() {
            Some(Selected::Session { machine, .. }) if machine == idx => None,
            Some(Selected::Session { machine, id }) if machine > idx => Some(Selected::Session {
                machine: machine - 1,
                id,
            }),
            other => other,
        };
        for wf in self.workflows.iter_mut() {
            let mut children_guard = wf.session.write().unwrap();
            for c in children_guard.children.iter_mut() {
                if c.machine_idx == idx {
                    // 保留机器名和远端会话关联，但标记为未绑定，避免下标
                    // 左移后误操作另一台机器。
                    c.machine_idx = usize::MAX;
                } else if c.machine_idx > idx {
                    c.machine_idx -= 1;
                }
            }
        }
        cx.notify();
    }

    fn confirm_remove_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        name: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认移除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("移除机器")
                .description(format!(
                    "确定移除机器「{name}」吗？其本地注册信息将被删除。"
                ))
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    this.update(cx, |this, cx| {
                        this.remove_machine(window, cx, idx);
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn open_quick_command_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<(String, String)>,
    ) {
        self.qc_edit_target = target.as_ref().map(|(name, _)| name.clone());
        self.qc_name_input.update(cx, |s, cx| {
            s.set_value(
                target.as_ref().map(|(name, _)| name.as_str()).unwrap_or(""),
                window,
                cx,
            );
        });
        self.qc_prompt_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|(_, prompt)| prompt.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_quick_command_form = true;
        cx.notify();
    }

    fn close_quick_command_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_quick_command_form = false;
        self.qc_edit_target = None;
        self.qc_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.qc_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    fn save_quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.qc_name_input.read(cx).value().trim().to_owned();
        let prompt = self.qc_prompt_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入指令名称。".into());
        } else if prompt.is_empty() {
            self.settings_form_error = Some("请输入指令内容。".into());
        } else {
            if let Some(old) = self.qc_edit_target.clone() {
                if old != name {
                    self.store.remove_quick_command(&old);
                    self.store.add_quick_command(&name, &prompt);
                } else {
                    self.store.update_quick_command(&old, &prompt);
                }
            } else {
                self.store.add_quick_command(&name, &prompt);
            }
            self.close_quick_command_form(window, cx);
            return;
        }
        cx.notify();
    }

    fn confirm_remove_quick_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let name_ok = name.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("删除快捷指令")
                .description(format!("确定删除快捷指令「{name}」吗？"))
                .on_ok(move |_ev, _window, cx| {
                    let name = name_ok.clone();
                    this.update(cx, |this, cx| {
                        this.store.remove_quick_command(&name);
                        cx.notify();
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn open_skill_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<SkillEntry>,
    ) {
        self.skill_edit_target = target.as_ref().map(|skill| skill.name.clone());
        self.skill_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.skill_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.description.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_skill_form = true;
        cx.notify();
    }

    fn close_skill_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_skill_form = false;
        self.skill_edit_target = None;
        self.skill_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.skill_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    fn save_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.skill_name_input.read(cx).value().trim().to_owned();
        let description = self.skill_desc_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入技能名称。".into());
        } else {
            if let Some(old) = self.skill_edit_target.clone() {
                if old != name {
                    self.store.remove_skill(&old);
                    self.store.add_skill(&name, &description);
                } else {
                    self.store.update_skill(&old, &description);
                }
            } else {
                self.store.add_skill(&name, &description);
            }
            self.close_skill_form(window, cx);
            return;
        }
        cx.notify();
    }

    fn confirm_remove_skill(&mut self, window: &mut Window, cx: &mut Context<Self>, name: String) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let name_ok = name.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("删除技能")
                .description(format!("确定删除技能「{name}」吗？"))
                .on_ok(move |_ev, _window, cx| {
                    let name = name_ok.clone();
                    this.update(cx, |this, cx| {
                        this.store.remove_skill(&name);
                        cx.notify();
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    fn open_skill_action_dialog(
        &mut self,
        skill: SkillEntry,
        action: SkillAction,
        cx: &mut Context<Self>,
    ) {
        self.skill_action_dialog = Some((skill, action));
        cx.notify();
    }

    fn open_template_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<WorkflowTemplate>,
    ) {
        self.tpl_edit_target = target.as_ref().map(|template| template.name.clone());
        self.tpl_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.tpl_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.plan.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_template_form = true;
        cx.notify();
    }

    fn close_template_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_template_form = false;
        self.tpl_edit_target = None;
        self.tpl_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tpl_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    fn save_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.tpl_name_input.read(cx).value().trim().to_owned();
        let plan = self.tpl_desc_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入模板名称。".into());
        } else if plan.is_empty() {
            self.settings_form_error = Some("请输入模板内容。".into());
        } else {
            if let Some(old) = self.tpl_edit_target.clone() {
                if old != name {
                    self.store.remove_template(&old);
                    self.store.add_template(&name, &plan);
                } else {
                    self.store.update_template(&old, &plan);
                }
            } else {
                self.store.add_template(&name, &plan);
            }
            self.close_template_form(window, cx);
            return;
        }
        cx.notify();
    }

    fn confirm_remove_template(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let name_ok = name.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("确认删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title("删除工作流模板")
                .description(format!("确定删除工作流模板「{name}」吗？"))
                .on_ok(move |_ev, _window, cx| {
                    let name = name_ok.clone();
                    this.update(cx, |this, cx| {
                        this.store.remove_template(&name);
                        cx.notify();
                    });
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    // ---- 右侧面板 ----

    const PANEL_RESIZE_HANDLE_WIDTH: f32 = 5.0;

    /// 面板逻辑宽度（px）。
    fn panel_width_logical(panel: Panel) -> f32 {
        match panel {
            Panel::Workspace => 520.0,
            Panel::Diff => 460.0,
            Panel::Detail => 360.0,
            Panel::Activities => 400.0,
        }
    }

    /// 打开/切换/关闭右侧上下文面板：窗口向右扩展（中间面板宽度不变）。
    fn set_panel(&mut self, window: &mut Window, cx: &mut Context<Self>, panel: Option<Panel>) {
        let new_delta = panel
            .map(Self::panel_width_logical)
            .map(|width| width + Self::PANEL_RESIZE_HANDLE_WIDTH)
            .unwrap_or(0.0)
            * window.scale_factor();
        let bounds = window.bounds();
        // 当前窗口宽度已经包含旧面板；先还原中间区域宽度，再应用新面板宽度。
        // 直接用 new_delta 计算会让 resize 成为 no-op，导致面板覆盖中间区域的悬浮按钮。
        let base = bounds.size.width - self.panel_delta_px.into();
        if panel == Some(Panel::Activities) && self.panel != Some(Panel::Activities) {
            self.activities_limit = 100;
            self.activities_scroll.scroll_to_bottom();
        }
        self.panel = panel;
        self.panel_delta_px = new_delta;
        self.panel_resize_origin = None;
        self.panel_resize_initial = new_delta;
        let width: gpui::Pixels = base + new_delta.into();
        window.resize(gpui::Size::new(width, bounds.size.height));
        cx.notify();
    }

    fn render_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let panel = match self.panel {
            Some(Panel::Workspace) => self.render_workspace_panel(window, cx),
            Some(Panel::Diff) => self.render_diff_panel(window, cx),
            Some(Panel::Detail) => self.render_detail_panel(window, cx),
            Some(Panel::Activities) => self.render_activities_panel(window, cx),
            None => return None,
        };
        let panel_width =
            self.panel_delta_px / window.scale_factor() - Self::PANEL_RESIZE_HANDLE_WIDTH;
        let handle = div()
            .id("panel-resize-handle")
            .w(px(Self::PANEL_RESIZE_HANDLE_WIDTH))
            .h_full()
            .bg(cx.theme().border.opacity(0.35))
            .hover(|d| d.bg(cx.theme().primary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    this.panel_resize_origin = Some(event.position.x.as_f32());
                    this.panel_resize_initial = this.panel_delta_px;
                }),
            )
            .on_drag((), |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<()>, window, cx| {
                let Some(origin) = this.panel_resize_origin else {
                    return;
                };
                let next = (this.panel_resize_initial + origin - event.event.position.x.as_f32())
                    .clamp(
                        (300.0 + Self::PANEL_RESIZE_HANDLE_WIDTH) * window.scale_factor(),
                        (800.0 + Self::PANEL_RESIZE_HANDLE_WIDTH) * window.scale_factor(),
                    );
                this.resize_panel(window, cx, next);
            }));
        Some(
            h_flex()
                .h_full()
                .child(handle)
                .child(div().w(px(panel_width)).h_full().min_w_0().child(panel))
                .into_any(),
        )
    }

    fn resize_panel(&mut self, window: &mut Window, cx: &mut Context<Self>, width: f32) {
        let current = self.panel_delta_px;
        let bounds = window.bounds();
        let base = bounds.size.width - current.into();
        self.panel_delta_px = width;
        window.resize(gpui::Size::new(base + width.into(), bounds.size.height));
        cx.notify();
    }
}

impl AmuxApp {
    // ---- 左侧边栏 ----

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = cx.theme().sidebar;
        let sidebar_border = cx.theme().sidebar_border;
        let foreground = cx.theme().foreground;
        let sidebar_width = self.sidebar_width_px / window.scale_factor();
        let sidebar_content = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap(crate::theme::SPACE_SM)
            .p(crate::theme::SPACE_MD)
            .bg(sidebar)
            .border_r_1()
            .border_color(sidebar_border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.)).rounded_full().bg(cx.theme().primary))
                    .child(
                        Label::new("amux")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("goto-new-session")
                            .small()
                            .label("＋")
                            .tooltip("新会话 / 工作流")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.selected = None;
                                this.set_panel(window, cx, None);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("sidebar-sessions")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_2()
                    .children(self.render_session_list(cx)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("settings")
                        .small()
                        .ghost()
                        .label("设置")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.show_settings = true;
                            if let Some(i) = this.active_machine() {
                                this.refresh_sessions(i, _window, cx);
                            }
                            cx.notify();
                        })),
                ),
            );
        let resize_handle = div()
            .id("sidebar-resize-handle")
            .w(px(5.0))
            .h_full()
            .bg(sidebar_border.opacity(0.6))
            .hover(|d| d.bg(cx.theme().primary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    this.sidebar_resize_origin = Some(event.position.x.as_f32());
                    this.sidebar_resize_initial = this.sidebar_width_px;
                }),
            )
            .on_drag((), |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<()>, window, cx| {
                let Some(origin) = this.sidebar_resize_origin else {
                    return;
                };
                let next = (this.sidebar_resize_initial
                    + (event.event.position.x.as_f32() - origin))
                    .clamp(180.0 * window.scale_factor(), 420.0 * window.scale_factor());
                this.sidebar_width_px = next;
                cx.notify();
            }));
        h_flex()
            .w(px(sidebar_width))
            .h_full()
            .child(sidebar_content)
            .child(resize_handle)
    }

    /// 会话列表：普通会话 + 工作流会话统一按最近活跃排序；工作流挂载的关联普通会话折叠。
    fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 子会话只挂在工作流会话下，顶层列表跳过
        let child_ids: std::collections::HashSet<String> = self
            .workflows
            .iter()
            .flat_map(|wf| {
                wf.session
                    .read()
                    .unwrap()
                    .children
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut items: Vec<(u64, SessionListItem)> = Vec::new();
        for (mi, m) in self.machines.iter().enumerate() {
            for s in &m.sessions {
                if child_ids.contains(s.id.as_str()) {
                    continue;
                }
                items.push((
                    s.last_active_at,
                    SessionListItem::Session {
                        machine: mi,
                        meta: s.clone(),
                    },
                ));
            }
        }
        for (wi, wf) in self.workflows.iter().enumerate() {
            let s_guard = wf.session.read().unwrap();
            let mut recency = s_guard.updated_at;
            for c in &s_guard.children {
                recency = recency.max(c.last_active_at);
                if let Some(mm) = self.machines.get(c.machine_idx) {
                    if let Some(s) = mm.sessions.iter().find(|s| s.id == c.id) {
                        recency = recency.max(s.last_active_at);
                    }
                }
            }
            items.push((recency, SessionListItem::Workflow { idx: wi }));
        }
        items.sort_by_key(|(rec, _)| std::cmp::Reverse(*rec));

        let mut rows: Vec<gpui::AnyElement> = items
            .into_iter()
            .map(|(_, item)| match item {
                SessionListItem::Session { machine, meta } => {
                    self.render_session_row(cx, machine, &meta)
                }
                SessionListItem::Workflow { idx } => self.render_workflow_row(cx, idx),
            })
            .collect();

        // 惰性加载：还有更早会话时底部显示「加载更早会话」
        for (mi, m) in self.machines.iter().enumerate() {
            if m.sessions_has_more {
                let name = m.config.name.clone();
                rows.push(
                    Button::new(format!("sessions-more-{mi}"))
                        .small()
                        .label(format!("加载更早会话（{name}）"))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.load_more_sessions(window, cx, mi);
                        }))
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// 普通会话行：标题 + 状态（工作中转圈右对齐）。
    fn render_session_row(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        s: &SessionMeta,
    ) -> gpui::AnyElement {
        let sid = s.id.clone();
        let sel = self.selected
            == Some(Selected::Session {
                machine,
                id: sid.clone(),
            });
        let title = if s.title.is_empty() {
            format!("（未命名）{}", short_cwd(&s.cwd))
        } else {
            s.title.clone()
        };
        let busy = s.state == SessionState::Busy;
        let label: SharedString = title.clone().into();
        let active = cx.theme().list_active;
        let border = cx.theme().list_active_border;

        // 正在重命名该会话：行内输入框 + 保存
        if self.renaming_session.as_ref() == Some(&(machine, sid.clone())) {
            let sid2 = sid.clone();
            return v_flex()
                .gap_1()
                .child(Input::new(&self.title_input))
                .child(
                    Button::new(format!("rename-save-{sid}"))
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            let title = this.title_input.read(cx).value().to_string();
                            this.rename_session(window, cx, machine, sid2.clone(), title);
                        })),
                )
                .into_any_element();
        }

        let sid_ctx = sid.clone();
        let sid_open = sid.clone();
        div()
            .id(format!("sess-row-{machine}-{sid}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(active.opacity(if sel { 1.0 } else { 0.0 }))
            .when(sel, |d| d.border_1().border_color(border))
            .hover(|d| d.bg(cx.theme().list_hover))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                this.open_session(window, cx, machine, sid_open.clone());
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    this.context_menu = Some(SessionContextMenu {
                        target: ContextMenuTarget::Session {
                            machine,
                            session_id: sid_ctx.clone(),
                        },
                        x: ev.position.x.as_f32(),
                        y: ev.position.y.as_f32(),
                    });
                    this.renaming_session = None;
                    this.renaming_workflow = None;
                    cx.notify();
                }),
            )
            .child(
                h_flex()
                    .w_full()
                    .h(px(32.))
                    .px_1()
                    .gap_1()
                    .items_center()
                    .child(
                        h_flex()
                            .id(format!("sess-title-{machine}-{sid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .child(Label::new(label).text_sm().flex_1().min_w_0().truncate()),
                    )
                    .child(if busy {
                        Spinner::new()
                            .color(hsla(0.6, 0.8, 0.5, 1.0))
                            .into_any_element()
                    } else {
                        div().w(px(14.)).h(px(14.)).into_any_element()
                    }),
            )
            .into_any_element()
    }

    /// 工作流行：标题 · 折叠子会话 · 状态 + 转圈。
    fn render_workflow_row(&self, cx: &mut Context<Self>, wi: usize) -> gpui::AnyElement {
        let Some(wf) = self.workflows.get(wi) else {
            return div().into_any();
        };
        let title = if wf.session.read().unwrap().title.is_empty() {
            "新工作流".to_string()
        } else {
            wf.session.read().unwrap().title.clone()
        };
        let state = if wf.session.read().unwrap().cancelled {
            "已取消"
        } else if wf.session.read().unwrap().done {
            "完成"
        } else if wf.session.read().unwrap().state == SessionState::Busy {
            "编排中…"
        } else {
            "空闲"
        };
        let expanded = self.expanded_workflows.contains(&wi);
        let header = h_flex()
            .gap_1()
            .items_center()
            .child(
                h_flex()
                    .id(format!("wf-title-{wi}"))
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .items_center()
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.open_workflow(window, cx, wi);
                    }))
                    .child(
                        Label::new(title.as_str())
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .child(
                        div()
                            .px_1()
                            .py(px(1.))
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .child(
                                Label::new(state)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    ),
            )
            .child(
                Button::new(format!("wf-toggle-{wi}"))
                    .small()
                    .ghost()
                    .label(if expanded { "▾" } else { "▸" })
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if !this.expanded_workflows.insert(wi) {
                            this.expanded_workflows.remove(&wi);
                        }
                        cx.notify();
                    })),
            )
            .child(if wf.session.read().unwrap().state == SessionState::Busy {
                Spinner::new()
                    .color(hsla(0.6, 0.8, 0.5, 1.0))
                    .into_any_element()
            } else {
                div().w(px(14.)).h(px(14.)).into_any_element()
            });

        // 子会话默认折叠、可展开下钻
        let mut children = wf.session.read().unwrap().children.clone();
        children.sort_by_key(|child| {
            std::cmp::Reverse(
                child.last_active_at.max(
                    self.machine(child.machine_idx)
                        .and_then(|machine| {
                            machine
                                .sessions
                                .iter()
                                .find(|session| session.id == child.id)
                        })
                        .map(|session| session.last_active_at)
                        .unwrap_or(0),
                ),
            )
        });
        let mut content = v_flex().gap_1();
        for c in &children {
            let cid = c.id.clone();
            let cid_open = cid.clone();
            let step = c.step_desc.clone();
            let machine_name = c.machine_name.clone();
            let machine_click = machine_name.clone();
            let agent = c.agent.clone();
            let busy = c.state == SessionState::Busy;
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Label::new("↳").text_color(cx.theme().muted_foreground))
                    .child(
                        h_flex()
                            .id(format!("wf-child-title-{wi}-{cid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let mi = this
                                    .machines
                                    .iter()
                                    .position(|mm| mm.config.name == machine_click)
                                    .unwrap_or(0);
                                this.open_session(window, cx, mi, cid_open.clone());
                            }))
                            .child(
                                Label::new(format!("{step} [{agent}@{machine_name}]"))
                                    .text_sm()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate(),
                            ),
                    )
                    .child(if busy {
                        Spinner::new()
                            .color(hsla(0.6, 0.8, 0.5, 1.0))
                            .into_any_element()
                    } else {
                        div().w(px(14.)).h(px(14.)).into_any_element()
                    }),
            );
        }

        if self.renaming_workflow == Some(wi) {
            return v_flex()
                .gap_1()
                .p_2()
                .bg(cx.theme().popover)
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(Input::new(&self.title_input))
                .child(
                    Button::new(format!("wf-rename-save-{wi}"))
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            let title = this.title_input.read(cx).value().to_string();
                            this.rename_workflow(cx, wi, title);
                        })),
                )
                .into_any_element();
        }

        let row = v_flex().gap_1().p_1().rounded_md().child(header).child(
            Collapsible::new()
                .open(self.expanded_workflows.contains(&wi))
                .content(content),
        );

        let wf_sel = self.selected == Some(Selected::Workflow { engine: wi });
        div()
            .id(format!("wf-row-{wi}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(cx
                .theme()
                .list_active
                .opacity(if wf_sel { 1.0 } else { 0.0 }))
            .when(wf_sel, |d| {
                d.border_1().border_color(cx.theme().list_active_border)
            })
            .hover(|d| d.bg(cx.theme().list_hover))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    this.context_menu = Some(SessionContextMenu {
                        target: ContextMenuTarget::Workflow { engine: wi },
                        x: ev.position.x.as_f32(),
                        y: ev.position.y.as_f32(),
                    });
                    this.renaming_workflow = None;
                    this.renaming_session = None;
                    cx.notify();
                }),
            )
            .child(row)
            .into_any_element()
    }

    // ---- 中间 ----

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.selected.is_none() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .p(crate::theme::SPACE_MD)
                .child(self.render_center(window, cx))
                .into_any();
        }
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .p(crate::theme::SPACE_MD)
            .bg(cx.theme().popover)
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(self.render_center(window, cx))
            .child(self.render_quick_buttons(cx))
            .child(self.render_activity_bar(cx))
            .child(self.render_input(window, cx))
            .into_any()
    }

    /// 中间面板：未选中会话→新会话视图；否则对话流 + 悬浮按钮。
    fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.selected.is_none() {
            return self.render_new_session_view(window, cx);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.render_session_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_dialog(window, cx))
                    .child(self.render_floating_buttons(window, cx)),
            )
            .into_any()
    }

    fn render_session_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, status) = match &self.selected {
            Some(Selected::Session { machine, id }) => {
                let Some(machine_view) = self.machine(*machine) else {
                    return h_flex().into_any();
                };
                let Some(session) = machine_view.sessions.iter().find(|s| s.id == *id) else {
                    return h_flex().into_any();
                };
                let available = machine_view.status.online()
                    && machine_view
                        .agents
                        .iter()
                        .any(|agent| agent.name == session.agent && agent.available);
                (
                    format!("{}@{}", session.agent, machine_view.config.name),
                    if available { "可用" } else { "不可用" },
                )
            }
            Some(Selected::Workflow { engine }) => {
                let Some(workflow) = self.workflows.get(*engine) else {
                    return h_flex().into_any();
                };
                (
                    "编排智能体".to_string(),
                    if workflow.session.read().unwrap().state == SessionState::Busy {
                        "工作中"
                    } else if self.store.orchestrator().is_configured() {
                        "可用"
                    } else {
                        "不可用"
                    },
                )
            }
            None => return h_flex().into_any(),
        };
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Label::new(label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(
                Label::new(status)
                    .text_xs()
                    .text_color(if status == "可用" {
                        cx.theme().success
                    } else {
                        cx.theme().danger
                    }),
            )
            .into_any()
    }

    /// 新会话视图：普通（机器与 Agent 并列一排 + 工作目录 + 创建按钮）/ 工作流模式。
    fn render_new_session_view(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mode = self.new_session_mode;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let warning_foreground = cx.theme().warning_foreground;
        let danger = cx.theme().danger;
        let mut card = v_flex()
            .w_full()
            .max_w(px(640.))
            .gap_3()
            .p_4()
            .bg(popover)
            .rounded_lg()
            .border_1()
            .border_color(border)
            .shadow_lg()
            .child(
                Label::new("新会话")
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(foreground),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ns-mode-direct")
                            .small()
                            .label("普通")
                            .when(mode == NewSessionMode::Direct, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.new_session_mode = NewSessionMode::Direct;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ns-mode-tpl")
                            .small()
                            .label("工作流")
                            .when(mode == NewSessionMode::Workflow, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.new_session_mode = NewSessionMode::Workflow;
                                cx.notify();
                            })),
                    ),
            );
        match mode {
            NewSessionMode::Direct => {
                if self.machines.is_empty() {
                    // PRD §桌面 GUI 设计：无机器时提示并引导到设置
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .bg(warning.opacity(0.18))
                            .rounded_md()
                            .child(
                                Label::new("尚未注册机器")
                                    .text_color(warning_foreground)
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new("请先在设置 → 机器管理中注册一台 amux server。")
                                    .text_sm()
                                    .text_color(warning_foreground),
                            )
                            .child(
                                Button::new("ns-goto-machine-settings")
                                    .small()
                                    .primary()
                                    .label("去注册机器")
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        this.show_settings = true;
                                        this.settings_category = SettingsCategory::Machines;
                                        cx.notify();
                                    })),
                            ),
                    );
                } else {
                    card = card
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_6()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("机器")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_machine_selector(cx)),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("Agent")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_harness_selector(cx)),
                                ),
                        )
                        .child(self.render_workspace_picker(cx))
                        .when_some(self.new_session_error.clone(), |view, error| {
                            view.child(Label::new(error).text_sm().text_color(danger))
                        })
                        .child(
                            Button::new("ns-create")
                                .primary()
                                .label("创建会话")
                                .on_click(cx.listener(|this, _ev, window, cx| {
                                    this.create_session_only(window, cx);
                                })),
                        )
                        .child(
                            Label::new("创建后进入会话页，在下方输入区发送首条指令")
                                .text_xs()
                                .text_color(muted_foreground),
                        );
                }
            }
            NewSessionMode::Workflow => {
                if !self.store.orchestrator().is_configured() {
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .p_2()
                            .bg(warning.opacity(0.18))
                            .rounded_md()
                            .child(
                                Label::new("编排智能体尚未配置")
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(warning_foreground),
                            )
                            .child(
                                Label::new("请先配置 API 格式、Base URL、API Key 和模型名称。")
                                    .text_sm()
                                    .text_color(warning_foreground),
                            )
                            .child(
                                Button::new("ns-goto-orch-settings")
                                    .small()
                                    .primary()
                                    .label("去配置编排 agent")
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        this.show_settings = true;
                                        this.settings_category = SettingsCategory::Orchestrator;
                                        cx.notify();
                                    })),
                            ),
                    );
                } else {
                    card = card
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("工作流模板")
                                        .text_sm()
                                        .text_color(muted_foreground),
                                )
                                .child(self.render_template_selector(cx)),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new(if self.workflow_template.is_some() {
                                        "本次工作流目标（可留空，稍后在会话中输入）"
                                    } else {
                                        "自然语言执行计划"
                                    })
                                    .text_sm()
                                    .text_color(muted_foreground),
                                )
                                .child(Input::new(&self.workflow_input)),
                        );
                    if let Some(err) = &self.workflow_error {
                        card = card.child(
                            v_flex()
                                .gap_1()
                                .p_2()
                                .bg(danger.opacity(0.12))
                                .rounded_md()
                                .child(Label::new(err).text_color(danger)),
                        );
                    }
                    card = card.child(
                        Button::new("ns-create-workflow")
                            .primary()
                            .label("创建工作流会话")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.create_workflow(window, cx);
                            })),
                    );
                }
            }
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(card)
            .into_any()
    }

    /// 工作目录选择器：输入框支持手动编辑，也可从当前机器的最近目录中选择。
    fn render_workspace_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return v_flex().into_any();
        };
        let dirs = self.store.recent_workspaces_for_machine(&m.config.name);
        let mut picker = v_flex()
            .gap_1()
            .child(
                Label::new("工作目录")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                h_flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _ev, _window, cx| {
                                    this.show_workspace_dropdown = true;
                                    cx.notify();
                                }),
                            )
                            .child(Input::new(&self.session_cwd_input)),
                    )
                    .child(
                        Button::new("ns-workspace-toggle")
                            .small()
                            .label(if self.show_workspace_dropdown {
                                "⌃"
                            } else {
                                "⌄"
                            })
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.show_workspace_dropdown = !this.show_workspace_dropdown;
                                cx.notify();
                            })),
                    ),
            );
        if self.show_workspace_dropdown && !dirs.is_empty() {
            let options = dirs
                .into_iter()
                .map(|dir| {
                    let label = dir.clone();
                    Button::new(format!("ns-workspace-option-{label}"))
                        .small()
                        .label(short_cwd(&label))
                        .tooltip(label.clone())
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.session_cwd_input
                                .update(cx, |s, cx| s.set_value(&label, window, cx));
                            this.show_workspace_dropdown = false;
                            this.new_session_error = None;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect::<Vec<_>>();
            picker = picker.child(
                v_flex()
                    .gap_1()
                    .p_1()
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .children(options),
            );
        }
        picker.into_any()
    }

    /// 机器选择（新会话视图）。
    fn render_machine_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        let mut row = h_flex().gap_1().flex_wrap();
        if self.machines.is_empty() {
            row = row.child(Label::new("（请先在设置中添加机器）"));
        }
        for (i, m) in self.machines.iter().enumerate() {
            let name = m.config.name.clone();
            let machine_name = name.clone();
            let selected = selected_machine == Some(i);
            row = row.child(
                Button::new(format!("ns-machine-{i}"))
                    .small()
                    .label(name)
                    .when(selected, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.new_session_machine = Some(i);
                        this.new_session_agent = None;
                        let cwd = this
                            .store
                            .recent_workspaces_for_machine(&machine_name)
                            .into_iter()
                            .next()
                            .unwrap_or_default();
                        this.session_cwd_input
                            .update(cx, |s, cx| s.set_value(&cwd, window, cx));
                        this.new_session_error = None;
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// Agent 选择（新会话视图）：所选机器的可用 agent。
    fn render_harness_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        let mut row = h_flex().gap_1().flex_wrap();
        let Some(mi) = machine else {
            return row.child(Label::new("（无机器）"));
        };
        let agents = self
            .machine(mi)
            .map(|m| m.agents.clone())
            .unwrap_or_default();
        if agents.is_empty() {
            return row.child(Label::new("（未发现 agent）"));
        }
        for a in &agents {
            let name = a.name.clone();
            let name_click = name.clone();
            let selected = self.new_session_agent.as_deref() == Some(name.as_str());
            let mut btn = Button::new(format!("ns-agent-{name}"))
                .small()
                .label(name)
                .when(selected, |b| b.primary());
            if !a.available {
                btn = btn.disabled(true);
            }
            let available = a.available;
            row = row.child(btn.on_click(cx.listener(move |this, _ev, _window, cx| {
                if available {
                    this.new_session_agent = Some(name_click.clone());
                    cx.notify();
                }
            })));
        }
        row
    }

    /// 工作流模板选择。
    fn render_template_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let templates = self.store.list_templates();
        let mut row = h_flex().gap_1().flex_wrap();
        if templates.is_empty() {
            return row.child(Label::new("（无模板，可在设置中添加）"));
        }
        for t in templates {
            let name = t.name.clone();
            let sel = self
                .workflow_template
                .as_ref()
                .map(|x| x.name == name)
                .unwrap_or(false);
            row = row.child(
                Button::new(format!("ns-tpl-{name}"))
                    .small()
                    .label(name)
                    .when(sel, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        let t = t.clone();
                        // 点击已选模板则取消选择
                        if this
                            .workflow_template
                            .as_ref()
                            .map(|x| x.name == t.name)
                            .unwrap_or(false)
                        {
                            this.workflow_template = None;
                        } else {
                            this.workflow_template = Some(t);
                        }
                        cx.notify();
                    })),
            );
        }
        row
    }

    // ---- 对话 / 活动 / 输入 ----

    fn render_dialog(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialog: Vec<DialogMsg> = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .map(|v| v.dialog.clone())
                .unwrap_or_default(),
            Some(Selected::Workflow { engine }) => self
                .workflows
                .get(*engine)
                .map(|w| {
                    let sg = w.session.read().unwrap();
                    let all = sg.to_dialog();
                    let start = all.len().saturating_sub(self.workflow_dialog_limit);
                    all[start..].to_vec()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let agent_label: SharedString = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| {
                    let machine_name = m.config.name.clone();
                    m.sessions
                        .iter()
                        .find(|s| &s.id == id)
                        .map(|s| format!("{}@{machine_name}", s.agent).into())
                })
                .unwrap_or_else(|| "Agent".into()),
            Some(Selected::Workflow { .. }) => "编排".into(),
            None => "Agent".into(),
        };
        let primary = cx.theme().primary;
        let primary_foreground = cx.theme().primary_foreground;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let muted_foreground = cx.theme().muted_foreground;
        let rows = dialog
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DialogMsg::UserMessage { content, timestamp } => {
                    div().id(("row", i)).w_full().child(
                        div()
                            .ml_auto()
                            .flex_none()
                            .max_w(px(720.))
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(primary)
                            .shadow_sm()
                            .child(
                                Label::new("我")
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(primary_foreground),
                            )
                            .child(
                                Label::new(format_timestamp(*timestamp))
                                    .text_xs()
                                    .text_color(cx.theme().primary_foreground.opacity(0.78)),
                            )
                            .child(
                                TextView::markdown(format!("umd-{i}"), block_text(content))
                                    .selectable(true)
                                    .text_color(primary_foreground)
                                    .style(TextViewStyle::default().inline_code(HighlightStyle {
                                        background_color: Some(rgba(0x1e40af80).into()),
                                        ..Default::default()
                                    })),
                            ),
                    )
                }
                DialogMsg::AgentMessage { content, timestamp } => {
                    div().id(("row", i)).w_full().child(
                        div()
                            .flex_none()
                            .max_w(px(720.))
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .shadow_sm()
                            .child(
                                Label::new(agent_label.clone())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted_foreground),
                            )
                            .child(
                                Label::new(format_timestamp(*timestamp))
                                    .text_xs()
                                    .text_color(muted_foreground),
                            )
                            .child(
                                TextView::markdown(format!("amd-{i}"), block_text(content))
                                    .selectable(true),
                            ),
                    )
                }
            })
            .collect::<Vec<_>>();
        let history_has_more = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .map(|v| v.history_has_more)
                .unwrap_or(false),
            _ => false,
        };
        let mut content = Vec::new();
        if let Some(Selected::Workflow { engine }) = &self.selected {
            let total = self
                .workflows
                .get(*engine)
                .map(|w| w.session.read().unwrap().transcript.len())
                .unwrap_or(0);
            if total > self.workflow_dialog_limit {
                content.push(
                    Button::new("load-more-workflow-history")
                        .small()
                        .ghost()
                        .label(format!("加载更早消息（共 {total} 条）"))
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.workflow_dialog_limit += 100;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        if history_has_more {
            content.push(
                Button::new("load-more-history")
                    .small()
                    .ghost()
                    .label("加载更早消息")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_history(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        content.extend(rows.into_iter().map(|r| r.into_any_element()));
        if content.is_empty() {
            div()
                .id("dialog-empty")
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("选择左侧会话查看对话，或输入消息开始").text_color(muted_foreground),
                )
                .into_any()
        } else {
            div()
                .id("dialog")
                .v_flex()
                .flex_1()
                .gap_4()
                .p_2()
                .overflow_y_scroll()
                .track_scroll(&self.dialog_scroll)
                .children(content)
                .into_any()
        }
    }

    /// 进行中的活动（一条或无）。
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current: Option<Activity> = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .and_then(|v| v.live.clone()),
            Some(Selected::Workflow { engine }) => {
                let busy = self
                    .workflows
                    .get(*engine)
                    .map(|wf| wf.session.read().unwrap().state == SessionState::Busy)
                    .unwrap_or(false);
                if busy {
                    Some(Activity::Thinking {
                        timestamp: 0,
                        content: "正在编排决策/推进…".into(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        let warning = cx.theme().warning;
        let warning_foreground = cx.theme().warning_foreground;
        let danger = cx.theme().danger;
        match &current {
            Some(Activity::Thinking { content, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!("思考中：{}", one_line(content, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::ToolCall { name, title, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!(
                        "工具调用：{} {}",
                        name,
                        one_line(title.as_deref().unwrap_or(""), 120)
                    ))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Compaction { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("上下文压缩：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Error { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(danger.opacity(0.12))
                .border_1()
                .border_color(danger.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("错误：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(danger),
                )
                .into_any(),
            None => div().id("activity-bar-empty").into_any(),
        }
    }

    /// 对话流右侧竖排悬浮按钮：diff / 会话详情 / 会话活动历史。
    fn render_floating_buttons(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = self.panel;
        v_flex()
            .gap_1()
            .p_1()
            .justify_center()
            .bg(cx.theme().popover)
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .shadow_sm()
            .child(
                Button::new("float-workspace")
                    .small()
                    .label("目录")
                    .when(panel == Some(Panel::Workspace), |b| b.primary())
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let next = if this.panel == Some(Panel::Workspace) {
                            None
                        } else {
                            Some(Panel::Workspace)
                        };
                        this.set_panel(window, cx, next);
                        if next == Some(Panel::Workspace) {
                            if let Some(machine) = this.active_machine() {
                                this.load_workspace_list(window, cx, machine, String::new(), 0);
                            }
                        }
                    })),
            )
            .child(
                Button::new("float-diff")
                    .small()
                    .label("改动")
                    .when(panel == Some(Panel::Diff), |b| b.primary())
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let next = if this.panel == Some(Panel::Diff) {
                            None
                        } else {
                            Some(Panel::Diff)
                        };
                        this.set_panel(window, cx, next);
                        if next == Some(Panel::Diff) {
                            if let Some(Selected::Session { machine, .. }) = this.selected.clone() {
                                this.load_diff(window, cx, machine);
                            }
                        }
                    })),
            )
            .child(
                Button::new("float-detail")
                    .small()
                    .label("详情")
                    .when(panel == Some(Panel::Detail), |b| b.primary())
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let next = if this.panel == Some(Panel::Detail) {
                            None
                        } else {
                            Some(Panel::Detail)
                        };
                        this.set_panel(window, cx, next);
                    })),
            )
            .child(
                Button::new("float-activities")
                    .small()
                    .label("活动")
                    .when(panel == Some(Panel::Activities), |b| b.primary())
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let next = if this.panel == Some(Panel::Activities) {
                            None
                        } else {
                            Some(Panel::Activities)
                        };
                        this.set_panel(window, cx, next);
                    })),
            )
    }

    /// 快捷指令栏 + 取消当前工作。
    fn render_quick_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commands = self.store.list_quick_commands();
        let mut row = h_flex().flex_wrap().gap_1();
        for c in commands {
            let name = c.name.clone();
            let cmd = c.clone();
            row = row.child(
                Button::new(format!("qc-{}", c.name))
                    .small()
                    .label(name)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.quick_command(window, cx, &cmd);
                    })),
            );
        }
        row
    }

    /// 输入区：多行文本 + 附件 + 发送。
    fn render_input(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let attachments: Vec<String> = self
            .input_attachments
            .iter()
            .map(|a| match a {
                InputAttachment::Path { path, .. } => format!("📎 {path}"),
                InputAttachment::Image { name, .. } => format!("🖼 {name}"),
            })
            .collect();
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(attachments.iter().map(|a| {
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .child(
                                Label::new(a.clone())
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground),
                            )
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(96.))
                            .id("input-drop-zone")
                            .child(Input::new(&self.input_state))
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                                if ev.keystroke.key == "enter"
                                    && !ev.keystroke.modifiers.control
                                    && !ev.keystroke.modifiers.shift
                                    && !ev.keystroke.modifiers.alt
                                    && !ev.keystroke.modifiers.platform
                                {
                                    window.prevent_default();
                                    this.send_prompt(window, cx);
                                }
                            }))
                            .can_drop(|dragged, _window, _cx| dragged.is::<ExternalPaths>())
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    for p in paths.paths() {
                                        this.input_attachments.push(external_path_attachment(
                                            &p.display().to_string(),
                                        ));
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        Button::new("send")
                            .primary()
                            .label("发送")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.send_prompt(window, cx);
                            })),
                    )
                    .when(self.can_cancel(), |this| {
                        this.child(Button::new("cancel-work").small().label("✕ 取消").on_click(
                            cx.listener(|this, _ev, window, cx| {
                                this.cancel_work(window, cx);
                            }),
                        ))
                    })
                    .when(!self.input_attachments.is_empty(), |row| {
                        row.child(
                            Button::new("clear-attachments")
                                .small()
                                .label("清空附件")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.input_attachments.clear();
                                    cx.notify();
                                })),
                        )
                    }),
            )
    }

    // ---- 右侧面板内容 ----

    fn render_workspace_tree(
        &self,
        machine_idx: usize,
        path: &str,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(machine) = self.machine(machine_idx) else {
            return Vec::new();
        };
        let Some(directory) = machine.workspace_directories.get(path) else {
            return if machine.workspace_loading.contains(path) {
                vec![Label::new("加载中…")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element()]
            } else {
                Vec::new()
            };
        };
        let entries = directory.entries.clone();
        let has_more = directory.has_more;
        let next_offset = directory.next_offset;
        let loading = machine.workspace_loading.contains(path);
        let expanded_paths = machine.workspace_expanded.clone();
        let selected_file = machine.workspace_file.as_deref();
        let mut children = Vec::new();

        for entry in entries {
            let entry_path = entry.path.clone();
            let is_dir = entry.is_dir;
            let expanded = is_dir && expanded_paths.contains(&entry_path);
            let selected = !is_dir && selected_file == Some(entry_path.as_str());
            let label = format!(
                "{} {}",
                if is_dir {
                    if expanded {
                        "▾"
                    } else {
                        "▸"
                    }
                } else {
                    "·"
                },
                entry.name
            );
            let click_path = entry_path.clone();
            let button = Button::new(format!("workspace-entry-{entry_path}"))
                .w_full()
                .small()
                .ghost()
                .px_2()
                .pl(px(8. + depth as f32 * 14.))
                .when(selected, |b| b.bg(cx.theme().list_active))
                .label(label)
                .on_click(cx.listener(move |this, _ev, window, cx| {
                    let Some(machine) = this.active_machine() else {
                        return;
                    };
                    if is_dir {
                        if !this
                            .machines
                            .get_mut(machine)
                            .is_some_and(|m| m.workspace_expanded.remove(&click_path))
                        {
                            if let Some(m) = this.machines.get_mut(machine) {
                                m.workspace_expanded.insert(click_path.clone());
                            }
                            let needs_load = this
                                .machine(machine)
                                .map(|m| !m.workspace_directories.contains_key(&click_path))
                                .unwrap_or(false);
                            if needs_load {
                                this.load_workspace_list(
                                    window,
                                    cx,
                                    machine,
                                    click_path.clone(),
                                    0,
                                );
                            }
                        }
                    } else {
                        this.load_workspace_file(window, cx, machine, click_path.clone(), 0);
                    }
                    cx.notify();
                }));
            let mut node = v_flex().child(h_flex().w_full().child(button));
            if expanded {
                node = node.children(self.render_workspace_tree(
                    machine_idx,
                    &entry_path,
                    depth + 1,
                    cx,
                ));
            }
            children.push(node.into_any_element());
        }

        if has_more {
            let path_for_click = path.to_string();
            children.push(
                Button::new(format!("workspace-load-more-{path}"))
                    .small()
                    .ghost()
                    .label(if loading {
                        "加载中…"
                    } else {
                        "加载更多"
                    })
                    .disabled(loading)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some(machine) = this.active_machine() {
                            this.load_workspace_list(
                                window,
                                cx,
                                machine,
                                path_for_click.clone(),
                                next_offset,
                            );
                        }
                    }))
                    .into_any_element(),
            );
        }
        if children.is_empty() && !loading {
            children.push(
                Label::new(if depth == 0 {
                    "（目录为空）"
                } else {
                    "（空目录）"
                })
                .px_2()
                .pl(px(8. + depth as f32 * 14.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .into_any_element(),
            );
        }
        children
    }

    fn render_workspace_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(machine_idx) = self.active_machine() else {
            return v_flex()
                .w_full()
                .h_full()
                .p_3()
                .bg(cx.theme().popover)
                .child(Label::new("未选择会话"))
                .into_any();
        };
        let Some(machine) = self.machine(machine_idx) else {
            return div().into_any();
        };
        let workspace_file = machine.workspace_file.clone();
        let workspace_content = machine.workspace_content.clone();
        let workspace_error = machine.workspace_error.clone();
        let workspace_read_loading = machine.workspace_read_loading;
        let read_has_more = machine.workspace_read_has_more;
        let read_next_offset = machine.workspace_read_next_offset;
        let file = workspace_file.clone();
        let tree = v_flex()
            .gap_0()
            .w(px(220.0))
            .p_1()
            .bg(cx.theme().muted.opacity(0.35))
            .rounded_md()
            .overflow_y_scrollbar()
            .children(self.render_workspace_tree(machine_idx, "", 0, cx));

        let mut content = v_flex().flex_1().min_w_0().h_full().gap_2().child(
            Label::new(
                workspace_file
                    .clone()
                    .unwrap_or_else(|| "选择文件查看内容".to_string()),
            )
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground),
        );
        if let Some(error) = workspace_error {
            content = content.child(Label::new(error).text_sm().text_color(cx.theme().danger));
        } else if workspace_read_loading {
            content = content.child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Spinner::new())
                    .child(
                        Label::new("正在读取文件…")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
        } else if let Some(path) = file {
            content = content.child(
                TextView::markdown(
                    "workspace-file-content",
                    format!("```text\n{}\n```", workspace_content),
                )
                .selectable(true),
            );
            if read_has_more {
                content = content.child(
                    Button::new("workspace-read-more")
                        .small()
                        .ghost()
                        .label(if workspace_read_loading {
                            "读取中…"
                        } else {
                            "加载更多内容"
                        })
                        .disabled(workspace_read_loading)
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.load_workspace_file(
                                    window,
                                    cx,
                                    machine,
                                    path.clone(),
                                    read_next_offset,
                                );
                            }
                        })),
                );
            }
        } else {
            content = content.child(
                Label::new("选择文件查看文本内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
        }

        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Label::new("工作目录")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-workspace")
                            .small()
                            .label("✕")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(tree)
                    .child(content),
            )
            .into_any()
    }

    fn render_detail_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(meta) = self.selected_meta() else {
            return div().w_full().child(Label::new("未选择会话")).into_any();
        };
        let mut body =
            v_flex()
                .w_full()
                .h_full()
                .gap_2()
                .p_3()
                .bg(cx.theme().popover)
                .border_l_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .items_center()
                        .child(
                            Label::new("会话详情")
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().foreground),
                        )
                        .child(div().flex_1())
                        .child(Button::new("close-panel2").small().label("✕").on_click(
                            cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            }),
                        )),
                )
                .child(info_row(
                    "ID",
                    &meta.id,
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ))
                .child(info_row(
                    "Agent",
                    &meta.agent,
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ))
                .child(info_row(
                    "工作目录",
                    &meta.cwd,
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ))
                .child(info_row(
                    "状态",
                    if meta.state == SessionState::Busy {
                        "工作中"
                    } else {
                        "空闲"
                    },
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ))
                .child(info_row(
                    "创建时间",
                    &format_timestamp(meta.created_at),
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ));
        if let Some(Selected::Session { machine, .. }) = &self.selected {
            if let Some(machine_view) = self.machine(*machine) {
                body = body
                    .child(info_row(
                        "机器",
                        &machine_view.config.name,
                        cx.theme().muted_foreground,
                        cx.theme().foreground,
                    ))
                    .child(info_row(
                        "机器状态",
                        &machine_view.status.label(),
                        cx.theme().muted_foreground,
                        cx.theme().foreground,
                    ));
            }
        }
        let title = if meta.title.is_empty() {
            "（未命名）".to_string()
        } else {
            meta.title.clone()
        };
        body = body.child(Label::new(format!("标题: {title}")));
        if let Some(Selected::Workflow { engine }) = self.selected.clone() {
            let done = self
                .workflows
                .get(engine)
                .map(|w| w.session.read().unwrap().done)
                .unwrap_or(false);
            body = body
                .child(Label::new("— 工作流会话 —"))
                .child(
                    Button::new("wf-cancel")
                        .small()
                        .label("取消")
                        .when(done, |b| b.disabled(true))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.cancel_workflow(window, cx, engine);
                        })),
                )
                .child(
                    Button::new("wf-delete")
                        .small()
                        .label("删除工作流会话")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.confirm_delete_workflow(window, cx, engine);
                        })),
                );
            for c in self
                .workflows
                .get(engine)
                .map(|w| w.session.read().unwrap().children.clone())
                .unwrap_or_default()
            {
                let step = c.step_desc.clone();
                body = body.child(Label::new(format!("子会话 {} · {}", c.id, step)));
            }
        }
        body.into_any()
    }

    /// 单条活动历史：过长内容折叠。
    fn activity_row(
        &self,
        key: &str,
        kind: &str,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded_activities.contains(key);
        let long = detail.chars().count() > 200;
        let shown = if long && !expanded {
            one_line(detail, 200)
        } else {
            detail.to_string()
        };
        let key_owned = key.to_string();
        let mut row = div()
            .id(key_owned.clone())
            .w_full()
            .p_1()
            .bg(cx.theme().muted)
            .rounded_md()
            .v_flex()
            .gap_1()
            .child(Label::new(format!("[{kind}] {shown}")).text_sm());
        if long {
            row = row.child(
                Button::new(format!("act-toggle-{key}"))
                    .small()
                    .ghost()
                    .label(if expanded { "收起" } else { "展开" })
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if !this.expanded_activities.remove(&key_owned) {
                            this.expanded_activities.insert(key_owned.clone());
                        }
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }

    fn render_activities_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut live: Option<Activity> = None;
        let mut activities_has_more = false;
        match &self.selected {
            Some(Selected::Session { machine, id }) => {
                let view = self.machine(*machine).and_then(|m| m.views.get(id));
                let activities = view.map(|v| v.activities.clone()).unwrap_or_default();
                activities_has_more = view.map(|v| v.activities_has_more).unwrap_or(false);
                rows = activities
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let (kind, detail) = activity_display(a);
                        self.activity_row(&format!("act-{i}"), &kind, &detail, cx)
                    })
                    .collect();
                live = view.and_then(|v| v.live.clone());
            }
            Some(Selected::Workflow { engine }) => {
                if let Some(wf) = self.workflows.get(*engine) {
                    let sg = wf.session.read().unwrap();
                    rows = sg
                        .activities
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let (kind, detail) = activity_display(a);
                            self.activity_row(&format!("wf-act-{i}"), &kind, &detail, cx)
                        })
                        .collect();
                    if wf.session.read().unwrap().state == SessionState::Busy {
                        live = Some(Activity::Thinking {
                            timestamp: wf.session.read().unwrap().updated_at,
                            content: "正在编排决策/推进…".into(),
                        });
                    }
                }
            }
            _ => {}
        }
        let total = rows.len();
        let start = if activities_has_more {
            0
        } else {
            total.saturating_sub(self.activities_limit)
        };
        let has_more = activities_has_more || start > 0;
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if has_more {
            children.push(
                Button::new("load-more-activities")
                    .small()
                    .ghost()
                    .label("加载更早活动")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_activities(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        children.extend(rows.into_iter().skip(start));
        if let Some(a) = &live {
            let (kind, detail) = activity_display(a);
            children.push(
                div()
                    .id("act-live")
                    .w_full()
                    .p_1()
                    .bg(cx.theme().warning.opacity(0.16))
                    .border_1()
                    .border_color(cx.theme().warning.opacity(0.45))
                    .rounded_md()
                    .child(Spinner::new())
                    .child(format!("[{kind}] {detail}"))
                    .into_any_element(),
            );
        }
        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("会话活动历史")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-activities")
                            .small()
                            .label("✕")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(
                div()
                    .id("activities-panel")
                    .flex_1()
                    .gap_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.activities_scroll)
                    .children(children),
            )
            .into_any()
    }

    fn render_diff_panel(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machine = self.active_machine();
        let files = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_files.clone())
            .unwrap_or_default();
        let not_repo = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_not_repo)
            .unwrap_or(false);
        let diff_loading = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_loading)
            .unwrap_or(false);
        let diff_error = machine
            .and_then(|i| self.machine(i))
            .and_then(|m| m.diff_error.clone());
        let has_selection = machine
            .and_then(|i| self.machine(i))
            .is_some_and(|m| !m.diff_selection.is_empty());
        let can_send = matches!(&self.selected, Some(Selected::Session { .. }));
        let diff_tree_collapsed = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_tree_collapsed)
            .unwrap_or(false);
        let diff_changes_collapsed = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_changes_collapsed)
            .unwrap_or(false);
        let diff_scroll = self.diff_scroll.clone();
        let mut content_children: Vec<gpui::AnyElement> = Vec::new();
        let toolbar = h_flex()
            .items_center()
            .gap_2()
            .child(
                Label::new("改动审查")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(div().flex_1())
            .when(has_selection && can_send, |h| {
                h.child(
                    Button::new("diff-send-selected")
                        .small()
                        .primary()
                        .label("发送选中到会话")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.send_selected_diff(window, cx, machine);
                            }
                        })),
                )
                .child(
                    Button::new("diff-clear-selection")
                        .small()
                        .label("清空选择")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.clear_diff_selection(machine, cx);
                                cx.notify();
                            }
                        })),
                )
            })
            .child(
                Button::new("diff-toggle-tree")
                    .small()
                    .ghost()
                    .label(if diff_tree_collapsed {
                        "展开文件树"
                    } else {
                        "折叠文件树"
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        if let Some(machine) = this.active_machine() {
                            if let Some(view) = this.machines.get_mut(machine) {
                                view.diff_tree_collapsed = !view.diff_tree_collapsed;
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("diff-toggle-changes")
                    .small()
                    .ghost()
                    .label(if diff_changes_collapsed {
                        "展开改动"
                    } else {
                        "折叠改动"
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        if let Some(machine) = this.active_machine() {
                            if let Some(view) = this.machines.get_mut(machine) {
                                view.diff_changes_collapsed = !view.diff_changes_collapsed;
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("close-panel-diff")
                    .small()
                    .label("✕")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.set_panel(window, cx, None);
                    })),
            )
            .into_any_element();
        if let Some(error) = diff_error {
            content_children.push(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Label::new(error).text_sm().text_color(cx.theme().danger))
                    .into_any_element(),
            );
        } else if diff_loading {
            content_children.push(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Spinner::new())
                    .child(
                        Label::new("正在加载改动…")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element(),
            );
        } else if not_repo {
            content_children.push(
                Label::new("当前工作目录不是 git 仓库")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            );
        } else if files.is_empty() {
            content_children.push(
                Label::new("暂无改动")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            );
        }
        let Some(machine_idx) = machine else {
            return v_flex()
                .w_full()
                .h_full()
                .gap_2()
                .p_3()
                .bg(cx.theme().popover)
                .border_l_1()
                .border_color(cx.theme().border)
                .child(toolbar)
                .into_any();
        };
        let mut tree_items: Vec<gpui::AnyElement> = Vec::new();
        for (fi, f) in files.iter().enumerate() {
            let path = f.path.clone();
            let patch = f.patch.clone();
            let additions = f.additions;
            let deletions = f.deletions;
            let file_selected = self.is_diff_selected(machine_idx, &path, None);
            let path_for_restore = path.clone();
            let patch_for_restore = patch.clone();
            let mut file_children: Vec<gpui::AnyElement> = Vec::new();
            let (status_label, status_color) = match &f.status {
                GitChangeStatus::Added => ("A", cx.theme().success),
                GitChangeStatus::Deleted => ("D", cx.theme().danger),
                GitChangeStatus::Modified => ("M", cx.theme().warning),
            };
            file_children.push(
                h_flex()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .items_center()
                    .bg(cx.theme().muted.opacity(0.35))
                    .child(
                        Button::new(format!("diff-sel-file-{fi}"))
                            .small()
                            .ghost()
                            .label(if file_selected { "☑" } else { "☐" })
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |this, _ev, _window, cx| {
                                    this.toggle_diff_selection(machine_idx, path.clone(), None, cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Label::new(path.clone())
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        Label::new(format!("+{additions}"))
                            .text_xs()
                            .text_color(cx.theme().success),
                    )
                    .child(
                        Label::new(format!("-{deletions}"))
                            .text_xs()
                            .text_color(cx.theme().danger),
                    )
                    .child(
                        Label::new(status_label)
                            .w(px(20.))
                            .text_center()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(status_color),
                    )
                    .child(
                        Button::new(format!("restore-{path}"))
                            .small()
                            .label("撤销该文件")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                if let Some((machine, _)) = this.selected_workspace() {
                                    this.restore_workspace(
                                        window,
                                        cx,
                                        machine,
                                        Some(path_for_restore.clone()),
                                        Some(patch_for_restore.clone()),
                                    );
                                }
                            })),
                    )
                    .into_any_element(),
            );
            // 折叠全部改动时仅保留文件头（路径 + 状态 + 统计），隐藏 diff 行
            let hunks_iter: Box<dyn Iterator<Item = (usize, &protocol::GitDiffHunk)>> =
                if diff_changes_collapsed {
                    Box::new(std::iter::empty())
                } else {
                    Box::new(f.hunks.iter().enumerate())
                };
            for (hi, h) in hunks_iter {
                let hunk_selected = self.is_diff_selected(machine_idx, &path, Some(hi));
                let hunk_path = path.clone();
                let hunk_patch = h.patch.clone();
                let mut hunk_children: Vec<gpui::AnyElement> = Vec::new();
                hunk_children.push(
                    h_flex()
                        .w_full()
                        .h(px(28.))
                        .items_center()
                        .gap_2()
                        .px_2()
                        .bg(cx.theme().primary.opacity(0.12))
                        .child(
                            Button::new(format!("diff-sel-hunk-{fi}-{hi}"))
                                .small()
                                .ghost()
                                .label(if hunk_selected { "☑" } else { "☐" })
                                .on_click(cx.listener({
                                    let hunk_path = hunk_path.clone();
                                    move |this, _ev, _window, cx| {
                                        this.toggle_diff_selection(
                                            machine_idx,
                                            hunk_path.clone(),
                                            Some(hi),
                                            cx,
                                        );
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            Label::new(h.header.clone())
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().primary),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(format!("restore-hunk-{fi}-{hi}"))
                                .small()
                                .label("撤销此块")
                                .on_click(cx.listener({
                                    let hunk_path = hunk_path.clone();
                                    move |this, _ev, window, cx| {
                                        if let Some((machine, _)) = this.selected_workspace() {
                                            this.restore_workspace(
                                                window,
                                                cx,
                                                machine,
                                                Some(hunk_path.clone()),
                                                Some(hunk_patch.clone()),
                                            );
                                        }
                                    }
                                })),
                        )
                        .into_any_element(),
                );
                for line in diff_lines(h) {
                    let (background, marker, marker_color) = match line.kind {
                        DiffLineKind::Addition => {
                            (cx.theme().success.opacity(0.16), "+", cx.theme().success)
                        }
                        DiffLineKind::Deletion => {
                            (cx.theme().danger.opacity(0.16), "-", cx.theme().danger)
                        }
                        DiffLineKind::Context => {
                            (cx.theme().popover, " ", cx.theme().muted_foreground)
                        }
                    };
                    hunk_children.push(
                        h_flex()
                            .w_full()
                            .min_h(px(22.))
                            .items_center()
                            .bg(background)
                            .child(
                                div()
                                    .w(px(48.))
                                    .h_full()
                                    .px_2()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(cx.theme().border.opacity(0.45))
                                    .child(
                                        Label::new(
                                            line.old_number
                                                .map(|number| number.to_string())
                                                .unwrap_or_default(),
                                        )
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(48.))
                                    .h_full()
                                    .px_2()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(cx.theme().border.opacity(0.45))
                                    .child(
                                        Label::new(
                                            line.new_number
                                                .map(|number| number.to_string())
                                                .unwrap_or_default(),
                                        )
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .child(
                                div().w(px(24.)).h_full().justify_center().child(
                                    Label::new(marker)
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(marker_color),
                                ),
                            )
                            .child(
                                Label::new(line.content)
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .whitespace_nowrap()
                                    .flex_shrink_0(),
                            )
                            .into_any_element(),
                    );
                }
                file_children.push(
                    v_flex()
                        .w_full()
                        .gap_0()
                        .children(hunk_children)
                        .into_any_element(),
                );
            }
            let tree_path = path.clone();
            let diff_scroll = diff_scroll.clone();
            tree_items.push(
                Button::new(format!("diff-tree-{fi}"))
                    .small()
                    .ghost()
                    .label(format!("{}  (+{additions}/-{deletions})", tree_path))
                    .on_click(cx.listener(move |_this, _ev, _window, _cx| {
                        diff_scroll.scroll_to_top_of_item(fi);
                    }))
                    .into_any_element(),
            );
            content_children.push(
                v_flex()
                    .id(format!("diff-file-{fi}"))
                    .w_full()
                    .gap_0()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .overflow_hidden()
                    .children(file_children)
                    .into_any_element(),
            );
        }
        let tree = if diff_tree_collapsed {
            v_flex()
                .w(px(28.0))
                .h_full()
                .child(Label::new("树"))
                .into_any_element()
        } else {
            v_flex()
                .w(px(220.0))
                .h_full()
                .min_h_0()
                .gap_1()
                .p_1()
                .bg(cx.theme().muted)
                .rounded_md()
                .overflow_y_scrollbar()
                .child(
                    Label::new("改动文件")
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .children(tree_items)
                .into_any_element()
        };
        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .gap_2()
                    .child(tree)
                    .child(
                        div()
                            .id("diff-panel")
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .gap_2()
                            .overflow_y_scroll()
                            .track_scroll(&self.diff_scroll)
                            .children(content_children),
                    ),
            )
            .into_any()
    }

    // ---- 右键菜单 ----

    fn render_context_menu(
        &self,
        menu: &SessionContextMenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = menu.target.clone();
        div()
            .absolute()
            .left(px(menu.x))
            .top(px(menu.y))
            .id("context-menu")
            .v_flex()
            .w(px(170.))
            .p_1()
            .gap_1()
            .bg(cx.theme().popover)
            .rounded_md()
            .shadow_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(match &target {
                ContextMenuTarget::Session {
                    machine,
                    session_id,
                } => match self
                    .machine(*machine)
                    .and_then(|m| m.sessions.iter().find(|s| &s.id == session_id))
                {
                    Some(s) => {
                        let sid = s.id.clone();
                        let sid_rename = sid.clone();
                        let sid_delete = sid.clone();
                        let title0 = s.title.clone();
                        let machine = *machine;
                        v_flex()
                            .gap_1()
                            .child(Label::new(truncate(&title0, 30)).text_sm())
                            .child(Button::new("ctx-rename").small().label("重命名").on_click(
                                cx.listener(move |this, _ev, window, cx| {
                                    this.selected = Some(Selected::Session {
                                        machine,
                                        id: sid_rename.clone(),
                                    });
                                    this.renaming_session = Some((machine, sid_rename.clone()));
                                    this.context_menu = None;
                                    this.title_input.update(cx, |s, cx| {
                                        s.set_value(&title0, window, cx);
                                    });
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("ctx-delete")
                                    .small()
                                    .label("删除会话")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let sid = sid_delete.clone();
                                        this.context_menu = None;
                                        this.confirm_delete_session(window, cx, machine, sid);
                                    })),
                            )
                            .into_any_element()
                    }
                    None => div().into_any_element(),
                },
                ContextMenuTarget::Workflow { engine } => {
                    let wi = *engine;
                    let title0 = self
                        .workflows
                        .get(wi)
                        .map(|w| w.session.read().unwrap().title.clone())
                        .unwrap_or_default();
                    v_flex()
                        .gap_1()
                        .child(Label::new(truncate(&title0, 30)).text_sm())
                        .child(
                            Button::new("ctx-wf-rename")
                                .small()
                                .label("重命名")
                                .on_click(cx.listener(move |this, _ev, _window, cx| {
                                    this.selected = Some(Selected::Workflow { engine: wi });
                                    this.renaming_workflow = Some(wi);
                                    this.context_menu = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("ctx-wf-delete")
                                .small()
                                .label("删除工作流")
                                .on_click(cx.listener(move |this, _ev, window, cx| {
                                    this.context_menu = None;
                                    this.confirm_delete_workflow(window, cx, wi);
                                })),
                        )
                        .into_any_element()
                }
            })
            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                // 阻止冒泡：避免触发底层面板处理
                cx.stop_propagation();
            })
    }

    // ---- 设置浮窗 ----

    fn render_settings_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("settings-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.show_settings = false;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .id("settings-card")
                    .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                        cx.stop_propagation();
                    })
                    .w_full()
                    .max_w(px(880.))
                    .h_full()
                    .max_h(px(620.))
                    .overflow_hidden()
                    .bg(cx.theme().popover)
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
            )
            .when(self.show_add_machine_form, |overlay| {
                overlay.child(self.render_add_machine_dialog(window, cx))
            })
            .when(self.show_quick_command_form, |overlay| {
                overlay.child(self.render_quick_command_dialog(window, cx))
            })
            .when(self.show_skill_form, |overlay| {
                overlay.child(self.render_skill_dialog(window, cx))
            })
            .when(self.show_template_form, |overlay| {
                overlay.child(self.render_template_dialog(window, cx))
            })
            .when(self.skill_action_dialog.is_some(), |overlay| {
                overlay.child(self.render_skill_action_dialog(window, cx))
            })
            // skills 弹窗（盖在设置浮窗之上）
            .when(self.machines.iter().any(|m| m.show_skills.is_some()), |o| {
                o.child(self.render_skills_dialog(window, cx))
            })
    }

    /// skills 弹窗：可滚动显示某 agent 的 skills 列表。
    fn render_skills_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (mi, agent, skills) = self
            .machines
            .iter()
            .enumerate()
            .find_map(|(i, m)| {
                m.show_skills
                    .as_ref()
                    .map(|(_, a)| (i, a.clone(), m.skills.clone()))
            })
            .unwrap_or((0, String::new(), Vec::new()));
        let _ = mi;
        let mut list = v_flex()
            .gap_1()
            .flex_1()
            .id("skills-list")
            .overflow_y_scroll()
            .p_1();
        if skills.is_empty() {
            list = list.child(
                Label::new("（无 skills）")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
        }
        for s in &skills {
            let s = s.clone();
            list = list.child(
                div()
                    .p_1()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(Label::new(s).text_sm()),
            );
        }
        div()
            .id("skills-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("skills-card")
                    .w(px(480.))
                    .h(px(420.))
                    .overflow_hidden()
                    .bg(cx.theme().popover)
                    .rounded_lg()
                    .shadow_lg()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .child(
                                Label::new(format!("Skills · {agent}"))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(div().flex_1())
                            .child(Button::new("skills-close").small().label("✕").on_click(
                                cx.listener(|this, _ev, _window, cx| {
                                    for m in this.machines.iter_mut() {
                                        m.show_skills = None;
                                    }
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(list),
            )
    }

    fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-nav")
            .w(px(190.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .items_center()
                    .px_1()
                    .py_1()
                    .child(
                        Label::new("设置")
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-close")
                            .small()
                            .ghost()
                            .label("✕")
                            .tooltip("关闭设置")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.show_settings = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(self.settings_nav_item(
                SettingsCategory::Machines,
                "cat-machines",
                "机器管理",
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::Orchestrator,
                "cat-orch",
                "编排智能体",
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::QuickCommands,
                "cat-qc",
                "快捷指令",
                cx,
            ))
            .child(self.settings_nav_item(SettingsCategory::Skills, "cat-skills", "技能管理", cx))
            .child(self.settings_nav_item(SettingsCategory::Templates, "cat-tpl", "工作流模板", cx))
            .child(div().flex_1())
            .child(
                Button::new("settings-back")
                    .small()
                    .label("关闭")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.show_settings = false;
                        cx.notify();
                    })),
            )
    }

    fn settings_nav_item(
        &self,
        target: SettingsCategory,
        id: &str,
        label: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id_owned = id.to_string();
        let label_owned = label.to_string();
        let btn = Button::new(id_owned)
            .small()
            .label(label_owned)
            .on_click(cx.listener(move |this, _ev, _window, cx| {
                this.settings_category = target;
                cx.notify();
            }));
        if self.settings_category == target {
            btn.primary()
        } else {
            btn
        }
    }

    fn render_settings_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_4()
            .overflow_y_scroll()
            .child(match self.settings_category {
                SettingsCategory::Machines => self.render_machines_settings(cx),
                SettingsCategory::Orchestrator => self.render_orchestrator_settings(cx),
                SettingsCategory::QuickCommands => self.render_quick_commands_settings(cx),
                SettingsCategory::Skills => self.render_skills_settings(cx),
                SettingsCategory::Templates => self.render_templates_settings(cx),
            })
    }

    /// 机器管理：纵向堆叠卡片式（机器头 + URL + 每 agent 一行 + 底部「+」表单）。
    fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines = self
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut item = v_flex()
                    .gap_1()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(&m.config.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(machine_status_badge(
                                (&m.status, m.notice.as_deref()),
                                cx.theme().success,
                                cx.theme().danger,
                                cx.theme().warning,
                            ))
                            .child(div().flex_1())
                            .child(
                                Button::new(format!("restart-m-{i}"))
                                    .small()
                                    .label("重连")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = this.machines[i].config.name.clone();
                                        this.confirm_reconnect_machine(window, cx, i, name);
                                    })),
                            )
                            .child(
                                Button::new(format!("remove-{i}"))
                                    .small()
                                    .label("移除")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = this.machines[i].config.name.clone();
                                        this.confirm_remove_machine(window, cx, i, name);
                                    })),
                            ),
                    )
                    .child(
                        Label::new(&m.config.url)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate(),
                    );
                for a in &m.agents {
                    let available = a.available;
                    let agent = a.name.clone();
                    let agent_skills = agent.clone();
                    let agent_restart = agent.clone();
                    item = item.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new(format!(
                                    "{} · {}",
                                    agent,
                                    if available { "可用" } else { "不可用" }
                                ))
                                .text_sm(),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new(format!("skills-{i}-{agent}"))
                                    .small()
                                    .label("skills")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let agent = agent_skills.clone();
                                        this.fetch_agent_skills(window, cx, i, agent.clone());
                                        if let Some(m) = this.machines.get_mut(i) {
                                            m.show_skills = Some((i, agent));
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(format!("restart-agent-{i}-{agent}"))
                                    .small()
                                    .label("重启")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let agent = agent_restart.clone();
                                        this.confirm_restart_agent(window, cx, i, agent);
                                    })),
                            ),
                    );
                }
                item
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "机器管理",
                        "接入 / 移除机器；每台机器自动发现 ACP agent，可查看 skills、重启",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-add-open")
                            .small()
                            .primary()
                            .label("+")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.show_add_machine_form = true;
                                this.machine_form_error = None;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.machines.is_empty(), |view| {
                view.child(
                    Label::new("还没有注册机器。点击右上角 + 添加 amux server。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(machines)
            .into_any()
    }

    fn render_add_machine_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut card = v_flex()
            .id("add-machine-card")
            .relative()
            .w(px(460.))
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                h_flex()
                    .items_center()
                    .child(Label::new("添加机器").font_weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .child(
                        Button::new("add-machine-close")
                            .small()
                            .label("✕")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.close_add_machine_form(window, cx);
                            })),
                    ),
            )
            .child(
                Label::new("注册一台 amux server，保存后会立即建立连接。")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                Label::new("名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_name_input))
            .child(
                Label::new("连接地址")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_url_input))
            .child(
                Label::new("Token")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_token_input));
        if let Some(error) = &self.machine_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("add-machine-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_add_machine_form(window, cx);
                        })),
                )
                .child(
                    Button::new("add-machine-submit")
                        .small()
                        .primary()
                        .label("添加机器")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            let name = this.machine_name_input.read(cx).value().to_string();
                            let url = this.machine_url_input.read(cx).value().to_string();
                            let token = this.machine_token_input.read(cx).value().to_string();
                            if this.add_machine(window, cx, name, url, token) {
                                this.close_add_machine_form(window, cx);
                            }
                        })),
                ),
        );
        div()
            .id("add-machine-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("add-machine-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_add_machine_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    fn render_quick_command_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.qc_edit_target.is_some() {
            "编辑快捷指令"
        } else {
            "新增快捷指令"
        };
        let mut card = v_flex()
            .id("quick-command-card")
            .relative()
            .w(px(520.))
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(Label::new(title).font_weight(FontWeight::SEMIBOLD))
            .child(
                Label::new("指令名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.qc_name_input))
            .child(
                Label::new("指令内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.qc_prompt_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("qc-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_quick_command_form(window, cx);
                        })),
                )
                .child(
                    Button::new("qc-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_quick_command(window, cx);
                        })),
                ),
        );
        div()
            .id("quick-command-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("quick-command-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_quick_command_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    fn render_skill_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.skill_edit_target.is_some() {
            "编辑技能"
        } else {
            "新增技能"
        };
        let mut card = v_flex()
            .id("skill-form-card")
            .relative()
            .w(px(520.))
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(Label::new(title).font_weight(FontWeight::SEMIBOLD))
            .child(
                Label::new("技能名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.skill_name_input))
            .child(
                Label::new("技能描述")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.skill_desc_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("skill-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_skill_form(window, cx);
                        })),
                )
                .child(
                    Button::new("skill-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_skill(window, cx);
                        })),
                ),
        );
        div()
            .id("skill-form-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("skill-form-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_skill_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    fn render_skill_action_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((skill, action)) = &self.skill_action_dialog else {
            return div().into_any_element();
        };
        let action = *action;
        let mut targets = Vec::new();
        for (machine_idx, machine) in self.machines.iter().enumerate() {
            for agent in &machine.agents {
                let skill = skill.clone();
                let agent_name = agent.name.clone();
                let label = format!("{} · {}", machine.config.name, agent.name);
                targets.push(
                    Button::new(format!(
                        "skill-target-{machine_idx}-{}-{}",
                        action.label(),
                        agent.name
                    ))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.skill_action_dialog = None;
                        this.manage_skill_on_agent(
                            window,
                            cx,
                            machine_idx,
                            agent_name.clone(),
                            skill.clone(),
                            action,
                        );
                        cx.notify();
                    })),
                );
            }
        }
        let mut card = v_flex()
            .id("skill-action-card")
            .relative()
            .w(px(520.))
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(Label::new(format!("{}技能", action.label())).font_weight(FontWeight::SEMIBOLD))
            .child(
                Label::new(format!("选择执行技能「{}」的机器和 agent", skill.name))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
        if targets.is_empty() {
            card = card.child(
                Label::new("当前没有可用的机器 agent。")
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        } else {
            card = card.child(h_flex().gap_1().flex_wrap().children(targets));
        }
        card = card.child(
            h_flex().justify_end().child(
                Button::new("skill-action-cancel")
                    .small()
                    .label("取消")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.skill_action_dialog = None;
                        cx.notify();
                    })),
            ),
        );
        div()
            .id("skill-action-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("skill-action-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.skill_action_dialog = None;
                        cx.notify();
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_template_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.tpl_edit_target.is_some() {
            "编辑工作流模板"
        } else {
            "新增工作流模板"
        };
        let mut card = v_flex()
            .id("template-form-card")
            .relative()
            .w(px(560.))
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(Label::new(title).font_weight(FontWeight::SEMIBOLD))
            .child(
                Label::new("模板名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.tpl_name_input))
            .child(
                Label::new("模板内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.tpl_desc_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("template-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_template_form(window, cx);
                        })),
                )
                .child(
                    Button::new("template-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_template(window, cx);
                        })),
                ),
        );
        div()
            .id("template-form-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("template-form-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_template_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_api_format = match self.orch_api_format {
            ApiFormat::ChatCompletions => Some(0),
            ApiFormat::Responses => Some(1),
            ApiFormat::Messages => Some(2),
        };
        let api_format_options = RadioGroup::horizontal("orch-api-format")
            .children(["chat_completions", "responses", "messages"])
            .selected_index(selected_api_format)
            .on_click(cx.listener(|this, selected: &usize, _window, cx| {
                this.orch_api_format = match *selected {
                    0 => ApiFormat::ChatCompletions,
                    1 => ApiFormat::Responses,
                    2 => ApiFormat::Messages,
                    _ => return,
                };
                this.orchestrator_form_error = None;
                this.orchestrator_form_status = None;
                cx.notify();
            }));
        let mut form = v_flex()
            .gap_1()
            .p_3()
            .bg(cx.theme().muted)
            .rounded_md()
            .child(
                Label::new("API 格式")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(api_format_options)
            .child(
                Label::new("Base URL")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_base_input))
            .child(
                Label::new("API Key")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_key_input))
            .child(
                Label::new("模型名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_model_input));
        if let Some(error) = &self.orchestrator_form_error {
            form = form.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        if let Some(status) = &self.orchestrator_form_status {
            form = form.child(
                Label::new(status.clone())
                    .text_sm()
                    .text_color(cx.theme().success),
            );
        }
        form = form.child(
            h_flex().justify_end().child(
                Button::new("orch-save")
                    .small()
                    .primary()
                    .label("保存")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.save_orchestrator(window, cx);
                    })),
            ),
        );
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "编排智能体",
                "配置工作流编排使用的大模型供应商连接信息",
                cx.theme().muted_foreground,
            ))
            .child(form)
            .into_any()
    }

    fn render_quick_commands_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commands = self.store.list_quick_commands();
        let items = commands
            .iter()
            .map(|command| {
                let edit = command.clone();
                let delete = command.name.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(&command.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(&command.prompt)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .line_clamp(2),
                            ),
                    )
                    .child(
                        Button::new(format!("qc-edit-{}", command.name))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_quick_command_form(
                                    window,
                                    cx,
                                    Some((edit.name.clone(), edit.prompt.clone())),
                                );
                            })),
                    )
                    .child(
                        Button::new(format!("qc-del-{}", command.name))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.confirm_remove_quick_command(window, cx, delete.clone());
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "快捷指令",
                        "自定义快捷指令，输入区上方一键发送",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("qc-add-open")
                            .small()
                            .primary()
                            .label("+")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_quick_command_form(window, cx, None);
                            })),
                    ),
            )
            .when(commands.is_empty(), |view| {
                view.child(
                    Label::new("还没有快捷指令。添加后会显示在会话输入区上方。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    fn render_skills_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let skills = self.store.list_skills();
        let items = skills
            .iter()
            .map(|skill| {
                let edit = skill.clone();
                let delete = skill.name.clone();
                let mut actions = Vec::new();
                for action in [
                    SkillAction::Install,
                    SkillAction::Update,
                    SkillAction::Uninstall,
                ] {
                    let skill = skill.clone();
                    actions.push(
                        Button::new(format!("skill-action-{}-{}", action.label(), skill.name))
                            .small()
                            .label(action.label())
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.open_skill_action_dialog(skill.clone(), action, cx);
                            })),
                    );
                }
                v_flex()
                    .gap_2()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Label::new(&skill.name)
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM),
                                    )
                                    .child(
                                        Label::new(&skill.description)
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .line_clamp(2),
                                    ),
                            )
                            .child(
                                Button::new(format!("skill-edit-{}", skill.name))
                                    .small()
                                    .label("编辑")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.open_skill_form(window, cx, Some(edit.clone()));
                                    })),
                            )
                            .child(
                                Button::new(format!("skill-del-{}", skill.name))
                                    .small()
                                    .label("删除")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.confirm_remove_skill(window, cx, delete.clone());
                                    })),
                            ),
                    )
                    .child(h_flex().gap_1().flex_wrap().children(actions))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "技能管理",
                        "已安装 / 管理的 ACP skills 清单",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("skill-add-open")
                            .small()
                            .primary()
                            .label("+")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_skill_form(window, cx, None);
                            })),
                    ),
            )
            .when(skills.is_empty(), |view| {
                view.child(
                    Label::new("还没有技能。点击右上角 + 添加技能说明。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    fn render_templates_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        let items = templates
            .iter()
            .map(|template| {
                let edit = template.clone();
                let delete = template.name.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(&template.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(&template.plan)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .line_clamp(2),
                            ),
                    )
                    .child(
                        Button::new(format!("tpl-edit-{}", template.name))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_template_form(window, cx, Some(edit.clone()));
                            })),
                    )
                    .child(
                        Button::new(format!("tpl-del-{}", template.name))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.confirm_remove_template(window, cx, delete.clone());
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "工作流模板",
                        "模板的 plan 会作为工作流编排的系统指令注入",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("tpl-add-open")
                            .small()
                            .primary()
                            .label("+")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_template_form(window, cx, None);
                            })),
                    ),
            )
            .when(templates.is_empty(), |view| {
                view.child(
                    Label::new("还没有工作流模板。点击右上角 + 创建一个可复用计划。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    /// 设置项标题。
    fn settings_header(
        &self,
        title: &str,
        subtitle: &str,
        muted_foreground: Hsla,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(title).text_lg().font_weight(FontWeight::MEDIUM))
            .child(Label::new(subtitle).text_sm().text_color(muted_foreground))
    }
}

impl AmuxApp {
    /// 重连机器：重建其 WS 连接视图。
    fn reconnect_machine(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        if idx >= self.machines.len() {
            return;
        }
        let cfg = self.machines[idx].config.clone();
        let client = WsClient::connect_with_token(machine_ws_url(&cfg), cfg.token.clone());
        self.machines[idx].client = client.clone();
        self.machines[idx].connection_epoch = self.machines[idx].connection_epoch.saturating_add(1);
        self.machines[idx].status = MachineStatus::Connecting;
        self.machines[idx].notice = None;
        self.machines[idx].views.clear();
        let t = self.spawn_machine_tasks(window, cx, idx, client);
        self._tasks.push(t);
        self.fetch_agents(idx, window, cx);
        self.refresh_sessions(idx, window, cx);
        cx.notify();
    }
}

/// 会话列表项（统一最近活跃排序）。
enum SessionListItem {
    Session { machine: usize, meta: SessionMeta },
    Workflow { idx: usize },
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.render_panel(window, cx);
        let title_bar = h_flex()
            .id("title-bar")
            .h(px(36.))
            .gap_2()
            .items_center()
            .pl(px(76.))
            .pr(px(12.))
            .bg(cx.theme().title_bar)
            .border_b_1()
            .border_color(cx.theme().title_bar_border)
            .on_mouse_down(MouseButton::Left, |_e, window, _cx| {
                window.start_window_move();
            })
            .child(
                Label::new("amux")
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(div().flex_1());

        let mut main_row = h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_sidebar(window, cx))
            .child(self.render_main(window, cx));
        if let Some(p) = panel {
            main_row = main_row.child(p);
        }
        let mut root = v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .child(title_bar)
            .child(main_row);
        if let Some(menu) = &self.context_menu {
            root = root.child(self.render_context_menu(menu, window, cx));
        }
        if self.show_settings {
            root = root.child(self.render_settings_overlay(window, cx));
        }
        if let Some(layer) = Root::render_sheet_layer(window, cx) {
            root = root.child(layer);
        }
        if let Some(layer) = Root::render_dialog_layer(window, cx) {
            root = root.child(layer);
        }
        if let Some(layer) = Root::render_notification_layer(window, cx) {
            root = root.child(layer);
        }
        root.into_any()
    }
}

/// 在 GUI 的 tokio runtime 上执行编排引擎任务（rig/reqwest 的 LLM 调用需要 tokio reactor；
/// GPUI 主线程无 runtime，docs/DESIGN.md §7「异步模型」）。
async fn run_engine_on_tokio<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::ws::runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.ok()
}

/// 消息/详情时间戳：本地时区；当日只显示时刻，跨天附日期（原实现为 UTC 当日
/// 时分秒——跨天消息时间倒序且无法分辨日期）。
fn format_timestamp(timestamp_ms: u64) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(timestamp_ms as i64) else {
        return String::new();
    };
    let ts = ts.to_zoned(jiff::tz::TimeZone::system());
    let now = jiff::Zoned::now();
    let time = ts.strftime("%H:%M:%S").to_string();
    if ts.date() == now.date() {
        time
    } else {
        format!("{} {}", ts.strftime("%m-%d").to_string(), time)
    }
}
