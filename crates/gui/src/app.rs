//! amux 主视图（docs/DESIGN.md §7 / PRD §3、§4）。
//! 三面板布局 + 多机器 + 工作流编排 + 设置浮窗（五分类）：
//! - 左侧：会话列表（按最近活跃排序；含工作流会话与折叠的子会话）+ 顶部「+」新建会话入口
//!   + 底部设置入口
//! - 中间：上方对话流（用户消息 + agent 完整输出气泡，非流式）+ 下方进行中活动条（一条或无）
//!   + 快捷指令栏 + 输入区（多行、@ 引用、拖拽文件、粘贴图片、语音）+ 右侧竖排悬浮按钮
//! - 右侧：上下文面板（默认折叠，悬浮按钮展开 diff / 会话详情 / 会话活动历史）
//! - 设置浮窗：机器管理（含 agent 默认模型与 skills 列表）/ 编排 agent / 快捷指令 / Skills / 工作流模板
//! - 工作流：GUI 本地工作流会话（rig 单 turn），子会话 idle 自动推进，取消/继续/介入，
//!   状态持久化于 GUI 本地，重开后恢复

use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    collapsible::Collapsible,
    dialog::DialogButtonProps,
    input::{Input, InputState},
    label::Label,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    text::TextView,
    WindowExt, *,
};

use serde_json::json;

use protocol::{
    Activity, ContentBlock, DialogItem, GitDiffFile, GitStatusResult, MachineInfo,
    PassthroughEvent, SessionMeta, SessionState,
};

use crate::aggregate::SessionView;
use crate::config::{
    machine_ws_url, ConfigStore, MachineConfig, OrchestratorConfig, QuickCommand, SkillEntry,
    WorkflowTemplate,
};
use crate::display::{activity_display, info_row, machine_status_badge, short_cwd};
use crate::logic::{
    compose_prompt, merge_session_window, parse_at_references, path_attachment, read_path_context,
    InputAttachment,
};
use crate::text::{block_text, one_line, truncate};
use crate::workflow::{MachineSummary, OrcBackend, OrcMsg, RigBackend, WorkflowEngine};
use crate::ws::{Notification, WsClient};

// ---- 视图状态 ----

/// 会话列表惰性分页窗口大小（PRD §4.1.1：首次只取最近活跃一窗，滚动加载更早）。
const SESSION_WINDOW: usize = 50;

/// 右侧面板（默认折叠，悬浮按钮展开）。
#[derive(Clone, Copy, PartialEq)]
enum Panel {
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

/// 会话列表项（统一最近活跃排序，docs/PRD §4.1.1）。
enum SessionListItem {
    /// 普通会话（机器下标 + 元数据）
    Session { machine: usize, meta: SessionMeta },
    /// 工作流会话（工作流，下标）
    Workflow { idx: usize },
}

/// diff 渲染模式（PRD §3.5：side-by-side 或 inline）。
#[derive(Clone, Copy, PartialEq)]
enum DiffMode {
    Inline,
    SideBySide,
}

/// 新会话创建模式。
#[derive(Clone, Copy, PartialEq)]
enum NewSessionMode {
    Direct,
    Workflow,
}

/// 单机器视图：独立连接 + 会话列表 + 各会话聚合视图（透传事件聚合，docs/DESIGN.md §5.1）+ diff 状态。
struct MachineView {
    config: MachineConfig,
    client: WsClient,
    status: String,
    /// get_info 结果（agent 发现 + 默认模型）
    info: Option<MachineInfo>,
    sessions: Vec<SessionMeta>,
    /// 会话列表惰性加载（PRD §4.1.1）：是否还有更早 + 下次 before 游标
    sessions_has_more: bool,
    sessions_next_before: Option<u64>,
    selected: Option<String>,
    /// 各会话的聚合视图（输出收敛 / 活动合并 / busy 派生；显示取当前会话）
    views: std::collections::HashMap<String, SessionView>,
    /// 惰性加载游标：下一次"加载更早消息"应传的 before
    dialog_before: usize,
    /// 是否还有更早历史
    dialog_has_more: bool,
    /// 当前查看 harness 的 skills 列表
    skills: Vec<String>,
    skills_harness: Option<String>,
    /// diff 状态（文件列表 + 统计）
    diff_status: Option<GitStatusResult>,
    diff_files: Vec<GitDiffFile>,
    diff_path: Option<String>,
    diff_mode: DiffMode,
}

impl MachineView {
    /// 当前选中会话的聚合视图（显示用）。
    fn selected_view(&self) -> Option<&SessionView> {
        self.selected.as_ref().and_then(|sid| self.views.get(sid))
    }

    fn new(config: MachineConfig) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            config,
            client: WsClient::connect(url),
            status: "连接中…".into(),
            info: None,
            sessions: Vec::new(),
            sessions_has_more: false,
            sessions_next_before: None,
            selected: None,
            views: std::collections::HashMap::new(),
            dialog_before: 0,
            dialog_has_more: false,
            skills: Vec::new(),
            skills_harness: None,
            diff_status: None,
            diff_files: Vec::new(),
            diff_path: None,
            diff_mode: DiffMode::Inline,
        }
    }
}

/// 当前选中的会话（普通会话 或 工作流会话）。
#[derive(Clone, PartialEq)]
enum Selected {
    Session { machine: usize, id: String },
    Workflow { engine: usize },
}

/// 右键菜单目标：普通会话 或 编排（工作流）会话。
#[derive(Clone)]
enum ContextMenuTarget {
    Session { machine: usize, session_id: String },
    Workflow { engine: usize },
}

/// 右键会话弹出的操作菜单（PRD §3.1：删除 / 重命名）。
struct SessionContextMenu {
    target: ContextMenuTarget,
    title: String,
    x: f32,
    y: f32,
}

/// Skills 安装目标选择对话框（PRD §3.6）：选机器与 agent，确认后创建会话安装。
#[derive(Clone)]
struct SkillInstallDialog {
    skill: SkillEntry,
    machine: Option<usize>,
    harness: Option<String>,
}

/// 工作流会话对话条目缓存（键：会话 id + 转录长度）。
type WorkflowDialogCache = Option<((String, usize), Vec<DialogItem>)>;

pub struct AmuxApp {
    store: Arc<ConfigStore>,
    machines: Vec<MachineView>,
    /// GUI 本地编排引擎（docs/DESIGN.md §10）
    workflows: Vec<WorkflowEngine>,
    workflow_dir: PathBuf,
    selected: Option<Selected>,
    panel: Option<Panel>,
    /// 右侧上下文面板展开时窗口向右扩展的物理像素（关闭时收回，docs/DESIGN.md §7）
    panel_delta_px: f32,
    show_settings: bool,
    settings_category: SettingsCategory,
    new_session_mode: NewSessionMode,
    /// 输入区
    input_state: Entity<InputState>,
    input_attachments: Vec<InputAttachment>,
    voice_recording: bool,
    session_cwd_input: Entity<InputState>,
    workflow_input: Entity<InputState>,
    /// 添加机器表单：名称 / 连接地址 / Token（分栏输入，不混在一个输入框）
    machine_name_input: Entity<InputState>,
    machine_url_input: Entity<InputState>,
    machine_token_input: Entity<InputState>,
    /// 设置表单输入
    qc_name_input: Entity<InputState>,
    qc_prompt_input: Entity<InputState>,
    skill_name_input: Entity<InputState>,
    skill_desc_input: Entity<InputState>,
    tpl_name_input: Entity<InputState>,
    tpl_desc_input: Entity<InputState>,
    /// 编排 agent wire API 单选（"chat" | "responses"）
    orch_wire_api: String,
    orch_base_input: Entity<InputState>,
    orch_key_input: Entity<InputState>,
    orch_model_input: Entity<InputState>,
    model_input: Entity<InputState>,
    title_input: Entity<InputState>,
    /// 快捷指令编辑目标（None = 新增）
    qc_edit_target: Option<String>,
    /// Skills 编辑目标
    skill_edit_target: Option<String>,
    /// 模板编辑目标
    tpl_edit_target: Option<String>,
    /// 右键会话操作菜单
    context_menu: Option<SessionContextMenu>,
    /// 正在重命名的会话（机器下标, 会话 id）
    renaming_session: Option<(usize, String)>,
    /// 正在重命名的工作流会话（引擎下标）
    renaming_workflow: Option<usize>,
    /// 新会话视图（PRD §4.1.2）：选中的机器与 agent
    new_session_machine: Option<usize>,
    new_session_harness: Option<String>,
    /// Skills 安装目标选择对话框（None = 未打开，PRD §3.6）
    skill_install_dialog: Option<SkillInstallDialog>,
    /// 创建工作流会话时的提示（如编排 agent 未配置 API）
    workflow_error: Option<String>,
    /// 已选择的工作流模板（新会话视图；选择后由「创建工作流会话」按钮统一创建）
    workflow_template: Option<WorkflowTemplate>,
    /// 对话流滚动句柄（打开会话/新消息自动滚到底部）
    dialog_scroll: ScrollHandle,
    /// 工作流会话对话条目缓存（键：会话 id + 转录长度；避免每次渲染重复克隆）
    workflow_dialog_cache: std::cell::RefCell<WorkflowDialogCache>,
    /// 活动历史滚动句柄（打开面板自动滚到底部）
    activities_scroll: ScrollHandle,
    /// 活动历史当前渲染条数（滚动加载：只渲染最近 N 条，向上加载更多）
    activities_limit: usize,
    /// 活动历史中已展开的长条目
    expanded_activities: std::collections::HashSet<String>,
    /// 工作流会话对话流当前渲染条数（滚动加载：只渲染最近 N 条，向上加载更多）
    workflow_dialog_limit: usize,
    /// 已展开子会话的工作流（"▾"折叠指示，PRD §4.1.1）
    expanded_workflows: std::collections::HashSet<usize>,
    _tasks: Vec<Task<()>>,
}

impl AmuxApp {
    pub fn new(store: Arc<ConfigStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入消息，Ctrl+Enter 发送；@ 引用文件/目录作为上下文")
                .multi_line(true)
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
        let machine_token_input = cx.new(|cx| InputState::new(window, cx).placeholder("Token"));
        let qc_name_input = cx.new(|cx| InputState::new(window, cx).placeholder("指令名"));
        let qc_prompt_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("提示词（发给 agent 的一段话）"));
        let skill_name_input = cx.new(|cx| InputState::new(window, cx).placeholder("名称"));
        let skill_desc_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("描述（仓库/资源 URL 或安装方法）"));
        let tpl_name_input = cx.new(|cx| InputState::new(window, cx).placeholder("模板名"));
        let tpl_desc_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("自然语言工作流描述")
                .multi_line(true)
        });
        let orch_base_input = cx.new(|cx| InputState::new(window, cx).placeholder("Base URL"));
        let orch_key_input = cx.new(|cx| InputState::new(window, cx).placeholder("API key"));
        let orch_model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("模型，如 gpt-4o-mini"));
        let model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("默认模型（可留空）"));
        let title_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));

        let machines = store
            .list_machines()
            .into_iter()
            .map(|config| MachineView::new(config.clone()))
            .collect::<Vec<_>>();

        let orchestrator = store.orchestrator();
        let workflow_dir = store.workflow_dir();

        let mut app = Self {
            store,
            machines,
            workflows: Vec::new(),
            workflow_dir,
            selected: None,
            panel: None,
            panel_delta_px: 0.0,
            show_settings: false,
            settings_category: SettingsCategory::Machines,
            new_session_mode: NewSessionMode::Direct,
            input_state,
            input_attachments: Vec::new(),
            voice_recording: false,
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
            orch_wire_api: orchestrator.wire_api.clone(),
            orch_base_input,
            orch_key_input,
            orch_model_input,
            model_input,
            title_input,
            qc_edit_target: None,
            skill_edit_target: None,
            tpl_edit_target: None,
            context_menu: None,
            renaming_session: None,
            renaming_workflow: None,
            new_session_machine: None,
            new_session_harness: None,
            skill_install_dialog: None,
            workflow_error: None,
            workflow_template: None,
            dialog_scroll: ScrollHandle::new(),
            workflow_dialog_cache: std::cell::RefCell::new(None),
            activities_scroll: ScrollHandle::new(),
            activities_limit: 100,
            expanded_activities: std::collections::HashSet::new(),
            workflow_dialog_limit: 50,
            expanded_workflows: std::collections::HashSet::new(),
            _tasks: Vec::new(),
        };
        // 预填编排配置表单
        app.fill_orchestrator_form(&orchestrator, window, cx);
        app.spawn_notify_tasks(window, cx);
        // 启动即拉取各机器会话列表与 get_info；恢复工作流会话
        for i in 0..app.machines.len() {
            app.refresh_sessions(i, window, cx);
            app.fetch_info(i, window, cx);
        }
        app.restore_workflows(window, cx);
        app
    }

    fn fill_orchestrator_form(
        &self,
        cfg: &OrchestratorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.orch_base_input.update(cx, |s, cx| {
            s.set_value(cfg.base_url.clone(), window, cx);
        });
        self.orch_key_input.update(cx, |s, cx| {
            s.set_value(cfg.api_key.clone(), window, cx);
        });
        self.orch_model_input.update(cx, |s, cx| {
            s.set_value(cfg.model.clone(), window, cx);
        });
    }

    /// wire API 单选（DESIGN §9：chat / responses）。
    fn render_wire_api_radio(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.orch_wire_api.as_str() {
            "responses" => 1,
            _ => 0,
        };
        let view = cx.entity();
        RadioGroup::horizontal("orch-wire-api")
            .selected_index(Some(selected))
            .child(Radio::new("wire-chat").label("chat").on_click({
                let view = view.clone();
                move |checked, _window, cx| {
                    if *checked {
                        view.update(cx, |this, cx| {
                            this.orch_wire_api = "chat".into();
                            cx.notify();
                        });
                    }
                }
            }))
            .child(Radio::new("wire-responses").label("responses").on_click({
                let view = view.clone();
                move |checked, _window, cx| {
                    if *checked {
                        view.update(cx, |this, cx| {
                            this.orch_wire_api = "responses".into();
                            cx.notify();
                        });
                    }
                }
            }))
    }

    // ---- 辅助 ----

    fn machine(&self, idx: usize) -> Option<&MachineView> {
        self.machines.get(idx)
    }

    fn machine_mut(&mut self, idx: usize) -> Option<&mut MachineView> {
        self.machines.get_mut(idx)
    }

    /// 选中机器（设置页/新会话默认机器）
    fn active_machine(&self) -> Option<usize> {
        match &self.selected {
            Some(Selected::Session { machine, .. }) => Some(*machine),
            _ => self
                .machines
                .iter()
                .position(|m| m.status == "已连接" || !m.status.starts_with("连接失败"))
                .or(Some(0))
                .filter(|_| !self.machines.is_empty()),
        }
    }

    /// 当前查看的会话元数据（普通或编排）。
    fn selected_meta(&self) -> Option<SessionMeta> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.sessions.iter().find(|s| &s.id == id))
                .cloned(),
            Some(Selected::Workflow { engine }) => {
                let wf = self.workflows.get(*engine)?;
                Some(SessionMeta {
                    id: wf.session.id.clone(),
                    harness: "编排".into(),
                    cwd: String::new(),
                    model: None,
                    state: wf.session.state,
                    interrupted: false,
                    title: wf.session.title.clone(),
                    created_at: wf.session.created_at,
                    last_event_at: wf.session.updated_at,
                })
            }
            None => None,
        }
    }

    // ---- 通知路由 ----

    fn spawn_notify_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.machines.len() {
            let mut notify_rx = self.machines[i].client.subscribe();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                while let Ok(n) = notify_rx.recv().await {
                    let _ = this.update_in(cx, |this, window, cx| {
                        Self::on_notify(this, window, cx, i, &n);
                    });
                }
            });
            self._tasks.push(t);
        }
    }

    fn on_notify(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &Notification,
    ) {
        let Some(m) = this.machines.get_mut(idx) else {
            return;
        };
        match n.method.as_str() {
            // 透传事件（docs/DESIGN.md §5.1）：GUI 应用聚合对话/活动并派生 busy/idle
            "passthrough" => {
                let Some(sid) = n
                    .params
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                else {
                    return;
                };
                let Some(ev) = n
                    .params
                    .get("event")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<PassthroughEvent>(v).ok())
                else {
                    return;
                };
                // busy/idle：采信 session_info_update + turn 边界（docs/DESIGN.md §5.1）
                let busy = match &ev {
                    PassthroughEvent::TurnStarted { .. } => true,
                    PassthroughEvent::TurnEnded { .. } => false,
                    PassthroughEvent::SessionInfo { state: Some(s), .. } => {
                        *s == SessionState::Busy
                    }
                    _ => m
                        .sessions
                        .iter()
                        .find(|s| s.id == sid)
                        .map(|s| s.state == SessionState::Busy)
                        .unwrap_or(false),
                };
                if let Some(s) = m.sessions.iter_mut().find(|s| s.id == sid) {
                    s.state = if busy {
                        SessionState::Busy
                    } else {
                        SessionState::Idle
                    };
                }
                // 聚合进该会话视图（所有会话都聚合，工作流子会话也能取到输出）
                let view = m.views.entry(sid.clone()).or_default();
                // 回显去重（docs/DESIGN.md §7.1：本机已本地渲染的用户消息，回显跳过，
                // 避免重复气泡；其他客户端发来的消息照常显示）
                let is_echo = matches!(
                    &ev,
                    PassthroughEvent::UserMessage { content, .. }
                        if crate::aggregate::is_user_message_echo(view, content)
                );
                if !is_echo {
                    crate::aggregate::merge_event(view, &ev);
                }
                // 当前打开会话：新输出/用户消息自动滚到底部（实时渲染）
                if m.selected.as_deref() == Some(sid.as_str())
                    && matches!(
                        ev,
                        PassthroughEvent::OutputChunk { .. } | PassthroughEvent::UserMessage { .. }
                    )
                {
                    this.dialog_scroll.scroll_to_bottom();
                }
                // 工作流自动推进：子会话变 idle（GUI 派生状态）→ 注入完成情况并推进
                let child_idle = matches!(ev, PassthroughEvent::TurnEnded { .. })
                    || matches!(
                        &ev,
                        PassthroughEvent::SessionInfo {
                            state: Some(SessionState::Idle),
                            ..
                        }
                    );
                if child_idle {
                    let session_id = sid;
                    let wi = this
                        .workflows
                        .iter()
                        .position(|wf| wf.session.children.iter().any(|c| c.id == session_id));
                    if let Some(wi) = wi {
                        // 已有推进进行中，或已取消/已完成：跳过自动推进
                        if !this.workflows[wi].is_advancing()
                            && !this.workflows[wi].session.cancelled
                            && !this.workflows[wi].session.done
                        {
                            let output = this
                                .machine(idx)
                                .and_then(|mm| mm.views.get(&session_id))
                                .and_then(|v| {
                                    v.dialog.iter().rev().find_map(|d| match d {
                                        DialogItem::AgentOutput { content, .. } => {
                                            Some(block_text(content))
                                        }
                                        _ => None,
                                    })
                                });
                            let ex = output.unwrap_or_default();
                            if let Some(wf) = this.workflows.get_mut(wi) {
                                wf.begin_busy();
                            }
                            let mut wf = this.workflows[wi].clone();
                            wf.start_advance();
                            let wf_id = wf.session.id.clone();
                            let workflow_dir = this.workflow_dir.clone();
                            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                                // 自动推进在 GUI 的 tokio runtime 上执行（rig 需要 reactor）
                                let result = run_engine_on_tokio(async move {
                                    let _ = wf
                                        .on_child_state(&session_id, SessionState::Idle, Some(ex))
                                        .await;
                                    let _ = wf.persist(&workflow_dir);
                                    wf
                                })
                                .await;
                                let _ = this.update_in(cx, |this, _window, cx| {
                                    this.finish_engine_task(cx, wi, &wf_id, result);
                                });
                            });
                            this._tasks.push(t);
                        }
                    }
                }
            }
            "session_created" | "session_deleted" | "session_updated" => {
                this.refresh_sessions(idx, window, cx);
            }
            // 断线重连：刷新会话列表并重开选中会话（按需拉取历史，关闭期间输出不丢，
            // docs/DESIGN.md §5.2/§5.4）
            "connected" => {
                this.refresh_sessions(idx, window, cx);
                this.fetch_info(idx, window, cx);
                if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                    if machine == idx {
                        this.open_session(window, cx, machine, id);
                    }
                }
            }
            // 连接断开：如实标记离线（PRD §3.3 在线状态），清空实时活动
            "disconnected" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = "离线（重连中…）".into();
                    if let Some(sid) = m.selected.clone() {
                        if let Some(v) = m.views.get_mut(&sid) {
                            v.live_activity = None;
                        }
                    }
                }
                cx.notify();
            }
            _ => {}
        }
        cx.notify();
    }

    fn machine_summaries(&self) -> Vec<MachineSummary> {
        self.machines
            .iter()
            .map(|m| MachineSummary {
                name: m.config.name.clone(),
                harnesses: m
                    .info
                    .as_ref()
                    .map(|i| {
                        i.harnesses
                            .iter()
                            .filter(|h| h.available)
                            .map(|h| h.name.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    // ---- 动作（按机器下标）----

    fn refresh_sessions(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 首次只取最近活跃一窗（PRD §4.1.1 惰性加载）
            if let Ok(res) = client
                .request(
                    protocol::method::LIST_SESSIONS,
                    Some(json!({ "limit": SESSION_WINDOW })),
                )
                .await
            {
                let sessions: Vec<SessionMeta> = res
                    .get("sessions")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("hasMore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res.get("nextBefore").and_then(|v| v.as_u64());
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        let (list, has_more, next) =
                            merge_session_window(&[], sessions, has_more, next_before);
                        m.sessions = list;
                        m.sessions_has_more = has_more;
                        m.sessions_next_before = next;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 滚动加载更早的会话（PRD §4.1.1）：以 before 游标取更早一窗并追加。
    fn load_more_sessions(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        let before = m.sessions_next_before;
        let existing = m.sessions.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(
                    protocol::method::LIST_SESSIONS,
                    Some(json!({ "limit": SESSION_WINDOW, "before": before })),
                )
                .await
            {
                let sessions: Vec<SessionMeta> = res
                    .get("sessions")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("hasMore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res.get("nextBefore").and_then(|v| v.as_u64());
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        let (list, has_more, next) =
                            merge_session_window(&existing, sessions, has_more, next_before);
                        m.sessions = list;
                        m.sessions_has_more = has_more;
                        m.sessions_next_before = next;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 拉取机器信息（agent 发现 + 默认模型）；失败时如实标记连接失败（PRD §3.3 在线状态）。
    fn fetch_info(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| match client
            .request(protocol::method::GET_INFO, None)
            .await
        {
            Ok(res) => {
                let info: MachineInfo =
                    serde_json::from_value(res).unwrap_or_else(|_| MachineInfo {
                        server_version: String::new(),
                        harnesses: Vec::new(),
                    });
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.info = Some(info);
                        m.status = "已连接".into();
                    }
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.status = format!("连接失败（{e}）");
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 刷新全部机器信息（打开机器管理设置时调用，保证 agent 列表最新）。
    fn refresh_machine_infos(&self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.machines.len() {
            self.fetch_info(i, window, cx);
        }
    }

    /// 该机器首个可用 agent（None = 未获取到 agent 列表）。
    fn available_harness(&self, idx: usize) -> Option<String> {
        self.machine(idx)
            .and_then(|m| m.info.as_ref())
            .and_then(|i| {
                i.harnesses
                    .iter()
                    .find(|h| h.available)
                    .map(|h| h.name.clone())
            })
    }

    /// 新会话视图"创建会话"（PRD §4.1.2）：按所选机器/agent/工作目录创建会话并跳转，
    /// 首条指令由用户在会话交互页的输入区发送（不在此处 prompt）。
    fn create_session_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return;
        };
        // agent 必须来自该机器自动发现的列表（不回落 stub，避免发给 server 报错）
        let harness = match self.new_session_harness.clone() {
            Some(h) => h,
            None => match self.available_harness(machine) {
                Some(h) => h,
                None => {
                    if let Some(m) = self.machine_mut(machine) {
                        m.status = "获取 agent 列表失败，请检查机器连接".into();
                    }
                    cx.notify();
                    return;
                }
            },
        };
        let cwd = self.session_cwd_input.read(cx).value().trim().to_string();
        if cwd.is_empty() {
            if let Some(m) = self.machine_mut(machine) {
                m.status = "请填写工作目录".into();
            }
            cx.notify();
            return;
        }
        let client = m.client.clone();
        protocol::log::info(
            "gui.app",
            format!("创建会话（machine={machine} harness={harness} cwd={cwd}）"),
        );
        // 立即反馈：agent 懒加载 + ACP session/new 可能耗时（首次拉起包装器），
        // 先给用户"创建中"提示，避免误以为卡住
        if let Some(m) = self.machine_mut(machine) {
            m.status = "正在创建会话…".into();
        }
        cx.notify();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request(
                    protocol::method::CREATE_SESSION,
                    Some(json!({ "harness": harness, "cwd": cwd })),
                )
                .await;
            let Ok(res) = res else {
                protocol::log::error("gui.app", "创建会话请求失败");
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machine_mut(machine) {
                        m.status = "创建会话失败".into();
                    }
                    cx.notify();
                });
                return;
            };
            let sid = res
                .get("session")
                .and_then(|s| s.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if sid.is_empty() {
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machine_mut(machine) {
                        m.status = "创建会话失败：未返回 id".into();
                    }
                    cx.notify();
                });
                return;
            }
            // 跳转到该会话视图：新会话历史为空，直接本地建空视图并选中，
            // 省去 open_session 往返（与 open_session 对空历史的返回等价）
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.selected = Some(sid.clone());
                    m.views
                        .insert(sid.clone(), crate::aggregate::SessionView::new());
                    m.dialog_before = 0;
                    m.dialog_has_more = false;
                    m.status = "已连接".into();
                }
                this.selected = Some(Selected::Session {
                    machine,
                    id: sid.clone(),
                });
                this.set_panel(window, cx, None);
                this.dialog_scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    fn open_session(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let mut events: Vec<PassthroughEvent> = Vec::new();
            let mut has_more = false;
            let mut next_before = 0usize;
            // 按需拉取：点击会话查看时才触发 session/load 重放（docs/DESIGN.md §5.2）；
            // 返回透传事件，GUI 应用聚合为对话内容与活动
            if let Ok(res) = client
                .request(
                    protocol::method::OPEN_SESSION,
                    Some(json!({ "sessionId": session_id, "limit": 200 })),
                )
                .await
            {
                events = res
                    .get("events")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Vec<PassthroughEvent>>(v).ok())
                    .unwrap_or_default();
                has_more = res
                    .get("hasMore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                next_before = res.get("nextBefore").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            }
            let view = crate::aggregate::aggregate_events(&events);
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.selected = Some(session_id.clone());
                    // 进行中的 turn 尚未落库：重放为空时保留已聚合的实时视图，
                    // 避免打开会话时清掉编排指令回显与实时输出
                    let keep_live = events.is_empty()
                        && m.views
                            .get(&session_id)
                            .map(|v| !v.dialog.is_empty() || !v.activities.is_empty())
                            .unwrap_or(false);
                    if !keep_live {
                        m.views.insert(session_id.clone(), view);
                    }
                    m.dialog_before = next_before;
                    m.dialog_has_more = has_more;
                }
                this.selected = Some(Selected::Session {
                    machine,
                    id: session_id,
                });
                this.set_panel(window, cx, None);
                // 打开后自动跳到对话底部（最新内容）
                this.dialog_scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    /// 惰性加载更早的会话历史（顶部"加载更早消息"按钮，PRD §3.2）。
    fn load_earlier_history(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let before = m.dialog_before;
        if before == 0 {
            return;
        }
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let mut older: Vec<PassthroughEvent> = Vec::new();
            let mut has_more = false;
            let mut next_before = 0usize;
            if let Ok(res) = client
                .request(
                    protocol::method::OPEN_SESSION,
                    Some(json!({ "sessionId": session_id, "limit": 200, "before": before })),
                )
                .await
            {
                older = res
                    .get("events")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Vec<PassthroughEvent>>(v).ok())
                    .unwrap_or_default();
                has_more = res
                    .get("hasMore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                next_before = res.get("nextBefore").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            }
            let _ = this.update_in(cx, |this, _window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    // 更早的历史前插（时间正序；跨窗同消息分块合并）
                    if let Some(v) = m.views.get_mut(&session_id) {
                        crate::aggregate::prepend_events(v, &older);
                    }
                    m.dialog_before = next_before;
                    m.dialog_has_more = has_more;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_diff(&self, window: &mut Window, cx: &mut Context<Self>, machine: usize) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let cwd = m
            .selected
            .as_ref()
            .and_then(|sid| m.sessions.iter().find(|s| s.id == *sid))
            .map(|s| s.cwd.clone())
            .or_else(|| {
                let v = self.session_cwd_input.read(cx).value().trim().to_string();
                (!v.is_empty()).then_some(v)
            });
        let Some(cwd) = cwd else { return };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // git_status：文件列表 + 增减行数
            let mut status = None;
            if let Ok(res) = client
                .request(protocol::method::GIT_STATUS, Some(json!({ "cwd": cwd })))
                .await
            {
                status = serde_json::from_value(res).ok();
            }
            // git_diff：全部文件的结构化 diff
            let mut diff_files = Vec::new();
            if let Ok(res) = client
                .request(protocol::method::GIT_DIFF, Some(json!({ "cwd": cwd })))
                .await
            {
                if let Ok(d) = serde_json::from_value::<protocol::GitDiffResult>(res) {
                    diff_files = d.files;
                }
            }
            let _ = this.update_in(cx, |this, _window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.diff_status = status;
                    m.diff_files = diff_files;
                    m.diff_path = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_file_diff(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let cwd = m
            .selected
            .as_ref()
            .and_then(|sid| m.sessions.iter().find(|s| s.id == *sid))
            .map(|s| s.cwd.clone())
            .unwrap_or_default();
        let client = m.client.clone();
        let path2 = path.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let mut files = Vec::new();
            if let Ok(res) = client
                .request(
                    protocol::method::GIT_DIFF,
                    Some(json!({ "cwd": cwd, "path": path })),
                )
                .await
            {
                if let Ok(d) = serde_json::from_value::<protocol::GitDiffResult>(res) {
                    files = d.files;
                }
            }
            let _ = this.update_in(cx, |this, _window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.diff_files = files;
                    m.diff_path = Some(path2);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// revert：单文件 / 单 hunk / 全部变更（PRD §3.5）。
    fn revert(
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
        let cwd = m
            .selected
            .as_ref()
            .and_then(|sid| m.sessions.iter().find(|s| s.id == *sid))
            .map(|s| s.cwd.clone())
            .unwrap_or_default();
        let client = m.client.clone();
        let mut params = json!({ "cwd": cwd });
        if let Some(p) = path {
            params["path"] = json!(p);
        }
        if let Some(p) = patch {
            params["patch"] = json!(p);
        }
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(protocol::method::GIT_REVERT, Some(params))
                .await;
            // 刷新 diff
            let _ = this.update_in(cx, |this, window, cx| {
                this.load_diff(window, cx, machine);
                cx.notify();
            });
        })
        .detach();
    }

    /// 编排异步任务收尾：原位换回引擎（校验 id，防删除竞态导致下标漂移）；
    /// 中断（oneshot 失效）时复位防重入标记，避免永久卡在 Busy。
    fn finish_engine_task(
        &mut self,
        cx: &mut Context<Self>,
        wi: usize,
        wf_id: &str,
        result: Option<WorkflowEngine>,
    ) {
        match result {
            Some(wf) => {
                let same = self
                    .workflows
                    .get(wi)
                    .map(|e| e.session.id == wf_id)
                    .unwrap_or(false);
                if same {
                    // 合并推进期间（克隆体执行中）新记录的用户介入消息：
                    // record_user 追加到原位引擎的消息不会出现在克隆体上，换回前补上
                    let was_cancelled = self.workflows[wi].session.cancelled;
                    let pending: Vec<OrcMsg> = self.workflows[wi]
                        .session
                        .transcript
                        .iter()
                        .skip(wf.session.transcript.len())
                        .cloned()
                        .collect();
                    self.workflows[wi] = wf;
                    if was_cancelled {
                        // 用户取消发生在推进进行中：换回后仍保持已取消（并回到空闲）
                        self.workflows[wi].session.cancelled = true;
                        self.workflows[wi].session.state = SessionState::Idle;
                    }
                    if !pending.is_empty() {
                        let w = &mut self.workflows[wi];
                        w.session.transcript.extend(pending);
                        w.session.updated_at = crate::workflow::now_ts();
                    }
                } else {
                    // 会话已被删除（删除竞态/下标漂移）：清理刚写入的持久化文件，避免重启复活
                    WorkflowEngine::remove(&self.workflow_dir, wf_id);
                }
            }
            None => {
                if self
                    .workflows
                    .get(wi)
                    .map(|e| e.session.id == wf_id)
                    .unwrap_or(false)
                {
                    self.workflows[wi].abort_busy();
                }
            }
        }
        cx.notify();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        let attachments = self.input_attachments.clone();
        if text.trim().is_empty() && attachments.is_empty() {
            return;
        }
        // @ 引用解析（PRD §4.2）
        let (clean_text, refs) = parse_at_references(&text);
        let mut all = attachments;
        for r in refs {
            all.push(path_attachment(&r));
        }
        let blocks = compose_prompt(&clean_text, &all);

        let Some(target) = self.selected.clone() else {
            protocol::log::warn("gui.app", "发送 prompt 但未选中会话");
            if let Some(m) = self.machine_mut(0) {
                m.status = "请先选择会话".into();
            }
            cx.notify();
            return;
        };
        match target {
            Selected::Session { machine, id } => {
                protocol::log::debug(
                    "gui.app",
                    format!("发送 prompt session={id}：{}", truncate(&clean_text, 60)),
                );
                let Some(m) = self.machine(machine) else {
                    return;
                };
                let client = m.client.clone();
                let input = blocks;
                // 本地立即渲染用户消息（docs/DESIGN.md §7.1：不依赖回显）；
                // server 回显到达时在 on_notify 按内容去重，避免重复气泡
                if let Some(m) = self.machine_mut(machine) {
                    let view = m.views.entry(id.clone()).or_default();
                    crate::aggregate::merge_event(
                        view,
                        &PassthroughEvent::UserMessage {
                            content: input.clone(),
                            timestamp: crate::workflow::now_ts(),
                        },
                    );
                }
                self.dialog_scroll.scroll_to_bottom();
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    if let Err(e) = client
                        .request(
                            protocol::method::PROMPT,
                            Some(json!({ "sessionId": id, "input": input })),
                        )
                        .await
                    {
                        protocol::log::error("gui.app", format!("prompt 请求失败: {e}"));
                        let msg = format!("prompt 失败: {e}");
                        let _ = this.update_in(cx, |this, _window, cx| {
                            if let Some(m) = this.machine_mut(machine) {
                                m.status = msg.clone();
                            }
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            Selected::Workflow { engine } => {
                // 向工作流会话发送输入（继续/介入/调整后续动作，docs/DESIGN.md §9）；
                // 用户消息同步落库（立即可见），异步推进在克隆体上执行、完成后原位换回；
                // 编排在 GUI 的 tokio runtime 上执行（rig LLM 调用需要 reactor）
                let should_advance = self
                    .workflows
                    .get_mut(engine)
                    .map(|wf| wf.record_user(&clean_text))
                    .unwrap_or(false);
                if should_advance {
                    if let Some(wf) = self.workflows.get_mut(engine) {
                        wf.begin_busy();
                    }
                    let mut wf = self.workflows[engine].clone();
                    wf.start_advance();
                    let wf_id = wf.session.id.clone();
                    let workflow_dir = self.workflow_dir.clone();
                    let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                        let result = run_engine_on_tokio(async move {
                            let _ = wf.advance().await;
                            let _ = wf.persist(&workflow_dir);
                            wf
                        })
                        .await;
                        let _ = this.update_in(cx, |this, _window, cx| {
                            this.finish_engine_task(cx, engine, &wf_id, result);
                        });
                    });
                    self._tasks.push(t);
                }
            }
        }
        self.input_state.update(cx, |s, cx| {
            s.set_value("", window, cx);
        });
        self.input_attachments.clear();
    }

    fn orchestrator_backend(&self, summaries: &[MachineSummary]) -> Arc<dyn OrcBackend> {
        let cfg = self.store.orchestrator();
        let names: Vec<String> = summaries.iter().map(|m| m.name.clone()).collect();
        Arc::new(RigBackend::new(cfg, names))
    }

    fn quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>, cmd: &QuickCommand) {
        let Some(Selected::Session { machine, id }) = self.selected.clone() else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let prompt_text = cmd.prompt.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::PROMPT,
                    Some(json!({ "sessionId": id, "input": [{ "type": "text", "text": prompt_text }] })),
                )
                .await;
            let _ = this.update_in(cx, |_this, _window, cx| {
                cx.notify();
            });
        })
        .detach();
    }

    /// 给某机器某 agent 安装/更新 skills：启动新会话并发送安装提示词（PRD §3.6）。
    fn install_skill(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        harness: String,
        skill: SkillEntry,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let prompt_text = format!(
            "请安装/更新以下 skill：{}\n说明：{}",
            skill.name, skill.description
        );
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(
                    protocol::method::CREATE_SESSION,
                    Some(json!({ "harness": harness, "cwd": "/tmp" })),
                )
                .await
            {
                let sid = res
                    .get("session")
                    .and_then(|s| s.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !sid.is_empty() {
                    let _ = client
                        .request(
                            protocol::method::PROMPT,
                            Some(json!({ "sessionId": sid, "input": [{ "type": "text", "text": prompt_text }] })),
                        )
                        .await;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                // 刷新会话列表（新会话出现）
                this.refresh_sessions(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 查看某 agent 的 skills 列表（PRD §3.3）。
    fn fetch_agent_skills(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        harness: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let h = harness.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let mut skills = Vec::new();
            if let Ok(res) = client
                .request(
                    protocol::method::LIST_AGENT_SKILLS,
                    Some(json!({ "harness": h })),
                )
                .await
            {
                skills = res
                    .get("skills")
                    .and_then(|s| s.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            let _ = this.update_in(cx, |this, _window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.skills = skills;
                    m.skills_harness = Some(harness);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_default_model(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        harness: String,
        model: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::SET_DEFAULT_MODEL,
                    Some(json!({ "harness": harness, "model": model })),
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.fetch_info(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ---- 工作流 ----

    fn persist_workflow(&self, idx: usize) {
        if let Some(wf) = self.workflows.get(idx) {
            let _ = wf.persist(&self.workflow_dir);
        }
    }

    /// 恢复 GUI 本地工作流会话（GUI 重开，docs/DESIGN.md §10 持久化与恢复）。
    /// 启动时不主动驱动工作流：子会话运行中变 idle 时经 passthrough 通知自动推进。
    fn restore_workflows(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let sessions = WorkflowEngine::load_all(&self.workflow_dir);
        if sessions.is_empty() {
            return;
        }
        let clients: Vec<WsClient> = self.machines.iter().map(|m| m.client.clone()).collect();
        let summaries = self.machine_summaries();
        let backend = self.orchestrator_backend(&summaries);
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

    /// 创建工作流会话（PRD §3.7/§4.1.2）：输入框内容作为本次目标/执行计划；
    /// 若已选择模板，模板仅作为系统提示词（preamble）、不进入会话历史、不作为用户输入。
    fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.workflow_input.read(cx).value().to_string();
        let template = self.workflow_template.take();
        let description = goal.trim().to_string();
        // 无模板时必须提供执行计划；有模板时目标可留空（之后在会话输入区继续）
        if template.is_none() && description.is_empty() {
            self.workflow_error = Some("请先用自然语言描述执行计划".into());
            cx.notify();
            return;
        }
        let preamble = template.map(|t| t.description);
        self.create_workflow_with(window, cx, description, preamble);
        self.workflow_input.update(cx, |s, cx| {
            s.set_value("", window, cx);
        });
    }

    /// 创建工作流会话（共用实现）。
    /// `preamble`：模板/系统指令，内置进编排 agent 的系统提示词、不进入会话历史（PRD §3.7）。
    fn create_workflow_with(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        description: String,
        preamble: Option<String>,
    ) {
        // 编排 agent 未配置 API 时不创建不可用的会话，给出提示并引导设置（PRD §4.3）
        if !self.store.orchestrator().is_configured() {
            self.workflow_error = Some(
                "编排 agent 未配置 API（Base URL / API key）。请先在 设置 → 编排 agent 中配置。"
                    .into(),
            );
            cx.notify();
            return;
        }
        self.workflow_error = None;
        // @ 引用解析为上下文
        let (clean, refs) = parse_at_references(&description);
        let context = refs
            .iter()
            .map(|r| read_path_context(r))
            .collect::<Vec<_>>()
            .join("\n");
        let clients: Vec<WsClient> = self.machines.iter().map(|m| m.client.clone()).collect();
        let summaries = self.machine_summaries();
        let backend = self.orchestrator_backend(&summaries);
        let engine = WorkflowEngine::new(
            &clean,
            &context,
            preamble.as_deref().unwrap_or(""),
            backend,
            clients.clone(),
            summaries,
        );
        let wi = self.workflows.len();
        let workflow_dir = self.workflow_dir.clone();
        self.workflows.push(engine);
        self.selected = Some(Selected::Workflow { engine: wi });

        // 只有存在实际目标/计划时才启动首个 turn；目标为空（如仅选模板未填目标）时
        // 等用户在会话输入区输入后再推进（与普通会话一致，不自动塞入模板作为用户输入）
        if !clean.trim().is_empty() {
            // 启动首个 turn（拆解步骤并下发指令）：同步标记开始（会话行/历史立即可见），
            // 异步推进在克隆体上执行、完成后原位换回；编排在 GUI 的 tokio runtime 上执行
            if let Some(wf) = self.workflows.get_mut(wi) {
                wf.begin_busy();
            }
            let mut wf = self.workflows[wi].clone();
            wf.start_advance();
            let wf_id = wf.session.id.clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                let result = run_engine_on_tokio(async move {
                    let _ = wf.start().await;
                    let _ = wf.persist(&workflow_dir);
                    wf
                })
                .await;
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.finish_engine_task(cx, wi, &wf_id, result);
                });
            });
            self._tasks.push(t);
        }
        cx.notify();
    }

    /// 删除工作流会话：删除其所有子会话（各机器 server）并清除本地持久化状态。
    fn delete_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        // 先收集子会话（机器下标 + 会话 id），再异步发 DELETE_SESSION
        let children: Vec<(usize, String)> = self
            .workflows
            .get(idx)
            .map(|w| {
                w.session
                    .children
                    .iter()
                    .map(|c| (c.machine_idx, c.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (machine, sid) in children {
            if let Some(m) = self.machine(machine) {
                let client = m.client.clone();
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    let _ = client
                        .request(
                            protocol::method::DELETE_SESSION,
                            Some(json!({ "sessionId": sid })),
                        )
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.refresh_sessions(machine, window, cx);
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        // 删除本地工作流状态
        if let Some(wf) = self.workflows.get(idx) {
            let id = wf.session.id.clone();
            WorkflowEngine::remove(&self.workflow_dir, &id);
        }
        if idx < self.workflows.len() {
            self.workflows.remove(idx);
        }
        if let Some(Selected::Workflow { engine }) = self.selected.clone() {
            if engine == idx {
                self.selected = None;
            }
        }
        cx.notify();
    }

    /// 删除工作流会话确认弹窗（连同所有子会话，不可恢复）。
    fn confirm_delete_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        let child_count = self
            .workflows
            .get(idx)
            .map(|w| w.session.children.len())
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
                    "确定删除该工作流会话吗？将同时删除其 {child_count} 个子会话，不可恢复。"
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

    /// 取消工作流会话及其所有子会话（终止整个工作流，PRD §3.7）。
    fn cancel_workflow(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 同步先落「已取消」状态（会话行/历史立即可见），再异步发 CANCEL 到各子会话；
        // 克隆体执行纯 IO 后换回，与 advance 走同一收尾路径。
        if let Some(wf) = self.workflows.get_mut(idx) {
            wf.mark_cancelled();
        }
        self.persist_workflow(idx);
        let Some(wf) = self.workflows.get(idx).cloned() else {
            return;
        };
        let wf_id = wf.session.id.clone();
        let workflow_dir = self.workflow_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                wf.send_cancel_to_children().await;
                let _ = wf.persist(&workflow_dir);
                wf
            })
            .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.finish_engine_task(cx, idx, &wf_id, result);
            });
        });
        self._tasks.push(t);
        cx.notify();
    }

    // ---- 语音输入 ----

    fn toggle_voice(&mut self, cx: &mut Context<Self>) {
        if self.voice_recording {
            // 停止录音：附加一段音频资源作为上下文（真实 STT 转写由运行环境提供）
            self.input_attachments.push(InputAttachment::Audio {
                name: format!("voice-{}.webm", crate::workflow::now_ts()),
                mime_type: "audio/webm".into(),
                data_base64: String::new(),
            });
            self.voice_recording = false;
        } else {
            self.voice_recording = true;
        }
        cx.notify();
    }

    /// 粘贴图片作为上下文（PRD §4.2）：从剪贴板读取图片条目 → 附件。
    fn paste_image(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            let has_image = item
                .entries
                .iter()
                .any(|e| matches!(e, gpui::ClipboardEntry::Image(_)));
            if has_image {
                self.input_attachments.push(InputAttachment::Image {
                    name: "clipboard.png".into(),
                    mime_type: "image/png".into(),
                    // 真实像素编码由运行环境/平台提供；机制（粘贴 → 附件 → prompt）已接通
                    data_base64: String::new(),
                });
            }
        }
        cx.notify();
    }

    // ---- 会话标题 ----

    // ---- 会话删除（PRD §3.1）----

    /// 删除会话：历史一并移除、不可恢复（PRD §3.1）。
    /// 调用方应先经确认弹窗（confirm_delete_session）。
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
        protocol::log::info("gui.app", format!("删除会话 {session_id}"));
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::DELETE_SESSION,
                    Some(json!({ "sessionId": session_id })),
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // 删除的是当前选中会话则回到新会话视图
                if let Some(Selected::Session { id, .. }) = this.selected.clone() {
                    if id == session_id {
                        this.selected = None;
                    }
                }
                this.refresh_sessions(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 重命名会话（右键 → 重命名，PRD §3.1 用户可随时修改标题）。
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
        protocol::log::info("gui.app", format!("重命名会话 {session_id} → {title}"));
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::SET_SESSION_TITLE,
                    Some(json!({ "sessionId": session_id, "title": title })),
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.renaming_session = None;
                this.refresh_sessions(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 重命名工作流会话（GUI 本地标题，右键 → 重命名）。
    fn rename_workflow(&mut self, cx: &mut Context<Self>, engine: usize, title: String) {
        if let Some(wf) = self.workflows.get_mut(engine) {
            wf.session.title = title.trim().to_string();
            let dir = self.workflow_dir.clone();
            let _ = wf.persist(&dir);
        }
        self.renaming_workflow = None;
        cx.notify();
    }

    /// 右键会话操作菜单（删除 / 重命名，PRD §3.1；普通会话与工作流会话通用）。
    fn render_context_menu(
        &self,
        menu: &SessionContextMenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = menu.target.clone();
        let rename_target = target.clone();
        div()
            .id("ctx-backdrop")
            .absolute()
            .inset_0()
            // 点击菜单外任意处关闭
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.context_menu = None;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("session-ctx-menu")
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .min_w(px(150.))
                    .gap_0p5()
                    .p_1()
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .shadow_lg()
                    .border_1()
                    .border_color(rgb(0xe5e7eb))
                    // 菜单内点击不冒泡到 backdrop（避免误关）
                    .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        Button::new("ctx-rename")
                            .small()
                            .label("重命名")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                // 预填当前标题后进入行内重命名
                                let title = this
                                    .context_menu
                                    .as_ref()
                                    .map(|m| m.title.clone())
                                    .unwrap_or_default();
                                this.title_input.update(cx, |s, cx| {
                                    s.set_value(title, window, cx);
                                });
                                match &rename_target {
                                    ContextMenuTarget::Session {
                                        machine,
                                        session_id,
                                    } => {
                                        this.renaming_session =
                                            Some((*machine, session_id.clone()));
                                        this.renaming_workflow = None;
                                    }
                                    ContextMenuTarget::Workflow { engine } => {
                                        this.renaming_workflow = Some(*engine);
                                        this.renaming_session = None;
                                    }
                                }
                                this.context_menu = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ctx-delete")
                            .small()
                            .label(if matches!(target, ContextMenuTarget::Workflow { .. }) {
                                "删除工作流"
                            } else {
                                "删除会话"
                            })
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                match &target {
                                    // 普通会话：专用确认弹窗（不可恢复，PRD §3.1）
                                    ContextMenuTarget::Session {
                                        machine,
                                        session_id,
                                    } => {
                                        this.confirm_delete_session(
                                            window,
                                            cx,
                                            *machine,
                                            session_id.clone(),
                                        );
                                    }
                                    // 工作流会话：删除本地状态并删除其所有子会话（弹窗确认）
                                    ContextMenuTarget::Workflow { engine } => {
                                        this.confirm_delete_workflow(window, cx, *engine);
                                    }
                                }
                                this.context_menu = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    /// 删除会话确认弹窗（专用 AlertDialog，不可恢复，PRD §3.1）。
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

    // ---- 设置 ----

    fn add_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
        url: String,
        token: String,
    ) {
        if name.trim().is_empty() || !url.starts_with("ws://") || token.trim().is_empty() {
            return;
        }
        let machine = self
            .store
            .add_machine(name.trim(), url.trim(), token.trim());
        // 清空添加表单（成功添加后）
        self.machine_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.machine_url_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.machine_token_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        let mut view = MachineView::new(machine);
        view.status = "已连接".into();
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 首次只取最近活跃一窗（PRD §4.1.1 惰性加载）
            if let Ok(res) = client
                .request(
                    protocol::method::LIST_SESSIONS,
                    Some(json!({ "limit": SESSION_WINDOW })),
                )
                .await
            {
                let sessions: Vec<SessionMeta> = res
                    .get("sessions")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("hasMore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res.get("nextBefore").and_then(|v| v.as_u64());
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        let (list, has_more, next) =
                            merge_session_window(&[], sessions, has_more, next_before);
                        m.sessions = list;
                        m.sessions_has_more = has_more;
                        m.sessions_next_before = next;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        self.fetch_info(idx, window, cx);
        // 通知任务
        let mut notify_rx = self.machines[idx].client.subscribe();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            while let Ok(n) = notify_rx.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    Self::on_notify(this, window, cx, idx, &n);
                });
            }
        });
        self._tasks.push(t);
        self.selected = Some(Selected::Session {
            machine: idx,
            id: self.machines[idx]
                .sessions
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_default(),
        });
        if self.machines[idx].sessions.is_empty() {
            self.selected = None;
        }
        cx.notify();
    }

    fn remove_machine(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.machines.len() {
            let id = self.machines[idx].config.id.clone();
            self.store.remove_machine(&id);
            self.machines.remove(idx);
            // 修正选中的机器下标
            self.selected = match self.selected.clone() {
                Some(Selected::Session { machine, id: _sid }) if machine == idx => None,
                Some(Selected::Session { machine, id: sid }) if machine > idx => {
                    Some(Selected::Session {
                        machine: machine - 1,
                        id: sid,
                    })
                }
                other => other,
            };
            // 修正工作流的 machine_idx（删除机器后下标偏移）
            for wf in self.workflows.iter_mut() {
                for c in wf.session.children.iter_mut() {
                    if c.machine_idx > idx {
                        c.machine_idx -= 1;
                    }
                }
            }
            cx.notify();
        }
    }

    // ---- 渲染 ----

    fn render_sidebar(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(270.))
            .h_full()
            .gap_2()
            .p_3()
            .bg(rgb(0xffffff))
            .border_r_1()
            .border_color(rgb(0xe5e7eb))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.)).rounded_full().bg(rgb(0x3b82f6)))
                    .child(
                        Label::new("amux")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x111827)),
                    )
                    .child(div().flex_1())
                    // 新会话入口（PRD §4.1.2）：新建会话视图在中间面板，侧边栏仅一个 + 号
                    .child(
                        Button::new("goto-new-session")
                            .small()
                            .label("＋")
                            .tooltip("新会话 / 工作流")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.selected = None;
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
                        .label("设置")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.show_settings = true;
                            this.refresh_machine_infos(window, cx);
                            cx.notify();
                        })),
                ),
            )
    }

    /// 会话列表：普通会话 + 工作流会话**统一按最近活跃排序**（docs/PRD §4.1.1；
    /// 子会话随父会话一起参与排序，不占顶层一行）。
    fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 子会话只挂在工作流会话下，顶层列表跳过
        let child_ids: std::collections::HashSet<&str> = self
            .workflows
            .iter()
            .flat_map(|wf| wf.session.children.iter().map(|c| c.id.as_str()))
            .collect();
        // 汇总：普通会话按 last_event_at；工作流按 max(父 updated_at, 子会话 last_event_at)
        let mut items: Vec<(u64, SessionListItem)> = Vec::new();
        for (mi, m) in self.machines.iter().enumerate() {
            for s in &m.sessions {
                if child_ids.contains(s.id.as_str()) {
                    continue;
                }
                items.push((
                    s.last_event_at,
                    SessionListItem::Session {
                        machine: mi,
                        meta: s.clone(),
                    },
                ));
            }
        }
        for (wi, wf) in self.workflows.iter().enumerate() {
            let mut recency = wf.session.updated_at;
            for c in &wf.session.children {
                if let Some(mm) = self.machines.get(c.machine_idx) {
                    if let Some(s) = mm.sessions.iter().find(|s| s.id == c.id) {
                        recency = recency.max(s.last_event_at);
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

        // 惰性加载（PRD §4.1.1）：还有更早会话时底部显示"加载更早会话"
        for (mi, m) in self.machines.iter().enumerate() {
            if m.sessions_has_more {
                let name = m.config.name.clone();
                rows.push(
                    Button::new(format!("sessions-more-{mi}"))
                        .small()
                        .label(format!("加载更早会话（{name}）"))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.load_more_sessions(mi, window, cx);
                        }))
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// 普通会话行：标题 + 状态（agent 与机器在对话气泡中展示，docs/PRD §4.1.1）。
    fn render_session_row(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        s: &SessionMeta,
    ) -> gpui::AnyElement {
        let sid = s.id.clone();
        let sid_open = sid.clone(); // 打开会话闭包用
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
        // 状态只区分 工作中 / 空闲：工作中前缀转圈，空闲无转圈（docs/PRD §4.1.1）
        let busy = s.state == SessionState::Busy;
        let label: SharedString = title.clone().into();

        // 正在重命名该会话：行内输入框 + 保存（右键 → 重命名）
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

        // 右键弹出操作菜单（删除 / 重命名，PRD §3.1）
        let sid_ctx = sid.clone();
        div()
            .id(format!("sess-row-{machine}-{sid}"))
            .relative()
            .w_full()
            .rounded_md()
            // 选中：浅灰底 + 浅灰边框（中性，默认主题 primary 为近黑、蓝边框刺眼，都不用）
            .bg(rgb(0xe5e5e5).opacity(if sel { 1.0 } else { 0.0 }))
            // 选中会话加边框（默认无边框）
            .border_1()
            .border_color(if sel {
                hsla(0.0, 0.0, 0.83, 1.0)
            } else {
                transparent_black()
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    this.context_menu = Some(SessionContextMenu {
                        target: ContextMenuTarget::Session {
                            machine,
                            session_id: sid_ctx.clone(),
                        },
                        title: title.clone(),
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
                    // 与工作流会话等高（内容 24 + 内边距 8，docs/PRD §4.1.1）
                    .h(px(32.))
                    .px_1()
                    .gap_1()
                    .items_center()
                    // 标题靠左对齐（与工作流会话一致；Button 会居中，故用 div+Label）
                    .child(
                        h_flex()
                            .id(format!("sess-title-{machine}-{sid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_session(window, cx, machine, sid_open.clone());
                            }))
                            .child(Label::new(label).text_sm().flex_1().min_w_0().truncate()),
                    )
                    // 工作中转圈（右侧）；空闲占位（保持对齐）
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

    /// 打开工作流会话交互页。
    fn open_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, wi: usize) {
        self.selected = Some(Selected::Workflow { engine: wi });
        self.set_panel(window, cx, None);
        // 滚动加载：打开时只渲染最近一窗
        self.workflow_dialog_limit = 50;
        // 与普通会话一致：打开后跳到对话底部（最新内容）
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 工作流行：标题 · 子会话数 + 打开按钮 + 状态徽章 + 折叠的子会话（docs/PRD §4.1.1）。
    fn render_workflow_row(&self, cx: &mut Context<Self>, wi: usize) -> gpui::AnyElement {
        let Some(wf) = self.workflows.get(wi) else {
            return div().into_any();
        };
        let title = if wf.session.title.is_empty() {
            "新工作流".to_string()
        } else {
            wf.session.title.clone()
        };
        let state = if wf.session.cancelled {
            "已取消"
        } else if wf.session.done {
            "完成"
        } else if wf.session.state == SessionState::Busy {
            "编排中…"
        } else {
            "空闲"
        };
        let expanded = self.expanded_workflows.contains(&wi);
        let header = h_flex()
            .gap_1()
            .items_center()
            // 标题区域：点击即打开会话交互页（与普通会话一致）
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
                    // 特殊状态徽章（已取消/完成）内联显示
                    .when(wf.session.cancelled || wf.session.done, |h| {
                        h.child(
                            div()
                                .px_1()
                                .py(px(1.))
                                .rounded_full()
                                .bg(rgb(0xf3f4f6))
                                .child(Label::new(state).text_xs().text_color(rgb(0x4b5563))),
                        )
                    }),
            )
            // 折叠指示（转圈左边）：点击展开/折叠子会话
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
            // 与普通会话一致：工作中转圈、空闲无转圈
            .child(if wf.session.state == SessionState::Busy {
                Spinner::new()
                    .color(hsla(0.6, 0.8, 0.5, 1.0))
                    .into_any_element()
            } else {
                div().w(px(14.)).h(px(14.)).into_any_element()
            });
        // 子会话默认折叠、可展开下钻（PRD §3.1/§4.1.1）
        let mut content = v_flex().gap_1();
        for c in &wf.session.children {
            let cid = c.id.clone();
            let cid_open = cid.clone();
            let step = c.step_desc.clone();
            let machine_name = c.machine_name.clone();
            let machine_click = machine_name.clone();
            let harness = c.harness.clone();
            let busy = c.state == SessionState::Busy;
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Label::new("↳").text_color(rgb(0x9ca3af)))
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
                                Label::new(format!("{step} [{harness}@{machine_name}]"))
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

        // 正在重命名该工作流会话：行内输入框 + 保存（右键 → 重命名）
        if self.renaming_workflow == Some(wi) {
            return v_flex()
                .gap_1()
                .p_2()
                .bg(rgb(0xffffff))
                .rounded_md()
                .border_1()
                .border_color(rgb(0xe5e7eb))
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

        let row = v_flex()
            .gap_1()
            .p_1()
            .rounded_md()
            // 与普通会话一致的紧凑行；子会话折叠区作为唯一"卡片感"来源
            .child(header)
            .child(
                Collapsible::new()
                    .open(self.expanded_workflows.contains(&wi))
                    .content(content),
            );

        // 右键弹出操作菜单（删除 / 重命名工作流，PRD §3.1）
        let title_ctx = title.clone();
        let wf_sel = self.selected == Some(Selected::Workflow { engine: wi });
        div()
            .id(format!("wf-row-{wi}"))
            .relative()
            .w_full()
            .rounded_md()
            // 选中：浅灰底 + 浅灰边框（与普通会话一致，中性不刺眼）
            .bg(rgb(0xe5e5e5).opacity(if wf_sel { 1.0 } else { 0.0 }))
            // 选中工作流会话加边框（默认无边框）
            .border_1()
            .border_color(if wf_sel {
                hsla(0.0, 0.0, 0.83, 1.0)
            } else {
                transparent_black()
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    this.context_menu = Some(SessionContextMenu {
                        target: ContextMenuTarget::Workflow { engine: wi },
                        title: title_ctx.clone(),
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

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 未选中会话：整块中间面板就是新会话视图（PRD §4.1.2），不显示对话/输入区
        if self.selected.is_none() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .p_2()
                .child(self.render_center(window, cx))
                .into_any();
        }
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .p_2()
            .bg(rgb(0xffffff))
            .rounded_md()
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .child(self.render_center(window, cx))
            .child(self.render_quick_buttons(cx))
            .child(self.render_activity_bar(cx))
            .child(self.render_input(_window_placeholder(window), cx))
            .into_any()
    }

    /// 中间面板：未选中会话时显示新会话视图（PRD §4.1.2），否则对话流 + 悬浮按钮。
    fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.selected.is_none() {
            return self.render_new_session_view(_window_placeholder(window), cx);
        }
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_dialog(_window_placeholder(window), cx))
            .child(self.render_floating_buttons(window, cx))
            .into_any()
    }

    /// 新会话视图（PRD §4.1.2）：自然语言输入 + 选择机器与 agent + 指定工作目录，
    /// 或切换"工作流"模式（从模板创建 / 直接输入自然语言计划）。
    fn render_new_session_view(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mode = self.new_session_mode;
        let mut card = v_flex()
            .w(px(640.))
            .gap_3()
            .p_4()
            .bg(rgb(0xffffff))
            .rounded_md()
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .shadow_lg()
            .child(
                Label::new("新会话")
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x111827)),
            )
            // 模式切换：普通 / 工作流
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
                card = card
                    // 机器 与 Agent 选择并列一排（PRD §4.1.2 选择机器与 agent）
                    .child(
                        h_flex()
                            .gap_6()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new("机器").text_sm().text_color(rgb(0x6b7280)))
                                    .child(self.render_machine_selector(cx)),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new("Agent").text_sm().text_color(rgb(0x6b7280)))
                                    .child(self.render_harness_selector(cx)),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("工作目录").text_sm().text_color(rgb(0x6b7280)))
                            .child(Input::new(&self.session_cwd_input)),
                    )
                    // 只创建会话；首条指令在会话交互页的输入区由用户发送
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
                            .text_color(rgb(0x9ca3af)),
                    );
            }
            NewSessionMode::Workflow => {
                card = card
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("工作流模板").text_sm().text_color(rgb(0x6b7280)))
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
                                .text_color(rgb(0x6b7280)),
                            )
                            .child(Input::new(&self.workflow_input)),
                    );
                // 编排 agent 未配置 API：给出提示并引导到设置页（PRD §4.3）
                if let Some(err) = &self.workflow_error {
                    card = card.child(
                        v_flex()
                            .gap_1()
                            .p_2()
                            .bg(rgb(0xffe6e6))
                            .rounded_md()
                            .child(Label::new(err).text_color(rgb(0xb91c1c)))
                            .child(
                                Button::new("ns-goto-orch-settings")
                                    .small()
                                    .label("去配置编排 agent")
                                    .on_click(cx.listener(|this, _ev, _window, cx| {
                                        this.show_settings = true;
                                        this.settings_category = SettingsCategory::Orchestrator;
                                        cx.notify();
                                    })),
                            ),
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
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(card)
            .into_any()
    }

    /// 机器选择（新会话视图）。
    fn render_machine_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().gap_1();
        if self.machines.is_empty() {
            row = row.child(Label::new("（请先在设置中添加机器）"));
        }
        for (i, m) in self.machines.iter().enumerate() {
            let sel = self.new_session_machine == Some(i)
                || (self.new_session_machine.is_none() && i == 0);
            let name = m.config.name.clone();
            row = row.child(
                Button::new(format!("ns-machine-{i}"))
                    .small()
                    .label(name.clone())
                    .when(sel, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        this.new_session_machine = Some(i);
                        this.new_session_harness = None; // 换机器后 agent 重选
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// agent 选择（新会话视图；来自该机器自动发现的 agent，PRD §3.3）。
    fn render_harness_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machine = self.new_session_machine.unwrap_or(0);
        let info = self.machine(machine).and_then(|m| m.info.clone());
        let mut row = h_flex().gap_1();
        match info {
            None => {
                row = row.child(Label::new("（正在获取该机器 agent 列表…）"));
            }
            Some(info) => {
                let harnesses: Vec<String> = info
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.clone())
                    .collect();
                if harnesses.is_empty() {
                    row = row.child(Label::new("（该机器未检测到 agent）"));
                } else {
                    for (i, h) in harnesses.iter().enumerate() {
                        let sel = self.new_session_harness.as_deref() == Some(h.as_str())
                            || (self.new_session_harness.is_none() && i == 0);
                        let hh = h.clone();
                        row = row.child(
                            Button::new(format!("ns-harness-{hh}"))
                                .small()
                                .label(hh.clone())
                                .when(sel, |b| b.primary())
                                .on_click(cx.listener(move |this, _ev, _window, cx| {
                                    this.new_session_harness = Some(hh.clone());
                                    cx.notify();
                                })),
                        );
                    }
                }
            }
        }
        row
    }

    /// Skills 安装目标选择对话框（PRD §3.6）：选机器与 agent，确认后创建会话安装。
    fn render_skill_install_dialog(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(dialog) = &self.skill_install_dialog else {
            return div().into_any();
        };
        let machine = dialog.machine;
        let harness = dialog.harness.clone();
        let skill_name = dialog.skill.name.clone();

        // 机器选择（pill）
        let mut machine_row = h_flex().gap_1().flex_wrap();
        if self.machines.is_empty() {
            machine_row = machine_row.child(Label::new("（请先在设置中添加机器）"));
        }
        for (i, m) in self.machines.iter().enumerate() {
            let sel = machine == Some(i) || (machine.is_none() && i == 0);
            let name = m.config.name.clone();
            machine_row = machine_row.child(
                Button::new(format!("skd-machine-{i}"))
                    .small()
                    .label(name.clone())
                    .when(sel, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if let Some(d) = this.skill_install_dialog.as_mut() {
                            d.machine = Some(i);
                            d.harness = None; // 换机器后 agent 重选
                        }
                        cx.notify();
                    })),
            );
        }

        // agent 选择（pill；来自该机器自动发现的 agent 列表）
        let mi = machine.unwrap_or(0);
        let info = self.machine(mi).and_then(|m| m.info.clone());
        let mut harness_row = h_flex().gap_1().flex_wrap();
        match info {
            None => {
                harness_row = harness_row.child(Label::new("（正在获取该机器 agent 列表…）"));
            }
            Some(info) => {
                let harnesses: Vec<String> = info
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.clone())
                    .collect();
                if harnesses.is_empty() {
                    harness_row = harness_row.child(Label::new("（该机器未检测到 agent）"));
                } else {
                    for (i, h) in harnesses.iter().enumerate() {
                        let sel =
                            harness.as_deref() == Some(h.as_str()) || (harness.is_none() && i == 0);
                        let hh = h.clone();
                        harness_row = harness_row.child(
                            Button::new(format!("skd-harness-{hh}"))
                                .small()
                                .label(hh.clone())
                                .when(sel, |b| b.primary())
                                .on_click(cx.listener(move |this, _ev, _window, cx| {
                                    if let Some(d) = this.skill_install_dialog.as_mut() {
                                        d.harness = Some(hh.clone());
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                }
            }
        }

        div()
            .id("skill-install-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("skill-install-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(hsla(0., 0., 0., 0.3))
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.skill_install_dialog = None;
                        cx.notify();
                    })),
            )
            .child(
                v_flex()
                    .id("skill-install-card")
                    .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                        cx.stop_propagation();
                    })
                    .w(px(460.))
                    .p_4()
                    .gap_3()
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .shadow_lg()
                    .child(
                        Label::new(format!("安装 skill：{}", skill_name))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new("选择安装到的机器")
                            .text_sm()
                            .text_color(rgb(0x6b7280)),
                    )
                    .child(machine_row)
                    .child(Label::new("选择 agent").text_sm().text_color(rgb(0x6b7280)))
                    .child(harness_row)
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_1()
                            .child(Button::new("skd-cancel").small().label("取消").on_click(
                                cx.listener(|this, _ev, _window, cx| {
                                    this.skill_install_dialog = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("skd-confirm")
                                    .small()
                                    .primary()
                                    .label("确认安装")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        // 确认：按所选机器/agent 创建会话并发送安装提示词
                                        let Some(d) = this.skill_install_dialog.clone() else {
                                            return;
                                        };
                                        this.skill_install_dialog = None;
                                        let mi = d.machine.or_else(|| this.active_machine());
                                        let Some(mi) = mi else {
                                            return;
                                        };
                                        let harness = d
                                            .harness
                                            .clone()
                                            .or_else(|| this.available_harness(mi));
                                        let Some(harness) = harness else {
                                            if let Some(m) = this.machine_mut(mi) {
                                                m.status =
                                                    "获取 agent 列表失败，请检查机器连接".into();
                                            }
                                            cx.notify();
                                            return;
                                        };
                                        this.install_skill(
                                            window,
                                            cx,
                                            mi,
                                            harness,
                                            d.skill.clone(),
                                        );
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 工作流模板选择（新会话视图，PRD §3.7 从模板创建）：点击仅选中，
    /// 由「创建工作流会话」按钮统一创建。
    fn render_template_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let templates = self.store.list_templates();
        let selected_id = self.workflow_template.as_ref().map(|t| t.id.clone());
        let mut row = h_flex().gap_1();
        if templates.is_empty() {
            row = row.child(Label::new("（暂无模板，可在设置中新建）"));
        }
        for t in templates {
            let tpl = t.clone();
            let selected = selected_id.as_deref() == Some(t.id.as_str());
            row = row.child(
                Button::new(format!("ns-tpl-{}", t.id))
                    .small()
                    .label(t.name.clone())
                    .when(selected, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        this.workflow_template = Some(tpl.clone());
                        this.workflow_error = None;
                        cx.notify();
                    })),
            );
        }
        row
    }

    fn render_dialog(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut workflow_hidden = 0usize;
        let dialog = match &self.selected {
            Some(Selected::Session { machine, id: _ }) => self
                .machine(*machine)
                .and_then(|m| m.selected_view())
                .map(|v| v.dialog.clone())
                .unwrap_or_default(),
            Some(Selected::Workflow { engine }) => self
                .workflows
                .get(*engine)
                .map(|w| {
                    let key = (w.session.id.clone(), w.session.transcript.len());
                    let mut cache = self.workflow_dialog_cache.borrow_mut();
                    if cache.as_ref().map(|(k, _)| k) != Some(&key) {
                        *cache = Some((key, w.session.to_dialog_items()));
                    }
                    let all = cache.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
                    // 滚动加载：只渲染最近一窗，向上加载更多
                    let start = all.len().saturating_sub(self.workflow_dialog_limit);
                    workflow_hidden = start;
                    all[start..].to_vec()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // agent 输出气泡标注 agent@机器（普通会话）；工作流会话气泡标注"编排"
        let agent_label: SharedString = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| {
                    let machine_name = m.config.name.clone();
                    m.sessions
                        .iter()
                        .find(|s| &s.id == id)
                        .map(|s| format!("{}@{machine_name}", s.harness).into())
                })
                .unwrap_or_else(|| "Agent".into()),
            Some(Selected::Workflow { .. }) => "编排".into(),
            None => "Agent".into(),
        };
        let rows = dialog
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DialogItem::UserMessage { content, .. } => div().id(("row", i)).w_full().child(
                    div()
                        .ml_auto()
                        .max_w(px(720.))
                        .p_3()
                        // 头部标签与消息内容之间留一点间隔
                        .v_flex()
                        .gap_1()
                        .rounded_md()
                        .bg(rgb(0x3b82f6))
                        .shadow_sm()
                        .child(
                            Label::new("我")
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff)),
                        )
                        // 用户消息：蓝底白字，可选中/复制（纯文本走 Markdown 解析为段落，继承白色）
                        .child(
                            TextView::markdown(format!("umd-{i}"), block_text(content))
                                .selectable(true)
                                .text_color(rgb(0xffffff)),
                        ),
                ),
                DialogItem::AgentOutput { content, .. } => div().id(("row", i)).w_full().child(
                    div()
                        .max_w(px(720.))
                        .p_3()
                        // 头部标签与消息内容之间留一点间隔
                        .v_flex()
                        .gap_1()
                        .rounded_md()
                        .bg(rgb(0xffffff))
                        .border_1()
                        .border_color(rgb(0xe5e7eb))
                        .shadow_sm()
                        .child(
                            Label::new(agent_label.clone())
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0x6b7280)),
                        )
                        // agent 输出按 Markdown 渲染（docs/DESIGN.md §7 / PRD §4.1.3），可选中/复制
                        .child(
                            TextView::markdown(format!("amd-{i}"), block_text(content))
                                .selectable(true),
                        ),
                ),
                DialogItem::SystemMessage { content, .. } => div().id(("row", i)).w_full().child(
                    div()
                        .ml_auto()
                        .max_w(px(720.))
                        .p_3()
                        .v_flex()
                        .gap_1()
                        .rounded_md()
                        .bg(rgb(0xf3f4f6))
                        .border_1()
                        .border_color(rgb(0xe5e7eb))
                        .shadow_sm()
                        .child(
                            Label::new("系统")
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0x6b7280)),
                        )
                        // 系统消息同样以气泡 + Markdown 渲染（与用户/编排消息一致）
                        .child(
                            TextView::markdown(format!("smd-{i}"), block_text(content))
                                .selectable(true),
                        ),
                ),
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            div()
                .id("dialog-empty")
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new("选择左侧会话查看对话，或输入消息开始").text_color(rgb(0x9ca3af)))
                .into_any()
        } else {
            let mut children: Vec<gpui::AnyElement> = Vec::new();
            // 惰性加载：还有更早历史时顶部显示"加载更早消息"（PRD §3.2）
            if let Some(Selected::Session { machine, id }) = self.selected.clone() {
                if let Some(m) = self.machine(machine) {
                    if m.dialog_has_more && m.dialog_before > 0 {
                        children.push(
                            h_flex()
                                .w_full()
                                .justify_center()
                                .child(
                                    Button::new("load-earlier")
                                        .small()
                                        .label("加载更早消息")
                                        .on_click(cx.listener(move |this, _ev, window, cx| {
                                            let id = id.clone();
                                            this.load_earlier_history(window, cx, machine, id);
                                        })),
                                )
                                .into_any_element(),
                        );
                    }
                }
            }
            // 工作流会话本地转录的滚动加载
            if workflow_hidden > 0 {
                children.push(
                    h_flex()
                        .w_full()
                        .justify_center()
                        .child(
                            Button::new("load-earlier-workflow")
                                .small()
                                .label(format!("加载更早消息（还有 {workflow_hidden} 条）"))
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.workflow_dialog_limit += 50;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
            children.extend(rows.into_iter().map(|r| r.into_any_element()));
            div()
                .id("dialog")
                // 必须是 flex 容器 gap 才生效（flex_1 只设 grow/shrink，不设 display）
                .v_flex()
                .flex_1()
                // 气泡之间间隔 16px（gap_4），带头部标签的消息流更易区分
                .gap_4()
                .p_2()
                .overflow_y_scroll()
                .track_scroll(&self.dialog_scroll)
                .children(children)
                .into_any()
        }
    }

    /// 中间面板下方：正在进行的活动（一条或无，空闲不显示，PRD §4.1.3）。
    /// 实时活动来自当前会话聚合视图的 live_activity（GUI 从透传事件合并）。
    fn render_activity_bar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let current = match &self.selected {
            Some(Selected::Session { machine, .. }) => self
                .machine(*machine)
                .and_then(|m| m.selected_view())
                .and_then(|v| v.live_activity.clone()),
            // 工作流会话：工作中显示"编排中…"
            Some(Selected::Workflow { engine }) => {
                let busy = self
                    .workflows
                    .get(*engine)
                    .map(|wf| wf.session.state == SessionState::Busy)
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
        match &current {
            Some(Activity::Thinking { content, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfcd34d))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!("思考中：{}", one_line(content, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(0x92400e)),
                )
                .into_any(),
            Some(Activity::ToolCall { name, title, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfcd34d))
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
                    .text_color(rgb(0x92400e)),
                )
                .into_any(),
            Some(Activity::Compaction { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfcd34d))
                .rounded_md()
                .child(
                    Label::new(format!("上下文压缩：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(0x92400e)),
                )
                .into_any(),
            None => div().id("activity-bar-empty").into_any(),
        }
    }

    /// 对话流右侧竖排悬浮按钮：diff / 会话详情 / 会话活动历史（PRD §4.1.3）。
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
            .bg(rgb(0xffffff))
            .rounded_md()
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .shadow_sm()
            .child(
                Button::new("float-diff")
                    .small()
                    .label("Diff")
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

    fn render_quick_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 快捷指令栏（PRD §3.4 / §4.1.3）：预设 + 用户自定义 + 取消当前工作（PRD §3.1）
        let commands = self.store.list_quick_commands();
        let mut row = h_flex().flex_wrap().gap_1();
        for c in commands {
            let name = c.name.clone();
            let cmd = c.clone();
            row = row.child(
                Button::new(format!("qc-{}", c.id))
                    .small()
                    .label(name)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.quick_command(window, cx, &cmd);
                    })),
            );
        }
        row.child(
            Button::new("cancel-work")
                .small()
                .label("✕ 取消")
                .on_click(cx.listener(|this, _ev, window, cx| {
                    if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                        if let Some(m) = this.machine(machine) {
                            let client = m.client.clone();
                            cx.spawn_in(window, async move |_this: WeakEntity<Self>, _cx| {
                                let _ = client
                                    .request(
                                        protocol::method::CANCEL,
                                        Some(json!({ "sessionId": id })),
                                    )
                                    .await;
                            })
                            .detach();
                        }
                    }
                })),
        )
    }

    /// 输入区：多行文本 + 附件（@ 引用/拖拽/粘贴图片/语音）+ 发送（PRD §4.2）。
    fn render_input(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let attachments: Vec<String> = self
            .input_attachments
            .iter()
            .map(|a| match a {
                InputAttachment::Path { path, .. } => format!("📎 {path}"),
                InputAttachment::Image { name, .. } => format!("🖼 {name}"),
                InputAttachment::Audio { name, .. } => format!("🎤 {name}"),
            })
            .collect();
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(rgb(0xe5e7eb))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(attachments.iter().map(|a| {
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded_full()
                            .bg(rgb(0xf3f4f6))
                            .child(Label::new(a.clone()).text_xs().text_color(rgb(0x4b5563)))
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(80.))
                            .id("input-drop-zone")
                            .child(Input::new(&self.input_state))
                            // Ctrl+Enter 发送（PRD §4.2：多行输入 + 快捷键发送）
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                                if ev.keystroke.modifiers.control && ev.keystroke.key == "enter" {
                                    this.send_prompt(window, cx);
                                }
                            }))
                            // 拖拽文件/目录 → 路径附件（PRD §4.2）：与 @ 引用走同一
                            // 附件管线（InputAttachment::Path → compose_prompt → PROMPT）
                            .can_drop(|dragged, _window, _cx| dragged.is::<ExternalPaths>())
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    for p in paths.paths() {
                                        this.input_attachments
                                            .push(path_attachment(&p.display().to_string()));
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
                    .child(
                        Button::new("voice")
                            .small()
                            .label(if self.voice_recording {
                                "■ 停止"
                            } else {
                                "🎤"
                            })
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.toggle_voice(cx);
                            })),
                    )
                    .child(
                        Button::new("paste-image")
                            .small()
                            .label("📋 粘贴图片")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.paste_image(cx);
                            })),
                    )
                    .child(
                        Button::new("clear-attachments")
                            .small()
                            .label("清空附件")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.input_attachments.clear();
                                cx.notify();
                            })),
                    ),
            )
    }

    // ---- 右侧面板 ----

    /// 面板逻辑宽度（px）；窗口向右扩展量据此计算。
    fn panel_width_logical(panel: Panel) -> f32 {
        match panel {
            Panel::Diff => 460.0,
            Panel::Detail => 360.0,
            Panel::Activities => 400.0,
        }
    }

    /// 打开/切换/关闭右侧上下文面板：窗口**向右扩展**（中间面板宽度不变），
    /// 关闭时收回（docs/DESIGN.md §7 / PRD §4.1.4）。
    fn set_panel(&mut self, window: &mut Window, cx: &mut Context<Self>, panel: Option<Panel>) {
        let new_delta = panel.map(Self::panel_width_logical).unwrap_or(0.0) * window.scale_factor();
        let bounds = window.bounds();
        // 基准宽度 = 当前宽度 - 已扩展量（手动缩放窗口时下次切换自动校正）
        let base = bounds.size.width - new_delta.into();
        // 打开活动历史面板：重置滚动加载窗口并滚到底部（最新）
        if panel == Some(Panel::Activities) && self.panel != Some(Panel::Activities) {
            self.activities_limit = 100;
            self.activities_scroll.scroll_to_bottom();
        }
        self.panel = panel;
        self.panel_delta_px = new_delta;
        let width: gpui::Pixels = base + new_delta.into();
        window.resize(gpui::Size::new(width, bounds.size.height));
        cx.notify();
    }

    fn render_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        match self.panel {
            Some(Panel::Diff) => Some(self.render_diff_panel(window, cx)),
            Some(Panel::Detail) => Some(self.render_detail_panel(window, cx)),
            Some(Panel::Activities) => Some(self.render_activities_panel(window, cx)),
            None => None,
        }
    }

    /// Diff Review 面板（PRD §3.5）：文件列表 + 增减统计 + inline/side-by-side + revert。
    fn render_diff_panel(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machine = match &self.selected {
            Some(Selected::Session { machine, .. }) => *machine,
            _ => 0,
        };
        let m = self.machine(machine);
        let status = m.and_then(|m| m.diff_status.clone());
        let files = m.map(|m| m.diff_files.clone()).unwrap_or_default();
        let not_repo = status.as_ref().map(|s| s.not_repo).unwrap_or(false);

        let mut body = v_flex().flex_1().gap_1().overflow_y_scrollbar();
        if not_repo {
            body = body.child(Label::new("当前工作目录不是 git 仓库（不提供 diff）"));
        } else if files.is_empty() {
            body = body.child(Label::new("（无变更或尚未加载）"));
        } else {
            // 文件列表 + 每文件增减行数统计
            let mut list = v_flex().gap_1();
            for f in &files {
                let path = f.path.clone();
                let adds = f.additions;
                let dels = f.deletions;
                let selected = m
                    .map(|m| m.diff_path.as_deref() == Some(path.as_str()))
                    .unwrap_or(false);
                let btn = Button::new(format!("diff-file-{path}"))
                    .small()
                    .label(format!("{}  +{adds} -{dels}", f.path))
                    .when(selected, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        let path = path.clone();
                        if let Some(Selected::Session { machine, .. }) = this.selected.clone() {
                            this.load_file_diff(window, cx, machine, path);
                        }
                    }));
                list = list.child(btn);
            }
            body = body.child(list);

            // 模式切换：inline / side-by-side
            let mode = m.map(|m| m.diff_mode).unwrap_or(DiffMode::Inline);
            body = body.child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("diff-inline")
                            .small()
                            .label("Inline")
                            .when(mode == DiffMode::Inline, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                if let Some(Selected::Session { machine, .. }) =
                                    this.selected.clone()
                                {
                                    if let Some(m) = this.machine_mut(machine) {
                                        m.diff_mode = DiffMode::Inline;
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("diff-side")
                            .small()
                            .label("Side-by-side")
                            .when(mode == DiffMode::SideBySide, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                if let Some(Selected::Session { machine, .. }) =
                                    this.selected.clone()
                                {
                                    if let Some(m) = this.machine_mut(machine) {
                                        m.diff_mode = DiffMode::SideBySide;
                                    }
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("diff-revert-all")
                            .small()
                            .label("全部 revert")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                if let Some(Selected::Session { machine, .. }) =
                                    this.selected.clone()
                                {
                                    this.revert(window, cx, machine, None, None);
                                }
                            })),
                    ),
            );

            // 渲染选中的文件（或全部）
            for f in &files {
                let show = m
                    .map(|m| m.diff_path.as_deref().map(|p| p == f.path).unwrap_or(true))
                    .unwrap_or(true);
                if !show {
                    continue;
                }
                let path = f.path.clone();
                let file = f.clone();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(format!(
                                    "{}  +{} -{}",
                                    f.path, f.additions, f.deletions
                                )))
                                .child(
                                    Button::new(format!("revert-file-{path}"))
                                        .small()
                                        .label("revert 文件")
                                        .on_click(cx.listener(move |this, _ev, window, cx| {
                                            let path = path.clone();
                                            if let Some(Selected::Session { machine, .. }) =
                                                this.selected.clone()
                                            {
                                                this.revert(window, cx, machine, Some(path), None);
                                            }
                                        })),
                                ),
                        )
                        .child(self.render_diff_content(&file, mode, machine, cx)),
                );
            }
        }
        v_flex()
            .w(px(460.))
            .h_full()
            .gap_2()
            .p_3()
            .bg(rgb(0xffffff))
            .border_l_1()
            .border_color(rgb(0xe5e7eb))
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("工作区 Diff")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x111827)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel")
                            .small()
                            .label("✕")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(body)
            .into_any()
    }

    /// hunk 操作行：revert 该 hunk + 选中该片段发送给 agent（PRD §3.5）。
    fn render_hunk_row(
        &self,
        file_path: String,
        hunk_header: String,
        hunk_patch: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let revert_patch = hunk_patch.clone();
        let send_path = file_path.clone();
        let send_header = hunk_header.clone();
        let send_patch = hunk_patch.clone();
        h_flex()
            .gap_1()
            .child(Label::new(&hunk_header))
            .child(
                Button::new(format!("hunk-revert-{file_path}-{hunk_header}"))
                    .small()
                    .label("revert hunk")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some(Selected::Session { machine, .. }) = this.selected.clone() {
                            this.revert(window, cx, machine, None, Some(revert_patch.clone()));
                        }
                    })),
            )
            .child(
                Button::new(format!("hunk-send-{send_path}-{send_header}"))
                    .small()
                    .label("发送给 agent")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        // 选中该片段直接向 agent 发送指令（PRD §3.5）
                        let instruction = format!("请处理以下代码变更片段：\n{}", send_patch);
                        if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                            let client = this.machine(machine).map(|m| m.client.clone());
                            if let Some(client) = client {
                                let input = vec![ContentBlock::Text { text: instruction }];
                                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                                    let _ = client
                                        .request(
                                            protocol::method::PROMPT,
                                            Some(json!({
                                                "sessionId": id,
                                                "input": input
                                            })),
                                        )
                                        .await;
                                    let _ = this.update_in(cx, |_this, _window, cx| {
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }
                        }
                    })),
            )
    }

    /// 单文件 diff 渲染：inline（带 +/- 着色）或 side-by-side（左右分栏）；
    /// 每个 hunk 提供 revert 与"发送给 agent"。
    fn render_diff_content(
        &self,
        file: &GitDiffFile,
        mode: DiffMode,
        _machine: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().gap_0();
        match mode {
            DiffMode::Inline => {
                for hunk in &file.hunks {
                    rows = rows.child(self.render_hunk_row(
                        file.path.clone(),
                        hunk.header.clone(),
                        hunk.patch.clone(),
                        cx,
                    ));
                    for (li, line) in hunk.patch.lines().enumerate() {
                        let (bg, text) = if line.starts_with('+') && !line.starts_with("+++") {
                            (Some(rgb(0xe6ffe6)), line.to_string())
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            (Some(rgb(0xffe6e6)), line.to_string())
                        } else {
                            (None, line.to_string())
                        };
                        let mut row = div().id(("diff-line", li)).w_full().child(text.clone());
                        if let Some(c) = bg {
                            row = row.bg(c);
                        }
                        rows = rows.child(row);
                    }
                }
            }
            DiffMode::SideBySide => {
                // side-by-side：简单双栏（旧/新），从 hunk 行解析
                for hunk in &file.hunks {
                    rows = rows.child(self.render_hunk_row(
                        file.path.clone(),
                        hunk.header.clone(),
                        hunk.patch.clone(),
                        cx,
                    ));
                    let mut left = v_flex().gap_0().flex_1();
                    let mut right = v_flex().gap_0().flex_1();
                    for line in hunk.patch.lines() {
                        if line.starts_with('-')
                            && !line.starts_with("---")
                            && !line.starts_with("diff")
                        {
                            left = left
                                .child(div().w_full().bg(rgb(0xffe6e6)).child(line.to_string()));
                            right = right.child(div().w_full().child(""));
                        } else if line.starts_with('+') && !line.starts_with("+++") {
                            left = left.child(div().w_full().child(""));
                            right = right
                                .child(div().w_full().bg(rgb(0xe6ffe6)).child(line.to_string()));
                        } else if !line.starts_with("@@") {
                            left = left.child(div().w_full().child(line.to_string()));
                            right = right.child(div().w_full().child(line.to_string()));
                        }
                    }
                    rows = rows.child(
                        h_flex()
                            .gap_1()
                            .child(left.child(Label::new("旧")))
                            .child(right.child(Label::new("新"))),
                    );
                }
            }
        }
        rows
    }

    /// 会话详情面板。
    fn render_detail_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(meta) = self.selected_meta() else {
            return div().w(px(340.)).child(Label::new("未选择会话")).into_any();
        };
        let mut body =
            v_flex()
                .w(px(360.))
                .h_full()
                .gap_2()
                .p_3()
                .bg(rgb(0xffffff))
                .border_l_1()
                .border_color(rgb(0xe5e7eb))
                .child(
                    h_flex()
                        .items_center()
                        .child(
                            Label::new("会话详情")
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0x111827)),
                        )
                        .child(div().flex_1())
                        .child(Button::new("close-panel2").small().label("✕").on_click(
                            cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            }),
                        )),
                )
                .child(info_row("ID", &meta.id))
                .child(info_row("Agent", &meta.harness))
                .child(info_row("工作目录", &meta.cwd))
                .child(info_row(
                    "状态",
                    if meta.interrupted {
                        "已中断"
                    } else if meta.state == SessionState::Busy {
                        "工作中"
                    } else {
                        "空闲"
                    },
                ));
        // 标题展示（重命名走会话列表右键菜单，PRD §3.1）
        let title = if meta.title.is_empty() {
            "（未命名）".to_string()
        } else {
            meta.title.clone()
        };
        body = body.child(Label::new(format!("标题: {title}")));
        // 普通会话：删除 / 重命名在左侧会话列表右键菜单
        if let Some(Selected::Session { .. }) = self.selected.clone() {
            body = body.child(
                Label::new("删除 / 重命名：在左侧会话列表右键该会话")
                    .text_xs()
                    .text_color(rgb(0x9ca3af)),
            );
        }
        // 工作流会话：取消/介入/子会话
        if let Some(Selected::Workflow { engine }) = self.selected.clone() {
            let done = self
                .workflows
                .get(engine)
                .map(|w| w.session.done)
                .unwrap_or(false);
            body = body
                .child(Label::new("— 工作流会话 —"))
                .child(
                    Button::new("wf-cancel")
                        .small()
                        .label("取消")
                        .when(done, |b| b.disabled(true))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.cancel_workflow(engine, window, cx);
                        })),
                )
                .child(Label::new("介入：在下方输入区输入指令发给编排 agent"))
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
                .map(|w| w.session.children.clone())
                .unwrap_or_default()
            {
                let step = c.step_desc.clone();
                body = body.child(Label::new(format!("子会话 {} · {}", c.id, step)));
            }
        }
        body.into_any()
    }

    /// 单条活动历史：过长内容折叠，可展开/收起。
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
            .bg(rgb(0xf5f6f8))
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

    /// 会话活动历史面板（PRD §4.1.4 会话活动）。
    fn render_activities_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 普通会话：当前会话聚合视图的活动历史（GUI 从透传事件聚合，docs/DESIGN.md §5.3）；
        // 工作流会话：仅实时编排状态（系统消息进入会话历史，不进活动）
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut live: Option<Activity> = None;
        match &self.selected {
            Some(Selected::Session { machine, .. }) => {
                let view = self.machine(*machine).and_then(|m| m.selected_view());
                let activities = view.map(|v| v.activities.clone()).unwrap_or_default();
                rows = activities
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let (kind, detail) = activity_display(a);
                        self.activity_row(&format!("act-{i}"), &kind, &detail, cx)
                    })
                    .collect();
                live = view.and_then(|v| v.live_activity.clone());
            }
            Some(Selected::Workflow { engine }) => {
                // 系统消息进入会话历史（气泡）；活动历史展示编排过程的活动（创建/介入子会话）
                if let Some(wf) = self.workflows.get(*engine) {
                    rows = wf
                        .session
                        .activities
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let (kind, detail) = activity_display(a);
                            self.activity_row(&format!("wf-act-{i}"), &kind, &detail, cx)
                        })
                        .collect();
                    if wf.session.state == SessionState::Busy {
                        live = Some(Activity::Thinking {
                            timestamp: wf.session.updated_at,
                            content: "正在编排决策/推进…".into(),
                        });
                    }
                }
            }
            _ => {}
        }
        // 滚动加载：只渲染最近 `activities_limit` 条，向上加载更多
        let total = rows.len();
        let start = total.saturating_sub(self.activities_limit);
        let has_more = start > 0;

        // 实时活动（turn 进行中合并流式的一条，追加在历史下方）
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if has_more {
            children.push(
                Button::new("load-more-activities")
                    .small()
                    .ghost()
                    .label(format!("加载更早活动（还有 {start} 条）"))
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.activities_limit += 100;
                        cx.notify();
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
                    .bg(rgb(0xfffbeb))
                    .border_1()
                    .border_color(rgb(0xfcd34d))
                    .rounded_md()
                    .child(Spinner::new())
                    .child(format!("[{kind}] {detail}"))
                    .into_any_element(),
            );
        }
        v_flex()
            .w(px(400.))
            .h_full()
            .gap_2()
            .p_3()
            .bg(rgb(0xffffff))
            .border_l_1()
            .border_color(rgb(0xe5e7eb))
            .child(
                Label::new("会话活动历史")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x111827)),
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

    // ---- 设置浮窗 ----

    fn render_settings_overlay(
        &self,
        _window: &mut Window,
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
                    .bg(hsla(0., 0., 0., 0.45))
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
                    .w(px(880.))
                    .h(px(620.))
                    .overflow_hidden()
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
            )
            // Skills 安装目标选择对话框（盖在设置浮窗之上）
            .when(self.skill_install_dialog.is_some(), |o| {
                o.child(self.render_skill_install_dialog(cx))
            })
    }

    fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-nav")
            .w(px(190.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(rgb(0xf0f1f4))
            .child(Label::new("设置"))
            .child(self.settings_nav_item(
                SettingsCategory::Machines,
                "cat-machines",
                "机器管理",
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::Orchestrator,
                "cat-orch",
                "编排 agent",
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::QuickCommands,
                "cat-qc",
                "快捷指令",
                cx,
            ))
            .child(self.settings_nav_item(SettingsCategory::Skills, "cat-skills", "Skills", cx))
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

    /// 设置页分类标题（美化）。
    fn settings_header(&self, title: &str, subtitle: &str) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(title).text_lg().font_weight(FontWeight::MEDIUM))
            .child(Label::new(subtitle).text_sm().text_color(rgb(0x6b7280)))
    }

    /// 设置项卡片：左侧文本列（flex-1 min_w_0 截断）+ 右侧操作区。
    /// `clamp`：0 = 不截断，1 = 单行省略号，n = 最多 n 行截断。
    fn settings_item(
        &self,
        title: &str,
        body: &str,
        clamp: usize,
        actions: impl IntoElement,
    ) -> impl IntoElement {
        let mut text_col = v_flex().flex_1().min_w_0().gap_0p5();
        text_col = text_col.child(Label::new(title).text_sm().font_weight(FontWeight::MEDIUM));
        let body_label = Label::new(body).text_sm().text_color(rgb(0x6b7280));
        text_col = text_col.child(match clamp {
            0 => body_label,
            1 => body_label.truncate(),
            n => body_label.line_clamp(n),
        });
        h_flex()
            .gap_2()
            .items_center()
            .p_2()
            .bg(rgb(0xf7f8fa))
            .rounded_md()
            .child(text_col)
            .child(actions)
    }

    /// 机器管理：接入/移除 + agent 默认模型 + skills 列表（PRD §4.3）。
    fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines = self
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut item = v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    // 机器头：名称 + 在线状态徽章 + 移除
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(&m.config.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(machine_status_badge(&m.status))
                            .child(div().flex_1())
                            .child(
                                Button::new(format!("remove-{i}"))
                                    .small()
                                    .label("移除")
                                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                                        this.remove_machine(i, cx);
                                    })),
                            ),
                    )
                    // URL 长文本单行省略
                    .child(
                        Label::new(&m.config.url)
                            .text_xs()
                            .text_color(rgb(0x9ca3af))
                            .truncate(),
                    );
                // agent 发现 + 默认模型
                if let Some(info) = &m.info {
                    for h in &info.harnesses {
                        let mut row = h_flex().gap_2().items_center().child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    Label::new(format!(
                                        "agent: {}（{}）",
                                        h.name,
                                        if h.available { "可用" } else { "不可用" }
                                    ))
                                    .text_sm(),
                                )
                                .child(
                                    Label::new(format!(
                                        "默认模型: {}",
                                        h.default_model.clone().unwrap_or_else(|| "未设置".into())
                                    ))
                                    .text_xs()
                                    .text_color(rgb(0x6b7280))
                                    .truncate(),
                                ),
                        );
                        let harness = h.name.clone();
                        let mi = i;
                        let harness_a = harness.clone();
                        let harness_b = harness.clone();
                        row = row
                            .child(
                                Button::new(format!("skills-{i}-{harness}"))
                                    .small()
                                    .label("skills")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let harness = harness_a.clone();
                                        this.fetch_agent_skills(window, cx, mi, harness);
                                    })),
                            )
                            .child(
                                Button::new(format!("model-{i}-{harness}"))
                                    .small()
                                    .label("设默认模型")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let harness = harness_b.clone();
                                        let model = this.model_input.read(cx).value().to_string();
                                        this.set_default_model(window, cx, mi, harness, model);
                                    })),
                            );
                        item = item.child(row);
                    }
                    // 当前查看的 skills 列表（长文本两行截断）
                    if m.skills_harness.is_some() {
                        item = item.child(
                            Label::new(format!(
                                "skills ({}): {}",
                                m.skills_harness.as_deref().unwrap_or(""),
                                if m.skills.is_empty() {
                                    "（无）".to_string()
                                } else {
                                    m.skills.join(", ")
                                }
                            ))
                            .text_xs()
                            .text_color(rgb(0x6b7280))
                            .line_clamp(2),
                        );
                    }
                }
                item
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "机器管理",
                "接入 / 移除机器；每台机器自动发现本机 ACP agent，可配置默认模型、查看 skills",
            ))
            .child(
                Label::new("默认模型输入：")
                    .text_sm()
                    .text_color(rgb(0x6b7280)),
            )
            .child(Input::new(&self.model_input))
            .children(machines)
            .child(self.settings_header("添加机器", ""))
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf7f8fa))
                    .rounded_md()
                    .child(Label::new("名称").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.machine_name_input))
                    .child(Label::new("连接地址").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.machine_url_input))
                    .child(Label::new("Token").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.machine_token_input))
                    .child(
                        Button::new("settings-add")
                            .small()
                            .primary()
                            .label("添加机器")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                let name = this.machine_name_input.read(cx).value().to_string();
                                let url = this.machine_url_input.read(cx).value().to_string();
                                let token = this.machine_token_input.read(cx).value().to_string();
                                this.add_machine(window, cx, name, url, token);
                            })),
                    ),
            )
            .into_any()
    }

    /// 编排 agent API 配置（PRD §4.3「编排 agent」）。
    fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let configured = self.store.orchestrator().is_configured();
        let mut col = v_flex().gap_2().child(self.settings_header(
            "编排 agent",
            "内置编排 agent（rig 单 turn）的 LLM API 配置；创建工作流前需填 Base URL、API key 与模型",
        ));
        // 配置状态提示
        col = col.child(
            h_flex().gap_1().items_center().child(
                Label::new(if configured {
                    "✓ 已配置，可创建工作流"
                } else {
                    "⚠ 未配置：Base URL 与 API key 为空，暂不能创建工作流"
                })
                .text_sm()
                .text_color(if configured {
                    rgb(0x16a34a)
                } else {
                    rgb(0xb91c1c)
                }),
            ),
        );
        col = col
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf7f8fa))
                    .rounded_md()
                    .child(Label::new("wire API").text_sm().text_color(rgb(0x6b7280)))
                    .child(self.render_wire_api_radio(cx))
                    .child(Label::new("Base URL").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.orch_base_input))
                    .child(Label::new("API key").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.orch_key_input))
                    .child(Label::new("模型").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.orch_model_input))
                    .child(
                        Label::new("wire API 取值：chat / responses")
                            .text_xs()
                            .text_color(rgb(0x9ca3af)),
                    ),
            )
            .child(
                Button::new("save-orch")
                    .small()
                    .primary()
                    .label("保存")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        let cfg = OrchestratorConfig {
                            wire_api: this.orch_wire_api.clone(),
                            base_url: this.orch_base_input.read(cx).value().to_string(),
                            api_key: this.orch_key_input.read(cx).value().to_string(),
                            model: this.orch_model_input.read(cx).value().to_string(),
                        };
                        this.store.save_orchestrator(&cfg);
                        this.workflow_error = None; // 配置好后清除创建工作流的提示
                        cx.notify();
                    })),
            );
        col.into_any()
    }

    /// 快捷指令：增删 + 修改提示词（PRD §3.4）。
    fn render_quick_commands_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commands = self.store.list_quick_commands();
        let items = commands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let id = c.id.clone();
                let id_edit = id.clone();
                let id_remove = id.clone();
                let name = c.name.clone();
                let prompt = c.prompt.clone();
                let actions = h_flex()
                    .gap_1()
                    .child(
                        Button::new(format!("qc-edit-name-{i}"))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let id = id_edit.clone();
                                let n = format!("{} ", name);
                                this.qc_name_input.update(cx, |s, cx| {
                                    s.set_value(n, window, cx);
                                });
                                this.qc_prompt_input.update(cx, |s, cx| {
                                    s.set_value(prompt.clone(), window, cx);
                                });
                                this.qc_edit_target = Some(id);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("qc-remove-{i}"))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.store.remove_quick_command(&id_remove);
                                cx.notify();
                            })),
                    );
                self.settings_item(&c.name, &c.prompt, 2, actions)
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "快捷指令",
                "预设 Commit & Push、Submit PR；每条即一段发给 agent 的提示词，可增删改",
            ))
            .children(items)
            .child(self.settings_header("新增 / 编辑", ""))
            .child(Input::new(&self.qc_name_input))
            .child(Input::new(&self.qc_prompt_input))
            .child(
                Button::new("qc-add")
                    .small()
                    .primary()
                    .label("保存指令")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let name = this.qc_name_input.read(cx).value().to_string();
                        let prompt = this.qc_prompt_input.read(cx).value().to_string();
                        if let Some(id) = this.qc_edit_target.clone() {
                            this.store.update_quick_command(&id, &name, &prompt);
                            this.qc_edit_target = None;
                        } else {
                            this.store.add_quick_command(&name, &prompt);
                        }
                        this.qc_name_input
                            .update(cx, |s, cx| s.set_value("", window, cx));
                        this.qc_prompt_input
                            .update(cx, |s, cx| s.set_value("", window, cx));
                        cx.notify();
                    })),
            )
            .into_any()
    }

    /// Skills 注册表（PRD §3.6）。
    fn render_skills_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let skills = self.store.list_skills();
        let items = skills
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let id = s.id.clone();
                let id_edit = id.clone();
                let id_remove = id.clone();
                let name = s.name.clone();
                let desc = s.description.clone();
                let skill = s.clone();
                let actions = h_flex()
                    .gap_1()
                    .child(
                        Button::new(format!("skill-install-{i}"))
                            .small()
                            .label("安装到 agent")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                // PRD §3.6：弹出选择框，选机器与 agent 后确认安装
                                this.skill_install_dialog = Some(SkillInstallDialog {
                                    skill: skill.clone(),
                                    machine: None,
                                    harness: None,
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("skill-edit-{i}"))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.skill_name_input.update(cx, |s, cx| {
                                    s.set_value(name.clone(), window, cx);
                                });
                                this.skill_desc_input.update(cx, |s, cx| {
                                    s.set_value(desc.clone(), window, cx);
                                });
                                this.skill_edit_target = Some(id_edit.clone());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("skill-remove-{i}"))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.store.remove_skill(&id_remove);
                                cx.notify();
                            })),
                    );
                // 描述可能是长 URL：单行省略号截断
                self.settings_item(&s.name, &s.description, 1, actions)
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "Skills 注册表",
                "每条只存一段描述：仓库/资源 URL 或下载安装方法说明；支持增删改",
            ))
            .children(items)
            .child(self.settings_header("新增 / 编辑", ""))
            .child(Input::new(&self.skill_name_input))
            .child(Input::new(&self.skill_desc_input))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("skill-add")
                            .small()
                            .primary()
                            .label("新增")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                let name = this.skill_name_input.read(cx).value().to_string();
                                let desc = this.skill_desc_input.read(cx).value().to_string();
                                if let Some(id) = this.skill_edit_target.clone() {
                                    this.store.update_skill(&id, &name, &desc);
                                    this.skill_edit_target = None;
                                } else {
                                    this.store.add_skill(&name, &desc);
                                }
                                this.skill_name_input
                                    .update(cx, |s, cx| s.set_value("", window, cx));
                                this.skill_desc_input
                                    .update(cx, |s, cx| s.set_value("", window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        Label::new("点击「安装到 agent」弹出选择框，选机器与 agent 后确认安装")
                            .text_xs()
                            .text_color(rgb(0x9ca3af)),
                    ),
            )
            .into_any()
    }

    /// 工作流模板（PRD §3.7）。
    fn render_templates_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        let items = templates
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let id = t.id.clone();
                let id_edit = id.clone();
                let id_remove = id.clone();
                let name = t.name.clone();
                let desc = t.description.clone();
                let actions = h_flex()
                    .gap_1()
                    .child(
                        Button::new(format!("tpl-edit-{i}"))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.tpl_name_input.update(cx, |s, cx| {
                                    s.set_value(name.clone(), window, cx);
                                });
                                this.tpl_desc_input.update(cx, |s, cx| {
                                    s.set_value(desc.clone(), window, cx);
                                });
                                this.tpl_edit_target = Some(id_edit.clone());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("tpl-remove-{i}"))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.store.remove_template(&id_remove);
                                cx.notify();
                            })),
                    );
                // 模板描述为自然语言：最多两行截断
                self.settings_item(&t.name, &t.description, 2, actions)
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "工作流模板",
                "可复用的自然语言工作流描述；支持查看 / 新建 / 编辑 / 删除",
            ))
            .children(items)
            .child(self.settings_header("新建 / 编辑", ""))
            .child(Input::new(&self.tpl_name_input))
            .child(Input::new(&self.tpl_desc_input))
            .child(
                Button::new("tpl-add")
                    .small()
                    .primary()
                    .label("保存模板")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        let name = this.tpl_name_input.read(cx).value().to_string();
                        let desc = this.tpl_desc_input.read(cx).value().to_string();
                        if let Some(id) = this.tpl_edit_target.clone() {
                            this.store.update_template(&id, &name, &desc);
                            this.tpl_edit_target = None;
                        } else {
                            this.store.add_template(&name, &desc);
                        }
                        this.tpl_name_input
                            .update(cx, |s, cx| s.set_value("", window, cx));
                        this.tpl_desc_input
                            .update(cx, |s, cx| s.set_value("", window, cx));
                        cx.notify();
                    })),
            )
            .into_any()
    }
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.render_panel(window, cx);
        // 无边框窗口：标题栏按住可拖动窗口（GPUI 无 OS 标题栏，需 start_window_move）
        let title_bar = h_flex()
            .id("title-bar")
            .h(px(28.))
            .gap_2()
            .items_center()
            .px_2()
            .bg(rgb(0xe8eaee))
            .border_b_1()
            .border_color(rgb(0xd8dbe0))
            .on_mouse_down(MouseButton::Left, |_e, window, _cx| {
                window.start_window_move();
            })
            .child(
                Label::new("amux")
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0x374151)),
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
            .child(title_bar)
            .child(main_row);
        if let Some(menu) = &self.context_menu {
            root = root.child(self.render_context_menu(menu, window, cx));
        }
        if self.show_settings {
            root = root.child(self.render_settings_overlay(window, cx));
        }
        // gpui-component 的 Root 不自动渲染 sheet/dialog/notification 层，
        // 需应用在最顶层显式挂载（否则 open_alert_dialog 等不显示）
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

/// 在 GUI 的 tokio runtime 上执行编排引擎任务（rig/reqwest 的 LLM 调用需要
/// tokio reactor；GPUI 主线程无 runtime，docs/DESIGN.md §7「异步模型」）。
/// 引擎状态（wf）经 oneshot 传回；中断时返回 None。
async fn run_engine_on_tokio<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::ws::runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.ok()
}

fn _window_placeholder(w: &mut Window) -> &mut Window {
    w
}
