//! 客户端 API（HTTPS）路由与处理函数，以及 Daemon 的 WebSocket 入口。
//!
//! 路径与语义对齐 docs/DESIGN.md「Client-Server 通信」一节。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use amux_common::api::*;
use amux_common::domain::{SESSION_LIST_DEFAULT_LIMIT, SESSION_PAGE_DEFAULT_LIMIT};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

#[derive(Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

impl Page {
    fn limit(&self, fallback: usize) -> usize {
        self.limit.unwrap_or(fallback).clamp(1, 500)
    }

    fn offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
pub struct ListDir {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// 只列目录（工作目录前缀匹配用）
    #[serde(default)]
    pub dirs_only: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReadFile {
    pub path: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/daemon", get(daemon))
        .route("/machines", get(list_machines))
        .route("/machines/{machine}/agents", get(list_agents))
        .route("/machines/{machine}/agents/rediscover", post(rediscover))
        .route(
            "/machines/{machine}/agents/{agent}/restart",
            post(restart_agent),
        )
        .route("/machines/{machine}/list_dir", get(list_dir))
        .route("/machines/{machine}/read_file", get(read_file))
        .route("/sessions", post(create_session).get(list_sessions))
        .route(
            "/sessions/{id}",
            get(get_session).post(prompt_session).delete(delete_session),
        )
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/sessions/{id}/configure", post(configure_session))
        .route("/sessions/{id}/config_options", get(session_config_options))
        .route("/sessions/{id}/slash_commands", get(session_slash_commands))
        .route("/sessions/{id}/plan", get(session_plan))
        .route("/sessions/{id}/context", get(session_context))
        .route("/sessions/{id}/history", get(session_history))
        .route("/sessions/{id}/activities", get(session_activities))
        .route("/sessions/{id}/ongoing_activity", get(session_ongoing))
        .route("/sessions/{id}/diff", get(session_diff))
        .route(
            "/sessions/{id}/terminals",
            post(open_terminal).get(list_terminals),
        )
        .route(
            "/sessions/{id}/terminals/{terminal}",
            post(terminal_input)
                .get(terminal_output)
                .delete(close_terminal),
        )
        .route(
            "/sessions/{id}/terminals/{terminal}/resize",
            post(resize_terminal),
        )
        .route("/workflows", post(create_workflow).get(list_workflows))
        .route(
            "/workflows/{id}",
            get(get_workflow)
                .post(prompt_workflow)
                .delete(delete_workflow),
        )
        .route("/workflows/{id}/configure", post(configure_workflow))
        .route("/workflows/{id}/history", get(workflow_history))
        .route("/workflows/{id}/activities", get(workflow_activities))
        .route("/workflows/{id}/ongoing_activity", get(workflow_ongoing))
        .route("/config/skills/", get(get_skills).put(put_skills))
        .route("/config/workflows/", get(get_plans).put(put_plans))
        .route(
            "/config/recent_workspaces/",
            get(get_recent_workspaces).put(put_recent_workspaces),
        )
        .route(
            "/config/quick_commands/",
            get(get_quick_commands).put(put_quick_commands),
        )
        .route(
            "/config/agent/",
            get(get_orchestrator).put(put_orchestrator),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

/// JSON 请求体上限：图片附件以内联 base64 发送（PRD「可拖拽文件和图片，可粘贴图片」），
/// axum 默认 2 MiB 不够；按尽量宽松但不至失控的量级取 50 MiB。
const MAX_REQUEST_BODY_BYTES: usize = 50 * 1024 * 1024;

// ---------- Daemon 接入 ----------

async fn daemon(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    state.machines.upgrade(&headers, upgrade)
}

// ---------- 机器与 agent ----------

async fn list_machines(State(state): State<Arc<AppState>>) -> Json<Vec<Machine>> {
    Json(
        state
            .machines
            .machines()
            .into_iter()
            .map(|info| Machine {
                name: info.name,
                os: info.os,
                arch: info.arch,
                hostname: info.hostname,
                temp_dir: info.temp_dir,
                version: info.version,
            })
            .collect(),
    )
}

async fn list_agents(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
) -> ApiResult<Vec<Agent>> {
    state
        .machines
        .agents(&machine)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn rediscover(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
) -> ApiResult<Vec<Agent>> {
    state
        .machines
        .rediscover(&machine)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn restart_agent(
    State(state): State<Arc<AppState>>,
    Path((machine, agent)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    state
        .machines
        .restart_agent(&machine, &agent)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn list_dir(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
    Query(query): Query<ListDir>,
) -> ApiResult<amux_common::domain::FsListResult> {
    state
        .machines
        .fs_list(
            &machine,
            amux_common::domain::FsListParams {
                path: query.path,
                limit: query
                    .limit
                    .unwrap_or(amux_common::domain::FS_LIST_PAGE_LIMIT),
                offset: query.offset.unwrap_or(0),
                dirs_only: query.dirs_only,
            },
        )
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
    Query(query): Query<ReadFile>,
) -> ApiResult<amux_common::domain::FsReadResult> {
    state
        .machines
        .fs_read(
            &machine,
            amux_common::domain::FsReadParams {
                path: query.path,
                offset: query.offset.unwrap_or(0),
                limit: query
                    .limit
                    .unwrap_or(amux_common::domain::FS_READ_PAGE_LIMIT),
            },
        )
        .await
        .map(Json)
        .map_err(bad_request)
}

// ---------- 普通会话 ----------

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Session> {
    state
        .sessions
        .create(
            &request.machine,
            &request.agent,
            &request.workspace,
            request.use_worktree,
        )
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<SessionList> {
    let (sessions, has_more) = state
        .sessions
        .list(page.limit(SESSION_LIST_DEFAULT_LIMIT), page.offset());
    Json(SessionList { sessions, has_more })
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Session> {
    state.sessions.get(&id).map(Json).map_err(not_found)
}

async fn prompt_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .prompt(&id, request.input)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .cancel(&id)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .delete(&id)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn configure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureSessionRequest>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .configure(&id, request.title, request.config)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn session_config_options(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ConfigOptions> {
    state
        .sessions
        .config_options(&id)
        .await
        .map(|options| Json(ConfigOptions { options }))
        .map_err(bad_request)
}

async fn session_slash_commands(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<SlashCommands> {
    state.sessions.get(&id).map_err(not_found)?;
    Ok(Json(SlashCommands {
        commands: state.sessions.slash_commands(&id),
    }))
}

async fn session_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Plan> {
    state.sessions.get(&id).map_err(not_found)?;
    Ok(Json(Plan {
        entries: state.sessions.plan(&id),
    }))
}

async fn session_context(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<ContextInfo> {
    state.sessions.get(&id).map_err(not_found)?;
    let (context_size, context_window_size) = state.sessions.context(&id);
    Ok(Json(ContextInfo {
        context_size,
        context_window_size,
    }))
}

async fn session_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<HistoryPage> {
    state.sessions.get(&id).map_err(not_found)?;
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let (items, has_more) = state.sessions.history(&id, limit, offset);
    Ok(Json(HistoryPage {
        items,
        has_more,
        next_offset: has_more.then_some(offset + limit),
    }))
}

async fn session_activities(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<ActivitiesPage> {
    state.sessions.get(&id).map_err(not_found)?;
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let (activities, has_more) = state.sessions.activities(&id, limit, offset);
    Ok(Json(ActivitiesPage {
        activities,
        has_more,
        next_offset: has_more.then_some(offset + limit),
    }))
}

async fn session_ongoing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OngoingActivity> {
    state.sessions.get(&id).map_err(not_found)?;
    Ok(Json(OngoingActivity {
        activity: state.sessions.ongoing_activity(&id),
    }))
}

async fn session_diff(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<DiffResponse> {
    state
        .sessions
        .diff(&id)
        .await
        .map(Json)
        .map_err(bad_request)
}

// ---------- 终端 ----------

async fn open_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<OpenTerminalRequest>,
) -> ApiResult<amux_common::domain::TerminalOpenResult> {
    state
        .sessions
        .terminal_open(&id, request.cwd, request.cols, request.rows)
        .await
        .map(|terminal_id| Json(amux_common::domain::TerminalOpenResult { terminal_id }))
        .map_err(bad_request)
}

async fn list_terminals(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<Terminal>> {
    state.sessions.get(&id).map_err(not_found)?;
    Ok(Json(state.sessions.terminals(&id)))
}

async fn terminal_input(
    State(state): State<Arc<AppState>>,
    Path((id, terminal)): Path<(String, String)>,
    Json(request): Json<TerminalInputRequest>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .terminal_input(&id, &terminal, request.data)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn terminal_output(
    State(state): State<Arc<AppState>>,
    Path((id, terminal)): Path<(String, String)>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + Send>,
    (StatusCode, String),
> {
    state.sessions.get(&id).map_err(not_found)?;
    let subscription = state
        .sessions
        .subscribe_terminal(&terminal)
        .ok_or_else(|| not_found("终端不存在".to_string()))?;
    let stream = futures_util::StreamExt::map(subscription.into_stream(), |output| {
        let output = output.expect("终端输出流不会失败");
        Ok(Event::default()
            .event("output")
            .json_data(output)
            .expect("终端输出事件始终可序列化"))
    });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

async fn close_terminal(
    State(state): State<Arc<AppState>>,
    Path((id, terminal)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .terminal_close(&id, &terminal)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn resize_terminal(
    State(state): State<Arc<AppState>>,
    Path((id, terminal)): Path<(String, String)>,
    Json(request): Json<TerminalResizeRequest>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .terminal_resize(&id, &terminal, request.cols, request.rows)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

// ---------- 工作流会话 ----------

async fn create_workflow(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateWorkflowRequest>,
) -> ApiResult<Workflow> {
    state
        .workflows
        .create(&request.plan, request.title)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn list_workflows(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<WorkflowList> {
    let (workflows, has_more) = state
        .workflows
        .list(page.limit(SESSION_LIST_DEFAULT_LIMIT), page.offset());
    Json(WorkflowList {
        workflows,
        has_more,
    })
}

async fn get_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Workflow> {
    state.workflows.get(&id).map(Json).map_err(not_found)
}

async fn prompt_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> ApiResult<OpAck> {
    state
        .workflows
        .prompt(&id, request.input)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn delete_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state
        .workflows
        .delete(&id)
        .await
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn configure_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureWorkflowRequest>,
) -> ApiResult<OpAck> {
    state
        .workflows
        .configure(&id, request.title)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn workflow_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<HistoryPage> {
    state.workflows.get(&id).map_err(not_found)?;
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let (items, has_more) = state.workflows.history(&id, limit, offset);
    Ok(Json(HistoryPage {
        items,
        has_more,
        next_offset: has_more.then_some(offset + limit),
    }))
}

async fn workflow_activities(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<ActivitiesPage> {
    state.workflows.get(&id).map_err(not_found)?;
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let (activities, has_more) = state.workflows.activities(&id, limit, offset);
    Ok(Json(ActivitiesPage {
        activities,
        has_more,
        next_offset: has_more.then_some(offset + limit),
    }))
}

async fn workflow_ongoing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OngoingActivity> {
    state.workflows.get(&id).map_err(not_found)?;
    Ok(Json(OngoingActivity {
        activity: state.workflows.ongoing_activity(&id),
    }))
}

// ---------- 配置 ----------

async fn get_skills(State(state): State<Arc<AppState>>) -> Json<Vec<Skill>> {
    Json(state.config.skills())
}

async fn put_skills(
    State(state): State<Arc<AppState>>,
    Json(skills): Json<Vec<Skill>>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_skills(&skills)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn get_plans(State(state): State<Arc<AppState>>) -> Json<Vec<WorkflowPlanItem>> {
    Json(state.config.workflow_plans())
}

async fn put_plans(
    State(state): State<Arc<AppState>>,
    Json(plans): Json<Vec<WorkflowPlanItem>>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_workflow_plans(&plans)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn get_recent_workspaces(State(state): State<Arc<AppState>>) -> Json<Vec<RecentWorkspace>> {
    Json(state.config.recent_workspaces())
}

async fn put_recent_workspaces(
    State(state): State<Arc<AppState>>,
    Json(workspaces): Json<Vec<RecentWorkspace>>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_recent_workspaces(&workspaces)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn get_quick_commands(State(state): State<Arc<AppState>>) -> Json<Vec<QuickCommand>> {
    Json(state.config.quick_commands())
}

async fn put_quick_commands(
    State(state): State<Arc<AppState>>,
    Json(commands): Json<Vec<QuickCommand>>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_quick_commands(&commands)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn get_orchestrator(State(state): State<Arc<AppState>>) -> Json<Option<OrchestratorConfig>> {
    Json(state.config.orchestrator())
}

async fn put_orchestrator(
    State(state): State<Arc<AppState>>,
    Json(config): Json<OrchestratorConfig>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_orchestrator(&config)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

/// 通用操作应答。
#[derive(Debug, serde::Serialize)]
pub struct OpAck {
    pub ok: bool,
}

fn bad_request(message: String) -> (StatusCode, String) {
    let status = if message.contains("不存在") || message.contains("未连接") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, message)
}

fn not_found(message: String) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, message)
}
