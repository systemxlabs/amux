//! 后台轮询：按各视图的刷新周期拉取 Server 数据（docs/DESIGN.md「应用」）。
//!
//! 单任务节拍内按到期时间触发各视图刷新；网络请求期间不持状态锁。

use std::time::{Duration, Instant};

use amux_common::domain::ContentBlock;

use crate::client::Client;
use crate::state::{
    ConnectionStatus, Core, ListEntry, OpenTarget, SharedCore, SidePanel, ACTIVITIES_INTERVAL,
    HISTORY_INTERVAL, ONGOING_INTERVAL, PLAN_INTERVAL, SESSION_LIST_INTERVAL, SETTINGS_INTERVAL,
    TERMINAL_INTERVAL,
};

/// 连接重试间隔。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// 单次节拍：按需刷新各视图。
pub async fn tick(core: SharedCore) {
    let (client, status, last_list, last_settings, open, settings_open, settings_tab, side_panel) = {
        let core = core.lock();
        (
            core.client.clone(),
            core.status.clone(),
            core.last.list,
            core.last.settings,
            core.open.clone(),
            core.settings_open,
            core.settings_tab,
            core.side_panel,
        )
    };
    let Some(client) = client else {
        return;
    };

    // 连接检查
    if status != ConnectionStatus::Online {
        let due = {
            let core = core.lock();
            core.due(core.last.list, RECONNECT_INTERVAL)
        };
        if due {
            match client.ping().await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.status = ConnectionStatus::Online;
                    core.last.list = None;
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

    if core_due(&core, |last| last.list, SESSION_LIST_INTERVAL, last_list) {
        refresh_list(&client, &core).await;
    }
    // 机器/agent 与编排智能体配置：新建会话视图常驻需要，按周期节流刷新
    if core_due(
        &core,
        |last| last.settings,
        SETTINGS_INTERVAL,
        last_settings,
    ) {
        refresh_config(&client, &core).await;
        if settings_open {
            refresh_settings(&client, &core, settings_tab).await;
        }
    }
    if let Some(target) = open {
        refresh_open(&client, &core, &target, side_panel).await;
    }
}

fn core_due(
    core: &SharedCore,
    pick: impl Fn(&crate::state::Ticks) -> Option<Instant>,
    interval: Duration,
    cached: Option<Instant>,
) -> bool {
    let core = core.lock();
    let last = pick(&core.last).or(cached);
    core.due(last, interval)
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
    let recent = client.recent_workspaces().await.ok();
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
    if let Some(recent) = recent {
        core.recent_workspaces = recent;
    }
}

/// 打开会话的视图数据：按各自周期刷新。
async fn refresh_open(
    client: &Client,
    core: &SharedCore,
    target: &OpenTarget,
    side_panel: Option<SidePanel>,
) {
    let activities_open = side_panel == Some(SidePanel::Activities);
    let plan_open = side_panel == Some(SidePanel::Plan);
    let detail_open = side_panel == Some(SidePanel::Detail);
    let terminal_open = side_panel == Some(SidePanel::Terminal);

    let (due_history, due_ongoing, due_activities, due_plan, due_terminal, terminal, cursor) = {
        let core = core.lock();
        (
            core.due(core.last.history, HISTORY_INTERVAL),
            core.due(core.last.ongoing, ONGOING_INTERVAL),
            core.due(core.last.activities, ACTIVITIES_INTERVAL),
            core.due(core.last.plan, PLAN_INTERVAL),
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
                        core.last.history = Some(Instant::now());
                    }
                }
            }
            if due_activities && activities_open {
                if let Ok(page) = client.activities(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.activities = page.activities;
                        core.last.activities = Some(Instant::now());
                    }
                }
            }
            if due_plan && plan_open {
                if let Ok(entries) = client.plan(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.plan = entries;
                        core.last.plan = Some(Instant::now());
                    }
                }
            }
            if due_plan {
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
            }
            if due_plan && detail_open {
                if let Ok(info) = client.context(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.context_size = info.context_size;
                        core.view.detail.context_window_size = info.context_window_size;
                    }
                }
            }
            if due_ongoing {
                if let Ok(activity) = client.ongoing_activity(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.ongoing = activity;
                        core.last.ongoing = Some(Instant::now());
                    }
                }
            }
            if due_terminal && terminal_open {
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
                    if let Ok(terminals) = client.terminals(id).await {
                        let mut core = core.lock();
                        if core.open.as_ref() == Some(target) {
                            core.view.detail.terminals = terminals;
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
                        core.last.history = Some(Instant::now());
                    }
                }
            }
            if due_activities && activities_open {
                if let Ok(activities) = client.workflow_activities(id, 200, 0).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.activities = activities;
                        core.last.activities = Some(Instant::now());
                    }
                }
            }
            if due_ongoing {
                if let Ok(activity) = client.workflow_ongoing_activity(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.ongoing = activity;
                        core.last.ongoing = Some(Instant::now());
                    }
                }
            }
        }
    }
}

/// 机器/agent 列表与编排智能体配置：新建会话视图与设置浮窗共用。
async fn refresh_config(client: &Client, core: &SharedCore) {
    let machines = client.machines().await.ok();
    let orchestrator = client.orchestrator().await.ok();
    let mut agents = Vec::new();
    if let Some(machines) = &machines {
        for machine in machines {
            let list = client.agents(&machine.name).await.unwrap_or_default();
            agents.push((machine.name.clone(), list));
        }
    }
    let mut core = core.lock();
    if let Some(machines) = machines {
        core.settings.machines = machines;
        core.settings.agents = agents;
    }
    if let Some(orchestrator) = orchestrator {
        core.settings.orchestrator = orchestrator;
    }
    core.last.settings = Some(Instant::now());
}

/// 设置面板当前分类的列表类配置（机器/agent 与编排智能体配置由 `refresh_config` 刷新）。
async fn refresh_settings(client: &Client, core: &SharedCore, tab: crate::state::SettingsTab) {
    use crate::state::SettingsTab;
    match tab {
        SettingsTab::Skills => {
            if let Ok(skills) = client.skills().await {
                core.lock().settings.skills = skills;
            }
        }
        SettingsTab::WorkflowPlans => {
            if let Ok(plans) = client.workflow_plans().await {
                core.lock().settings.plans = plans;
            }
        }
        SettingsTab::QuickCommands => {
            if let Ok(commands) = client.quick_commands().await {
                core.lock().settings.quick_commands = commands;
            }
        }
        SettingsTab::Connection | SettingsTab::Machines | SettingsTab::Orchestrator => {}
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

/// 发送指令（普通会话或工作流会话）。
pub async fn send_prompt(client: &Client, target: &OpenTarget, text: &str) -> Result<(), String> {
    let input = vec![ContentBlock::Text {
        text: text.to_string(),
    }];
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
