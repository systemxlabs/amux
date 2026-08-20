//! amux 主视图（docs/DESIGN.md §7 / PRD §「桌面 GUI 设计」）。
//! 三栏布局（左侧边栏 / 中间交互 / 右侧 Panel）+ 多机器 + 工作流编排 + 设置浮窗（五分类）。
//!
//! - 左侧：会话列表（普通会话 + 工作流会话统一按最近活跃排序；工作流挂载的关联普通会话
//!   默认折叠、可展开下钻）+ 顶部「+」新建会话入口 + 底部设置入口
//! - 中间：未选中会话显示新建会话视图（机器/Agent/工作目录/常用目录一屏并列、创建会话按钮）；
//!   选中会话显示对话历史气泡 + 实时活动条 + 快捷按钮栏 + 输入区（含取消）+ 右侧竖向悬浮按钮
//! - 右侧：代码审查 / 会话详情 / 会话活动，默认折叠，点悬浮按钮展开
//! - 设置浮窗：半透明遮罩 + 分类导航侧边栏（机器管理 / 编排智能体 / 快捷指令 / 技能管理 /
//!   工作流模板）+ 右侧内容；机器管理为竖向堆叠卡片式（机器头名称+状态徽章+移除+重连+URL
//!   单行截断；每 agent 一行=名称+可用/不可用+skills+重启；底部「+」弹表单；移除/重连/重启
//!   确认弹窗；skills 弹窗可滚动）
//!
//! 数据为拉取式：会话列表 `session.list` 定时 10s + 主动；对话 `session.history` 打开才刷 10s；
//! 活动 `session.activities` 打开才刷 10s；实时 `session.ongoing_activity` 5s；`session.state_change`
//! 通知用于工作流驱动。

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
    radio::RadioGroup,
    spinner::Spinner,
    text::{TextView, TextViewStyle},
    WindowExt, *,
};

use serde_json::{json, Value};

use protocol::{
    Activity, AgentInfo, AgentListResult, AgentParams, AgentSkillsResult, ContentBlock,
    GitDiffFile, HistoryItem, SessionConfigureParams, SessionIdParams, SessionMeta,
    SessionNewParams, SessionPageParams, SessionPromptParams, SessionState, SkillEntry,
    WorkspaceDiffResult,
};

use crate::aggregate::SessionView;
use crate::config::{
    machine_ws_url, ConfigStore, MachineConfig, OrchestratorConfig, QuickCommand, WorkflowTemplate,
};
use crate::display::{activity_display, info_row, machine_status_badge, short_cwd};
use crate::logic::{
    compose_prompt, merge_session_window, parse_at_references, path_attachment, read_path_context,
    DialogMsg, InputAttachment,
};
use crate::text::{block_text, one_line, truncate};
use crate::workflow::{now_ts, AgentSlot, MachineSummary, OrcBackend, RigBackend, WorkflowEngine};
use crate::ws::{Notification, WsClient};

/// 会话列表惰性分页窗口大小（PRD §4.1.1：首次只取最近活跃一窗）。
const PAGE_LIMIT: usize = 50;

/// 字符串 → SessionState（server 通知负载用 snake_case）。
fn state_from_str(s: &str) -> Option<SessionState> {
    match s {
        "busy" => Some(SessionState::Busy),
        "idle" => Some(SessionState::Idle),
        _ => None,
    }
}

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

/// 新会话创建模式（普通 / 工作流）。
#[derive(Clone, Copy, PartialEq)]
enum NewSessionMode {
    Direct,
    Workflow,
}

/// diff 渲染模式（PRD §3.5）。
#[derive(Clone, Copy, PartialEq)]
enum DiffMode {
    Inline,
    SideBySide,
}

/// 单机器视图：独立 WS 连接 + agent 列表 + 会话列表 + 各会话聚合视图 + diff/skills 状态。
struct MachineView {
    config: MachineConfig,
    client: WsClient,
    connection_epoch: u64,
    status: String,
    agents: Vec<AgentInfo>,
    sessions: Vec<SessionMeta>,
    sessions_has_more: bool,
    sessions_next_before: Option<u64>,
    /// 各会话的聚合视图（对话 / 活动 / 实时）。
    views: std::collections::HashMap<String, SessionView>,
    diff_files: Vec<GitDiffFile>,
    diff_path: Option<String>,
    diff_mode: DiffMode,
    diff_not_repo: bool,
    skills: Vec<String>,
    skills_agent: Option<String>,
    /// skills 弹窗的引用来源（机器下标 + agent 名）。
    show_skills: Option<(usize, String)>,
}

impl MachineView {
    fn new(config: MachineConfig) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            client: WsClient::connect_with_token(url, config.token.clone()),
            config,
            connection_epoch: 1,
            status: "连接中…".into(),
            agents: Vec::new(),
            sessions: Vec::new(),
            sessions_has_more: false,
            sessions_next_before: None,
            views: std::collections::HashMap::new(),
            diff_files: Vec::new(),
            diff_path: None,
            diff_mode: DiffMode::Inline,
            diff_not_repo: false,
            skills: Vec::new(),
            skills_agent: None,
            show_skills: None,
        }
    }
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
    title: String,
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
    panel_delta_px: f32,
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
    orch_api_format: String,
    orch_base_input: Entity<InputState>,
    orch_key_input: Entity<InputState>,
    orch_model_input: Entity<InputState>,
    orchestrator_form_error: Option<String>,
    title_input: Entity<InputState>,
    qc_edit_target: Option<String>,
    skill_edit_target: Option<String>,
    tpl_edit_target: Option<String>,
    context_menu: Option<SessionContextMenu>,
    renaming_session: Option<(usize, String)>,
    renaming_workflow: Option<usize>,
    new_session_machine: Option<usize>,
    new_session_agent: Option<String>,
    workflow_error: Option<String>,
    workflow_template: Option<WorkflowTemplate>,
    dialog_scroll: ScrollHandle,
    activities_scroll: ScrollHandle,
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
        let tpl_desc_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("执行计划（自然语言描述）"));
        let orch_base_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Base URL（如 https://api…/v1）"));
        let orch_key_input = cx.new(|cx| InputState::new(window, cx).placeholder("API Key"));
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
            panel_delta_px: 0.0,
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
            orch_api_format: "chat_completions".into(),
            orchestrator_form_error: None,
            title_input,
            qc_edit_target: None,
            skill_edit_target: None,
            tpl_edit_target: None,
            context_menu: None,
            renaming_session: None,
            renaming_workflow: None,
            new_session_machine: None,
            new_session_agent: None,
            workflow_error: None,
            workflow_template: None,
            dialog_scroll: ScrollHandle::new(),
            activities_scroll: ScrollHandle::new(),
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

    fn save_orchestrator(&mut self, cx: &mut Context<Self>) {
        let api_format = self.orch_api_format.clone();
        let base_url = self.orch_base_input.read(cx).value().trim().to_owned();
        let api_key = self.orch_key_input.read(cx).value().trim().to_owned();
        let model = self.orch_model_input.read(cx).value().trim().to_owned();
        let error = if !matches!(
            api_format.as_str(),
            "chat_completions" | "responses" | "messages"
        ) {
            Some("API 格式必须是 chat_completions、responses 或 messages。")
        } else if base_url.is_empty() {
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
        } else {
            self.store.save_orchestrator(&OrchestratorConfig {
                api_format,
                base_url,
                api_key,
                model,
            });
            self.orchestrator_form_error = None;
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
            _ => Some(0).filter(|_| !self.machines.is_empty()),
        }
    }

    // ---- 通知路由（connected/disconnected/auth + session.state_change 工作流驱动）----

    fn on_notify(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &Notification,
    ) {
        match n.method.as_str() {
            "connected" | "auth_ok" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = "已连接".into();
                }
                this.refresh_sessions(idx, window, cx);
                this.fetch_agents(idx, window, cx);
            }
            "auth_failed" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    let msg = n
                        .params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("认证失败")
                        .to_string();
                    m.status = format!("认证失败（{msg}）");
                }
            }
            "disconnected" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = "离线（重连中…）".into();
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
        n: &Notification,
    ) {
        let Some(sid) = n
            .params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return;
        };
        let old_state = n
            .params
            .get("oldState")
            .and_then(|v| v.as_str())
            .and_then(state_from_str)
            .unwrap_or(SessionState::Idle);
        let new_state = n
            .params
            .get("newState")
            .and_then(|v| v.as_str())
            .and_then(state_from_str)
            .unwrap_or(SessionState::Idle);
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
            .position(|wf| wf.session.children.iter().any(|c| c.id == sid))
        else {
            return;
        };
        // 用户取消工作流会话导致的子会话状态变更不注入（docs/DESIGN.md §工作流会话驱动）
        if this
            .workflows
            .get(wi)
            .map(|wf| wf.session.cancelled)
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
        // 子会话变 idle：抽取其最新输出并异步推进该工作流
        let output = this
            .machines
            .get(idx)
            .and_then(|m| m.views.get(&sid))
            .and_then(|v| {
                v.dialog.iter().rev().find_map(|d| match d {
                    DialogMsg::AgentMessage { content, .. } => Some(block_text(content)),
                    _ => None,
                })
            })
            .unwrap_or_default();
        let mut wf = match this.workflows.get(wi) {
            Some(wf) => wf.clone(),
            None => return,
        };
        wf.on_child_state_local(&sid, new_state);
        let session_dir = this.session_dir.clone();
        let wf_id = wf.session.id.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                let _ = wf
                    .on_child_state(&sid, old_state, new_state, Some(output))
                    .await;
                let _ = wf.persist(&session_dir);
                Some(wf)
            })
            .await;
            let _ = this.update_in(cx, |this, _w, cx| {
                this.finish_engine_task(cx, wi, &wf_id, result.unwrap_or(None));
            });
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
                .request(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                let sessions: Vec<SessionMeta> = res
                    .get("sessions")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res.get("next_before").and_then(|v| v.as_u64());
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
            .request(protocol::method::AGENT_LIST, None)
            .await
        {
            Ok(res) => {
                let result: AgentListResult =
                    serde_json::from_value(res).unwrap_or(AgentListResult { agents: Vec::new() });
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.agents = result.agents;
                        if m.status.starts_with("已连接") || m.status.starts_with("认证成功")
                        {
                            m.status = "已连接".into();
                        }
                    }
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        if m.status.is_empty() || m.status == "连接中…" {
                            m.status = format!("连接失败（{e}）");
                        }
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
                .request(
                    protocol::method::SESSION_HISTORY,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let items: Vec<HistoryItem> = res
                    .get("items")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res
                    .get("next_before")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
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
                .request(
                    protocol::method::SESSION_ACTIVITIES,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let acts: Vec<Activity> = res
                    .get("activities")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res
                    .get("next_before")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
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
                before: Some(before),
            };
            if let Ok(res) = client
                .request(
                    protocol::method::SESSION_ACTIVITIES,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let acts: Vec<Activity> = res
                    .get("activities")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res
                    .get("next_before")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
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
                before: Some(before),
            };
            if let Ok(res) = client
                .request(
                    protocol::method::SESSION_HISTORY,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let items: Vec<HistoryItem> = res
                    .get("items")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res
                    .get("next_before")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
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
                .request(
                    protocol::method::SESSION_ONGOING_ACTIVITY,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let act = res
                    .get("activity")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Activity>(v).ok());
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
        let Some(before) = m.sessions_next_before else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = json!({ "limit": PAGE_LIMIT, "before": before });
            if let Ok(res) = client
                .request(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                let sessions: Vec<SessionMeta> = res
                    .get("sessions")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let has_more = res
                    .get("has_more")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let next_before = res.get("next_before").and_then(|v| v.as_u64());
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
                let snapshot = client
                    .request(
                        protocol::method::SESSION_LIST,
                        Some(json!({ "limit": PAGE_LIMIT })),
                    )
                    .await
                    .ok();
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(res) = &snapshot {
                        let Some(i) = this.machines.iter().position(|m| {
                            m.config.name == machine_name && m.connection_epoch == epoch
                        }) else {
                            return;
                        };
                        if let Some(m) = this.machines.get_mut(i) {
                            let sessions: Vec<SessionMeta> = res
                                .get("sessions")
                                .cloned()
                                .and_then(|v| serde_json::from_value(v).ok())
                                .unwrap_or_default();
                            let has_more = res
                                .get("has_more")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let next_before = res.get("next_before").and_then(|v| v.as_u64());
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
                    }
                    cx.notify();
                });
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
        let cwd = self.session_cwd_input.read(cx).value().to_string();
        let agent = match self.new_session_agent.clone() {
            Some(a) => a,
            None => match self.available_agent(machine) {
                Some(a) => a,
                None => {
                    if let Some(m) = self.machine_mut(machine) {
                        m.status = "无可用 agent".into();
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
                .request(
                    protocol::method::SESSION_NEW,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if let Ok(res) = &res {
                    let new_id = res
                        .get("session")
                        .and_then(|s| s.get("id"))
                        .and_then(|i| i.as_str())
                        .map(str::to_string);
                    if let Some(new_id) = new_id {
                        this.store
                            .record_recent_workspace(&machine_name, &cwd, now_ts());
                        this.refresh_sessions(machine, w, cx);
                        this.open_session(w, cx, machine, new_id);
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
                    let _ = client
                        .request(
                            protocol::method::SESSION_PROMPT,
                            Some(serde_json::to_value(&params).unwrap()),
                        )
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
                        this.refresh_dialog(w, cx, machine, id);
                        cx.notify();
                    });
                })
                .detach();
            }
            Selected::Workflow { engine } => {
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
                    let session_dir = self.session_dir.clone();
                    let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                        let result = run_engine_on_tokio(async move {
                            let _ = wf.advance().await;
                            let _ = wf.persist(&session_dir);
                            wf
                        })
                        .await;
                        let _ = this.update_in(cx, |this, _w, cx| {
                            this.finish_engine_task(cx, engine, &wf_id, result);
                        });
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
                    let _ = client
                        .request(
                            protocol::method::SESSION_PROMPT,
                            Some(serde_json::to_value(&params).unwrap()),
                        )
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
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
                .is_some_and(|wf| wf.session.state == SessionState::Busy && !wf.session.done),
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
            let _ = client
                .request(
                    protocol::method::SESSION_CANCEL,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
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
            let _ = client
                .request(
                    protocol::method::SESSION_DELETE,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
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
            let _ = client
                .request(
                    protocol::method::SESSION_CONFIGURE,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, _w, cx| {
                this.renaming_session = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn rename_workflow(&mut self, cx: &mut Context<Self>, wi: usize, title: String) {
        if let Some(wf) = self.workflows.get_mut(wi) {
            wf.session.title = title.trim().to_string();
            let _ = wf.persist(&self.session_dir);
        }
        self.renaming_workflow = None;
        cx.notify();
    }

    // ---- 工作流 ----

    fn restore_workflows(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let sessions = WorkflowEngine::load_all(&self.session_dir);
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
        if !clean.trim().is_empty() || preamble.as_deref().is_some_and(|p| !p.trim().is_empty()) {
            if let Some(wf) = self.workflows.get_mut(wi) {
                wf.begin_busy();
            }
            let mut wf = self.workflows[wi].clone();
            wf.start_advance();
            let wf_id = wf.session.id.clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                let result = run_engine_on_tokio(async move {
                    let _ = wf.advance().await;
                    let _ = wf.persist(&session_dir);
                    wf
                })
                .await;
                let _ = this.update_in(cx, |this, _w, cx| {
                    this.finish_engine_task(cx, wi, &wf_id, result);
                });
            });
            self._tasks.push(t);
        }
        cx.notify();
    }

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
                    let was_cancelled = self.workflows[wi].session.cancelled;
                    self.workflows[wi] = wf;
                    if was_cancelled {
                        self.workflows[wi].session.cancelled = true;
                        self.workflows[wi].session.state = SessionState::Idle;
                    }
                } else {
                    WorkflowEngine::remove(&self.session_dir, wf_id);
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

    fn cancel_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>, idx: usize) {
        if let Some(wf) = self.workflows.get_mut(idx) {
            wf.mark_cancelled();
        }
        let session_dir = self.session_dir.clone();
        let Some(wf) = self.workflows.get(idx).cloned() else {
            return;
        };
        let wf_id = wf.session.id.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                let _ = wf.send_cancel_to_children().await;
                let _ = wf.persist(&session_dir);
                wf
            })
            .await;
            let _ = this.update_in(cx, |this, _w, cx| {
                this.finish_engine_task(cx, idx, &wf_id, result);
            });
        });
        self._tasks.push(t);
        let _ = cx;
    }

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
                self.delete_child(client, sid.clone());
            }
            if let Some(m) = self.machine_mut(machine) {
                m.sessions.retain(|session| session.id != sid);
                m.views.remove(&sid);
            }
        }
        if let Some(wf) = self.workflows.get(idx) {
            let id = wf.session.id.clone();
            WorkflowEngine::remove(&self.session_dir, &id);
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
        let _ = window;
    }

    fn delete_child(&self, client: WsClient, sid: String) {
        let params = SessionIdParams { session_id: sid };
        crate::ws::runtime().spawn(async move {
            let _ = client
                .request(
                    protocol::method::SESSION_DELETE,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
        });
    }

    fn orchestrator_backend(&self) -> Arc<dyn OrcBackend> {
        let cfg = self.store.orchestrator();
        Arc::new(RigBackend::new(cfg))
    }

    fn machine_summaries(&self) -> Vec<MachineSummary> {
        self.machines
            .iter()
            .map(|m| MachineSummary {
                name: m.config.name.clone(),
                online: m.status == "已连接",
                agents: m
                    .agents
                    .iter()
                    .filter(|a| a.available)
                    .map(|a| AgentSlot {
                        name: a.name.clone(),
                        available: true,
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
                Some(SessionMeta {
                    id: wf.session.id.clone(),
                    agent: "编排".into(),
                    cwd: String::new(),
                    state: wf.session.state,
                    title: wf.session.title.clone(),
                    created_at: wf.session.created_at,
                    last_active_at: wf.session.updated_at,
                })
            }
            None => None,
        }
    }

    // ---- workspace.diff（代码审查面板）----

    fn load_diff(&self, window: &mut Window, cx: &mut Context<Self>, machine: usize) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let cwd = match &self.selected {
            Some(Selected::Session { id, .. }) => m
                .sessions
                .iter()
                .find(|s| &s.id == id)
                .map(|s| s.cwd.clone()),
            _ => None,
        };
        let Some(cwd) = cwd else { return };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = json!({ "cwd": cwd.clone() });
            let res = client
                .request(protocol::method::WORKSPACE_DIFF, Some(params))
                .await;
            let _ = this.update_in(cx, |this, _w, cx| {
                if let Some(m) = this.machines.get_mut(machine) {
                    match &res {
                        Ok(res) => {
                            let r: WorkspaceDiffResult = serde_json::from_value(res.clone())
                                .unwrap_or(WorkspaceDiffResult {
                                    files: Vec::new(),
                                    not_repo: false,
                                });
                            m.diff_files = r.files;
                            m.diff_not_repo = r.not_repo;
                        }
                        Err(e) => {
                            m.diff_files = Vec::new();
                            m.diff_not_repo = false;
                            m.status = format!("diff 失败（{e}）");
                        }
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
        cwd: String,
        path: Option<String>,
        patch: Option<String>,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = json!({ "cwd": cwd, "path": path, "patch": patch });
            let _ = client
                .request(protocol::method::WORKSPACE_RESTORE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                this.load_diff(w, cx, machine);
                cx.notify();
            });
        })
        .detach();
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
            let mut skills = Vec::new();
            if let Ok(res) = client
                .request(
                    protocol::method::AGENT_SKILLS,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await
            {
                let r: AgentSkillsResult =
                    serde_json::from_value(res).unwrap_or(AgentSkillsResult { skills: Vec::new() });
                skills = r.skills;
            }
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
                    .request(
                        protocol::method::SESSION_NEW,
                        Some(
                            serde_json::to_value(SessionNewParams { agent, cwd })
                                .map_err(|e| e.to_string())?,
                        ),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let session_id = session
                    .get("session")
                    .and_then(|s| s.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "创建技能操作会话响应缺少 session.id".to_string())?
                    .to_string();
                client
                    .request(
                        protocol::method::SESSION_PROMPT,
                        Some(
                            serde_json::to_value(SessionPromptParams {
                                session_id: session_id.clone(),
                                input: vec![ContentBlock::Text {
                                    text: operation_prompt,
                                }],
                            })
                            .map_err(|e| e.to_string())?,
                        ),
                    )
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
                        m.status = format!("技能{}失败：{error}", action.label());
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
                .request(
                    protocol::method::AGENT_RESTART,
                    Some(serde_json::to_value(&params).unwrap()),
                )
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
        let mut view = MachineView::new(machine);
        view.status = "已连接".into();
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
            for c in wf.session.children.iter_mut() {
                if c.machine_idx > idx {
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

    // ---- 右侧面板 ----

    /// 面板逻辑宽度（px）。
    fn panel_width_logical(panel: Panel) -> f32 {
        match panel {
            Panel::Diff => 460.0,
            Panel::Detail => 360.0,
            Panel::Activities => 400.0,
        }
    }

    /// 打开/切换/关闭右侧上下文面板：窗口向右扩展（中间面板宽度不变）。
    fn set_panel(&mut self, window: &mut Window, cx: &mut Context<Self>, panel: Option<Panel>) {
        let new_delta = panel.map(Self::panel_width_logical).unwrap_or(0.0) * window.scale_factor();
        let bounds = window.bounds();
        let base = bounds.size.width - new_delta.into();
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
}

impl AmuxApp {
    // ---- 左侧边栏 ----

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
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.show_settings = true;
                            if let Some(i) = this.active_machine() {
                                this.refresh_sessions(i, _window, cx);
                            }
                            cx.notify();
                        })),
                ),
            )
    }

    /// 会话列表：普通会话 + 工作流会话统一按最近活跃排序；工作流挂载的关联普通会话折叠。
    fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 子会话只挂在工作流会话下，顶层列表跳过
        let child_ids: std::collections::HashSet<&str> = self
            .workflows
            .iter()
            .flat_map(|wf| wf.session.children.iter().map(|c| c.id.as_str()))
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
            let mut recency = wf.session.updated_at;
            for c in &wf.session.children {
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
        let sid_open = sid.clone();
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
        div()
            .id(format!("sess-row-{machine}-{sid}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(rgb(0xe5e5e5).opacity(if sel { 1.0 } else { 0.0 }))
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
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_session(window, cx, machine, sid_open.clone());
                            }))
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
            .child(if wf.session.state == SessionState::Busy {
                Spinner::new()
                    .color(hsla(0.6, 0.8, 0.5, 1.0))
                    .into_any_element()
            } else {
                div().w(px(14.)).h(px(14.)).into_any_element()
            });

        // 子会话默认折叠、可展开下钻
        let mut content = v_flex().gap_1();
        for c in &wf.session.children {
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

        let row = v_flex().gap_1().p_1().rounded_md().child(header).child(
            Collapsible::new()
                .open(self.expanded_workflows.contains(&wi))
                .content(content),
        );

        let title_ctx = title.clone();
        let wf_sel = self.selected == Some(Selected::Workflow { engine: wi });
        div()
            .id(format!("wf-row-{wi}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(rgb(0xe5e5e5).opacity(if wf_sel { 1.0 } else { 0.0 }))
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

    // ---- 中间 ----

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.render_input(window, cx))
            .into_any()
    }

    /// 中间面板：未选中会话→新会话视图；否则对话流 + 悬浮按钮。
    fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.selected.is_none() {
            return self.render_new_session_view(window, cx);
        }
        h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(self.render_dialog(window, cx))
            .child(self.render_floating_buttons(window, cx))
            .into_any()
    }

    /// 新会话视图：普通（机器与 Agent 并列一排 + 工作目录 + 创建按钮）/ 工作流模式。
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
                            .bg(rgb(0xfff3cd))
                            .rounded_md()
                            .child(
                                Label::new("尚未注册机器")
                                    .text_color(rgb(0x92400e))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new("请先在设置 → 机器管理中注册一台 amux server。")
                                    .text_sm()
                                    .text_color(rgb(0x92400e)),
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
                                .gap_6()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("机器").text_sm().text_color(rgb(0x6b7280)),
                                        )
                                        .child(self.render_machine_selector(cx)),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("Agent").text_sm().text_color(rgb(0x6b7280)),
                                        )
                                        .child(self.render_harness_selector(cx)),
                                ),
                        )
                        .child(self.render_recent_workspaces(cx))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(Label::new("工作目录").text_sm().text_color(rgb(0x6b7280)))
                                .child(Input::new(&self.session_cwd_input)),
                        )
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

    /// 常用工作目录快速选择：当前所选机器的最近使用目录，点击预填到工作目录输入框。
    fn render_recent_workspaces(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return v_flex().into_any();
        };
        let dirs = self.store.recent_workspaces_for_machine(&m.config.name);
        if dirs.is_empty() {
            return v_flex().into_any();
        }
        let mut row = h_flex().gap_1().flex_wrap();
        for dir in dirs {
            let label = dir.clone();
            row = row.child(
                Button::new(format!("ns-recent-{}", label))
                    .small()
                    .label(short_cwd(&label))
                    .tooltip(label.clone())
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.session_cwd_input
                            .update(cx, |s, cx| s.set_value(&label, window, cx));
                        cx.notify();
                    })),
            );
        }
        v_flex()
            .gap_1()
            .child(
                Label::new("常用工作目录")
                    .text_sm()
                    .text_color(rgb(0x6b7280)),
            )
            .child(row)
            .into_any()
    }

    /// 机器选择（新会话视图）。
    fn render_machine_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex().gap_1();
        if self.machines.is_empty() {
            row = row.child(Label::new("（请先在设置中添加机器）"));
        }
        for (i, m) in self.machines.iter().enumerate() {
            let name = m.config.name.clone();
            let selected = self.new_session_machine == Some(i);
            row = row.child(
                Button::new(format!("ns-machine-{i}"))
                    .small()
                    .label(name)
                    .when(selected, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        this.new_session_machine = Some(i);
                        this.new_session_agent = None;
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
        let mut row = h_flex().gap_1();
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
                    let all = w.session.to_dialog();
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
        let rows = dialog
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DialogMsg::UserMessage { content, timestamp } => {
                    div().id(("row", i)).w_full().child(
                        div()
                            .ml_auto()
                            .max_w(px(720.))
                            .p_3()
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
                            .child(
                                Label::new(format_timestamp(*timestamp))
                                    .text_xs()
                                    .text_color(rgb(0xdbeafe)),
                            )
                            .child(
                                TextView::markdown(format!("umd-{i}"), block_text(content))
                                    .selectable(true)
                                    .text_color(rgb(0xffffff))
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
                            .max_w(px(720.))
                            .p_3()
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
                            .child(
                                Label::new(format_timestamp(*timestamp))
                                    .text_xs()
                                    .text_color(rgb(0x9ca3af)),
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
                .child(Label::new("选择左侧会话查看对话，或输入消息开始").text_color(rgb(0x9ca3af)))
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
    fn render_activity_bar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let current: Option<Activity> = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .and_then(|v| v.live.clone()),
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
            Some(Activity::Error { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(rgb(0xfef2f2))
                .border_1()
                .border_color(rgb(0xfca5a5))
                .rounded_md()
                .child(
                    Label::new(format!("错误：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(0x991b1b)),
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
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                                if ev.keystroke.modifiers.control && ev.keystroke.key == "enter" {
                                    this.send_prompt(window, cx);
                                }
                            }))
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
                    .when(self.can_cancel(), |this| {
                        this.child(Button::new("cancel-work").small().label("✕ 取消").on_click(
                            cx.listener(|this, _ev, window, cx| {
                                this.cancel_work(window, cx);
                            }),
                        ))
                    })
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

    // ---- 右侧面板内容 ----

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
                .child(info_row("Agent", &meta.agent))
                .child(info_row("工作目录", &meta.cwd))
                .child(info_row(
                    "状态",
                    if meta.state == SessionState::Busy {
                        "工作中"
                    } else {
                        "空闲"
                    },
                ));
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
                .map(|w| w.session.children.clone())
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
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        children.push(
            h_flex()
                .items_center()
                .child(
                    Label::new("代码审查")
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0x111827)),
                )
                .child(div().flex_1())
                .child(
                    Button::new("close-panel-diff")
                        .small()
                        .label("✕")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.set_panel(window, cx, None);
                        })),
                )
                .into_any_element(),
        );
        if not_repo {
            children.push(
                Label::new("当前工作目录不是 git 仓库")
                    .text_sm()
                    .text_color(rgb(0x9ca3af))
                    .into_any_element(),
            );
        } else if files.is_empty() {
            children.push(
                Label::new("暂无改动")
                    .text_sm()
                    .text_color(rgb(0x9ca3af))
                    .into_any_element(),
            );
        }
        for f in &files {
            let path = f.path.clone();
            let patch = f.patch.clone();
            let additions = f.additions;
            let deletions = f.deletions;
            let summary = format!("{path}  (+{additions}/-{deletions})");
            let path2 = path.clone();
            let patch2 = patch.clone();
            children.push(
                v_flex()
                    .gap_1()
                    .child(Label::new(summary).text_sm().font_weight(FontWeight::MEDIUM))
                    .child(
                        TextView::markdown(format!("diff-{path}"), one_line(&patch, 200))
                            .selectable(true),
                    )
                    .child(
                        Button::new(format!("restore-{path}"))
                            .small()
                            .label("撤销该文件")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                if let Some(machine) = this.active_machine() {
                                    if let Some(m) = this.machine(machine) {
                                        let cwd = m
                                            .sessions
                                            .iter()
                                            .find(|s| {
                                                matches!(
                                                    &this.selected,
                                                    Some(Selected::Session { id, .. }) if id == &s.id
                                                )
                                            })
                                            .map(|s| s.cwd.clone())
                                            .unwrap_or_default();
                                        this.restore_workspace(
                                            window,
                                            cx,
                                            machine,
                                            cwd,
                                            Some(path2.clone()),
                                            Some(patch2.clone()),
                                        );
                                    }
                                }
                            })),
                    )
                    .into_any_element(),
            );
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
                div()
                    .id("diff-panel")
                    .v_flex()
                    .flex_1()
                    .gap_2()
                    .overflow_y_scroll()
                    .children(children),
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
            .bg(rgb(0xffffff))
            .rounded_md()
            .shadow_lg()
            .border_1()
            .border_color(rgb(0xe5e7eb))
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
                        .map(|w| w.session.title.clone())
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
            .when(self.show_add_machine_form, |overlay| {
                overlay.child(self.render_add_machine_dialog(window, cx))
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
                    .text_color(rgb(0x9ca3af)),
            );
        }
        for s in &skills {
            let s = s.clone();
            list = list.child(
                div()
                    .p_1()
                    .bg(rgb(0xf7f8fa))
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
                    .bg(rgb(0xffffff))
                    .rounded_md()
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
                    .bg(rgb(0xf5f6f8))
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
                            .child(machine_status_badge(&m.status))
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
                            .text_color(rgb(0x9ca3af))
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
            .child(self.settings_header(
                "机器管理",
                "接入 / 移除机器；每台机器自动发现 ACP agent，可查看 skills、重启",
            ))
            .children(machines)
            .child(
                h_flex().justify_end().child(
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
            .bg(rgb(0xffffff))
            .rounded_md()
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
                    .text_color(rgb(0x6b7280)),
            )
            .child(Label::new("名称").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.machine_name_input))
            .child(Label::new("连接地址").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.machine_url_input))
            .child(Label::new("Token").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.machine_token_input));
        if let Some(error) = &self.machine_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(rgb(0xb91c1c)),
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
                    .bg(hsla(0., 0., 0., 0.45))
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_add_machine_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_api_format = match self.orch_api_format.as_str() {
            "chat_completions" => Some(0),
            "responses" => Some(1),
            "messages" => Some(2),
            _ => None,
        };
        let api_format_options = RadioGroup::horizontal("orch-api-format")
            .children(["chat_completions", "responses", "messages"])
            .selected_index(selected_api_format)
            .on_click(cx.listener(|this, selected: &usize, _window, cx| {
                this.orch_api_format = match *selected {
                    0 => "chat_completions",
                    1 => "responses",
                    2 => "messages",
                    _ => return,
                }
                .into();
                this.orchestrator_form_error = None;
                cx.notify();
            }));
        let mut form = v_flex()
            .gap_1()
            .p_3()
            .bg(rgb(0xf7f8fa))
            .rounded_md()
            .child(Label::new("API 格式").text_sm().text_color(rgb(0x6b7280)))
            .child(api_format_options)
            .child(Label::new("Base URL").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.orch_base_input))
            .child(Label::new("API Key").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.orch_key_input))
            .child(Label::new("模型名称").text_sm().text_color(rgb(0x6b7280)))
            .child(Input::new(&self.orch_model_input));
        if let Some(error) = &self.orchestrator_form_error {
            form = form.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(rgb(0xb91c1c)),
            );
        }
        form = form.child(
            h_flex().justify_end().child(
                Button::new("orch-save")
                    .small()
                    .primary()
                    .label("保存")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.save_orchestrator(cx);
                    })),
            ),
        );
        v_flex()
            .gap_2()
            .child(self.settings_header("编排智能体", "配置工作流编排使用的大模型供应商连接信息"))
            .child(form)
            .into_any()
    }

    fn render_quick_commands_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commands = self.store.list_quick_commands();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        for c in &commands {
            let name = c.name.clone();
            let prompt = c.prompt.clone();
            let existing = self.qc_edit_target.as_deref() == Some(name.as_str());
            if existing {
                items.push(
                    v_flex()
                        .gap_1()
                        .child(Label::new("名称").text_sm().text_color(rgb(0x6b7280)))
                        .child(Input::new(&self.qc_name_input))
                        .child(Label::new("提示词").text_sm().text_color(rgb(0x6b7280)))
                        .child(Input::new(&self.qc_prompt_input))
                        .child(
                            Button::new("qc-save")
                                .small()
                                .primary()
                                .label("保存")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    let name = this.qc_edit_target.clone().unwrap_or_default();
                                    let new_name = this.qc_name_input.read(cx).value().to_string();
                                    let prompt = this.qc_prompt_input.read(cx).value().to_string();
                                    if !new_name.trim().is_empty() && new_name != name {
                                        this.store.remove_quick_command(&name);
                                        this.store.add_quick_command(new_name.trim(), &prompt);
                                    } else {
                                        this.store.update_quick_command(&name, &prompt);
                                    }
                                    this.qc_edit_target = None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            } else {
                let name_edit = name.clone();
                let name_del = name.clone();
                let prompt_edit = prompt.clone();
                items.push(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .p_2()
                        .bg(rgb(0xf7f8fa))
                        .rounded_md()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(&name).text_sm().font_weight(FontWeight::MEDIUM))
                                .child(
                                    Label::new(&prompt)
                                        .text_xs()
                                        .text_color(rgb(0x6b7280))
                                        .line_clamp(2),
                                ),
                        )
                        .child(
                            Button::new(format!("qc-edit-{name}"))
                                .small()
                                .label("编辑")
                                .on_click(cx.listener(move |this, _ev, window, cx| {
                                    this.qc_edit_target = Some(name_edit.clone());
                                    let prompt = prompt_edit.clone();
                                    this.qc_name_input.update(cx, |s, cx| {
                                        s.set_value(name_edit.clone(), window, cx);
                                    });
                                    this.qc_prompt_input.update(cx, |s, cx| {
                                        s.set_value(prompt, window, cx);
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(format!("qc-del-{name}"))
                                .small()
                                .label("删除")
                                .on_click(cx.listener(move |this, _ev, _window, cx| {
                                    this.store.remove_quick_command(&name_del);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        v_flex()
            .gap_2()
            .child(self.settings_header("快捷指令", "自定义快捷指令，输入区上方一键发送"))
            .children(items)
            .child(self.settings_header("＋ 新增快捷指令", ""))
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("指令名").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.qc_name_input))
                    .child(Label::new("提示词").text_sm().text_color(rgb(0x6b7280)))
                    .child(Input::new(&self.qc_prompt_input))
                    .child(
                        Button::new("qc-add")
                            .small()
                            .primary()
                            .label("添加")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                let name = this.qc_name_input.read(cx).value().to_string();
                                let prompt = this.qc_prompt_input.read(cx).value().to_string();
                                if !name.trim().is_empty() && !prompt.trim().is_empty() {
                                    this.store.add_quick_command(name.trim(), prompt.trim());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    fn render_skills_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let skills = self.store.list_skills();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        for s in &skills {
            let name = s.name.clone();
            let desc = s.description.clone();
            let existing = self.skill_edit_target.as_deref() == Some(name.as_str());
            if existing {
                items.push(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.skill_name_input))
                        .child(Input::new(&self.skill_desc_input))
                        .child(
                            Button::new("skill-save")
                                .small()
                                .primary()
                                .label("保存")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    let old = this.skill_edit_target.clone().unwrap_or_default();
                                    let new_name =
                                        this.skill_name_input.read(cx).value().to_string();
                                    let desc = this.skill_desc_input.read(cx).value().to_string();
                                    if !new_name.trim().is_empty() && new_name != old {
                                        this.store.remove_skill(&old);
                                        this.store.add_skill(new_name.trim(), desc.trim());
                                    } else {
                                        this.store.update_skill(&old, desc.trim());
                                    }
                                    this.skill_edit_target = None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            } else {
                let name_edit = name.clone();
                let name_del = name.clone();
                let desc_edit = desc.clone();
                let mut target_buttons = Vec::new();
                for (machine_idx, machine) in self.machines.iter().enumerate() {
                    for (agent_idx, agent_info) in machine.agents.iter().enumerate() {
                        for action in [
                            SkillAction::Install,
                            SkillAction::Update,
                            SkillAction::Uninstall,
                        ] {
                            let skill = s.clone();
                            let agent = agent_info.name.clone();
                            let label = format!(
                                "{} {}@{}",
                                action.label(),
                                agent_info.name,
                                machine.config.name
                            );
                            target_buttons.push(
                                Button::new(format!(
                                    "skill-action-{machine_idx}-{agent_idx}-{}-{}",
                                    action.label(),
                                    name
                                ))
                                .small()
                                .label(label)
                                .on_click(cx.listener(
                                    move |this, _ev, window, cx| {
                                        this.manage_skill_on_agent(
                                            window,
                                            cx,
                                            machine_idx,
                                            agent.clone(),
                                            skill.clone(),
                                            action,
                                        );
                                    },
                                )),
                            );
                        }
                    }
                }
                items.push(
                    v_flex()
                        .gap_2()
                        .p_2()
                        .bg(rgb(0xf7f8fa))
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
                                            Label::new(&name)
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM),
                                        )
                                        .child(
                                            Label::new(&desc)
                                                .text_xs()
                                                .text_color(rgb(0x6b7280))
                                                .line_clamp(2),
                                        ),
                                )
                                .child(
                                    Button::new(format!("skill-edit-{name}"))
                                        .small()
                                        .label("编辑")
                                        .on_click(cx.listener(move |this, _ev, window, cx| {
                                            this.skill_edit_target = Some(name_edit.clone());
                                            let desc = desc_edit.clone();
                                            this.skill_name_input.update(cx, |s, cx| {
                                                s.set_value(name_edit.clone(), window, cx);
                                            });
                                            this.skill_desc_input.update(cx, |s, cx| {
                                                s.set_value(desc, window, cx);
                                            });
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(format!("skill-del-{name}"))
                                        .small()
                                        .label("删除")
                                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                                            this.store.remove_skill(&name_del);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(h_flex().gap_1().flex_wrap().children(target_buttons))
                        .into_any_element(),
                );
            }
        }
        v_flex()
            .gap_2()
            .child(self.settings_header("技能管理", "已安装 / 管理的 ACP skills 清单"))
            .children(items)
            .child(self.settings_header("＋ 新增技能", ""))
            .child(
                v_flex()
                    .gap_1()
                    .child(Input::new(&self.skill_name_input))
                    .child(Input::new(&self.skill_desc_input))
                    .child(
                        Button::new("skill-add")
                            .small()
                            .primary()
                            .label("添加")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                let name = this.skill_name_input.read(cx).value().to_string();
                                let desc = this.skill_desc_input.read(cx).value().to_string();
                                if !name.trim().is_empty() {
                                    this.store.add_skill(name.trim(), desc.trim());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    fn render_templates_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        for t in &templates {
            let name = t.name.clone();
            let plan = t.plan.clone();
            let existing = self.tpl_edit_target.as_deref() == Some(name.as_str());
            if existing {
                items.push(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.tpl_name_input))
                        .child(Input::new(&self.tpl_desc_input))
                        .child(
                            Button::new("tpl-save")
                                .small()
                                .primary()
                                .label("保存")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    let old = this.tpl_edit_target.clone().unwrap_or_default();
                                    let new_name = this.tpl_name_input.read(cx).value().to_string();
                                    let plan = this.tpl_desc_input.read(cx).value().to_string();
                                    if !new_name.trim().is_empty() && new_name != old {
                                        this.store.remove_template(&old);
                                        this.store.add_template(new_name.trim(), plan.trim());
                                    } else {
                                        this.store.update_template(&old, plan.trim());
                                    }
                                    this.tpl_edit_target = None;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            } else {
                let name_edit = name.clone();
                let name_del = name.clone();
                let plan_edit = plan.clone();
                items.push(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .p_2()
                        .bg(rgb(0xf7f8fa))
                        .rounded_md()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(&name).text_sm().font_weight(FontWeight::MEDIUM))
                                .child(
                                    Label::new(&plan)
                                        .text_xs()
                                        .text_color(rgb(0x6b7280))
                                        .line_clamp(2),
                                ),
                        )
                        .child(
                            Button::new(format!("tpl-edit-{name}"))
                                .small()
                                .label("编辑")
                                .on_click(cx.listener(move |this, _ev, window, cx| {
                                    this.tpl_edit_target = Some(name_edit.clone());
                                    let plan = plan_edit.clone();
                                    this.tpl_name_input.update(cx, |s, cx| {
                                        s.set_value(name_edit.clone(), window, cx);
                                    });
                                    this.tpl_desc_input.update(cx, |s, cx| {
                                        s.set_value(plan, window, cx);
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(format!("tpl-del-{name}"))
                                .small()
                                .label("删除")
                                .on_click(cx.listener(move |this, _ev, _window, cx| {
                                    this.store.remove_template(&name_del);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        v_flex()
            .gap_2()
            .child(self.settings_header("工作流模板", "模板的 plan 会作为工作流编排的系统指令注入"))
            .children(items)
            .child(self.settings_header("＋ 新增模板", ""))
            .child(
                v_flex()
                    .gap_1()
                    .child(Input::new(&self.tpl_name_input))
                    .child(Input::new(&self.tpl_desc_input))
                    .child(
                        Button::new("tpl-add")
                            .small()
                            .primary()
                            .label("添加")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                let name = this.tpl_name_input.read(cx).value().to_string();
                                let plan = this.tpl_desc_input.read(cx).value().to_string();
                                if !name.trim().is_empty() {
                                    this.store.add_template(name.trim(), plan.trim());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any()
    }

    /// 设置项标题。
    fn settings_header(&self, title: &str, subtitle: &str) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(title).text_lg().font_weight(FontWeight::MEDIUM))
            .child(Label::new(subtitle).text_sm().text_color(rgb(0x6b7280)))
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
        self.machines[idx].status = "连接中…".into();
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

fn format_timestamp(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1_000;
    let day_seconds = seconds % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60,
        day_seconds % 60
    )
}
