use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    dialog::DialogButtonProps,
    input::{InputEvent, InputState},
    label::Label,
    *,
};

use protocol::{
    OpResult, SessionConfigOption, SessionMeta, SessionState, SessionStateChange, SlashCommand,
    StateChangeReason, TerminalOpenParams, TerminalOpenResult,
};

use base64::Engine as _;

use crate::config::{ConfigStore, SkillEntry};
use crate::logic::InputAttachment;
use crate::machine::{MachineStatus, MachineView};
use crate::workflow::{HubEvent, MachineHub, WorkflowEngine};
use crate::ws::{Notification as WsNotification, WsClient};

/// 会话列表惰性分页窗口大小。
pub(crate) const PAGE_LIMIT: usize = 50;

// 关闭设置浮窗（Escape）。浮窗为手搓 overlay，焦点落在其内部时该动作才可达；
// 处理顺序 = 叠层从顶到底：内嵌表单对话框（含技能表单/操作弹窗）> 整个设置浮窗。
actions!(amux, [CloseSettingsOverlay]);

#[derive(Clone, Copy, PartialEq)]
pub enum Panel {
    Workspace,
    Diff,
    Detail,
    Activities,
    Plan,
    Terminal,
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
    pub(crate) text: String,
    pub(crate) attachments: Vec<InputAttachment>,
}

/// 草稿交换的纯逻辑：把当前输入按旧会话身份存入草稿表（空内容则移除旧键），
/// 返回应换入的新会话草稿（无则返回空草稿）。返回的草稿同时从表中取出，
/// 与 `set_selected` 的存入/换出配对，保证每个会话只看到自己的输入。
pub(crate) fn swap_draft(
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

/// 选中普通会话的会话选项状态（经 `session.config_options` 查询；
/// docs/DESIGN.md「普通会话选项」：存储在 Server 内存，以 Agent 侧数据为权威）。
#[derive(Default)]
pub struct SelectedConfigOptions {
    pub machine: usize,
    pub session_id: String,
    pub loading: bool,
    pub options: Vec<SessionConfigOption>,
}

/// 选中普通会话的斜杠命令（经 `session.slash_commands` 查询；
/// docs/DESIGN.md「普通会话斜杠命令」：存储在 Server 内存，以 Agent 侧数据为权威）。
#[derive(Default)]
pub struct SelectedSlashCommands {
    pub machine: usize,
    pub session_id: String,
    pub commands: Vec<SlashCommand>,
}

#[derive(Clone, PartialEq)]
pub enum Selected {
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
    pub machines: Vec<MachineView>,
    /// 机器运行时注册表：机器增删/重连/状态变化时同步，
    /// 工作流引擎每次推进经此取最新连接（不持有陈旧快照）。
    pub(crate) machine_hub: Arc<MachineHub>,
    /// 工作流会话引擎（集成测试直接注入/断言工作流状态）
    pub workflows: Vec<WorkflowEngine>,
    pub(crate) data_dir: PathBuf,
    pub selected: Option<Selected>,
    pub panel: Option<Panel>,
    pub sidebar_width_px: f32,
    pub(crate) sidebar_resize_origin: Option<f32>,
    pub(crate) sidebar_resize_initial: f32,
    pub panel_delta_px: f32,
    pub(crate) panel_resize_origin: Option<f32>,
    pub(crate) panel_resize_initial: f32,
    pub(crate) new_session_mode: NewSessionMode,
    pub input_state: Entity<InputState>,
    pub(crate) input_attachments: Vec<InputAttachment>,
    /// 各会话未发送的输入草稿（输入框为全局单例，切换会话时按 DraftKey 换入换出）
    pub(crate) drafts: HashMap<DraftKey, Draft>,
    pub(crate) session_cwd_input: Entity<InputState>,
    pub(crate) workflow_input: Entity<InputState>,
    pub(crate) title_input: Entity<InputState>,
    /// 设置域状态（浮窗开关/导航与全部表单）：所有权与逻辑归 settings.rs
    pub(crate) settings: crate::settings::SettingsState,
    pub(crate) renaming_session: Option<(usize, String)>,
    /// 同 Selected：以工作流会话 ID 为身份
    pub(crate) renaming_workflow: Option<String>,
    pub(crate) new_session_machine: Option<usize>,
    pub(crate) new_session_agent: Option<String>,
    /// 新会话是否以 git worktree 方式工作（docs/PRD.md 新建会话「worktree 开关」）
    pub(crate) new_session_worktree: bool,
    pub(crate) new_session_error: Option<String>,
    pub(crate) show_workspace_dropdown: bool,
    /// 新建工作流视图的工作流下拉弹层开启态
    pub(crate) show_workflow_dropdown: bool,
    pub(crate) workflow_error: Option<String>,
    pub(crate) dialog_scroll: ScrollHandle,
    pub(crate) activities_scroll: ScrollHandle,
    pub(crate) plan_scroll: ScrollHandle,
    pub diff_scroll: VirtualListScrollHandle,
    pub(crate) workflow_dialog_limit: usize,
    /// 会话列表滚动查询的页数 N（docs/DESIGN.md「会话列表滚动查询」）：
    /// 「加载更多」每点击一次 +1，所有在线机器统一查询前 N 页。
    pub(crate) list_pages: usize,
    pub(crate) activities_limit: usize,
    pub(crate) expanded_activities: std::collections::HashSet<String>,
    /// 以工作流会话 ID 为身份（同 Selected）
    pub(crate) expanded_workflows: std::collections::HashSet<String>,
    /// 选中普通会话的会话选项（`session.config_options`；以 Agent 侧数据为权威）
    pub config_options: Option<SelectedConfigOptions>,
    /// 选中普通会话的斜杠命令（`session.slash_commands`；以 Agent 侧数据为权威）
    pub slash_commands: Option<SelectedSlashCommands>,
    /// 持有订阅以避免其随 drop 自动取消
    pub(crate) _subs: Vec<Subscription>,
    pub(crate) _tasks: Vec<Task<()>>,
}

/// 侧栏拖拽手柄的载荷类型。`on_drag_move` 是窗口级全局监听，仅按载荷
/// TypeId 区分来源：两个手柄若共用 `()`，任一拖拽都会触发两者的调整逻辑。
struct SidebarResizeDrag;

/// 右侧面板拖拽手柄的载荷类型（见 `SidebarResizeDrag`）。
struct PanelResizeDrag;

impl AmuxApp {
    pub(crate) const PANEL_RESIZE_HANDLE_WIDTH: f32 = 5.0;
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
                .placeholder("手动输入工作流计划，或点击右侧箭头选择已保存的工作流…")
                .auto_grow(3, 10)
                .multi_line(true)
        });
        let settings = crate::settings::SettingsState::new(window, cx);
        let title_input = cx.new(|cx| InputState::new(window, cx));

        let mut app = AmuxApp {
            store,
            machines: Vec::new(),
            machine_hub: Arc::new(MachineHub::default()),
            workflows: Vec::new(),
            data_dir: PathBuf::new(),
            selected: None,
            panel: None,
            sidebar_width_px: crate::theme::SIDEBAR_WIDTH * window.scale_factor(),
            sidebar_resize_origin: None,
            sidebar_resize_initial: crate::theme::SIDEBAR_WIDTH * window.scale_factor(),
            panel_delta_px: 0.0,
            panel_resize_origin: None,
            panel_resize_initial: 0.0,
            settings,
            new_session_mode: NewSessionMode::Direct,
            input_state,
            input_attachments: Vec::new(),
            drafts: HashMap::new(),
            session_cwd_input,
            workflow_input,
            title_input,
            renaming_session: None,
            renaming_workflow: None,
            new_session_machine: None,
            new_session_agent: None,
            new_session_worktree: false,
            new_session_error: None,
            show_workspace_dropdown: false,
            show_workflow_dropdown: false,
            workflow_error: None,
            dialog_scroll: ScrollHandle::new(),
            activities_scroll: ScrollHandle::new(),
            plan_scroll: ScrollHandle::new(),
            diff_scroll: VirtualListScrollHandle::new(),
            workflow_dialog_limit: 50,
            list_pages: 1,
            activities_limit: 100,
            expanded_activities: std::collections::HashSet::new(),
            expanded_workflows: std::collections::HashSet::new(),
            config_options: None,
            slash_commands: None,
            _subs: Vec::new(),
            _tasks: Vec::new(),
        };
        // Escape → 关闭设置浮窗：仅当焦点在浮窗（SettingsOverlay key_context 内）时命中
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseSettingsOverlay,
            Some("SettingsOverlay"),
        )]);
        // Tab/Shift+Tab 进终端输入：终端 context 比 Root 的全局 tab（焦点循环）
        // 更深、优先级更高，防止按 Tab 抢走焦点导致终端收不到输入
        crate::terminal::init(cx);
        // 数据根（~/.amux/app）：session.sqlite 与 sessions/ JSONL 的统一根，
        // 与 amux_common::session_log 的目录约定一致
        app.data_dir = app.store.data_dir();
        // Enter 提交发送：Input 组件在 submit_on_enter 时消费 Enter 键并发出
        // PressEnter，父级无法再通过 on_key_down 捕获，因此在此订阅事件。
        app._subs.push(cx.subscribe_in(
            &app.input_state,
            window,
            |this, _input, event, window, cx| {
                match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.send_prompt(window, cx);
                    }
                    // 输入变化触发整窗重绘：斜杠命令上拉框按当前输入前缀派生
                    //（docs/PRD.md「会话交互视图」）
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }
            },
        ));
        // 标题重命名输入框（会话/工作流共用）：Enter 保存；取消走「取消」按钮。
        // 此前 Enter 无响应、也无取消路径，重命名会一直挂在编辑态
        app._subs.push(cx.subscribe_in(
            &app.title_input,
            window,
            |this, _input, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    if let Some((machine, sid)) = this.renaming_session.clone() {
                        let title = this.title_input.read(cx).value().to_string();
                        this.rename_session(window, cx, machine, sid, title);
                    } else if let Some(wf) = this.renaming_workflow.clone() {
                        let title = this.title_input.read(cx).value().to_string();
                        this.rename_workflow(cx, &wf, title);
                    }
                }
            },
        ));
        for m in app.store.list_machines() {
            app.machines.push(MachineView::new(m, cx));
        }
        app.sync_machine_hub();
        for i in 0..app.machines.len() {
            let name = app.machines[i].config.name.clone();
            let client = app.machines[i].client.clone();
            let t = app.spawn_machine_tasks(window, cx, name, client);
            app._tasks.push(t);
        }
        // 编排引擎事件订阅：create_session 挂载新子会话后即时刷新对应机器
        // 会话列表，让新会话立即以「关联普通会话」形式出现在工作流会话下
        //（否则下次轮询/状态变更前会以独立普通会话身份展示）
        let mut hub_events = app.machine_hub.subscribe();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            loop {
                match hub_events.recv().await {
                    Ok(HubEvent::ChildMounted { machine_name, .. }) => {
                        let _ = this.update_in(cx, |this, window, cx| {
                            if let Some(idx) = this
                                .machines
                                .iter()
                                .position(|m| m.config.name == machine_name)
                            {
                                this.refresh_sessions(idx, window, cx);
                            }
                            cx.notify();
                        });
                    }
                    // 事件积压丢帧只影响刷新时机（10s 轮询兜底），继续消费
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        app._tasks.push(t);
        // 初始数据不在此处拉取：认证握手完成前请求会被拒绝，
        // 由 on_notify 的 auth_ok 分支统一拉取（同加机/重连流程）
        app.restore_workflows(window, cx);
        app.spawn_polling(window, cx);
        app.setup_orch_inputs(window, cx);
        app
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
                    // 终端随连接生死（docs/DESIGN.md「终端」），server 已释放，UI 同步清理
                    m.terminals.clear();
                    m.active_terminal = None;
                }
            }
            // 唯一主动推送 `session.state_change`：
            // 1) 更新普通会话本地状态；2) 若属于某工作流的关联普通会话则驱动工作流。
            _ if n.method == protocol::notify::SESSION_STATE_CHANGE => {
                Self::on_state_change(this, window, cx, idx, n);
            }
            _ if n.method == protocol::notify::TERMINAL_OUTPUT => {
                Self::on_terminal_output(this, cx, idx, n);
            }
            _ if n.method == protocol::notify::TERMINAL_EXIT => {
                Self::on_terminal_exit(this, cx, idx, n);
            }
            _ => {}
        }
        // 在线状态可能已变：工作流推进前依赖 hub 快照看到最新机器视图
        this.sync_machine_hub();
        cx.notify();
    }

    /// 终端输出：按 terminal_id 路由到对应视图实体并喂入 VT 状态机。
    pub(crate) fn on_terminal_output(
        this: &mut Self,
        cx: &mut Context<Self>,
        idx: usize,
        n: &WsNotification,
    ) {
        let Ok(payload) =
            serde_json::from_value::<protocol::TerminalOutputNotification>(n.params.clone())
        else {
            log::warn!("terminal.output 通知负载解析失败");
            return;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&payload.data) else {
            return;
        };
        let Some(m) = this.machines.get_mut(idx) else {
            return;
        };
        let Some(entry) = m
            .terminals
            .iter()
            .find(|t| t.id == payload.terminal_id)
            .cloned()
        else {
            return;
        };
        entry.view.update(cx, |state, cx| state.advance(&bytes, cx));
    }

    /// 终端进程退出：标记残屏，由用户关闭标签。
    pub(crate) fn on_terminal_exit(
        this: &mut Self,
        cx: &mut Context<Self>,
        idx: usize,
        n: &WsNotification,
    ) {
        let Ok(payload) =
            serde_json::from_value::<protocol::TerminalExitNotification>(n.params.clone())
        else {
            return;
        };
        let Some(m) = this.machines.get_mut(idx) else {
            return;
        };
        let Some(entry) = m
            .terminals
            .iter()
            .find(|t| t.id == payload.terminal_id)
            .cloned()
        else {
            return;
        };
        entry.view.update(cx, |state, cx| {
            state.exited = true;
            cx.notify();
        });
    }

    /// 在当前选中普通会话的上下文新建终端（cwd 取工作目录/worktree）。
    /// docs/DESIGN.md「终端」：终端不归属会话、连接绑定；open 携带初始行列，
    /// 避免先 80×24 再 resize 的全屏程序初始渲染错乱。
    pub(crate) fn spawn_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_idx: usize,
    ) {
        let Some((_, cwd)) = self.selected_workspace() else {
            return;
        };
        let Some(session_id) = self.open_session_target().map(|(_, id)| id) else {
            return;
        };
        let Some(m) = self.machine_mut(machine_idx) else {
            return;
        };
        if !m.status.online() {
            m.notice = Some("机器离线，无法打开终端".into());
            cx.notify();
            return;
        }
        let client = m.client.clone();
        // 面板宽 560（上下扣掉标题栏/输入区等约 320）：与打开后的实际网格接近，
        // 打开后的画布实测仍会触发一次 resize 精调
        let cols = 68u16;
        let rows = 24u16;
        let title = cwd
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&cwd)
            .to_string();
        cx.spawn_in(window, async move |this, cx| {
            let opened = client
                .request::<_, TerminalOpenResult>(
                    protocol::method::TERMINAL_OPEN,
                    Some(TerminalOpenParams {
                        cwd: cwd.clone(),
                        cols,
                        rows,
                    }),
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(m) = this.machine_mut(machine_idx) else {
                    return;
                };
                match opened {
                    Ok(result) => {
                        let terminal_id = result.terminal_id.clone();
                        let view = cx.new(|cx| {
                            crate::terminal::TerminalState::new(
                                terminal_id.clone(),
                                client.clone(),
                                cols,
                                rows,
                                cx,
                            )
                        });
                        m.terminals.push(crate::terminal::TerminalEntry {
                            id: terminal_id.clone(),
                            session_id: session_id.clone(),
                            title: title.clone(),
                            view: view.clone(),
                        });
                        m.active_terminal = Some(terminal_id.clone());
                        window.focus(&view.read(cx).focus.clone(), cx);
                    }
                    Err(e) => {
                        m.notice = Some(format!("打开终端失败：{e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 关闭并移除指定终端（server 侧杀 PTY；断连时 server 自行回收）。
    pub(crate) fn close_terminal(
        &mut self,
        cx: &mut Context<Self>,
        machine_idx: usize,
        id: String,
    ) {
        let Some(m) = self.machine_mut(machine_idx) else {
            return;
        };
        m.terminals.retain(|t| t.id != id);
        if m.active_terminal.as_deref() == Some(id.as_str()) {
            m.active_terminal = m.terminals.last().map(|t| t.id.clone());
        }
        let client = m.client.clone();
        cx.spawn(async move |_, _cx| {
            let _: Result<OpResult, _> = client
                .request(
                    protocol::method::TERMINAL_CLOSE,
                    Some(protocol::TerminalIdParams { terminal_id: id }),
                )
                .await;
        })
        .detach();
        cx.notify();
    }

    /// 处理 `session.state_change` 通知：更新本地会话状态并驱动工作流推进
    ///（集成测试经此模拟 server 推送）。
    pub fn on_state_change(
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
        }
        this.refresh_sessions(idx, window, cx);

        // turn 结束后选项可能经 config_option_update / available_commands_update
        // 变化：选中会话刷新会话选项与斜杠命令（docs/DESIGN.md：均以 Agent 侧
        // 数据为权威）
        if idle {
            let selected_matches = matches!(
                &this.selected,
                Some(Selected::Session { machine, id }) if *machine == idx && *id == sid
            );
            if selected_matches {
                this.refresh_config_options(cx, idx, sid.clone());
                this.refresh_slash_commands(window, cx, idx, sid.clone());
            }
        }

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
        let data_dir = this.data_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            run_engine_on_tokio(async move {
                if let Err(e) = wf.on_child_state(&sid, old_state, new_state, reason).await {
                    log::error!("推进工作流失败：{e}");
                }
                if let Err(e) = wf.persist(&data_dir) {
                    log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                }
            })
            .await;
            let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
        });
        // 会话状态在引擎内部共享，后台任务无需把整个引擎写回 UI。
        this._tasks.push(t);
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
                    this.refresh_activities(window, cx, machine, id.clone());
                    this.refresh_plan(window, cx, machine, id);
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
        name: String,
        _client: WsClient,
    ) -> Task<()> {
        let mut notify_rx = self
            .machine_idx_by_name(&name)
            .and_then(|idx| self.machines.get(idx))
            .expect("机器已存在")
            .client
            .subscribe();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 以机器名（稳定域身份）为捕获，而非 Vec 下标——删机会使下标左移，
            // 旧任务按旧 idx 写状态会串到另一台机器。机器移除后任务退出。
            loop {
                match notify_rx.recv().await {
                    Ok(n) => {
                        let mut gone = false;
                        let _ = this.update_in(cx, |this, window, cx| {
                            match this.machine_idx_by_name(&name) {
                                Some(idx) => Self::on_notify(this, window, cx, idx, &n),
                                None => gone = true,
                            }
                        });
                        if gone {
                            return;
                        }
                    }
                    // Lagged 只是慢消费者丢消息，通道仍存活——必须继续消费，
                    // 否则通知洪峰过后该机器的 state_change 驱动将永久失效
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        log::warn!("机器 {name} 通知积压，跳过 {missed} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// 用当前机器视图整体刷新运行时注册表（增删/重连/状态变化后调用）。
    pub(crate) fn sync_machine_hub(&self) {
        let clients = self.machines.iter().map(|m| m.client.clone()).collect();
        self.machine_hub.sync(self.machine_summaries(), clients);
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
        // 工作目录/文件改动/会话计划/终端仅普通会话展示（docs/PRD.md「右侧面板」）：
        // 切到工作流会话时关闭残留的普通会话专属面板
        if !matches!(self.selected, Some(Selected::Session { .. }))
            && matches!(
                self.panel,
                Some(Panel::Workspace)
                    | Some(Panel::Diff)
                    | Some(Panel::Plan)
                    | Some(Panel::Terminal)
            )
        {
            self.panel = None;
        }
    }

    /// 当前被选中的普通会话（machine 下标 + id）。
    pub(crate) fn open_session_target(&self) -> Option<(usize, String)> {
        match &self.selected {
            Some(Selected::Session { machine, id }) => Some((*machine, id.clone())),
            _ => None,
        }
    }

    pub(crate) fn machine(&self, i: usize) -> Option<&MachineView> {
        self.machines.get(i)
    }

    pub(crate) fn machine_mut(&mut self, i: usize) -> Option<&mut MachineView> {
        self.machines.get_mut(i)
    }

    /// 工作流会话 ID → 引擎引用。
    pub(crate) fn workflow(&self, wf_id: &str) -> Option<&WorkflowEngine> {
        self.workflow_idx(wf_id)
            .and_then(|wi| self.workflows.get(wi))
    }

    /// 工作流会话 ID → 引擎下标（UI 身份用 ID，引擎存放在 Vec，仅作解析）。
    pub(crate) fn workflow_idx(&self, wf_id: &str) -> Option<usize> {
        self.workflows.iter().position(|wf| wf.id() == wf_id)
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

    pub(crate) fn panel_width_logical(panel: Panel) -> f32 {
        match panel {
            Panel::Workspace => 520.0,
            // diff 行内容较宽，默认给足横向空间（可拖拽 300-800 再调）
            Panel::Diff => 560.0,
            Panel::Detail => 360.0,
            Panel::Activities => 400.0,
            Panel::Plan => 360.0,
            Panel::Terminal => 560.0,
        }
    }

    /// 打开/切换/关闭右侧上下文面板：面板锚定窗口右缘**向左展开**——窗口
    /// 尺寸不变，中间列被压缩（同 Codex/VS Code 的 docked 分栏惯例）。
    /// 面板是 main_row 的 flex 兄弟节点，宽度归零即完全折叠。
    pub(crate) fn set_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        panel: Option<Panel>,
    ) {
        // 在已打开的面板之间切换时保持用户调整后的宽度；
        // 首次打开或关闭时回退到面板默认值。
        let new_delta = if panel.is_some() && self.panel.is_some() {
            self.panel_delta_px
        } else {
            panel
                .map(Self::panel_width_logical)
                .map(|width| width + Self::PANEL_RESIZE_HANDLE_WIDTH)
                .unwrap_or(0.0)
                * window.scale_factor()
        };
        if panel == Some(Panel::Activities) && self.panel != Some(Panel::Activities) {
            self.activities_limit = 100;
            self.activities_scroll.scroll_to_bottom();
        }
        self.panel = panel;
        self.panel_delta_px = new_delta;
        self.panel_resize_origin = None;
        self.panel_resize_initial = new_delta;
        cx.notify();
    }

    pub fn render_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let panel = match self.panel {
            Some(Panel::Workspace) => self.render_workspace_panel(window, cx),
            Some(Panel::Diff) => self.render_diff_panel(window, cx),
            Some(Panel::Detail) => self.render_detail_panel(window, cx),
            Some(Panel::Activities) => self.render_activities_panel(window, cx),
            Some(Panel::Plan) => self.render_plan_panel(window, cx),
            Some(Panel::Terminal) => self.render_terminal_panel(window, cx),
            None => return None,
        };
        let panel_width =
            self.panel_delta_px / window.scale_factor() - Self::PANEL_RESIZE_HANDLE_WIDTH;
        let handle = div()
            .id("panel-resize-handle")
            .debug_selector(|| "panel-resize-handle".into())
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
            .on_drag(PanelResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<PanelResizeDrag>, window, cx| {
                    let Some(origin) = this.panel_resize_origin else {
                        return;
                    };
                    // 向左拖（x 变小）即面板变宽；上限同时受 800 逻辑像素与
                    // 窗口可用宽度约束（保住侧栏与最小中间列宽）
                    let scale = window.scale_factor();
                    let avail = window.bounds().size.width.as_f32() / scale
                        - crate::theme::SIDEBAR_WIDTH
                        - 320.0; // 最小中间列宽
                    let max_w = 800.0f32.min(avail.max(300.0));
                    let next = (this.panel_resize_initial + origin
                        - event.event.position.x.as_f32())
                    .clamp(
                        (300.0 + Self::PANEL_RESIZE_HANDLE_WIDTH) * scale,
                        (max_w + Self::PANEL_RESIZE_HANDLE_WIDTH) * scale,
                    );
                    this.resize_panel(window, cx, next);
                },
            ));
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

    pub(crate) fn resize_panel(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
        width: f32,
    ) {
        self.panel_delta_px = width;
        cx.notify();
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

    pub(crate) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sidebar = cx.theme().sidebar;
        let sidebar_border = cx.theme().sidebar_border;
        let foreground = cx.theme().foreground;
        let sidebar_width = self.sidebar_width_px / window.scale_factor();
        let sidebar_content = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_3()
            .bg(sidebar)
            .border_r_1()
            .border_color(sidebar_border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size_2().rounded_full().bg(cx.theme().primary))
                    .child(
                        Label::new("会话")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("goto-new-session")
                            .small()
                            .icon(IconName::Plus)
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
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.open_settings(window, cx, None);
                            if let Some(i) = this.active_machine() {
                                this.refresh_sessions(i, window, cx);
                            }
                        })),
                ),
            );
        let resize_handle = div()
            .id("sidebar-resize-handle")
            .debug_selector(|| "sidebar-resize-handle".into())
            .w(px(5.0)) // 拖拽手柄宽度：物理命中区域
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
            .on_drag(SidebarResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<SidebarResizeDrag>, window, cx| {
                    let Some(origin) = this.sidebar_resize_origin else {
                        return;
                    };
                    let next = (this.sidebar_resize_initial
                        + (event.event.position.x.as_f32() - origin))
                        .clamp(180.0 * window.scale_factor(), 420.0 * window.scale_factor());
                    this.sidebar_width_px = next;
                    cx.notify();
                },
            ));
        h_flex()
            .w(px(sidebar_width)) // 拖拽解析出的运行时宽度（随 pointer 事件更新）
            .h_full()
            .child(sidebar_content)
            .child(resize_handle)
    }

    pub(crate) fn render_main(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.selected.is_none() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .p_3()
                .child(self.render_center(window, cx))
                .into_any();
        }
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .p_3()
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
            // 中间面板：悬浮按钮叠加在其右缘之上（不占布局空间），
            // 面板向左展开时随中列右缘移动，始终可见
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .child(self.render_main(window, cx))
                    .when(self.selected.is_some(), |wrapper| {
                        wrapper.child(
                            // PRD「悬浮按钮——悬浮于右侧上方，竖向排列」：
                            // 右上角贴边，不占布局空间
                            div()
                                .absolute()
                                .top_2()
                                .right_1()
                                .child(self.render_floating_buttons(window, cx)),
                        )
                    }),
            );
        if let Some(p) = panel {
            main_row = main_row.child(p);
        }
        let mut root = v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .child(title_bar)
            .child(main_row);
        if self.settings.show {
            root = root.child(self.render_settings_overlay(cx));
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
pub(crate) async fn run_engine_on_tokio<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::ws::runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.ok()
}
