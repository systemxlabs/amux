use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::*;
use gpui_component::{
    button::*,
    dialog::DialogButtonProps,
    input::{InputEvent, InputState},
    label::Label,
    notification::Notification as UiNotification,
    *,
};

use serde_json::json;

use protocol::{
    ActivitiesResult, AgentListResult, AgentParams, ContentBlock, HistoryResult,
    OngoingActivityResult, OpResult, SessionConfigureParams, SessionIdParams, SessionInfoParams,
    SessionInfoResult, SessionListResult, SessionMeta, SessionNewParams, SessionPageParams,
    SessionPromptParams, SessionResult, SessionState, SessionStateChange, StateChangeReason,
    WorkspaceDiffParams, WorkspaceDiffResult, WorkspaceListResult, WorkspaceReadParams,
    WorkspaceReadResult, WorkspaceRestoreParams,
};

use crate::config::{
    machine_ws_url, ApiFormat, ConfigStore, OrchestratorConfig, QuickCommand, SkillEntry,
    WorkflowTemplate,
};
use crate::logic::{
    compose_prompt, compose_workflow_text, merge_session_window, parse_at_references,
    path_attachment, read_path_context, DialogMsg, InputAttachment,
};
use crate::machine::{MachineStatus, MachineView, WorkspaceDirectory};
use crate::workflow::{now, AgentSlot, MachineSummary, OrcBackend, RigBackend, WorkflowEngine};
use crate::ws::{Notification as WsNotification, WsClient};

/// 会话列表惰性分页窗口大小。
const PAGE_LIMIT: usize = 50;

// 关闭设置浮窗（Escape）。浮窗为手搓 overlay，焦点落在其内部时该动作才可达；
// 处理顺序 = 叠层从顶到底：内嵌表单对话框（含技能表单/操作弹窗）> 整个设置浮窗。
actions!(amux, [CloseSettingsOverlay]);

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Panel {
    Workspace,
    Diff,
    Detail,
    Activities,
}

/// 未发送输入草稿的会话身份：普通会话以机器名（store 内的持久身份，
/// 下标会随删机重排）+ 会话 ID 定位；工作流以工作流会话 ID 定位。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum DraftKey {
    Session { machine: String, id: String },
    Workflow { id: String },
}

/// 切走会话时暂存的未发送输入内容与附件。
pub(crate) struct Draft {
    text: String,
    attachments: Vec<InputAttachment>,
}

/// 草稿交换的纯逻辑：把当前输入按旧会话身份存入草稿表（空内容则移除旧键），
/// 返回应换入的新会话草稿（无则返回空草稿）。返回的草稿同时从表中取出，
/// 与 `set_selected` 的存入/换出配对，保证每个会话只看到自己的输入。
fn swap_draft(
    drafts: &mut HashMap<DraftKey, Draft>,
    old_key: Option<DraftKey>,
    current: Draft,
    new_key: Option<DraftKey>,
) -> Draft {
    let empty = current.text.trim().is_empty() && current.attachments.is_empty();
    if let Some(key) = old_key {
        if empty {
            drafts.remove(&key);
        } else {
            drafts.insert(key, current);
        }
    }
    // 未选中会话时输入区不可达，此处内容无主，直接丢弃
    new_key
        .and_then(|key| drafts.remove(&key))
        .unwrap_or(Draft {
            text: String::new(),
            attachments: Vec::new(),
        })
}

/// 设置浮窗分类。
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SettingsCategory {
    Machines,
    Orchestrator,
    QuickCommands,
    Skills,
    Templates,
}

/// 新会话创建模式（普通 / 工作流）。
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NewSessionMode {
    Direct,
    Workflow,
}

#[derive(Clone, PartialEq)]
pub(crate) enum Selected {
    Session {
        machine: usize,
        id: String,
    },
    /// 以工作流会话 ID（而非 Vec 下标）为身份：列表按活跃度重排、删除会移位下标，
    /// 用下标会让选中态静默漂移到另一个工作流。
    Workflow {
        id: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum SkillAction {
    Install,
    Update,
    Uninstall,
}

impl SkillAction {
    pub(crate) fn prompt(self, skill: &SkillEntry) -> String {
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

    pub(crate) fn label(self) -> &'static str {
        match self {
            SkillAction::Install => "安装",
            SkillAction::Update => "更新",
            SkillAction::Uninstall => "卸载",
        }
    }
}

pub struct AmuxApp {
    pub(crate) store: Arc<ConfigStore>,
    pub(crate) machines: Vec<MachineView>,
    pub(crate) workflows: Vec<WorkflowEngine>,
    pub(crate) session_dir: PathBuf,
    pub(crate) selected: Option<Selected>,
    pub(crate) panel: Option<Panel>,
    pub(crate) sidebar_width_px: f32,
    pub(crate) sidebar_resize_origin: Option<f32>,
    pub(crate) sidebar_resize_initial: f32,
    pub(crate) panel_delta_px: f32,
    pub(crate) panel_resize_origin: Option<f32>,
    pub(crate) panel_resize_initial: f32,
    pub(crate) show_settings: bool,
    pub(crate) show_add_machine_form: bool,
    pub(crate) machine_form_error: Option<String>,
    pub(crate) settings_category: SettingsCategory,
    pub(crate) new_session_mode: NewSessionMode,
    pub(crate) input_state: Entity<InputState>,
    pub(crate) input_attachments: Vec<InputAttachment>,
    /// 各会话未发送的输入草稿（输入框为全局单例，切换会话时按 DraftKey 换入换出）
    pub(crate) drafts: HashMap<DraftKey, Draft>,
    pub(crate) session_cwd_input: Entity<InputState>,
    pub(crate) workflow_input: Entity<InputState>,
    pub(crate) machine_name_input: Entity<InputState>,
    pub(crate) machine_url_input: Entity<InputState>,
    pub(crate) machine_token_input: Entity<InputState>,
    pub(crate) qc_name_input: Entity<InputState>,
    pub(crate) qc_prompt_input: Entity<InputState>,
    pub(crate) skill_name_input: Entity<InputState>,
    pub(crate) skill_desc_input: Entity<InputState>,
    pub(crate) tpl_name_input: Entity<InputState>,
    pub(crate) tpl_desc_input: Entity<InputState>,
    pub(crate) orch_api_format: ApiFormat,
    pub(crate) orch_base_input: Entity<InputState>,
    pub(crate) orch_key_input: Entity<InputState>,
    pub(crate) orch_model_input: Entity<InputState>,
    pub(crate) orchestrator_form_error: Option<String>,
    pub(crate) orchestrator_form_status: Option<String>,
    pub(crate) settings_form_error: Option<String>,
    pub(crate) title_input: Entity<InputState>,
    pub(crate) qc_edit_target: Option<String>,
    pub(crate) skill_edit_target: Option<String>,
    pub(crate) tpl_edit_target: Option<String>,
    pub(crate) show_quick_command_form: bool,
    pub(crate) show_skill_form: bool,
    pub(crate) show_template_form: bool,
    pub(crate) skill_action_dialog: Option<(SkillEntry, SkillAction)>,
    pub(crate) renaming_session: Option<(usize, String)>,
    /// 同 Selected：以工作流会话 ID 为身份
    pub(crate) renaming_workflow: Option<String>,
    pub(crate) new_session_machine: Option<usize>,
    pub(crate) new_session_agent: Option<String>,
    /// 新会话是否以 git worktree 方式工作（docs/PRD.md 新建会话「worktree 开关」）
    pub(crate) new_session_worktree: bool,
    pub(crate) new_session_error: Option<String>,
    pub(crate) show_workspace_dropdown: bool,
    pub(crate) workflow_error: Option<String>,
    pub(crate) workflow_template: Option<WorkflowTemplate>,
    pub(crate) dialog_scroll: ScrollHandle,
    pub(crate) activities_scroll: ScrollHandle,
    pub(crate) diff_scroll: ScrollHandle,
    pub(crate) workflow_dialog_limit: usize,
    /// 会话列表滚动查询的页数 N（docs/DESIGN.md「会话列表滚动查询」）：
    /// 「加载更多」每点击一次 +1，所有在线机器统一查询前 N 页。
    pub(crate) list_pages: usize,
    pub(crate) activities_limit: usize,
    pub(crate) expanded_activities: std::collections::HashSet<String>,
    /// 以工作流会话 ID 为身份（同 Selected）
    pub(crate) expanded_workflows: std::collections::HashSet<String>,
    /// 会话详情页展开的 select 配置选项（key = `{machine}:{session_id}:{config_id}`）
    pub(crate) expanded_config_options: std::collections::HashSet<String>,
    /// 设置浮窗焦点锚：打开时把焦点移入浮窗，Escape 动作（绑定
    /// SettingsOverlay key_context）才能被派发到 on_action
    pub(crate) settings_focus: FocusHandle,
    /// 持有订阅以避免其随 drop 自动取消
    pub(crate) _subs: Vec<Subscription>,
    pub(crate) _tasks: Vec<Task<()>>,
}

impl AmuxApp {
    pub fn new(store: Arc<ConfigStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入消息，Enter 发送；Shift+Enter 换行；@ 引用文件/目录作为上下文")
                .auto_grow(3, 8)
                .submit_on_enter(true)
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
                .placeholder("计划名")
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
            drafts: HashMap::new(),
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
            renaming_session: None,
            renaming_workflow: None,
            new_session_machine: None,
            new_session_agent: None,
            new_session_worktree: false,
            new_session_error: None,
            show_workspace_dropdown: false,
            workflow_error: None,
            workflow_template: None,
            dialog_scroll: ScrollHandle::new(),
            activities_scroll: ScrollHandle::new(),
            diff_scroll: ScrollHandle::new(),
            workflow_dialog_limit: 50,
            list_pages: 1,
            activities_limit: 100,
            expanded_activities: std::collections::HashSet::new(),
            expanded_workflows: std::collections::HashSet::new(),
            expanded_config_options: std::collections::HashSet::new(),
            settings_focus: cx.focus_handle(),
            _subs: Vec::new(),
            _tasks: Vec::new(),
        };
        // Escape → 关闭设置浮窗：仅当焦点在浮窗（SettingsOverlay key_context 内）时命中
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseSettingsOverlay,
            Some("SettingsOverlay"),
        )]);
        app.session_dir = app.store.session_dir();
        // Enter 提交发送：Input 组件在 submit_on_enter 时消费 Enter 键并发出
        // PressEnter，父级无法再通过 on_key_down 捕获，因此在此订阅事件。
        app._subs.push(cx.subscribe_in(
            &app.input_state,
            window,
            |this, _input, event, window, cx| {
                if let InputEvent::PressEnter { shift, .. } = event {
                    if !shift {
                        this.send_prompt(window, cx);
                    }
                }
            },
        ));
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
        app.setup_orch_inputs(window, cx);
        app
    }

    pub(crate) fn setup_orch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.store.orchestrator();
        self.orch_api_format = cfg.api_format;
        self.orch_base_input
            .update(cx, |s, cx| s.set_value(&cfg.base_url, window, cx));
        self.orch_key_input
            .update(cx, |s, cx| s.set_value(&cfg.api_key, window, cx));
        self.orch_model_input
            .update(cx, |s, cx| s.set_value(&cfg.model, window, cx));
    }

    pub(crate) fn save_orchestrator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn machine(&self, i: usize) -> Option<&MachineView> {
        self.machines.get(i)
    }
    pub(crate) fn machine_mut(&mut self, i: usize) -> Option<&mut MachineView> {
        self.machines.get_mut(i)
    }

    /// 当前被选中的普通会话（machine 下标 + id）。
    pub(crate) fn open_session_target(&self) -> Option<(usize, String)> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => Some((*machine, id.clone())),
            _ => None,
        }
    }

    /// 工作流会话 ID → 引擎下标（UI 身份用 ID，引擎存放在 Vec，仅作解析）。
    pub(crate) fn workflow_idx(&self, wf_id: &str) -> Option<usize> {
        self.workflows.iter().position(|wf| wf.id() == wf_id)
    }

    /// 工作流会话 ID → 引擎引用。
    pub(crate) fn workflow(&self, wf_id: &str) -> Option<&WorkflowEngine> {
        self.workflow_idx(wf_id)
            .and_then(|wi| self.workflows.get(wi))
    }

    /// 默认机器下标：有选中会话则用它，否则第一台。
    pub(crate) fn active_machine(&self) -> Option<usize> {
        match &self.selected {
            Some(Selected::Session { machine, .. }) => Some(*machine),
            _ => (!self.machines.is_empty()).then_some(0),
        }
    }

    /// 当前选中会话的生效工作目录：启用 worktree 的会话 agent 实际工作在
    /// 工作树内，目录浏览/改动审查/还原都应对准工作树而非用户指定的主仓库。
    pub(crate) fn selected_workspace(&self) -> Option<(usize, String)> {
        let Selected::Session { machine, id } = self.selected.as_ref()? else {
            return None;
        };
        let session = self
            .machine(*machine)?
            .sessions
            .iter()
            .find(|session| session.id == *id)?;
        let cwd = if session.worktree_dir.is_empty() {
            session.cwd.clone()
        } else {
            session.worktree_dir.clone()
        };
        Some((*machine, cwd))
    }

    pub(crate) fn on_notify(
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
                // 连接失败（server 不可达）即离线；不自动重连，由用户手动触发
                if let Some(m) = this.machines.get_mut(idx) {
                    m.status = MachineStatus::Offline;
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

    pub(crate) fn on_state_change(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &WsNotification,
    ) {
        let Ok(change) = serde_json::from_value::<SessionStateChange>(n.params.clone()) else {
            log::warn!("state_change 通知负载解析失败");
            return;
        };
        let SessionStateChange {
            session_id: sid,
            old_state,
            new_state,
            reason,
        } = change;
        let idle = new_state == SessionState::Idle;
        if let Some(m) = this.machines.get_mut(idx) {
            if let Some(s) = m.sessions.iter_mut().find(|s| s.id == sid) {
                s.state = new_state;
            }
            if let Some(v) = m.views.get_mut(&sid) {
                v.set_busy(!idle);
            }
        }
        this.refresh_sessions(idx, window, cx);

        let Some(wi) = this.workflows.iter().position(|wf| {
            wf.session
                .read()
                .unwrap()
                .children
                .iter()
                .any(|c| c.id == sid)
        }) else {
            return;
        };
        // 取消导致的状态变更不注入，避免编排者与用户的取消拉锯。
        if reason == StateChangeReason::Cancelled {
            // 不推进，但忙碌计数仍要记账，否则取消后计数会永久偏高。
            if let Some(wf) = this.workflows.get_mut(wi) {
                wf.note_child_state(&sid, old_state, new_state);
            }
            return;
        }
        if !idle {
            if let Some(wf) = this.workflows.get_mut(wi) {
                wf.note_child_state(&sid, old_state, new_state);
            }
            return;
        }
        let wf = match this.workflows.get(wi) {
            Some(wf) => wf.clone(),
            None => return,
        };
        let session_dir = this.session_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            run_engine_on_tokio(async move {
                if let Err(e) = wf.on_child_state(&sid, old_state, new_state, reason).await {
                    log::error!("推进工作流失败：{e}");
                }
                if let Err(e) = wf.persist(&session_dir) {
                    log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                }
            })
            .await;
            let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
        });
        // 会话状态在引擎内部共享，后台任务无需把整个引擎写回 UI。
        this._tasks.push(t);
    }

    pub(crate) fn refresh_sessions(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let count = self.list_pages * PAGE_LIMIT;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 滚动查询（docs/DESIGN.md「会话列表滚动查询」）：按数量查询前 N 页
            //（N × 每页 PAGE_LIMIT 条）
            let params = json!({ "limit": count });
            let (pages, has_more) = match client
                .request::<_, SessionListResult>(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                // 刷新失败保持现状：离线/断连由列表的机器在线过滤兜底
                Ok(res) => (res.sessions, res.has_more),
                Err(_) => return,
            };

            // 应用查询结果，并计算工作流关联会话中尚未出现在其中的 id
            let missing = match this.update_in(cx, |this, _w, _cx| {
                let Some(m) = this.machines.get_mut(idx) else {
                    return Vec::new();
                };
                let (list, hm) = merge_session_window(&m.sessions, pages, has_more);
                m.sessions = list;
                m.sessions_has_more = hm;
                crate::logic::sort_sessions_recent(&mut m.sessions);
                let child_ids: Vec<(usize, String)> = this
                    .workflows
                    .iter()
                    .flat_map(|wf| {
                        let children = wf.session.read().unwrap().children.clone();
                        children.into_iter().map(|c| (c.machine_idx, c.id))
                    })
                    .collect();
                child_ids
                    .iter()
                    .filter(|(cmi, cid)| *cmi == idx && !m.sessions.iter().any(|s| s.id == *cid))
                    .map(|(_, cid)| cid.clone())
                    .collect::<Vec<_>>()
            }) {
                Ok(missing) => missing,
                Err(_) => return,
            };
            if missing.is_empty() {
                let _ = this.update_in(cx, |_, _, cx| cx.notify());
                return;
            }

            // 第 2 步补齐：批量查询缺失的工作流关联会话
            if let Ok(res) = client
                .request::<_, SessionInfoResult>(
                    protocol::method::SESSION_INFO,
                    Some(SessionInfoParams {
                        session_ids: missing,
                    }),
                )
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        let (list, _) =
                            merge_session_window(&m.sessions, res.sessions, m.sessions_has_more);
                        m.sessions = list;
                        crate::logic::sort_sessions_recent(&mut m.sessions);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn fetch_agents(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
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

    /// 滚动容器是否贴底：gpui 的 offset.y 范围为 [-max_offset.y, 0]
    /// （顶部 0、底部 -max），贴底即接近区间下端，留 1px 浮点容差。
    pub(crate) fn scroll_handle_at_bottom(handle: &gpui::ScrollHandle) -> bool {
        let off = handle.offset().y;
        let max = handle.max_offset().y;
        off <= -max + px(1.0)
    }

    /// 对话历史滚动区当前是否贴底。
    pub(crate) fn dialog_at_bottom(&self) -> bool {
        Self::scroll_handle_at_bottom(&self.dialog_scroll)
    }

    /// 活动历史滚动区当前是否贴底，语义同 dialog_at_bottom。
    pub(crate) fn activities_at_bottom(&self) -> bool {
        Self::scroll_handle_at_bottom(&self.activities_scroll)
    }

    pub(crate) fn refresh_dialog(
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
                    // 新消息到达前若已在底部，追加内容后保持贴底，避免新消息被遮挡。
                    let was_at_bottom = this.dialog_at_bottom();
                    if let Some(m) = this.machines.get_mut(machine) {
                        if let Some(v) = m.views.get_mut(&session_id) {
                            v.set_history_page(&items, has_more, next_before);
                        }
                    }
                    if was_at_bottom {
                        this.dialog_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_activities(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        // 面板未打开时不主动刷新活动。
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
                    // 新活动到达前若已在底部，追加内容后保持贴底。
                    let was_at_bottom = this.activities_at_bottom();
                    if let Some(m) = this.machines.get_mut(machine) {
                        if let Some(v) = m.views.get_mut(&session_id) {
                            v.set_activities_page(acts, has_more, next_before);
                        }
                    }
                    if was_at_bottom {
                        this.activities_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn load_more_activities(&self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn load_more_history(&self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn refresh_ongoing(
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
                .request::<_, OngoingActivityResult>(
                    protocol::method::SESSION_ONGOING_ACTIVITY,
                    Some(params),
                )
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

    /// 「加载更早会话」：滚动查询页数 N +1，所有在线机器统一按前 N 页重新查询，
    /// 随后各自补齐工作流关联会话（docs/DESIGN.md「会话列表滚动查询」）。
    pub(crate) fn load_more_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_pages += 1;
        for i in 0..self.machines.len() {
            if matches!(self.machines[i].status, MachineStatus::Online) {
                self.refresh_sessions(i, window, cx);
            }
        }
    }

    pub(crate) fn spawn_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.machines.len();
        for i in 0..n {
            let machine_name = self.machines[i].config.name.clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(10))
                        .await;
                    // 定时刷新统一走 refresh_sessions（含滚动查询 N 页与工作流
                    // 关联会话补齐）；按机器名定位 idx，重连后 idx 仍有效
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(idx) = this
                            .machines
                            .iter()
                            .position(|m| m.config.name == machine_name)
                        {
                            this.refresh_sessions(idx, window, cx);
                        }
                    });
                }
            });
            self._tasks.push(t);
        }

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
            cx.background_executor().timer(Duration::from_secs(2)).await;
        });
        self._tasks.push(t);
    }

    pub(crate) fn spawn_machine_tasks(
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

    pub(crate) fn selected_draft_key(&self) -> Option<DraftKey> {
        match self.selected.as_ref()? {
            Selected::Session { machine, id } => Some(DraftKey::Session {
                machine: self.machines.get(*machine)?.config.name.clone(),
                id: id.clone(),
            }),
            Selected::Workflow { id } => Some(DraftKey::Workflow { id: id.clone() }),
        }
    }

    /// 切换选中会话。输入框是全局单例，直接换会话会把 A 的未发送内容串到 B，
    /// 因此先把当前内容按会话身份存入草稿表，再取出新会话的草稿换入。
    pub(crate) fn set_selected(
        &mut self,
        next: Option<Selected>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.input_state.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.input_attachments);
        let old_key = self.selected_draft_key();
        self.selected = next;
        let new_key = self.selected_draft_key();
        let draft = swap_draft(
            &mut self.drafts,
            old_key,
            Draft { text, attachments },
            new_key,
        );
        self.input_attachments = draft.attachments;
        self.input_state
            .update(cx, |s, cx| s.set_value(&draft.text, window, cx));
    }

    pub(crate) fn open_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        self.set_selected(
            Some(Selected::Session {
                machine,
                id: session_id.clone(),
            }),
            window,
            cx,
        );
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

    pub(crate) fn open_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        if let Some(wf) = self.workflow(&wf_id) {
            // 惰性加载：仅在打开会话渲染对话/活动视图时，从 JSONL 按需补齐 payload。
            if let Err(e) = wf.backfill(&self.session_dir) {
                log::error!("补齐工作流历史失败：{e}");
            }
        }
        self.set_selected(Some(Selected::Workflow { id: wf_id }), window, cx);
        self.set_panel(window, cx, None);
        self.workflow_dialog_limit = 50;
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn create_session_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            use_worktree: self.new_session_worktree,
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
                                .record_recent_workspace(&machine_name, &cwd, now());
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

    pub(crate) fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                if let Some(m) = self.machine_mut(machine) {
                    // 本地仅缓存对话视图；会话状态由服务端权威维护，
                    // 经 state_change 推送 / 会话列表轮询同步，应用侧不做乐观改写
                    let v = m.views.entry(id.clone()).or_default();
                    v.dialog.push(DialogMsg::UserMessage {
                        content: blocks.clone(),
                        timestamp: now(),
                    });
                }
                self.dialog_scroll.scroll_to_bottom();
                let prompt_params = params;
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    let result = client
                        .request_ok(protocol::method::SESSION_PROMPT, Some(prompt_params))
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
                        match &result {
                            Err(error) => {
                                w.push_notification(
                                    UiNotification::error(format!("发送失败：{error}"))
                                        .title("消息未发送"),
                                    cx,
                                );
                            }
                            // 发送用户消息即触发列表刷新（docs/DESIGN.md 会话列表刷新机制）
                            Ok(()) => {
                                this.refresh_sessions(machine, w, cx);
                            }
                        }
                        this.refresh_dialog(w, cx, machine, id);
                        cx.notify();
                    });
                })
                .detach();
            }
            Selected::Workflow { id } => {
                let session_dir = self.session_dir.clone();
                let workflow_text = compose_workflow_text(&clean_text, &all);
                let Some(engine) = self.workflow_idx(&id) else {
                    return;
                };
                let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
                    let should_advance = wf.record_user(&workflow_text);
                    if should_advance {
                        wf.begin_busy();
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        log::error!("工作流用户消息持久化失败 {}: {e}", wf.id());
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
                                log::error!("推进工作流失败 {}: {e}", wf.id());
                            }
                            if let Err(e) = wf.persist(&session_dir) {
                                log::error!("工作流状态持久化失败 {}: {e}", wf.id());
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

    pub(crate) fn quick_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        cmd: &QuickCommand,
    ) {
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
                        .request_ok(
                            protocol::method::SESSION_PROMPT,
                            Some(serde_json::to_value(&params).unwrap()),
                        )
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
                // 快捷指令作为用户输入进入工作流会话。
                self.input_state
                    .update(cx, |s, cx| s.set_value(&cmd.prompt, window, cx));
                self.send_prompt(window, cx);
            }
            None => {}
        }
    }

    pub(crate) fn cancel_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Selected::Workflow { id }) = self.selected.clone() {
            self.cancel_workflow(window, cx, id);
            cx.notify();
            return;
        }
        let Some(Selected::Session { machine, id }) = self.selected.clone() else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        // 空闲会话本就无可取消：ACP 侧报错属预期，静默忽略以免污染状态徽章；
        // 忙碌中取消失败才值得提示
        let was_busy = m
            .sessions
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.state == SessionState::Busy);
        let client = m.client.clone();
        let sid = id.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: sid.clone(),
            };
            let res = client
                .request_ok(
                    protocol::method::SESSION_CANCEL,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if let Err(error) = &res {
                    if was_busy {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("取消失败（{error}）"));
                        }
                    }
                }
                this.refresh_dialog(w, cx, machine, sid);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn delete_session(
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
                .request_ok(
                    protocol::method::SESSION_DELETE,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match res {
                    Ok(_) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.sessions.retain(|s| s.id != sid);
                            m.views.remove(&sid);
                        }
                        let machine_name = this
                            .machine(machine)
                            .map(|m| m.config.name.clone())
                            .unwrap_or_default();
                        if let Some(Selected::Session { id, .. }) = this.selected.clone() {
                            if id == sid {
                                this.set_selected(None, w, cx);
                            }
                        }
                        this.drafts.retain(|key, _| match key {
                            DraftKey::Session { id, machine } => {
                                *id != sid || *machine != machine_name
                            }
                            DraftKey::Workflow { .. } => true,
                        });
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

    /// 通用危险/确认弹窗：统一 alert_dialog 结构（按钮文案、危险变体、
    /// 取消按钮），on_ok 动作经闭包注入。各 `confirm_*` 入口共用，避免重复。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn confirm_dialog<F>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ok_text: &'static str,
        danger: bool,
        title: &'static str,
        description: String,
        on_ok: F,
    ) where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
    {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let this = this.clone();
            let on_ok = on_ok.clone();
            alert
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text)
                        .ok_variant(if danger {
                            ButtonVariant::Danger
                        } else {
                            ButtonVariant::Primary
                        })
                        .cancel_text("取消")
                        .show_cancel(true),
                )
                .title(title)
                .description(description.clone())
                .on_ok(move |_ev, window, cx| {
                    let this = this.clone();
                    this.update(cx, |this, cx| on_ok(this, window, cx));
                    true
                })
                .on_cancel(|_ev, _window, _cx| true)
        });
    }

    pub(crate) fn confirm_delete_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除会话",
            format!("确定删除会话 {session_id} 吗？删除后历史一并移除，不可恢复。"),
            move |this, window, cx| {
                let sid = session_id.clone();
                this.delete_session(window, cx, machine, sid);
            },
        );
    }

    pub(crate) fn rename_session(
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
                .request_ok(
                    protocol::method::SESSION_CONFIGURE,
                    Some(serde_json::to_value(&params).unwrap()),
                )
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

    pub(crate) fn rename_workflow(&mut self, cx: &mut Context<Self>, wf_id: &str, title: String) {
        if let Some(wf) = self
            .workflow_idx(wf_id)
            .and_then(|wi| self.workflows.get_mut(wi))
        {
            wf.session.write().unwrap().title = title.trim().to_string();
            let _ = wf.persist(&self.session_dir);
        }
        self.renaming_workflow = None;
        cx.notify();
    }

    pub(crate) fn restore_workflows(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let sessions = match WorkflowEngine::load_all(&self.session_dir) {
            Ok(sessions) => sessions,
            Err(e) => {
                log::error!("加载工作流失败：{e}");
                return;
            }
        };
        if sessions.is_empty() {
            return;
        }
        let clients: Vec<WsClient> = self.machines.iter().map(|m| m.client.clone()).collect();
        let summaries = self.machine_summaries();
        for s in sessions {
            // 每个工作流独立 backend：RigBackend 的 synced_children/synced_activities
            // 是单轮 decide 的回传槽位，共享实例会在并发推进时互相覆盖
            // （A 可能取到 B 的子会话快照）
            self.workflows.push(WorkflowEngine::restore(
                s,
                self.orchestrator_backend(),
                clients.clone(),
                summaries.clone(),
                &self.session_dir,
            ));
        }
        cx.notify();
    }

    pub(crate) fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn create_workflow_with(
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
            &self.session_dir,
        );
        let wi = self.workflows.len();
        let session_dir = self.session_dir.clone();
        self.workflows.push(engine);
        let wf_id = self.workflows[wi].id();
        self.set_selected(Some(Selected::Workflow { id: wf_id }), window, cx);
        let should_advance =
            !clean.trim().is_empty() || preamble.as_deref().is_some_and(|p| !p.trim().is_empty());
        if should_advance {
            if let Some(wf) = self.workflows.get_mut(wi) {
                wf.begin_busy();
            }
        }
        if let Some(wf) = self.workflows.get(wi) {
            if let Err(e) = wf.persist(&session_dir) {
                log::error!("工作流创建后持久化失败 {}: {e}", wf.id());
            }
        }
        if should_advance {
            let wf = self.workflows[wi].clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                run_engine_on_tokio(async move {
                    if let Err(e) = wf.advance().await {
                        log::error!("推进工作流失败 {}: {e}", wf.id());
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                    }
                })
                .await;
                let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
            });
            self._tasks.push(t);
        }
        cx.notify();
    }

    pub(crate) fn cancel_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let session_dir = self.session_dir.clone();
        let Some(engine) = self.workflow_idx(&wf_id) else {
            return;
        };
        let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
            let should_advance = wf.cancel();
            if should_advance {
                wf.begin_busy();
            }
            if let Err(e) = wf.persist(&session_dir) {
                log::error!("工作流取消消息持久化失败 {}: {e}", wf.id());
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
                        log::error!("取消推进工作流失败 {}: {e}", wf.id());
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                    }
                })
                .await;
                let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
            });
            self._tasks.push(t);
        }
    }

    pub(crate) fn confirm_delete_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let child_count = self
            .workflow_idx(&wf_id)
            .and_then(|idx| self.workflows.get(idx))
            .map(|w| w.child_count())
            .unwrap_or(0);
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除工作流会话",
            format!(
                "确定删除该工作流会话吗？将同时删除其 {child_count} 个关联普通会话，不可恢复。"
            ),
            move |this, window, cx| {
                let wf_id = wf_id.clone();
                this.delete_workflow(window, cx, wf_id);
            },
        );
    }

    pub(crate) fn delete_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let Some(idx) = self.workflow_idx(&wf_id) else {
            return;
        };
        let children: Vec<(usize, String)> = self
            .workflows
            .get(idx)
            .map(|w| {
                w.children()
                    .iter()
                    .map(|c| (c.machine_idx, c.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if self
            .workflows
            .get(idx)
            .is_some_and(|workflow| workflow.state() == SessionState::Busy)
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
            let _ = this.update_in(cx, |this, w, cx| {
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
                                // 选中态以工作流会话 ID 为身份：删除后无需平移其他引用
                                this.workflows.retain(|workflow| workflow.id() != wf_id);
                                if this.selected == Some(Selected::Workflow { id: wf_id.clone() }) {
                                    this.set_selected(None, w, cx);
                                }
                                this.drafts.retain(|key, _| match key {
                                    DraftKey::Workflow { id } => id != &wf_id,
                                    // 关联的普通会话已一并删除，草稿随之清理
                                    DraftKey::Session { id, .. } => {
                                        !children.iter().any(|(_, sid)| sid == id)
                                    }
                                });
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

    pub(crate) fn orchestrator_backend(&self) -> Arc<dyn OrcBackend> {
        let cfg = self.store.orchestrator();
        Arc::new(RigBackend::new(cfg))
    }

    pub(crate) fn machine_summaries(&self) -> Vec<MachineSummary> {
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

    pub(crate) fn available_agent(&self, idx: usize) -> Option<String> {
        self.machine(idx).and_then(|m| {
            m.agents
                .iter()
                .find(|a| a.available)
                .map(|a| a.name.clone())
        })
    }

    pub(crate) fn selected_meta(&self) -> Option<SessionMeta> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.sessions.iter().find(|s| s.id == *id))
                .cloned(),
            Some(Selected::Workflow { id }) => {
                let wf = self.workflows.get(self.workflow_idx(id)?)?;
                let sg = wf.snapshot();
                Some(SessionMeta {
                    id: sg.id.clone(),
                    agent: "编排".into(),
                    cwd: String::new(),
                    state: sg.state,
                    title: sg.title.clone(),
                    created_at: sg.created_at,
                    last_active_at: sg.updated_at,
                    worktree_dir: String::new(),
                    context_size: 0,
                    context_window_size: 0,
                    config_options: Vec::new(),
                })
            }
            None => None,
        }
    }

    pub(crate) fn load_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
    ) {
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

    pub(crate) fn load_workspace_list(
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

    pub(crate) fn load_workspace_file(
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

    pub(crate) fn restore_workspace(
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

    pub(crate) fn toggle_diff_selection(
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

    pub(crate) fn is_diff_selected(&self, machine: usize, path: &str, hunk: Option<usize>) -> bool {
        self.machine(machine)
            .is_some_and(|m| m.diff_selection.contains(&(path.to_string(), hunk)))
    }

    pub(crate) fn clear_diff_selection(&mut self, machine: usize, _cx: &mut Context<Self>) {
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff_selection.clear();
        }
    }

    pub(crate) fn send_selected_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
    ) {
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
                .request_ok(
                    protocol::method::SESSION_PROMPT,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                this.refresh_dialog(w, cx, machine, session_id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 通过普通会话执行技能操作，保留完整会话供用户继续干预。
    pub(crate) fn manage_skill_on_agent(
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
        // PRD：技能操作的临时会话固定在工作目录为系统临时目录的普通会话中执行
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let operation_prompt = action.prompt(&skill);
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = async {
                let session = client
                    .request::<_, SessionResult>(
                        protocol::method::SESSION_NEW,
                        Some(SessionNewParams {
                            agent: agent.clone(),
                            cwd: cwd.clone(),
                            // 技能操作是临时会话：在系统临时目录执行
                            use_worktree: false,
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
                    input: vec![ContentBlock::Text {
                        text: operation_prompt,
                    }],
                };
                client
                    .request_ok(
                        protocol::method::SESSION_PROMPT,
                        Some(serde_json::to_value(&input).unwrap()),
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
                        m.notice = Some(format!("技能{}失败：{error}", action.label()));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn restart_agent(
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
                .request_ok(
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

    pub(crate) fn confirm_restart_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "重启",
            false,
            "重启 agent",
            format!("确定重启 agent「{agent}」吗？"),
            move |this, window, cx| {
                let agent = agent.clone();
                this.restart_agent(window, cx, machine, agent);
            },
        );
    }

    pub(crate) fn confirm_reconnect_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "重连",
            false,
            "重连机器",
            format!("确定重连机器「{name}」吗？"),
            move |this, window, cx| {
                this.reconnect_machine(window, cx, idx);
            },
        );
    }

    pub(crate) fn close_add_machine_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn add_machine(
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

    pub(crate) fn remove_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
    ) {
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
        // 草稿键以机器名定位，须在下标重排/机器移除前完成旧草稿保存与新草稿换入
        let next = match self.selected.clone() {
            Some(Selected::Session { machine, .. }) if machine == idx => None,
            Some(Selected::Session { machine, id }) if machine > idx => Some(Selected::Session {
                machine: machine - 1,
                id,
            }),
            other => other,
        };
        self.set_selected(next, window, cx);
        self.store.remove_machine(&name);
        self.drafts.retain(|key, _| match key {
            DraftKey::Session { machine, .. } => machine != &name,
            DraftKey::Workflow { .. } => true,
        });
        self.machines.remove(idx);
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

    pub(crate) fn confirm_remove_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认移除",
            true,
            "移除机器",
            format!("确定移除机器「{name}」吗？其本地注册信息将被删除。"),
            move |this, window, cx| {
                this.remove_machine(window, cx, idx);
            },
        );
    }

    pub(crate) fn open_quick_command_form(
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

    pub(crate) fn close_quick_command_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_quick_command_form = false;
        self.qc_edit_target = None;
        self.qc_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.qc_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn confirm_remove_quick_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除快捷指令",
            format!("确定删除快捷指令「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_quick_command(&name);
                cx.notify();
            },
        );
    }

    pub(crate) fn open_skill_form(
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

    pub(crate) fn close_skill_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_skill_form = false;
        self.skill_edit_target = None;
        self.skill_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.skill_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn confirm_remove_skill(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除技能",
            format!("确定删除技能「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_skill(&name);
                cx.notify();
            },
        );
    }

    pub(crate) fn open_skill_action_dialog(
        &mut self,
        skill: SkillEntry,
        action: SkillAction,
        cx: &mut Context<Self>,
    ) {
        self.skill_action_dialog = Some((skill, action));
        cx.notify();
    }

    pub(crate) fn open_template_form(
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

    pub(crate) fn close_template_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_template_form = false;
        self.tpl_edit_target = None;
        self.tpl_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tpl_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.tpl_name_input.read(cx).value().trim().to_owned();
        let plan = self.tpl_desc_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入计划名称。".into());
        } else if plan.is_empty() {
            self.settings_form_error = Some("请输入计划内容。".into());
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

    pub(crate) fn confirm_remove_template(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除工作流计划",
            format!("确定删除工作流计划「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_template(&name);
                cx.notify();
            },
        );
    }

    const PANEL_RESIZE_HANDLE_WIDTH: f32 = 5.0;

    pub(crate) fn panel_width_logical(panel: Panel) -> f32 {
        match panel {
            Panel::Workspace => 520.0,
            Panel::Diff => 460.0,
            Panel::Detail => 360.0,
            Panel::Activities => 400.0,
        }
    }

    /// 打开/切换/关闭右侧上下文面板：窗口向右扩展（中间面板宽度不变）。
    pub(crate) fn set_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        panel: Option<Panel>,
    ) {
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

    /// 打开设置浮窗并把焦点移入：Escape（绑定 SettingsOverlay key_context）
    /// 只有焦点落在浮窗内部时才会派发到 on_action。
    pub(crate) fn open_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        category: Option<SettingsCategory>,
    ) {
        self.show_settings = true;
        if let Some(category) = category {
            self.settings_category = category;
        }
        window.focus(&self.settings_focus, cx);
        cx.notify();
    }

    pub(crate) fn render_panel(
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
            .w(px(Self::PANEL_RESIZE_HANDLE_WIDTH)) // 拖拽手柄宽度：物理命中区域
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
                .child(
                    div()
                        .w(px(panel_width)) // 拖拽解析出的运行时宽度（随 pointer 事件更新）
                        .h_full()
                        .min_w_0()
                        .child(panel),
                )
                .into_any(),
        )
    }

    pub(crate) fn resize_panel(&mut self, window: &mut Window, cx: &mut Context<Self>, width: f32) {
        let current = self.panel_delta_px;
        let bounds = window.bounds();
        let base = bounds.size.width - current.into();
        self.panel_delta_px = width;
        window.resize(gpui::Size::new(base + width.into(), bounds.size.height));
        cx.notify();
    }
}

impl AmuxApp {
    /// 重连机器：重建其 WS 连接视图。
    pub(crate) fn reconnect_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
    ) {
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
        // 不在此处立即拉取：连接任务在 auth 握手完成前会拒绝一切请求，
        // 提前发的 agent.list 必然失败并把 notice 染成「agent 列表获取失败」。
        // 初始数据由 on_notify 的 auth_ok 分支统一拉取（同启动流程）。
        cx.notify();
    }
}

pub(crate) enum SessionListItem {
    Session { machine: usize, meta: SessionMeta },
    Workflow { idx: usize },
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.render_panel(window, cx);
        // 库 TitleBar：拖拽/双击最大化、Linux/Windows 窗口控制按钮由组件负责。
        // 外层按下时压制窗口级文本选择（同 Button/Input 机制）：标题栏拖动走
        // WM 交互移动，Wayland/X11 下松开事件被合成器吞掉，选择控制器收不到
        // MouseUp 会滞留拖选态——不启动选区则丢失的 MouseUp 无害
        let title_bar = div()
            .id("title-bar-wrap")
            .w_full()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                TitleBar::new().child(
                    Label::new("amux")
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground),
                ),
            );

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
/// GPUI 主线程无 Tokio runtime。
async fn run_engine_on_tokio<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::ws::runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_key(machine: &str, id: &str) -> DraftKey {
        DraftKey::Session {
            machine: machine.into(),
            id: id.into(),
        }
    }

    fn draft(text: &str) -> Draft {
        Draft {
            text: text.into(),
            attachments: vec![],
        }
    }

    #[::core::prelude::v1::test]
    fn draft_isolated_per_session() {
        let mut drafts = HashMap::new();

        // 在 A 输入后切到 B：A 的草稿留存，B 拿到空草稿
        swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-a")),
            draft("a"),
            Some(session_key("m1", "s-b")),
        );
        assert_eq!(drafts[&session_key("m1", "s-a")].text, "a");

        // 在 B 输入后切回 A：两边各自看到自己的内容
        let for_a = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-b")),
            draft("b"),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(drafts[&session_key("m1", "s-b")].text, "b");
        assert_eq!(for_a.text, "a");
        assert!(!drafts.contains_key(&session_key("m1", "s-a")));
    }

    #[::core::prelude::v1::test]
    fn empty_input_on_leaving_clears_draft() {
        // 曾在 A 留过草稿，之后清空输入再离开，不应残留旧草稿
        let mut drafts = HashMap::new();
        swap_draft(&mut drafts, None, draft(""), Some(session_key("m1", "s-a")));
        swap_draft(&mut drafts, None, draft(""), Some(session_key("m1", "s-b")));

        // 回到 A 带出旧草稿
        let for_a = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-b")),
            draft(""),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(for_a.text, "");

        // 带着空输入再次离开 A
        let for_b = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-a")),
            draft(""),
            Some(session_key("m1", "s-b")),
        );
        assert_eq!(for_b.text, "");
        assert!(drafts.is_empty());
    }

    #[::core::prelude::v1::test]
    fn unowned_input_is_dropped_without_selection() {
        // 未选中会话时输入区无主，切换不应把内容挂到新会话头上
        let mut drafts = HashMap::new();
        let for_a = swap_draft(
            &mut drafts,
            None,
            draft("x"),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(for_a.text, "");
        assert!(drafts.is_empty());
    }
}
