//! 后台轮询：按各视图的刷新周期拉取 Server 数据（docs/DESIGN.md「应用」）。
//!
//! 单任务节拍内按到期时间触发各视图刷新；网络请求期间不持状态锁。
//! 设置类数据（机器/agents、编排智能体、计划、快捷指令、技能）不做定时刷新，
//! 由视图打开时调用 [`refresh_new_session`] / [`refresh_interaction`] / [`refresh_settings`] 实时拉取。

use std::time::{Duration, Instant};

use amux_common::domain::ContentBlock;

use crate::client::Client;
use crate::state::{
    ConnectionStatus, Core, ListEntry, OpenTarget, SettingsTab, SharedCore, SidePanel,
    ACTIVITIES_INTERVAL, HISTORY_INTERVAL, ONGOING_INTERVAL, OPTIONS_INTERVAL, PLAN_INTERVAL,
    SESSION_LIST_INTERVAL, TERMINAL_INTERVAL,
};

/// 连接重试间隔。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// 单次节拍：按需刷新各视图。
pub async fn tick(core: SharedCore) {
    let (client, status, open, side_panel) = {
        let core = core.lock();
        (
            core.client.clone(),
            core.status.clone(),
            core.open.clone(),
            core.side_panel,
        )
    };
    let Some(client) = client else {
        return;
    };

    // 连接检查
    if status != ConnectionStatus::Online {
        if core_due(&core, |last| last.list, RECONNECT_INTERVAL) {
            match client.ping().await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.status = ConnectionStatus::Online;
                    core.last.list = None;
                    // 连接建立后重取当前视图的常驻数据（此前的拉取可能已失败）
                    core.loaded_view = None;
                }
                Err(error) => {
                    let mut core = core.lock();
                    core.status = ConnectionStatus::Failed(error);
                    core.last.list = Some(Instant::now());
                }
            }
        }
        return;
    }

    if core_due(&core, |last| last.list, SESSION_LIST_INTERVAL) {
        refresh_list(&client, &core).await;
    }
    if let Some(target) = open {
        refresh_open(&client, &core, &target, side_panel).await;
    }
}

fn core_due(
    core: &SharedCore,
    pick: impl Fn(&crate::state::Ticks) -> Option<Instant>,
    interval: Duration,
) -> bool {
    let core = core.lock();
    core.due(pick(&core.last), interval)
}

/// 会话列表：普通会话 + 工作流会话（工作流会话内含关联普通会话），按最近活跃排序。
async fn refresh_list(client: &Client, core: &SharedCore) {
    let (limit, offset) = {
        let core = core.lock();
        (core.list_limit.max(20), 0usize)
    };
    let _ = offset;
    let sessions = client.sessions(limit, offset).await;
    let workflows = client.workflows(limit, offset).await;
    let (sessions, workflows) = match (sessions, workflows) {
        (Ok(sessions), Ok(workflows)) => (sessions, workflows),
        (Err(error), _) | (_, Err(error)) => {
            let mut core = core.lock();
            core.last.list = Some(Instant::now());
            core.status = ConnectionStatus::Failed(error);
            return;
        }
    };
    let mut entries: Vec<ListEntry> = Vec::new();
    entries.extend(sessions.sessions.into_iter().map(ListEntry::Session));
    entries.extend(workflows.workflows.into_iter().map(ListEntry::Workflow));
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at()));
    let mut core = core.lock();
    core.last.list = Some(Instant::now());
    core.entries = entries;
}

/// 打开会话的视图数据：按各自周期刷新（会话详情与工作目录视图只在打开时刷新一次，
/// 由 UI 打开面板时直接拉取，见 docs/DESIGN.md「应用」）。
async fn refresh_open(
    client: &Client,
    core: &SharedCore,
    target: &OpenTarget,
    side_panel: Option<SidePanel>,
) {
    let activities_open = side_panel == Some(SidePanel::Activities);
    let plan_open = side_panel == Some(SidePanel::Plan);
    let terminal_open = side_panel == Some(SidePanel::Terminal);

    let (
        due_history,
        due_ongoing,
        due_activities,
        due_plan,
        due_options,
        due_terminal,
        terminal,
        cursor,
    ) = {
        let core = core.lock();
        (
            core.due(core.last.history, HISTORY_INTERVAL),
            core.due(core.last.ongoing, ONGOING_INTERVAL),
            core.due(core.last.activities, ACTIVITIES_INTERVAL),
            core.due(core.last.plan, PLAN_INTERVAL),
            core.due(core.last.options, OPTIONS_INTERVAL),
            core.due(core.last.terminal, TERMINAL_INTERVAL),
            core.view.detail.active_terminal.clone(),
            core.last.terminal_cursor,
        )
    };

    match target {
        OpenTarget::Session(id) => {
            if due_history {
                if let Ok(session) = client.session(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.session = Some(session);
                    }
                }
                if let Ok(page) = client.history(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.history = page.items;
                        core.view.detail.history_has_more = page.has_more;
                    }
                }
                core.lock().last.history = Some(Instant::now());
            }
            if due_activities && activities_open {
                if let Ok(page) = client.activities(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.activities = page.activities;
                    }
                }
                core.lock().last.activities = Some(Instant::now());
            }
            if due_plan && plan_open {
                if let Ok(entries) = client.plan(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.plan = entries;
                    }
                }
                core.lock().last.plan = Some(Instant::now());
            }
            // 会话选项与斜杠命令随交互视图常驻，与计划面板是否打开无关，按自身周期节流
            if due_options {
                if let Ok(options) = client.config_options(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.config_options = options;
                    }
                }
                if let Ok(commands) = client.slash_commands(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.slash_commands = commands;
                    }
                }
                core.lock().last.options = Some(Instant::now());
            }
            if due_ongoing {
                if let Ok(activity) = client.ongoing_activity(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.ongoing = activity;
                    }
                }
                core.lock().last.ongoing = Some(Instant::now());
            }
            if due_terminal && terminal_open {
                // 终端列表无活动终端时也要刷新（重开会话后列出已有终端、退出状态等）
                if let Ok(terminals) = client.terminals(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.terminals = terminals;
                    }
                }
                if let Some(terminal_id) = terminal {
                    if let Ok(output) = client.terminal_output(id, &terminal_id, Some(cursor)).await
                    {
                        let bytes = base64::Engine::decode(
                            &base64::engine::general_purpose::STANDARD,
                            &output.data,
                        )
                        .unwrap_or_default();
                        let mut core = core.lock();
                        if core.open.as_ref() == Some(target)
                            && core.view.detail.active_terminal.as_deref() == Some(&terminal_id)
                        {
                            if output.truncated {
                                core.view.detail.terminal_output.clear();
                            }
                            core.view.detail.terminal_output.extend_from_slice(&bytes);
                            core.last.terminal_cursor = output.next_cursor;
                        }
                    }
                }
                let mut core = core.lock();
                core.last.terminal = Some(Instant::now());
            }
        }
        OpenTarget::Workflow(id) => {
            if due_history {
                if let Ok(workflow) = client.workflow(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.workflow = Some(workflow);
                    }
                }
                if let Ok(items) = client.workflow_history(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.history = items;
                    }
                }
                core.lock().last.history = Some(Instant::now());
            }
            if due_activities && activities_open {
                if let Ok(activities) = client.workflow_activities(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.activities = activities;
                    }
                }
                core.lock().last.activities = Some(Instant::now());
            }
            if due_ongoing {
                if let Ok(activity) = client.workflow_ongoing_activity(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.ongoing = activity;
                    }
                }
                core.lock().last.ongoing = Some(Instant::now());
            }
        }
    }
}

// ---------- 视图打开时的实时拉取 ----------
//
// 设置类数据不做定时刷新，由视图打开时拉取一次（docs/DESIGN.md「应用」各视图小节）。

/// 新建会话视图：机器、agents、常用工作目录，以及工作流模式下用到的编排智能体配置与已保存计划。
pub async fn refresh_new_session(client: &Client, core: &SharedCore) {
    refresh_machines(client, core).await;
    refresh_orchestrator(client, core).await;
    refresh_plans(client, core).await;
    if let Ok(recent) = client.recent_workspaces().await {
        core.lock().recent_workspaces = recent;
    }
}

/// 会话交互视图常驻数据：机器/agents（可用性标记）、编排智能体配置（工作流会话）
/// 与快捷指令（输入区按钮）。
pub async fn refresh_interaction(client: &Client, core: &SharedCore) {
    refresh_machines(client, core).await;
    refresh_orchestrator(client, core).await;
    refresh_quick_commands(client, core).await;
}

/// 设置浮窗当前分类的配置数据。
pub async fn refresh_settings(client: &Client, core: &SharedCore, tab: SettingsTab) {
    match tab {
        SettingsTab::Connection => {}
        SettingsTab::Machines => refresh_machines(client, core).await,
        SettingsTab::Orchestrator => refresh_orchestrator(client, core).await,
        SettingsTab::QuickCommands => refresh_quick_commands(client, core).await,
        SettingsTab::Skills => refresh_skills(client, core).await,
        SettingsTab::WorkflowPlans => refresh_plans(client, core).await,
    }
}

/// 机器与各机器上的 agents。
async fn refresh_machines(client: &Client, core: &SharedCore) {
    let Ok(machines) = client.machines().await else {
        return;
    };
    let mut agents = Vec::new();
    for machine in &machines {
        let list = client.agents(&machine.name).await.unwrap_or_default();
        agents.push((machine.name.clone(), list));
    }
    let mut core = core.lock();
    core.settings.machines = machines;
    core.settings.agents = agents;
}

async fn refresh_orchestrator(client: &Client, core: &SharedCore) {
    if let Ok(orchestrator) = client.orchestrator().await {
        core.lock().settings.orchestrator = orchestrator;
    }
}

async fn refresh_plans(client: &Client, core: &SharedCore) {
    if let Ok(plans) = client.workflow_plans().await {
        core.lock().settings.plans = plans;
    }
}

async fn refresh_quick_commands(client: &Client, core: &SharedCore) {
    if let Ok(commands) = client.quick_commands().await {
        core.lock().settings.quick_commands = commands;
    }
}

async fn refresh_skills(client: &Client, core: &SharedCore) {
    if let Ok(skills) = client.skills().await {
        core.lock().settings.skills = skills;
    }
}

// ---------- 用户动作（UI 调用） ----------

/// 打开普通会话：重置视图与轮询节拍。
pub fn open_session(core: &mut Core, id: &str) {
    core.open = Some(OpenTarget::Session(id.to_string()));
    core.view = Default::default();
    core.last = Default::default();
}

/// 打开工作流会话。
pub fn open_workflow(core: &mut Core, id: &str) {
    core.open = Some(OpenTarget::Workflow(id.to_string()));
    core.view = Default::default();
    core.last = Default::default();
}

/// 发送指令（普通会话或工作流会话）：文本与附件内容块一并作为用户输入。
pub async fn send_prompt(
    client: &Client,
    target: &OpenTarget,
    input: Vec<ContentBlock>,
) -> Result<(), String> {
    match target {
        OpenTarget::Session(id) => client.prompt(id, input).await,
        OpenTarget::Workflow(id) => client.prompt_workflow(id, input).await,
    }
}

/// 取消进行中的工作。
pub async fn cancel(client: &Client, target: &OpenTarget) -> Result<(), String> {
    match target {
        OpenTarget::Session(id) => client.cancel(id).await,
        // 工作流会话取消采用用户消息方式（docs/PRD.md）
        OpenTarget::Workflow(id) => {
            client
                .prompt_workflow(
                    id,
                    vec![ContentBlock::Text {
                        text: "取消当前进行中的全部工作".to_string(),
                    }],
                )
                .await
        }
    }
}

/// 删除会话（工作流会话连同关联普通会话）。
pub async fn delete(client: &Client, entry: &ListEntry) -> Result<(), String> {
    match entry {
        ListEntry::Session(session) => client.delete_session(&session.id).await,
        ListEntry::Workflow(workflow) => client.delete_workflow(&workflow.id).await,
    }
}
