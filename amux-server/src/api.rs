//! 客户端 API（HTTPS）路由与处理函数，以及 Daemon 的 WebSocket 入口。
//!
//! 路径与语义对齐 docs/DESIGN.md「Client-Server 通信」一节。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use amux_common::api::*;
use amux_common::domain::{SESSION_LIST_DEFAULT_LIMIT, SESSION_PAGE_DEFAULT_LIMIT};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::attachments::{AttachmentOwner, MAX_ATTACHMENT_BYTES};
use crate::sessions::SessionWorktree;
use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

#[derive(Debug, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// 按所属项目过滤（`GET /sessions`、`GET /workflows`）。
    #[serde(default)]
    pub project: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    pub base: String,
}

#[derive(Debug, Deserialize)]
pub struct RepoQuery {
    pub repo: String,
}

#[derive(Debug, Deserialize)]
pub struct UploadAttachment {
    pub filename: String,
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
        .route(
            "/machines/{machine}/list_branches",
            get(machine_list_branches),
        )
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
        .route("/sessions/{id}/usage", get(session_usage))
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
        .route(
            "/sessions/{id}/attachments",
            get(list_session_attachments)
                .post(upload_session_attachment)
                .delete(delete_session_attachments),
        )
        .route(
            "/sessions/{id}/attachments/{name}",
            axum::routing::delete(delete_session_attachment),
        )
        .route("/workflows", post(create_workflow).get(list_workflows))
        .route(
            "/workflows/{id}",
            get(get_workflow)
                .post(prompt_workflow)
                .delete(delete_workflow),
        )
        .route("/workflows/{id}/configure", post(configure_workflow))
        .route("/workflows/{id}/usage", get(workflow_usage))
        .route("/workflows/{id}/history", get(workflow_history))
        .route("/workflows/{id}/activities", get(workflow_activities))
        .route("/workflows/{id}/ongoing_activity", get(workflow_ongoing))
        .route(
            "/workflows/{id}/attachments",
            get(list_workflow_attachments)
                .post(upload_workflow_attachment)
                .delete(delete_workflow_attachments),
        )
        .route(
            "/workflows/{id}/attachments/{name}",
            axum::routing::delete(delete_workflow_attachment),
        )
        .route("/config/skills/", get(get_skills).put(put_skills))
        .route("/config/workflows/", get(get_plans).put(put_plans))
        .route(
            "/config/recent_workspaces/",
            get(get_recent_workspaces).put(put_recent_workspaces),
        )
        .route("/config/projects/", get(get_projects).post(create_project))
        .route("/config/projects/order", post(set_project_order))
        .route(
            "/config/projects/{name}",
            put(update_project).delete(delete_project),
        )
        .route(
            "/config/quick_commands/",
            get(get_quick_commands).put(put_quick_commands),
        )
        .route(
            "/config/workflow_agent/",
            get(get_orchestrator).put(put_orchestrator),
        )
        .route(
            "/config/automation",
            get(get_automation_config).put(put_automation_config),
        )
        .route(
            "/automations",
            get(list_automations).post(create_automation),
        )
        .route(
            "/automations/{id}",
            put(update_automation).delete(delete_automation),
        )
        .route("/automations/{id}/runs", get(automation_runs))
        .route("/automations/{id}/trigger", post(trigger_automation))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/sessions/{id}/attachments/{name}",
            get(download_session_attachment),
        )
        .route(
            "/workflows/{id}/attachments/{name}",
            get(download_workflow_attachment),
        )
        .route("/webhooks/github", post(github_webhook))
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

async fn machine_list_branches(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
    Query(query): Query<RepoQuery>,
) -> ApiResult<amux_common::domain::GitBranchListResult> {
    state
        .machines
        .git_branches(&machine, &query.repo)
        .await
        .map(Json)
        .map_err(bad_request)
}

// ---------- 普通会话 ----------

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Session> {
    let worktree = SessionWorktree::new(request.use_worktree, request.worktree_base.as_ref())
        .map_err(bad_request)?;
    state
        .sessions
        .create(crate::sessions::CreateSessionParams {
            machine: &request.machine,
            agent: &request.agent,
            workspace: &request.workspace,
            worktree,
            project: request.project.as_deref(),
        })
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<SessionList> {
    let (sessions, has_more) = state.sessions.list(
        page.limit(SESSION_LIST_DEFAULT_LIMIT),
        page.offset(),
        page.project.as_deref(),
    );
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
    state.sessions.delete(&id).await.map_err(bad_request)?;
    if let Err(error) = state.attachments.delete_all(AttachmentOwner::Session(&id)) {
        log::warn!("删除会话附件失败: session={id}: {error}");
    }
    Ok(Json(OpAck { ok: true }))
}

async fn configure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureSessionRequest>,
) -> ApiResult<OpAck> {
    state
        .sessions
        .configure(
            &id,
            request.title,
            request.config,
            request.project,
            request.pinned,
        )
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

async fn session_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Option<Usage>> {
    state.sessions.get(&id).map_err(not_found)?;
    Ok(Json(state.sessions.usage(&id)))
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
    Query(query): Query<DiffQuery>,
) -> ApiResult<DiffResponse> {
    if query.base.trim().is_empty() {
        return Err(bad_request("base 不能为空".to_string()));
    }
    state
        .sessions
        .diff(&id, &query.base)
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

// ---------- 会话附件 ----------

async fn list_session_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AttachmentList> {
    state.sessions.get(&id).map_err(not_found)?;
    state
        .attachments
        .list(AttachmentOwner::Session(&id), page.limit(50), page.offset())
        .map(Json)
        .map_err(bad_request)
}

async fn upload_session_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(upload): Query<UploadAttachment>,
    body: Bytes,
) -> ApiResult<Attachment> {
    state.sessions.get(&id).map_err(not_found)?;
    upload_attachment(
        &state,
        AttachmentOwner::Session(&id),
        &upload.filename,
        body,
    )
}

async fn delete_session_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state.sessions.get(&id).map_err(not_found)?;
    state
        .attachments
        .delete_all(AttachmentOwner::Session(&id))
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn delete_session_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    state.sessions.get(&id).map_err(not_found)?;
    state
        .attachments
        .delete(AttachmentOwner::Session(&id), &name)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn download_session_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, String)> {
    state.sessions.get(&id).map_err(not_found)?;
    download_attachment(&state, AttachmentOwner::Session(&id), &name)
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
        .create(&request.plan, request.title, request.project.as_deref())
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn list_workflows(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<WorkflowList> {
    let (workflows, has_more) = state.workflows.list(
        page.limit(SESSION_LIST_DEFAULT_LIMIT),
        page.offset(),
        page.project.as_deref(),
    );
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
    let workflow = state.workflows.get(&id).map_err(not_found)?;
    state.workflows.delete(&id).await.map_err(bad_request)?;
    if let Err(error) = state.attachments.delete_all(AttachmentOwner::Workflow(&id)) {
        log::warn!("删除工作流附件失败: workflow={id}: {error}");
    }
    for session in workflow.linked_sessions {
        if let Err(error) = state
            .attachments
            .delete_all(AttachmentOwner::Session(&session.id))
        {
            log::warn!("删除关联会话附件失败: session={}: {error}", session.id);
        }
    }
    Ok(Json(OpAck { ok: true }))
}

async fn list_workflow_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AttachmentList> {
    state.workflows.get(&id).map_err(not_found)?;
    state
        .attachments
        .list(
            AttachmentOwner::Workflow(&id),
            page.limit(50),
            page.offset(),
        )
        .map(Json)
        .map_err(bad_request)
}

async fn upload_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(upload): Query<UploadAttachment>,
    body: Bytes,
) -> ApiResult<Attachment> {
    state.workflows.get(&id).map_err(not_found)?;
    upload_attachment(
        &state,
        AttachmentOwner::Workflow(&id),
        &upload.filename,
        body,
    )
}

async fn delete_workflow_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state.workflows.get(&id).map_err(not_found)?;
    state
        .attachments
        .delete_all(AttachmentOwner::Workflow(&id))
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn delete_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    state.workflows.get(&id).map_err(not_found)?;
    state
        .attachments
        .delete(AttachmentOwner::Workflow(&id), &name)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn download_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, (StatusCode, String)> {
    state.workflows.get(&id).map_err(not_found)?;
    download_attachment(&state, AttachmentOwner::Workflow(&id), &name)
}

async fn configure_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureWorkflowRequest>,
) -> ApiResult<OpAck> {
    state
        .workflows
        .configure(&id, request.title, request.project, request.pinned)
        .map(|_| Json(OpAck { ok: true }))
        .map_err(bad_request)
}

async fn workflow_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Option<Usage>> {
    state.workflows.get(&id).map_err(not_found)?;
    Ok(Json(state.workflows.usage(&id)))
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

// ---------- 自动化 ----------

async fn list_automations(State(state): State<Arc<AppState>>) -> Json<Vec<Automation>> {
    Json(state.automations.list())
}

async fn create_automation(
    State(state): State<Arc<AppState>>,
    Json(input): Json<AutomationInput>,
) -> ApiResult<Automation> {
    state
        .automations
        .create(input)
        .map(Json)
        .map_err(bad_request)
}

async fn update_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<AutomationInput>,
) -> ApiResult<Automation> {
    state
        .automations
        .update(&id, input)
        .map(Json)
        .map_err(bad_request)
}

async fn delete_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state.automations.delete(&id).map_err(bad_request)?;
    Ok(Json(OpAck { ok: true }))
}

async fn automation_runs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AutomationRunList> {
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let (runs, has_more) = state
        .automations
        .runs_page(&id, limit, offset)
        .map_err(bad_request)?;
    Ok(Json(AutomationRunList { runs, has_more }))
}

async fn trigger_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<AutomationRun> {
    state
        .automations
        .trigger(&id)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<OpAck> {
    let event = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| bad_request("缺少 X-Hub-Signature-256 请求头".into()))?;
    verify_github_signature(&state.token, signature, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| bad_request(format!("Webhook JSON 无效: {error}")))?;
    let matched = state
        .automations
        .github_webhook(event, &body)
        .await
        .map_err(bad_request)?;
    log::info!("GitHub Webhook 已处理: event={event} matched={matched}");
    Ok(Json(OpAck { ok: true }))
}

// ---------- 配置 ----------

async fn get_skills(State(state): State<Arc<AppState>>) -> Json<Vec<Skill>> {
    Json(state.config.skills())
}

async fn put_skills(
    State(state): State<Arc<AppState>>,
    Json(skills): Json<Vec<Skill>>,
) -> ApiResult<OpAck> {
    state.config.set_skills(&skills).map_err(bad_request)?;
    log::info!("技能配置已更新: {} 项", skills.len());
    Ok(Json(OpAck { ok: true }))
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
        .map_err(bad_request)?;
    log::info!("工作流计划配置已更新: {} 项", plans.len());
    Ok(Json(OpAck { ok: true }))
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

// ---------- 项目管理 ----------

async fn get_projects(State(state): State<Arc<AppState>>) -> Json<Vec<Project>> {
    Json(state.config.projects())
}

async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(project): Json<Project>,
) -> ApiResult<OpAck> {
    state.config.add_project(&project).map_err(bad_request)?;
    log::info!("项目已创建: {}", project.name);
    Ok(Json(OpAck { ok: true }))
}

async fn set_project_order(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProjectOrderRequest>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_project_order(&request.names)
        .map_err(bad_request)?;
    log::info!("项目顺序已更新: {} 项", request.names.len());
    Ok(Json(OpAck { ok: true }))
}

async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(request): Json<UpdateProjectRequest>,
) -> ApiResult<OpAck> {
    state
        .config
        .update_project(&name, &request.description)
        .map_err(bad_request)?;
    log::info!("项目已更新: {name}");
    Ok(Json(OpAck { ok: true }))
}

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<OpAck> {
    state.config.delete_project(&name).map_err(bad_request)?;
    log::info!("项目已删除: {name}");
    // 项目删除后，其下会话回到未归属（docs/PRD.md「项目管理」）
    state.sessions.unassign_project(&name);
    state.workflows.unassign_project(&name);
    Ok(Json(OpAck { ok: true }))
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
        .map_err(bad_request)?;
    log::info!("快捷指令配置已更新: {} 项", commands.len());
    Ok(Json(OpAck { ok: true }))
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
        .map_err(bad_request)?;
    log::info!("内置智能体配置已更新: {}", config.model);
    Ok(Json(OpAck { ok: true }))
}

async fn get_automation_config(
    State(state): State<Arc<AppState>>,
) -> Json<Option<AutomationConfig>> {
    Json(state.config.automation())
}

async fn put_automation_config(
    State(state): State<Arc<AppState>>,
    Json(config): Json<AutomationConfig>,
) -> ApiResult<OpAck> {
    state.config.set_automation(&config).map_err(bad_request)?;
    log::info!("自动化决策模型配置已更新: {}", config.model);
    Ok(Json(OpAck { ok: true }))
}

/// 通用操作应答。
#[derive(Debug, serde::Serialize)]
pub struct OpAck {
    pub ok: bool,
}

fn verify_github_signature(
    secret: &str,
    signature: &str,
    body: &[u8],
) -> Result<(), (StatusCode, String)> {
    let signature = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| bad_request("X-Hub-Signature-256 格式无效".into()))?;
    let signature =
        decode_hex(signature).ok_or_else(|| bad_request("X-Hub-Signature-256 格式无效".into()))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|error| bad_request(format!("校验 Webhook 签名失败: {error}")))?;
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| bad_request("Webhook 签名校验失败".into()))
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
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

fn upload_attachment(
    state: &AppState,
    owner: AttachmentOwner<'_>,
    filename: &str,
    body: Bytes,
) -> ApiResult<Attachment> {
    if body.len() > MAX_ATTACHMENT_BYTES {
        return Err(bad_request("单个附件不得超过 20 MB".into()));
    }
    state
        .attachments
        .save(owner, filename, &body)
        .map(Json)
        .map_err(bad_request)
}

fn download_attachment(
    state: &AppState,
    owner: AttachmentOwner<'_>,
    name: &str,
) -> Result<Response, (StatusCode, String)> {
    let bytes = state.attachments.read(owner, name).map_err(not_found)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .body(Body::from(bytes))
        .map_err(|error| bad_request(format!("构造下载响应失败: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_signature_is_verified_with_server_token() {
        const TOKEN: &str = "secret-token";
        let body = br#"{"repository":{"full_name":"owner/repo"}}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(TOKEN.as_bytes()).unwrap();
        mac.update(body);
        let signature = format!("sha256={}", encode_hex(&mac.finalize().into_bytes()));

        assert!(verify_github_signature(TOKEN, &signature, body).is_ok());
        assert!(verify_github_signature(TOKEN, &signature, br#"{"repository":{}}"#).is_err());
        assert!(verify_github_signature(TOKEN, "sha256=00", body).is_err());
    }

    fn encode_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
        output
    }
}
