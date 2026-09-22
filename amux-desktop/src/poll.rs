//! 后台轮询：按各视图的刷新周期拉取 Server 数据（docs/DESIGN.md「应用」）。
//!
//! 单任务节拍内按到期时间触发各视图刷新；网络请求期间不持状态锁。
//! 设置类数据（机器/agents、内置智能体、计划、快捷指令、技能）不做定时刷新，
//! 由视图打开时调用 [`refresh_new_session`] / [`refresh_interaction`] / [`refresh_settings`] 实时拉取。

use std::time::{Duration, Instant};

use amux_common::domain::{Activity, ContentBlock, HistoryItem};

use crate::client::Client;
use crate::state::{
    set_terminals, ConnectionStatus, Core, ListEntry, OpenTarget, Paging, SettingsTab, SharedCore,
    SidePanel, ACTIVITIES_INTERVAL, HISTORY_INTERVAL, ONGOING_INTERVAL, PLAN_INTERVAL,
    SESSION_LIST_INTERVAL,
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

/// 会话列表刷新：重取已加载窗口（普通会话与工作流会话各 `list_loaded` 条）。
///
/// 窗口贴着最新一端，删改与排序变化都在整窗重取后自然生效（docs/DESIGN.md
/// 「会话列表刷新机制」）。
async fn refresh_list(client: &Client, core: &SharedCore) {
    let limit = {
        let core = core.lock();
        core.list_loaded.max(core.list_paging.page_size)
    };
    let sessions = client.sessions(limit, 0, None).await;
    let workflows = client.workflows(limit, 0, None).await;
    let (sessions, workflows) = match (sessions, workflows) {
        (Ok(sessions), Ok(workflows)) => (sessions, workflows),
        (Err(error), _) | (_, Err(error)) => {
            let mut core = core.lock();
            core.last.list = Some(Instant::now());
            core.status = ConnectionStatus::Failed(error);
            return;
        }
    };
    let has_older = sessions.has_more || workflows.has_more;
    let mut entries: Vec<ListEntry> = Vec::new();
    entries.extend(sessions.sessions.into_iter().map(ListEntry::Session));
    entries.extend(workflows.workflows.into_iter().map(ListEntry::Workflow));
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at()));
    let mut core = core.lock();
    core.last.list = Some(Instant::now());
    core.entries = entries;
    core.list_loaded = limit;
    core.list_paging.has_older = has_older;
}

/// 会话列表更早一页：两个来源各取一页并合并到窗口（docs/DESIGN.md「会话列表滚动机制」）。
pub async fn load_older_sessions(client: &Client, core: &SharedCore) {
    let (offset, limit) = {
        let mut core = core.lock();
        let loaded = core.list_loaded;
        match begin_older_page(&mut core.list_paging, loaded) {
            Some(page) => page,
            None => return,
        }
    };
    let sessions = client.sessions(limit, offset, None).await;
    let workflows = client.workflows(limit, offset, None).await;
    let (sessions, workflows) = match (sessions, workflows) {
        (Ok(sessions), Ok(workflows)) => (sessions, workflows),
        _ => {
            core.lock().list_paging.loading_older = false;
            return;
        }
    };
    let has_older = sessions.has_more || workflows.has_more;
    let page: Vec<ListEntry> = sessions
        .sessions
        .into_iter()
        .map(ListEntry::Session)
        .chain(workflows.workflows.into_iter().map(ListEntry::Workflow))
        .collect();
    let mut core = core.lock();
    core.list_paging.loading_older = false;
    core.list_paging.has_older = has_older;
    core.list_loaded = offset + limit;
    for entry in page {
        // 已加载过的条目（翻页之间发生更新或位移）按 id 覆盖，避免重复
        match core.entries.iter_mut().find(|item| item.id() == entry.id()) {
            Some(existing) => *existing = entry,
            None => core.entries.push(entry),
        }
    }
    core.entries
        .sort_by_key(|entry| std::cmp::Reverse(entry.updated_at()));
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

    let (due_history, due_ongoing, due_activities, due_plan) = {
        let core = core.lock();
        (
            core.due(core.last.history, HISTORY_INTERVAL),
            core.due(core.last.ongoing, ONGOING_INTERVAL),
            core.due(core.last.activities, ACTIVITIES_INTERVAL),
            core.due(core.last.plan, PLAN_INTERVAL),
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
                refresh_history(client, core, target).await;
                core.lock().last.history = Some(Instant::now());
            }
            if due_activities && activities_open {
                refresh_activities(client, core, target).await;
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
            if due_ongoing {
                if let Ok(activity) = client.ongoing_activity(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.ongoing = activity;
                    }
                }
                core.lock().last.ongoing = Some(Instant::now());
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
                refresh_history(client, core, target).await;
                core.lock().last.history = Some(Instant::now());
            }
            if due_activities && activities_open {
                refresh_activities(client, core, target).await;
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

// ---------- 滚动分页 ----------
//
// 三个列表（会话列表 / 对话历史 / 活动列表）共用同一套分页模型：窗口贴着「最新」一端，
// 页大小由视图按面板可视高度写入，随滚动向更早方向按页扩展，并预取相邻一页作为缓冲
// （docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」「活动列表滚动机制」）。
// 刷新只针对最新一段：更早的已加载条目不再改动。

/// 开始拉取更早一页：返回（偏移, 条数）；没有更早条目或已有在途拉取时为 None。
fn begin_older_page(paging: &mut Paging, loaded: usize) -> Option<(usize, usize)> {
    if !paging.has_older || paging.loading_older {
        return None;
    }
    paging.loading_older = true;
    Some((loaded, paging.page_size.max(1)))
}

/// 刷新时的拉取条数：至少覆盖已加载窗口，保证刷新后窗口仍是连续的一段。
fn refresh_limit(loaded: usize, page_size: usize) -> usize {
    loaded.max(page_size).max(1)
}

/// 把最新一页并入窗口（升序展示、最新在末尾）。
///
/// 页里的条目按标识替换窗口中的同一条目（流式输出会改内容，同一条目的位置也可能变到最新端），
/// 其余条目保持不动、且都排在页之前：页是「最新的一段」，窗口只向更新的一端扩展，不会出现缺口。
pub fn merge_newest<T>(
    items: &mut Vec<T>,
    paging: &mut Paging,
    page: Vec<T>,
    page_has_more: bool,
    id: fn(&T) -> &str,
) {
    paging.has_older = page_has_more;
    if items.is_empty() {
        *items = page;
        return;
    }
    let page_ids: Vec<String> = page.iter().map(|item| id(item).to_string()).collect();
    items.retain(|item| !page_ids.iter().any(|page_id| page_id == id(item)));
    items.extend(page);
}

/// 更早一页插到窗口前面（升序展示、最新在末尾）。
///
/// 插入会让可视内容整体下移，因此记下插入条数为位移量，
/// 由视图在下一节拍把首条目滚回原位（docs/DESIGN.md 各滚动机制小节）。
pub fn prepend_older<T>(items: &mut Vec<T>, paging: &mut Paging, page: Vec<T>) {
    paging.shift = Some(page.len());
    let mut merged = page;
    merged.append(items);
    *items = merged;
}

/// 对话历史刷新：拉取最新一段并与窗口合并（普通会话与工作流会话）。
async fn refresh_history(client: &Client, core: &SharedCore, target: &OpenTarget) {
    let limit = {
        let core = core.lock();
        refresh_limit(
            core.view.detail.history.len(),
            core.view.detail.history_paging.page_size,
        )
    };
    let page = match target {
        OpenTarget::Session(id) => client.history(id, limit, 0).await,
        OpenTarget::Workflow(id) => client.workflow_history(id, limit, 0).await,
    };
    let Ok(page) = page else { return };
    let mut core = core.lock();
    if core.open.as_ref() != Some(target) {
        return;
    }
    let detail = &mut core.view.detail;
    merge_newest(
        &mut detail.history,
        &mut detail.history_paging,
        page.items,
        page.has_more,
        HistoryItem::id,
    );
}

/// 活动历史刷新：拉取最新一段并与窗口合并（普通会话与工作流会话）。
async fn refresh_activities(client: &Client, core: &SharedCore, target: &OpenTarget) {
    let limit = {
        let core = core.lock();
        refresh_limit(
            core.view.detail.activities.len(),
            core.view.detail.activities_paging.page_size,
        )
    };
    let page = match target {
        OpenTarget::Session(id) => client.activities(id, limit, 0).await,
        OpenTarget::Workflow(id) => client.workflow_activities(id, limit, 0).await,
    };
    let Ok(page) = page else { return };
    let mut core = core.lock();
    if core.open.as_ref() != Some(target) {
        return;
    }
    let detail = &mut core.view.detail;
    merge_newest(
        &mut detail.activities,
        &mut detail.activities_paging,
        page.activities,
        page.has_more,
        Activity::id,
    );
}

/// 对话历史更早一页：插到窗口前面，并锚定滚动位置。
pub async fn load_older_history(client: &Client, core: &SharedCore, target: &OpenTarget) {
    let (offset, limit) = {
        let mut core = core.lock();
        let loaded = core.view.detail.history.len();
        match begin_older_page(&mut core.view.detail.history_paging, loaded) {
            Some(page) => page,
            None => return,
        }
    };
    let page = match target {
        OpenTarget::Session(id) => client.history(id, limit, offset).await,
        OpenTarget::Workflow(id) => client.workflow_history(id, limit, offset).await,
    };
    let Ok(page) = page else {
        core.lock().view.detail.history_paging.loading_older = false;
        return;
    };
    let mut core = core.lock();
    core.view.detail.history_paging.loading_older = false;
    if core.open.as_ref() != Some(target) {
        return;
    }
    let detail = &mut core.view.detail;
    detail.history_paging.has_older = page.has_more;
    prepend_older(&mut detail.history, &mut detail.history_paging, page.items);
}

/// 活动历史更早一页：插到窗口前面，并锚定滚动位置。
pub async fn load_older_activities(client: &Client, core: &SharedCore, target: &OpenTarget) {
    let (offset, limit) = {
        let mut core = core.lock();
        let loaded = core.view.detail.activities.len();
        match begin_older_page(&mut core.view.detail.activities_paging, loaded) {
            Some(page) => page,
            None => return,
        }
    };
    let page = match target {
        OpenTarget::Session(id) => client.activities(id, limit, offset).await,
        OpenTarget::Workflow(id) => client.workflow_activities(id, limit, offset).await,
    };
    let Ok(page) = page else {
        core.lock().view.detail.activities_paging.loading_older = false;
        return;
    };
    let mut core = core.lock();
    core.view.detail.activities_paging.loading_older = false;
    if core.open.as_ref() != Some(target) {
        return;
    }
    let detail = &mut core.view.detail;
    detail.activities_paging.has_older = page.has_more;
    prepend_older(
        &mut detail.activities,
        &mut detail.activities_paging,
        page.activities,
    );
}

// ---------- 视图打开时的实时拉取 ----------
//
// 设置类数据不做定时刷新，由视图打开时拉取一次（docs/DESIGN.md「应用」各视图小节）。

/// 新建视图打开时刷新，不依赖当前表单模式。
pub async fn refresh_new_session(client: &Client, core: &SharedCore) {
    refresh_machines(client, core).await;
    if let Ok(recent) = client.recent_workspaces().await {
        core.lock().recent_workspaces = recent;
    }
    refresh_plans(client, core).await;
    refresh_projects(client, core).await;
}

/// 进入工作流模式时检查内置智能体是否已配置。
pub async fn refresh_workflow_setup(client: &Client, core: &SharedCore) {
    refresh_orchestrator(client, core).await;
}

/// 会话交互视图常驻数据：机器/agents（可用性标记）、内置智能体配置（工作流会话）、
/// 快捷指令（输入区按钮）与普通会话的选项和斜杠命令。
pub async fn refresh_interaction(client: &Client, core: &SharedCore) {
    tokio::join!(
        refresh_machines(client, core),
        refresh_orchestrator(client, core),
        refresh_quick_commands(client, core),
        refresh_session_controls(client, core),
        refresh_terminal_list(client, core),
    );
}

/// 终端视图打开时从 Server 拉取一次终端列表，并移除服务端已消失的终端；
/// 不做周期性轮询（docs/DESIGN.md「终端视图」）。
async fn refresh_terminal_list(client: &Client, core: &SharedCore) {
    let id = {
        let open = core.lock();
        let Some(OpenTarget::Session(id)) = open.open.clone() else {
            return;
        };
        if open.side_panel != Some(SidePanel::Terminal) {
            return;
        }
        id
    };
    if let Ok(terminals) = client.terminals(&id).await {
        let mut core = core.lock();
        if core.open.as_ref() == Some(&OpenTarget::Session(id)) {
            set_terminals(&mut core, terminals);
        }
    }
}

/// 普通会话的会话选项与斜杠命令只在打开会话时拉取一次，不定时刷新；
/// agent 侧后续变更由 `session/update` 推送经 Server 落地后随重开视图读到。
async fn refresh_session_controls(client: &Client, core: &SharedCore) {
    let Some(OpenTarget::Session(id)) = core.lock().open.clone() else {
        return;
    };
    // 查询选项会惰性创建或恢复 ACP 会话，随后再读取其已发布的斜杠命令
    if let Ok(options) = client.config_options(&id).await {
        let mut core = core.lock();
        if core.open.as_ref() == Some(&OpenTarget::Session(id.clone())) {
            core.view.detail.config_options = options;
        }
    }
    if core.lock().open.as_ref() != Some(&OpenTarget::Session(id.clone())) {
        return;
    }
    if let Ok(commands) = client.slash_commands(&id).await {
        let mut core = core.lock();
        if core.open.as_ref() == Some(&OpenTarget::Session(id)) {
            core.view.detail.slash_commands = commands;
        }
    }
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
        SettingsTab::Projects => refresh_projects(client, core).await,
    }
}

/// 机器与各机器上的 agents。
pub async fn refresh_machines(client: &Client, core: &SharedCore) {
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
        let mut core = core.lock();
        core.settings.orchestrator = orchestrator;
        core.settings.orchestrator_loaded = true;
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

async fn refresh_projects(client: &Client, core: &SharedCore) {
    if let Ok(projects) = client.projects().await {
        core.lock().settings.projects = projects;
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
