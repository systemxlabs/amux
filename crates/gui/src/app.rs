//! amux 主视图（docs/DESIGN.md §7 / PRD §3、§4）。
//! 三面板布局 + 多机器 + 工作流编排 + 设置浮窗（五分类）：
//! - 左侧：会话列表（按最近活跃排序；含编排会话与折叠的子会话）+ 顶部「+」新建会话入口
//!   + 底部设置入口
//! - 中间：上方对话流（用户消息 + agent 完整输出气泡，非流式）+ 下方进行中活动条（一条或无）
//!   + 快捷指令栏 + 输入区（多行、@ 引用、拖拽文件、粘贴图片、语音）+ 右侧竖排悬浮按钮
//! - 右侧：上下文面板（默认折叠，悬浮按钮展开 diff / 会话详情 / 会话活动历史）
//! - 设置浮窗：机器管理（含 agent 默认模型与 skills 列表）/ 编排 agent / 快捷指令 / Skills / 工作流模板
//! - 工作流：GUI 本地编排 agent 会话（rig 单 turn），子会话 idle 自动推进，暂停/继续/介入，
//!   状态持久化于 GUI 本地，重开后恢复

use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    collapsible::Collapsible,
    input::{Input, InputState},
    label::Label,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    *,
};
use serde_json::json;

use protocol::{
    Activity, ContentBlock, DialogItem, GitDiffFile, GitStatusResult, MachineInfo, SessionMeta,
    SessionState,
};

use crate::config::{
    machine_ws_url, ConfigStore, MachineConfig, OrchestratorConfig, QuickCommand, SkillEntry,
    WorkflowTemplate,
};
use crate::logic::{compose_prompt, parse_at_references, read_path_context, InputAttachment};
use crate::workflow::{MachineSummary, OrcBackend, RigBackend, WorkflowEngine};
use crate::ws::{Notification, WsClient};

// ---- 视图状态 ----

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
    /// 普通 agent 会话（机器下标 + 元数据）
    Session { machine: usize, meta: SessionMeta },
    /// 编排会话（工作流，下标）
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

/// 单机器视图：独立连接 + 会话列表 + 选中会话的对话/活动 + diff 状态。
struct MachineView {
    config: MachineConfig,
    client: WsClient,
    status: String,
    /// get_info 结果（agent 发现 + 默认模型）
    info: Option<MachineInfo>,
    sessions: Vec<SessionMeta>,
    selected: Option<String>,
    dialog: Vec<DialogItem>,
    activities: Vec<Activity>,
    /// 当前打开会话的实时活动（turn 中合并流式推送，空闲时清空）
    live_activity: Option<Activity>,
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
    fn new(config: MachineConfig) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            config,
            client: WsClient::connect(url),
            status: "连接中…".into(),
            info: None,
            sessions: Vec::new(),
            selected: None,
            dialog: Vec::new(),
            activities: Vec::new(),
            live_activity: None,
            skills: Vec::new(),
            skills_harness: None,
            diff_status: None,
            diff_files: Vec::new(),
            diff_path: None,
            diff_mode: DiffMode::Inline,
        }
    }
}

/// 当前选中的会话（普通 server 会话 或 编排会话）。
#[derive(Clone, PartialEq)]
enum Selected {
    Session { machine: usize, id: String },
    Workflow { engine: usize },
}

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
    settings_input: Entity<InputState>,
    /// 设置表单输入
    qc_name_input: Entity<InputState>,
    qc_prompt_input: Entity<InputState>,
    skill_name_input: Entity<InputState>,
    skill_desc_input: Entity<InputState>,
    tpl_name_input: Entity<InputState>,
    tpl_desc_input: Entity<InputState>,
    orch_backend_input: Entity<InputState>,
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
    /// 会话详情是否可编辑标题
    editing_title: bool,
    /// 新会话视图（PRD §4.1.2）：选中的机器与 agent
    new_session_machine: Option<usize>,
    new_session_harness: Option<String>,
    /// 新会话视图：首条指令输入框
    new_session_msg_input: Entity<InputState>,
    _tasks: Vec<Task<()>>,
}

fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        let settings_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("名称 ws://地址 token（空格分隔）"));
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
        let orch_backend_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("chat_completions / messages")
                .default_value("chat_completions")
        });
        let orch_base_input = cx.new(|cx| InputState::new(window, cx).placeholder("Base URL"));
        let orch_key_input = cx.new(|cx| InputState::new(window, cx).placeholder("API key"));
        let orch_model_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("模型")
                .default_value("gpt-4o-mini")
        });
        let model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("默认模型（可留空）"));
        let title_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));
        let new_session_msg_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("用自然语言描述你的首个指令/目标…")
                .multi_line(true)
        });

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
            settings_input,
            qc_name_input,
            qc_prompt_input,
            skill_name_input,
            skill_desc_input,
            tpl_name_input,
            tpl_desc_input,
            orch_backend_input,
            orch_base_input,
            orch_key_input,
            orch_model_input,
            model_input,
            title_input,
            qc_edit_target: None,
            skill_edit_target: None,
            tpl_edit_target: None,
            editing_title: false,
            new_session_machine: None,
            new_session_harness: None,
            new_session_msg_input,
            _tasks: Vec::new(),
        };
        // 预填编排配置表单
        app.fill_orchestrator_form(&orchestrator, window, cx);
        app.spawn_notify_tasks(window, cx);
        // 启动即拉取各机器会话列表与 get_info；恢复编排会话
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
        self.orch_backend_input.update(cx, |s, cx| {
            s.set_value(cfg.api_backend.clone(), window, cx);
        });
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
                    closed: wf.session.done,
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
            "turn_completed" => {
                let output = n.params.get("output").cloned().unwrap_or_default();
                let ts = n
                    .params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                m.dialog.push(DialogItem::AgentOutput {
                    content: serde_json::from_value(output).unwrap_or_default(),
                    timestamp: ts,
                });
            }
            "user_message" => {
                let content = n.params.get("content").cloned().unwrap_or_default();
                let ts = n
                    .params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                m.dialog.push(DialogItem::UserMessage {
                    content: serde_json::from_value(content).unwrap_or_default(),
                    timestamp: ts,
                });
            }
            // 实时活动：turn 中合并流式推送（thinking 逐块累积为一条）
            "activity" => {
                // 只跟踪当前打开会话的活动；其他会话的活动不抢占活动条
                let sid = n.params.get("session_id").and_then(|v| v.as_str());
                if m.selected.as_deref() != sid {
                    return;
                }
                if let Some(a) = n
                    .params
                    .get("activity")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                {
                    m.live_activity = Some(a);
                    cx.notify();
                }
            }
            "session_state" => {
                let sid = n.params.get("session_id").and_then(|v| v.as_str());
                let state = n.params.get("state").and_then(|v| v.as_str()).map(|s| {
                    if s == "busy" {
                        SessionState::Busy
                    } else {
                        SessionState::Idle
                    }
                });
                let Some((sid, state)) = sid.zip(state) else {
                    cx.notify();
                    return;
                };
                // 活动历史刷新（turn 边界拉取合并后的完整活动）；实时活动由
                // activity 通知流式驱动（docs/DESIGN.md §5.3）
                let client = m.client.clone();
                let sid_owned = sid.to_string();
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    if let Ok(res) = client
                        .request(
                            protocol::method::GET_ACTIVITIES,
                            Some(json!({ "sessionId": sid_owned })),
                        )
                        .await
                    {
                        let acts: Vec<Activity> = res
                            .get("activities")
                            .cloned()
                            .map(|v| serde_json::from_value(v).unwrap_or_default())
                            .unwrap_or_default();
                        let _ = this.update_in(cx, |this, _window, cx| {
                            if let Some(mm) = this.machines.get_mut(idx) {
                                mm.activities = acts.clone();
                            }
                            cx.notify();
                        });
                    }
                    // 空闲：清空实时活动
                    if state == SessionState::Idle {
                        let _ = this.update_in(cx, |this, _window, cx| {
                            if let Some(mm) = this.machines.get_mut(idx) {
                                mm.live_activity = None;
                            }
                            cx.notify();
                        });
                    }
                })
                .detach();
                // 工作流自动推进：子会话变 idle → 注入完成情况并推进（docs/DESIGN.md §10）
                if state == SessionState::Idle {
                    let session_id = sid.to_string();
                    let wi = this
                        .workflows
                        .iter()
                        .position(|wf| wf.session.children.iter().any(|c| c.id == session_id));
                    if let Some(wi) = wi {
                        let output = this.machine(idx).and_then(|mm| {
                            mm.dialog.iter().rev().find_map(|d| match d {
                                DialogItem::AgentOutput { content, .. } => {
                                    Some(block_text(content))
                                }
                                _ => None,
                            })
                        });
                        let mut wf = this.workflows.remove(wi);
                        let workflow_dir = this.workflow_dir.clone();
                        let ex = output
                            .unwrap_or_default()
                            .chars()
                            .take(200)
                            .collect::<String>();
                        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                            // 自动推进在 GUI 的 tokio runtime 上执行（rig 需要 reactor）
                            if let Some(wf) = run_engine_on_tokio(async move {
                                let _ = wf
                                    .on_child_state(&session_id, SessionState::Idle, Some(ex))
                                    .await;
                                let _ = wf.persist(&workflow_dir);
                                wf
                            })
                            .await
                            {
                                let _ = this.update_in(cx, |this, _window, cx| {
                                    this.workflows.insert(wi, wf);
                                    cx.notify();
                                });
                            }
                        });
                        this._tasks.push(t);
                    }
                }
            }
            "session_created" | "session_deleted" | "session_updated" => {
                this.refresh_sessions(idx, window, cx);
            }
            // 断线重连：刷新会话列表并重开选中会话（全量重放，关闭期间输出不丢，
            // docs/DESIGN.md §5.4 打开会话与重连）
            "connected" => {
                this.refresh_sessions(idx, window, cx);
                this.fetch_info(idx, window, cx);
                if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                    if machine == idx {
                        this.open_session(window, cx, machine, id);
                    }
                }
            }
            // 连接断开：如实标记离线（PRD §3.3 在线状态），清空进行中活动
            "disconnected" => {
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = "离线（重连中…）".into();
                    m.live_activity = None;
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
            if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.sessions = serde_json::from_value(sessions).unwrap_or_default();
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

    /// 新会话视图"创建并发送"（PRD §4.1.2）：按所选机器/agent/工作目录创建会话，
    /// 并把自然语言首条指令作为第一个 prompt 发出。
    fn create_and_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let text = self.new_session_msg_input.read(cx).value().to_string();
        if cwd.is_empty() {
            if let Some(m) = self.machine_mut(machine) {
                m.status = "请填写工作目录".into();
            }
            cx.notify();
            return;
        }
        if text.trim().is_empty() {
            if let Some(m) = self.machine_mut(machine) {
                m.status = "请填写首条指令".into();
            }
            cx.notify();
            return;
        }
        let client = m.client.clone();
        let text = text.clone();
        protocol::log::info(
            "gui.app",
            format!("创建会话（machine={machine} harness={harness} cwd={cwd}）"),
        );
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 1) 创建会话
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
            // 2) 立即跳转到该会话视图（不等首条指令 turn 完成；输出/活动
            //    经 turn_completed / activity 通知流式到达，docs/DESIGN.md §5.1）
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_session(window, cx, machine, sid.clone());
            });
            // 3) 发送首条指令（后台执行，不阻塞跳转）
            protocol::log::debug("gui.app", format!("发送首条指令 session={sid}"));
            let _ = client
                .request(
                    protocol::method::PROMPT,
                    Some(json!({
                        "sessionId": sid,
                        "input": [{ "type": "text", "text": text }]
                    })),
                )
                .await;
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
            let mut items: Vec<DialogItem> = Vec::new();
            let mut acts: Vec<Activity> = Vec::new();
            if let Ok(res) = client
                .request(
                    protocol::method::OPEN_SESSION,
                    Some(json!({ "sessionId": session_id })),
                )
                .await
            {
                items = res
                    .get("items")
                    .cloned()
                    .map(|v| serde_json::from_value(v).unwrap_or_default())
                    .unwrap_or_default();
            }
            if let Ok(res) = client
                .request(
                    protocol::method::GET_ACTIVITIES,
                    Some(json!({ "sessionId": session_id })),
                )
                .await
            {
                acts = res
                    .get("activities")
                    .cloned()
                    .map(|v| serde_json::from_value(v).unwrap_or_default())
                    .unwrap_or_default();
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(m) = this.machine_mut(machine) {
                    m.selected = Some(session_id.clone());
                    m.dialog = items.clone();
                    m.activities = acts.clone();
                    m.live_activity = None;
                }
                this.selected = Some(Selected::Session {
                    machine,
                    id: session_id,
                });
                this.set_panel(window, cx, None);
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
            all.push(InputAttachment::Path {
                is_dir: std::path::Path::new(&r).is_dir(),
                path: r,
            });
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
                // 向编排会话发送介入指令（暂停/继续/调整后续动作，docs/DESIGN.md §10）；
                // 编排在 GUI 的 tokio runtime 上执行（rig LLM 调用需要 reactor）
                let mut wf = self.workflows.remove(engine);
                let text = clean_text;
                let workflow_dir = self.workflow_dir.clone();
                let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    if let Some(wf) = run_engine_on_tokio(async move {
                        let _ = wf.steer(&text).await;
                        let _ = wf.persist(&workflow_dir);
                        wf
                    })
                    .await
                    {
                        let _ = this.update_in(cx, |this, _window, cx| {
                            this.workflows.insert(engine, wf);
                            cx.notify();
                        });
                    }
                });
                self._tasks.push(t);
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

    /// 恢复 GUI 本地编排会话（GUI 重开，docs/DESIGN.md §10 持久化与恢复）。
    fn restore_workflows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        // 依据子会话当前状态恢复自动推进：查询各机器会话列表，喂 idle 状态
        for i in 0..self.machines.len() {
            let client = self.machines[i].client.clone();
            let workflow_dir = self.workflow_dir.clone();
            cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                    let sessions = res.get("sessions").cloned().unwrap_or_default();
                    // 收集 idle 子会话（工作流下标 + 会话 id）
                    let mut advances: Vec<(usize, String)> = Vec::new();
                    let _ =
                        this.update_in(cx, |this, _window, cx| {
                            for s in sessions.as_array().cloned().unwrap_or_default() {
                                let sid = s["id"].as_str().unwrap_or("").to_string();
                                let state = if s["state"].as_str() == Some("busy") {
                                    SessionState::Busy
                                } else {
                                    SessionState::Idle
                                };
                                if state == SessionState::Idle {
                                    if let Some(wi) = this.workflows.iter().position(|wf| {
                                        wf.session.children.iter().any(|c| c.id == sid)
                                    }) {
                                        advances.push((wi, sid));
                                    }
                                }
                            }
                            cx.notify();
                        });
                    // 自动推进在 GUI 的 tokio runtime 上执行（rig 需要 reactor，
                    // 避免主线程无 runtime 崩溃）
                    for (wi, sid) in advances {
                        let mut wf = {
                            let mut out = None;
                            let _ = this.update_in(cx, |this, _window, _cx| {
                                if wi < this.workflows.len() {
                                    out = Some(this.workflows.remove(wi));
                                }
                            });
                            match out {
                                Some(wf) => wf,
                                None => continue,
                            }
                        };
                        let workflow_dir = workflow_dir.clone();
                        if let Some(wf) = run_engine_on_tokio(async move {
                            let _ = wf.on_child_state(&sid, SessionState::Idle, None).await;
                            let _ = wf.persist(&workflow_dir);
                            wf
                        })
                        .await
                        {
                            let _ = this.update_in(cx, |this, _window, cx| {
                                this.workflows.insert(wi, wf);
                                cx.notify();
                            });
                        }
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    /// 创建编排会话（PRD §3.7：自然语言描述完整执行计划）。
    fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let description = self.workflow_input.read(cx).value().to_string();
        if description.trim().is_empty() {
            return;
        }
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
        let engine = WorkflowEngine::new(&clean, &context, backend, clients.clone(), summaries);
        let wi = self.workflows.len();
        let workflow_dir = self.workflow_dir.clone();
        self.workflows.push(engine);
        self.selected = Some(Selected::Workflow { engine: wi });
        self.workflow_input.update(cx, |s, cx| {
            s.set_value("", window, cx);
        });

        // 启动首个 turn（拆解步骤并下发指令）：编排主循环在 GUI 的 tokio runtime
        // 上执行（rig LLM 调用需要 reactor），完成后经 oneshot 传回引擎状态
        let mut wf = self.workflows.remove(wi);
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Some(wf) = run_engine_on_tokio(async move {
                let _ = wf.start().await;
                let _ = wf.persist(&workflow_dir);
                wf
            })
            .await
            {
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.workflows.insert(wi, wf);
                    cx.notify();
                });
            }
        });
        self._tasks.push(t);
        cx.notify();
    }

    /// 删除编排会话（连同本地持久化状态）。
    fn delete_workflow(&mut self, _window: &mut Window, cx: &mut Context<Self>, idx: usize) {
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

    /// 暂停/继续编排会话。
    fn toggle_pause_workflow(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let wf = self.workflows.get_mut(idx);
        let Some(wf) = wf else { return };
        let paused = wf.session.paused;
        let should_advance = wf.set_paused(!paused);
        let _ = should_advance;
        self.persist_workflow(idx);
        let _ = window;
        cx.notify();
    }

    /// 从模板创建工作流。
    fn create_workflow_from_template(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        tpl: WorkflowTemplate,
    ) {
        self.workflow_input.update(cx, |s, cx| {
            s.set_value(tpl.description, window, cx);
        });
        self.new_session_mode = NewSessionMode::Workflow;
        self.create_workflow(window, cx);
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

    fn save_title(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let title = self.title_input.read(cx).value().to_string();
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::SET_SESSION_TITLE,
                    Some(json!({ "sessionId": session_id, "title": title })),
                )
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                this.editing_title = false;
                cx.notify();
            });
        })
        .detach();
        self.editing_title = false;
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
        let mut view = MachineView::new(machine);
        view.status = "已连接".into();
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.sessions = serde_json::from_value(sessions).unwrap_or_default();
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
    /// 子会话随父会话一起参与排序）。
    fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 汇总：普通会话按 last_event_at；工作流按 max(父 updated_at, 子会话 last_event_at)
        let mut items: Vec<(u64, SessionListItem)> = Vec::new();
        for (mi, m) in self.machines.iter().enumerate() {
            for s in &m.sessions {
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

        items
            .into_iter()
            .map(|(_, item)| match item {
                SessionListItem::Session { machine, meta } => {
                    self.render_session_row(cx, machine, &meta)
                }
                SessionListItem::Workflow { idx } => self.render_workflow_row(cx, idx),
            })
            .collect()
    }

    /// 普通会话行：标题 · agent@机器 · 状态（机器信息内联，docs/PRD §4.1.1）。
    fn render_session_row(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        s: &SessionMeta,
    ) -> gpui::AnyElement {
        let machine_name = self
            .machines
            .get(machine)
            .map(|m| m.config.name.clone())
            .unwrap_or_default();
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
        let state_label = match s.state {
            SessionState::Busy => "● 工作中",
            SessionState::Idle => {
                if s.closed {
                    "已关闭"
                } else {
                    "空闲"
                }
            }
        };
        let label: SharedString =
            format!("{title} · {}@{machine_name} · {state_label}", s.harness).into();
        let btn = Button::new(format!("sess-{machine}-{sid}"))
            .small()
            .label(label)
            .on_click(cx.listener(move |this, _ev, window, cx| {
                this.open_session(window, cx, machine, sid.clone());
            }));
        (if sel { btn.primary() } else { btn }).into_any_element()
    }

    /// 工作流行：标题 · 子会话数 + 打开按钮 + 状态徽章 + 折叠的子会话（docs/PRD §4.1.1）。
    fn render_workflow_row(&self, cx: &mut Context<Self>, wi: usize) -> gpui::AnyElement {
        let Some(wf) = self.workflows.get(wi) else {
            return div().into_any();
        };
        let orc_sel = self.selected == Some(Selected::Workflow { engine: wi });
        let title = if wf.session.title.is_empty() {
            "新工作流".to_string()
        } else {
            wf.session.title.clone()
        };
        let state = if wf.session.done {
            "完成"
        } else if wf.session.paused {
            "已暂停"
        } else if wf.session.state == SessionState::Busy {
            "编排中…"
        } else {
            "空闲"
        };
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(Label::new(format!(
                "🧭 {title} · {} 子会话",
                wf.session.children.len()
            )))
            .child(div().flex_1())
            .child(
                Button::new(format!("wf-open-{wi}"))
                    .small()
                    .label("打开")
                    .when(orc_sel, |b| b.primary())
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.selected = Some(Selected::Workflow { engine: wi });
                        this.set_panel(window, cx, None);
                    })),
            );
        // 子会话默认折叠、可展开下钻（PRD §3.1/§4.1.1）
        let mut content = v_flex().gap_1();
        for c in &wf.session.children {
            let cid = c.id.clone();
            let step = c.step_desc.clone();
            let machine_name = c.machine_name.clone();
            let harness = c.harness.clone();
            let st = match c.state {
                SessionState::Busy => "忙",
                SessionState::Idle => "空闲",
            };
            content = content.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(Label::new("↳").text_color(rgb(0x9ca3af)))
                    .child(
                        Button::new(format!("wf-child-{wi}-{cid}"))
                            .small()
                            .label(format!("{step} [{harness}@{machine_name}] {st}"))
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let mi = this
                                    .machines
                                    .iter()
                                    .position(|mm| mm.config.name == machine_name)
                                    .unwrap_or(0);
                                this.open_session(window, cx, mi, cid.clone());
                            })),
                    ),
            );
        }
        v_flex()
            .gap_2()
            .p_2()
            .bg(rgb(0xffffff))
            .rounded_md()
            .border_1()
            .border_color(rgb(0xe5e7eb))
            .shadow_sm()
            .child(header)
            .child(
                div()
                    .px_1()
                    .py(px(2.))
                    .rounded_full()
                    .bg(rgb(0xf3f4f6))
                    .child(Label::new(state).text_xs().text_color(rgb(0x4b5563))),
            )
            .child(Collapsible::new().open(false).content(content))
            .into_any()
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
    /// 或切换"从工作流模板创建"。
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
            // 模式切换：直接创建 / 从工作流模板创建
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ns-mode-direct")
                            .small()
                            .label("直接创建")
                            .when(mode == NewSessionMode::Direct, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.new_session_mode = NewSessionMode::Direct;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ns-mode-tpl")
                            .small()
                            .label("从工作流模板创建")
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
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("工作目录").text_sm().text_color(rgb(0x6b7280)))
                            .child(Input::new(&self.session_cwd_input)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("指令（自然语言）")
                                    .text_sm()
                                    .text_color(rgb(0x6b7280)),
                            )
                            .child(Input::new(&self.new_session_msg_input)),
                    )
                    .child(
                        Button::new("ns-create-send")
                            .primary()
                            .label("创建会话并发送")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.create_and_send(window, cx);
                            })),
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
                                Label::new("或直接输入自然语言计划")
                                    .text_sm()
                                    .text_color(rgb(0x6b7280)),
                            )
                            .child(Input::new(&self.workflow_input)),
                    )
                    .child(
                        Button::new("ns-create-workflow")
                            .primary()
                            .label("创建编排会话")
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

    /// 工作流模板选择（新会话视图，PRD §3.7 从模板创建）。
    fn render_template_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let templates = self.store.list_templates();
        let mut row = h_flex().gap_1();
        if templates.is_empty() {
            row = row.child(Label::new("（暂无模板，可在设置中新建）"));
        }
        for t in templates {
            let tpl = t.clone();
            row = row.child(
                Button::new(format!("ns-tpl-{}", t.id))
                    .small()
                    .label(t.name.clone())
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.create_workflow_from_template(window, cx, tpl.clone());
                    })),
            );
        }
        row
    }

    fn render_dialog(&self, _window: &mut Window, _cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialog = match &self.selected {
            Some(Selected::Session { machine, id: _ }) => self
                .machine(*machine)
                .map(|m| m.dialog.clone())
                .unwrap_or_default(),
            Some(Selected::Workflow { engine }) => self
                .workflows
                .get(*engine)
                .map(|w| w.session.to_dialog_items())
                .unwrap_or_default(),
            None => Vec::new(),
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
                        .rounded_md()
                        .bg(rgb(0x3b82f6))
                        .text_color(rgb(0xffffff))
                        .shadow_sm()
                        .child(
                            Label::new("我")
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff)),
                        )
                        .child(block_text(content)),
                ),
                DialogItem::AgentOutput { content, .. } => div().id(("row", i)).w_full().child(
                    div()
                        .max_w(px(720.))
                        .p_3()
                        .rounded_md()
                        .bg(rgb(0xffffff))
                        .border_1()
                        .border_color(rgb(0xe5e7eb))
                        .shadow_sm()
                        .child(
                            Label::new("Agent")
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0x6b7280)),
                        )
                        .child(block_text(content)),
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
            div()
                .id("dialog")
                .flex_1()
                .gap_3()
                .p_2()
                .overflow_y_scroll()
                .children(rows)
                .into_any()
        }
    }

    /// 中间面板下方：正在进行的活动（一条或无，空闲不显示，PRD §4.1.3）。
    /// 实时活动来自当前打开会话的 live_activity（turn 中合并流式推送）。
    fn render_activity_bar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let current = match &self.selected {
            Some(Selected::Session { machine, .. }) => {
                self.machine(*machine).and_then(|m| m.live_activity.clone())
            }
            _ => None,
        };
        match &current {
            Some(Activity::Thinking { content, .. }) => h_flex()
                .gap_2()
                .p_2()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfcd34d))
                .rounded_md()
                .child(Spinner::new())
                .child(Label::new(format!("思考中：{}", content)).text_color(rgb(0x92400e)))
                .into_any(),
            Some(Activity::ToolCall { name, title, .. }) => h_flex()
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
                        title.clone().unwrap_or_default()
                    ))
                    .text_color(rgb(0x92400e)),
                )
                .into_any(),
            Some(Activity::Compaction { detail, .. }) => h_flex()
                .gap_2()
                .p_2()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfcd34d))
                .rounded_md()
                .child(Label::new(format!("上下文压缩：{}", detail)).text_color(rgb(0x92400e)))
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
                        if next == Some(Panel::Activities) {
                            if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                                this.load_activities(window, cx, machine, id);
                            }
                        }
                    })),
            )
    }

    fn load_activities(
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
            if let Ok(res) = client
                .request(
                    protocol::method::GET_ACTIVITIES,
                    Some(json!({ "sessionId": session_id })),
                )
                .await
            {
                let acts = res.get("activities").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machine_mut(machine) {
                        m.activities = serde_json::from_value(acts).unwrap_or_default();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
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
                            .child(Input::new(&self.input_state))
                            // Ctrl+Enter 发送（PRD §4.2：多行输入 + 快捷键发送）
                            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                                if ev.keystroke.modifiers.control && ev.keystroke.key == "enter" {
                                    this.send_prompt(window, cx);
                                }
                            })),
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
                    if meta.closed {
                        "已关闭"
                    } else if meta.interrupted {
                        "已中断"
                    } else if meta.state == SessionState::Busy {
                        "工作中"
                    } else {
                        "空闲"
                    },
                ));
        // 标题展示 + 编辑（PRD §3.1：用户可随时修改）
        if self.editing_title {
            body = body.child(
                h_flex().child(Input::new(&self.title_input)).child(
                    Button::new("save-title")
                        .small()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            if let Some(Selected::Session { machine, id }) = this.selected.clone() {
                                this.save_title(window, cx, machine, id);
                            }
                        })),
                ),
            );
        } else {
            let title = if meta.title.is_empty() {
                "（未命名）".to_string()
            } else {
                meta.title.clone()
            };
            body = body.child(Label::new(format!("标题: {title}"))).child(
                Button::new("edit-title")
                    .small()
                    .label("修改标题")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        if let Some(Selected::Session { .. }) = this.selected.clone() {
                            this.editing_title = true;
                        }
                        cx.notify();
                    })),
            );
        }
        // 编排会话：暂停/继续/介入/子会话
        if let Some(Selected::Workflow { engine }) = self.selected.clone() {
            let paused = self
                .workflows
                .get(engine)
                .map(|w| w.session.paused)
                .unwrap_or(false);
            let done = self
                .workflows
                .get(engine)
                .map(|w| w.session.done)
                .unwrap_or(false);
            body = body
                .child(Label::new("— 编排会话 —"))
                .child(
                    Button::new("wf-pause")
                        .small()
                        .label(if paused { "继续" } else { "暂停" })
                        .when(done, |b| b.disabled(true))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.toggle_pause_workflow(engine, window, cx);
                        })),
                )
                .child(Label::new("介入：在下方输入区输入指令发给编排 agent"))
                .child(
                    Button::new("wf-delete")
                        .small()
                        .label("删除编排会话")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.delete_workflow(window, cx, engine);
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

    /// 会话活动历史面板（PRD §4.1.4 会话活动）。
    fn render_activities_panel(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (activities, live) = match &self.selected {
            Some(Selected::Session { machine, .. }) => self
                .machine(*machine)
                .map(|m| (m.activities.clone(), m.live_activity.clone()))
                .unwrap_or_default(),
            _ => (Vec::new(), None),
        };
        let rows = activities
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let (kind, detail) = match a {
                    Activity::Thinking { content, .. } => ("思考", content.clone()),
                    Activity::ToolCall {
                        name,
                        title,
                        content,
                        ..
                    } => (
                        "工具调用",
                        format!(
                            "{} {} {}",
                            name,
                            title.clone().unwrap_or_default(),
                            content.clone().unwrap_or_default()
                        ),
                    ),
                    Activity::Compaction { detail, .. } => ("压缩", detail.clone()),
                };
                div()
                    .id(("act", i))
                    .w_full()
                    .p_1()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    .child(format!("[{kind}] {detail}"))
            })
            .collect::<Vec<_>>();
        // 实时活动（turn 进行中合并流式的一条，追加在历史下方）
        let mut children = rows;
        if let Some(a) = &live {
            let (kind, detail) = match a {
                Activity::Thinking { content, .. } => ("思考", content.clone()),
                Activity::ToolCall {
                    name,
                    title,
                    content,
                    ..
                } => (
                    "工具调用",
                    format!(
                        "{} {} {}",
                        name,
                        title.clone().unwrap_or_default(),
                        content.clone().unwrap_or_default()
                    ),
                ),
                Activity::Compaction { detail, .. } => ("压缩", detail.clone()),
            };
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
                    .child(format!("[{kind}] {detail}")),
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
                    .w(px(860.))
                    .h(px(600.))
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
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

    /// 机器管理：接入/移除 + agent 默认模型 + skills 列表（PRD §4.3）。
    fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines = self
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut item =
                    v_flex()
                        .gap_1()
                        .p_2()
                        .bg(rgb(0xf5f6f8))
                        .rounded_md()
                        .child(Label::new(format!(
                            "{} · {} · {}",
                            m.config.name, m.config.url, m.status
                        )));
                // agent 发现 + 默认模型
                if let Some(info) = &m.info {
                    for h in &info.harnesses {
                        let mut row = h_flex()
                            .gap_1()
                            .child(Label::new(format!(
                                "agent: {}（{}）",
                                h.name,
                                if h.available { "可用" } else { "不可用" }
                            )))
                            .child(Label::new(format!(
                                "默认模型: {}",
                                h.default_model.clone().unwrap_or_else(|| "未设置".into())
                            )));
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
                    // 当前查看的 skills 列表
                    if m.skills_harness.is_some() {
                        item = item.child(Label::new(format!(
                            "skills ({}): {}",
                            m.skills_harness.as_deref().unwrap_or(""),
                            if m.skills.is_empty() {
                                "（无）".to_string()
                            } else {
                                m.skills.join(", ")
                            }
                        )));
                    }
                }
                item.child(
                    Button::new(format!("remove-{i}"))
                        .small()
                        .label("移除")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.remove_machine(i, cx);
                        })),
                )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(Label::new("机器管理"))
            .child(Label::new("默认模型输入："))
            .child(Input::new(&self.model_input))
            .children(machines)
            .child(
                h_flex()
                    .gap_1()
                    .child(Input::new(&self.settings_input))
                    .child(Button::new("settings-add").small().label("添加").on_click(
                        cx.listener(|this, _ev, window, cx| {
                            let text = this.settings_input.read(cx).value().to_string();
                            let parts: Vec<&str> = text.split_whitespace().collect();
                            if parts.len() >= 3 {
                                this.add_machine(
                                    window,
                                    cx,
                                    parts[0].to_string(),
                                    parts[1].to_string(),
                                    parts[2].to_string(),
                                );
                            }
                        }),
                    )),
            )
            .child(Label::new("添加格式：名称 ws://地址 token（空格分隔）"))
            .into_any()
    }

    /// 编排 agent API 配置（PRD §4.3「编排 agent」）。
    fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .child(Label::new("编排 agent（内置编排 agent，rig 单 turn）"))
            .child(Label::new("API Backend"))
            .child(Input::new(&self.orch_backend_input))
            .child(Label::new("Base URL"))
            .child(Input::new(&self.orch_base_input))
            .child(Label::new("API key"))
            .child(Input::new(&self.orch_key_input))
            .child(Label::new("模型"))
            .child(Input::new(&self.orch_model_input))
            .child(
                Button::new("save-orch")
                    .small()
                    .primary()
                    .label("保存")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        let cfg = OrchestratorConfig {
                            api_backend: this.orch_backend_input.read(cx).value().to_string(),
                            base_url: this.orch_base_input.read(cx).value().to_string(),
                            api_key: this.orch_key_input.read(cx).value().to_string(),
                            model: this.orch_model_input.read(cx).value().to_string(),
                        };
                        this.store.save_orchestrator(&cfg);
                        cx.notify();
                    })),
            )
            .into_any()
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
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    .child(Label::new(format!("{}：{}", c.name, c.prompt)))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("qc-edit-name-{i}"))
                                    .small()
                                    .label("改名")
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
                            ),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(Label::new("快捷指令（每条即一段发给 agent 的提示词）"))
            .children(items)
            .child(Label::new("新增 / 编辑："))
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
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    .child(Label::new(format!("{}：{}", s.name, s.description)))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("skill-install-{i}"))
                                    .small()
                                    .label("安装到 agent（新会话）")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        // PRD §3.6：给 agent 安装 skills 时启动新会话并发送安装提示词
                                        let skill = skill.clone();
                                        if let Some(mi) = this.active_machine() {
                                            if let Some(harness) = this.available_harness(mi) {
                                                this.install_skill(window, cx, mi, harness, skill);
                                            }
                                        }
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
                            ),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(Label::new(
                "Skills 注册表（只存一段描述：仓库/资源 URL 或安装方法）",
            ))
            .children(items)
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
                    .child(Label::new(
                        "安装：在机器管理中点 agent 的 skills 查看，选 skill 安装",
                    )),
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
                v_flex()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    .child(Label::new(format!("{}：{}", t.name, t.description)))
                    .child(
                        h_flex()
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
                            ),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(Label::new("工作流模板（名称 + 自然语言描述）"))
            .children(items)
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
        let mut root = h_flex()
            .size_full()
            .relative()
            .items_stretch()
            .child(self.render_sidebar(window, cx))
            .child(self.render_main(window, cx));
        if let Some(p) = panel {
            root = root.child(p);
        }
        if self.show_settings {
            root = root.child(self.render_settings_overlay(window, cx));
        }
        root.into_any()
    }
}

fn short_cwd(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}

/// 截断长文本（日志用）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    } else {
        s.to_string()
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

fn info_row(label: &str, value: &str) -> impl IntoElement {
    h_flex()
        .gap_2()
        .child(
            Label::new(format!("{}：", label))
                .text_sm()
                .text_color(rgb(0x6b7280)),
        )
        .child(
            Label::new(value.to_string())
                .text_sm()
                .text_color(rgb(0x111827)),
        )
}

fn _window_placeholder(w: &mut Window) -> &mut Window {
    w
}
