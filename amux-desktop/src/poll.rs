//! 后台轮询：按各视图的刷新周期拉取 Server 数据（docs/DESIGN.md「应用」）。
//!
//! 单任务节拍内按到期时间触发各视图刷新；网络请求期间不持状态锁。

use std::time::{Duration, Instant};

use amux_common::domain::ContentBlock;

use crate::client::Client;
use crate::state::{
    ConnectionStatus, Core, ListEntry, OpenTarget, SharedCore, SidePanel, ACTIVITIES_INTERVAL,
    CONTEXT_INTERVAL, HISTORY_INTERVAL, ONGOING_INTERVAL, OPTIONS_INTERVAL, PLAN_INTERVAL,
    SESSION_LIST_INTERVAL, SETTINGS_INTERVAL, TERMINAL_INTERVAL,
};

/// 连接重试间隔。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// 单次节拍：按需刷新各视图。
pub async fn tick(core: SharedCore) {
    let (client, status, open, settings_open, settings_tab, side_panel) = {
        let core = core.lock();
        (
            core.client.clone(),
            core.status.clone(),
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
        if core_due(&core, |last| last.list, RECONNECT_INTERVAL) {
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

    if core_due(&core, |last| last.list, SESSION_LIST_INTERVAL) {
        refresh_list(&client, &core).await;
    }
    // 机器/agent 与列表类配置：新建会话视图与交互视图常驻需要，按周期节流刷新
    if core_due(&core, |last| last.settings, SETTINGS_INTERVAL) {
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
) -> bool {
    let core = core.lock();
    let last = pick(&core.last);
    core.due(last, interval)
}

/// 会话列表：普通会话 + 工作流会话（工作流会话内含关联普通会话），按最近活跃排序。
async fn refresh_list(client: &Client, core: &SharedCore) {
    let limit = core.lock().list_limit.max(20);
    let sessions = client.sessions(limit, 0).await;
    let workflows = client.workflows(limit, 0).await;
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

    let (
        due_history,
        due_ongoing,
        due_activities,
        due_plan,
        due_options,
        due_context,
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
            core.due(core.last.context, CONTEXT_INTERVAL),
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
            // 会话选项与斜杠命令随交互视图常驻：与计划面板是否打开无关，按自身周期节流
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
            if due_context && detail_open {
                if let Ok(info) = client.context(id).await {
                    let mut core = core.lock();
                    if core.open.as_ref() == Some(target) {
                        core.view.detail.context_size = info.context_size;
                        core.view.detail.context_window_size = info.context_window_size;
                    }
                }
                core.lock().last.context = Some(Instant::now());
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

/// 机器/agent、编排智能体、工作流计划与快捷指令：新建会话视图、交互视图与设置浮窗共用。
async fn refresh_config(client: &Client, core: &SharedCore) {
    let machines = client.machines().await.ok();
    let orchestrator = client.orchestrator().await.ok();
    let plans = client.workflow_plans().await.ok();
    let quick_commands = client.quick_commands().await.ok();
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
    if let Some(plans) = plans {
        core.settings.plans = plans;
    }
    if let Some(commands) = quick_commands {
        core.settings.quick_commands = commands;
    }
    core.last.settings = Some(Instant::now());
}

/// 设置面板当前分类的列表类配置（其余配置数据由 `refresh_config` 常驻刷新）。
async fn refresh_settings(client: &Client, core: &SharedCore, tab: crate::state::SettingsTab) {
    use crate::state::SettingsTab;
    match tab {
        SettingsTab::Skills => {
            if let Ok(skills) = client.skills().await {
                core.lock().settings.skills = skills;
            }
        }
        SettingsTab::Connection
        | SettingsTab::Machines
        | SettingsTab::Orchestrator
        | SettingsTab::WorkflowPlans
        | SettingsTab::QuickCommands => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 端点桩的响应体：只覆盖会话视图用到的端点，其余端点返回空对象（调用方按错误忽略）。
    fn stub(path: &str) -> &'static str {
        match path {
            "/sessions" => r#"{"sessions":[],"hasMore":false}"#,
            "/workflows" => r#"{"workflows":[],"hasMore":false}"#,
            "/sessions/s1" => {
                r#"{"id":"s1","machine":"pc","agent":"codex","title":"t","state":"idle","workspace":"/tmp","worktreeDir":"","createdAt":1,"updatedAt":1}"#
            }
            "/sessions/s1/history" => r#"{"items":[],"hasMore":false}"#,
            "/sessions/s1/plan" => r#"{"entries":[]}"#,
            "/sessions/s1/context" => r#"{"contextSize":1,"contextWindowSize":2}"#,
            "/sessions/s1/config_options" => {
                r#"{"options":[{"id":"model","name":"模型","type":"select","current_value":"a","options":[{"value":"a","name":"A"}]}]}"#
            }
            "/sessions/s1/slash_commands" => {
                r#"{"commands":[{"name":"goal","description":"目标"}]}"#
            }
            _ => "{}",
        }
    }

    /// 启动记录请求路径的端点桩，返回 Server 地址与请求记录。
    async fn start_stub() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::http::StatusCode;
        use axum::Router;

        let hits: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&hits);
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let hits = Arc::clone(&recorded);
            async move {
                let path = request.uri().path().to_string();
                let body = stub(&path);
                hits.lock().unwrap().push(path);
                (StatusCode::OK, body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    /// 打开会话 s1 的 Core：状态置为在线（跳过连接检查），所有节拍自零开始。
    fn opened_core(server: String) -> SharedCore {
        let connection = crate::config::Connection {
            server,
            token: "tk".into(),
        };
        let mut core = Core::new(connection);
        core.status = ConnectionStatus::Online;
        core.open = Some(OpenTarget::Session("s1".to_string()));
        Arc::new(parking_lot::Mutex::new(core))
    }

    fn hit_count(hits: &Arc<std::sync::Mutex<Vec<String>>>, path: &str) -> usize {
        hits.lock()
            .unwrap()
            .iter()
            .filter(|hit| *hit == path)
            .count()
    }

    /// 计划面板未打开时会话选项与斜杠命令也必须按自身周期节流（回归：曾随 250ms 节拍重发）。
    #[tokio::test]
    async fn options_and_slash_commands_are_throttled() {
        let (server, hits) = start_stub().await;
        let core = opened_core(server);
        for _ in 0..3 {
            tick(Arc::clone(&core)).await;
        }

        assert_eq!(hit_count(&hits, "/sessions/s1/config_options"), 1);
        assert_eq!(hit_count(&hits, "/sessions/s1/slash_commands"), 1);
        assert_eq!(hit_count(&hits, "/sessions/s1/plan"), 0, "计划面板未打开");

        let core = core.lock();
        assert_eq!(core.view.detail.config_options.len(), 1);
        assert_eq!(core.view.detail.slash_commands.len(), 1);
    }

    /// 详情面板打开后上下文信息同样按自身周期节流。
    #[tokio::test]
    async fn context_is_throttled_when_detail_panel_open() {
        let (server, hits) = start_stub().await;
        let core = opened_core(server);
        core.lock().side_panel = Some(SidePanel::Detail);
        for _ in 0..3 {
            tick(Arc::clone(&core)).await;
        }

        assert_eq!(hit_count(&hits, "/sessions/s1/context"), 1);
        assert_eq!(core.lock().view.detail.context_size, 1);
    }
}
