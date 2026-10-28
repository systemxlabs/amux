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
use crate::db;
use crate::error::ServerError;
use crate::sessions::SessionWorktree;
use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, ServerError>;

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
        .route(
            "/machines/{machine}/agents/{agent}/models",
            get(list_agent_models),
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
        .route("/sessions/{id}/read", post(read_session))
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
        .route("/workflows/{id}/read", post(read_workflow))
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
        .route("/groups", post(create_group).get(list_groups))
        .route(
            "/groups/{id}",
            get(get_group).post(prompt_group).delete(delete_group),
        )
        .route("/groups/{id}/cancel", post(cancel_group))
        .route("/groups/{id}/configure", post(configure_group))
        .route("/groups/{id}/read", post(read_group))
        .route("/groups/{id}/history", get(group_history))
        .route(
            "/groups/{id}/attachments",
            get(list_group_attachments)
                .post(upload_group_attachment)
                .delete(delete_group_attachments),
        )
        .route(
            "/groups/{id}/attachments/{name}",
            axum::routing::delete(delete_group_attachment),
        )
        .route("/config/skills/", get(get_skills).put(put_skills))
        .route("/config/workflows/", get(get_plans).put(put_plans))
        .route(
            "/config/group_members",
            get(get_group_members).put(put_group_members),
        )
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
        .route(
            "/groups/{id}/attachments/{name}",
            get(download_group_attachment),
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
    state.machines.agents(&machine).await.map(Json)
}

async fn rediscover(
    State(state): State<Arc<AppState>>,
    Path(machine): Path<String>,
) -> ApiResult<Vec<Agent>> {
    state.machines.rediscover(&machine).await.map(Json)
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
}

async fn list_agent_models(
    State(state): State<Arc<AppState>>,
    Path((machine, agent)): Path<(String, String)>,
) -> ApiResult<Vec<amux_common::api::AgentModel>> {
    state
        .machines
        .agent_models(&machine, &agent)
        .await
        .map(Json)
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
}

// ---------- 普通会话 ----------

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<Session> {
    let worktree = SessionWorktree::new(request.use_worktree, request.worktree_base.as_ref())?;
    state
        .sessions
        .create(crate::sessions::CreateSessionParams {
            machine: &request.machine,
            agent: &request.agent,
            model: request.model.as_deref(),
            workspace: &request.workspace,
            worktree,
            title: None,
            project: request.project.as_deref(),
        })
        .await
        .map(Json)
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<SessionList> {
    let sessions = Arc::clone(&state.sessions);
    let limit = page.limit(SESSION_LIST_DEFAULT_LIMIT);
    let offset = page.offset();
    let project = page.project.clone();
    let (sessions, has_more) =
        match db::blocking(move || sessions.list(limit, offset, project.as_deref())).await {
            Ok(page) => page,
            Err(error) => {
                log::warn!("读取会话列表失败: {error}");
                Default::default()
            }
        };
    Json(SessionList { sessions, has_more })
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Session> {
    let sessions = Arc::clone(&state.sessions);
    db::blocking(move || sessions.get(&id)).await?.map(Json)
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
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state.sessions.delete(&id).await?;
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
}

async fn read_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let sessions = Arc::clone(&state.sessions);
    db::blocking(move || sessions.read(&id))
        .await
        .map(|_| Json(OpAck { ok: true }))
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
}

async fn session_slash_commands(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<SlashCommands> {
    let sessions = Arc::clone(&state.sessions);
    let commands = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.slash_commands(&id))
    })
    .await??;
    Ok(Json(SlashCommands { commands }))
}

async fn session_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Plan> {
    let sessions = Arc::clone(&state.sessions);
    let entries = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.plan(&id))
    })
    .await??;
    Ok(Json(Plan { entries }))
}

async fn session_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Option<Usage>> {
    let sessions = Arc::clone(&state.sessions);
    let usage = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.usage(&id))
    })
    .await??;
    Ok(Json(usage))
}

async fn session_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<HistoryPage> {
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let sessions = Arc::clone(&state.sessions);
    let (items, has_more) = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.history(&id, limit, offset))
    })
    .await??;
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
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let sessions = Arc::clone(&state.sessions);
    let (activities, has_more) = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.activities(&id, limit, offset))
    })
    .await??;
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
    let sessions = Arc::clone(&state.sessions);
    let activity = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.ongoing_activity(&id))
    })
    .await??;
    Ok(Json(OngoingActivity { activity }))
}

async fn session_diff(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ApiResult<DiffResponse> {
    if query.base.trim().is_empty() {
        return Err(ServerError::invalid_input("base 不能为空"));
    }
    state.sessions.diff(&id, &query.base).await.map(Json)
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
}

async fn list_terminals(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<Terminal>> {
    let sessions = Arc::clone(&state.sessions);
    let terminals = db::blocking(move || {
        sessions.get(&id)?;
        Ok::<_, ServerError>(sessions.terminals(&id))
    })
    .await??;
    Ok(Json(terminals))
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
}

async fn terminal_output(
    State(state): State<Arc<AppState>>,
    Path((id, terminal)): Path<(String, String)>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + Send>, ServerError> {
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
    let subscription = state
        .sessions
        .subscribe_terminal(&terminal)
        .ok_or_else(|| ServerError::not_found("终端不存在"))?;
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
}

// ---------- 会话附件 ----------

async fn list_session_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AttachmentList> {
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
    state
        .attachments
        .list(AttachmentOwner::Session(&id), page.limit(50), page.offset())
        .map(Json)
}

async fn upload_session_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(upload): Query<UploadAttachment>,
    body: Bytes,
) -> ApiResult<Attachment> {
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
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
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
    state
        .attachments
        .delete_all(AttachmentOwner::Session(&id))
        .map(|_| Json(OpAck { ok: true }))
}

async fn delete_session_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
    state
        .attachments
        .delete(AttachmentOwner::Session(&id), &name)
        .map(|_| Json(OpAck { ok: true }))
}

async fn download_session_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    let sessions_service = Arc::clone(&state.sessions);
    let check = id.clone();
    db::blocking(move || sessions_service.get(&check)).await??;
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
}

// ---------- 群聊会话 ----------

async fn create_group(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateGroupRequest>,
) -> ApiResult<Group> {
    state
        .groups
        .create(&request.members, None, request.project.as_deref())
        .await
        .map(Json)
}

async fn list_groups(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<GroupList> {
    let groups = Arc::clone(&state.groups);
    let limit = page.limit(SESSION_LIST_DEFAULT_LIMIT);
    let offset = page.offset();
    let project = page.project.clone();
    let (groups, has_more) = db::blocking(move || groups.list(limit, offset, project.as_deref()))
        .await
        .unwrap_or_default();
    Json(GroupList { groups, has_more })
}

async fn get_group(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Group> {
    let groups = Arc::clone(&state.groups);
    db::blocking(move || groups.get(&id)).await?.map(Json)
}

async fn prompt_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> ApiResult<OpAck> {
    state
        .groups
        .prompt(&id, request.input)
        .await
        .map(|_| Json(OpAck { ok: true }))
}

async fn cancel_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    state
        .groups
        .cancel(&id)
        .await
        .map(|_| Json(OpAck { ok: true }))
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    let group = db::blocking(move || groups.get(&check)).await??;
    state.groups.delete(&id).await?;
    if let Err(error) = state.attachments.delete_all(AttachmentOwner::Group(&id)) {
        log::warn!("删除群聊附件失败: group={id}: {error}");
    }
    for member in group.members {
        if let Err(error) = state
            .attachments
            .delete_all(AttachmentOwner::Session(&member.session_id))
        {
            log::warn!(
                "删除成员会话附件失败: session={}: {error}",
                member.session_id
            );
        }
    }
    Ok(Json(OpAck { ok: true }))
}

async fn configure_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureGroupRequest>,
) -> ApiResult<OpAck> {
    let groups = Arc::clone(&state.groups);
    db::blocking(move || groups.configure(&id, request.title, request.project, request.pinned))
        .await?
        .map(|_| Json(OpAck { ok: true }))
}

async fn read_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let groups = Arc::clone(&state.groups);
    db::blocking(move || groups.read(&id))
        .await?
        .map(|_| Json(OpAck { ok: true }))
}

async fn group_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<GroupHistoryPage> {
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let groups = Arc::clone(&state.groups);
    let (messages, has_more) = db::blocking(move || {
        groups.get(&id)?;
        Ok::<_, ServerError>(groups.history(&id, limit, offset))
    })
    .await??;
    Ok(Json(GroupHistoryPage {
        items: messages,
        has_more,
        next_offset: has_more.then_some(offset + limit),
    }))
}

async fn list_group_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AttachmentList> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    db::blocking(move || groups.get(&check)).await??;
    state
        .attachments
        .list(AttachmentOwner::Group(&id), page.limit(50), page.offset())
        .map(Json)
}

async fn upload_group_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(upload): Query<UploadAttachment>,
    body: Bytes,
) -> ApiResult<Attachment> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    db::blocking(move || groups.get(&check)).await??;
    upload_attachment(&state, AttachmentOwner::Group(&id), &upload.filename, body)
}

async fn delete_group_attachments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    db::blocking(move || groups.get(&check)).await??;
    state
        .attachments
        .delete_all(AttachmentOwner::Group(&id))
        .map(|_| Json(OpAck { ok: true }))
}

async fn delete_group_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    db::blocking(move || groups.get(&check)).await??;
    state
        .attachments
        .delete(AttachmentOwner::Group(&id), &name)
        .map(|_| Json(OpAck { ok: true }))
}

async fn download_group_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    let groups = Arc::clone(&state.groups);
    let check = id.clone();
    db::blocking(move || groups.get(&check)).await??;
    download_attachment(&state, AttachmentOwner::Group(&id), &name)
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
}

async fn list_workflows(
    State(state): State<Arc<AppState>>,
    Query(page): Query<Page>,
) -> Json<WorkflowList> {
    let workflows_service = Arc::clone(&state.workflows);
    let limit = page.limit(SESSION_LIST_DEFAULT_LIMIT);
    let offset = page.offset();
    let project = page.project.clone();
    let (workflows, has_more) =
        db::blocking(move || workflows_service.list(limit, offset, project.as_deref()))
            .await
            .unwrap_or_default();
    Json(WorkflowList {
        workflows,
        has_more,
    })
}

async fn get_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Workflow> {
    let workflows = Arc::clone(&state.workflows);
    db::blocking(move || workflows.get(&id)).await?.map(Json)
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
}

async fn delete_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    let workflow = db::blocking(move || workflows.get(&check)).await??;
    state.workflows.delete(&id).await?;
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
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    db::blocking(move || workflows.get(&check)).await??;
    state
        .attachments
        .list(
            AttachmentOwner::Workflow(&id),
            page.limit(50),
            page.offset(),
        )
        .map(Json)
}

async fn upload_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(upload): Query<UploadAttachment>,
    body: Bytes,
) -> ApiResult<Attachment> {
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    db::blocking(move || workflows.get(&check)).await??;
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
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    db::blocking(move || workflows.get(&check)).await??;
    state
        .attachments
        .delete_all(AttachmentOwner::Workflow(&id))
        .map(|_| Json(OpAck { ok: true }))
}

async fn delete_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<OpAck> {
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    db::blocking(move || workflows.get(&check)).await??;
    state
        .attachments
        .delete(AttachmentOwner::Workflow(&id), &name)
        .map(|_| Json(OpAck { ok: true }))
}

async fn download_workflow_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    let workflows = Arc::clone(&state.workflows);
    let check = id.clone();
    db::blocking(move || workflows.get(&check)).await??;
    download_attachment(&state, AttachmentOwner::Workflow(&id), &name)
}

async fn configure_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ConfigureWorkflowRequest>,
) -> ApiResult<OpAck> {
    let workflows = Arc::clone(&state.workflows);
    db::blocking(move || workflows.configure(&id, request.title, request.project, request.pinned))
        .await?
        .map(|_| Json(OpAck { ok: true }))
}

async fn read_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let workflows = Arc::clone(&state.workflows);
    db::blocking(move || workflows.read(&id))
        .await?
        .map(|_| Json(OpAck { ok: true }))
}

async fn workflow_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Option<Usage>> {
    let workflows = Arc::clone(&state.workflows);
    let usage = db::blocking(move || {
        workflows.get(&id)?;
        Ok::<_, ServerError>(workflows.usage(&id))
    })
    .await??;
    Ok(Json(usage))
}

async fn workflow_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<HistoryPage> {
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let workflows = Arc::clone(&state.workflows);
    let (items, has_more) = db::blocking(move || {
        workflows.get(&id)?;
        Ok::<_, ServerError>(workflows.history(&id, limit, offset))
    })
    .await??;
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
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let workflows = Arc::clone(&state.workflows);
    let (activities, has_more) = db::blocking(move || {
        workflows.get(&id)?;
        Ok::<_, ServerError>(workflows.activities(&id, limit, offset))
    })
    .await??;
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
    let workflows = Arc::clone(&state.workflows);
    let activity = db::blocking(move || {
        workflows.get(&id)?;
        Ok::<_, ServerError>(workflows.ongoing_activity(&id))
    })
    .await??;
    Ok(Json(OngoingActivity { activity }))
}

// ---------- 自动化 ----------

async fn list_automations(State(state): State<Arc<AppState>>) -> Json<Vec<Automation>> {
    let automations = Arc::clone(&state.automations);
    Json(
        db::blocking(move || automations.list())
            .await
            .unwrap_or_default(),
    )
}

async fn create_automation(
    State(state): State<Arc<AppState>>,
    Json(input): Json<AutomationInput>,
) -> ApiResult<Automation> {
    let automations = Arc::clone(&state.automations);
    db::blocking(move || automations.create(input))
        .await?
        .map(Json)
}

async fn update_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<AutomationInput>,
) -> ApiResult<Automation> {
    let automations = Arc::clone(&state.automations);
    db::blocking(move || automations.update(&id, input))
        .await?
        .map(Json)
}

async fn delete_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<OpAck> {
    let automations = Arc::clone(&state.automations);
    db::blocking(move || automations.delete(&id)).await??;
    Ok(Json(OpAck { ok: true }))
}

async fn automation_runs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<AutomationRunList> {
    let limit = page.limit(SESSION_PAGE_DEFAULT_LIMIT);
    let offset = page.offset();
    let automations = Arc::clone(&state.automations);
    let (runs, has_more) =
        db::blocking(move || automations.runs_page(&id, limit, offset)).await??;
    Ok(Json(AutomationRunList { runs, has_more }))
}

async fn trigger_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<AutomationRun> {
    state.automations.trigger(&id).await.map(Json)
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
        .ok_or_else(|| ServerError::invalid_input("缺少 X-Hub-Signature-256 请求头"))?;
    verify_github_signature(&state.token, signature, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| ServerError::invalid_input(format!("Webhook JSON 无效: {error}")))?;
    let matched = state.automations.github_webhook(event, &body).await?;
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
    state
        .config
        .set_skills(&skills)
        .map_err(ServerError::invalid_input)?;
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
        .map_err(ServerError::invalid_input)?;
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
        .map_err(ServerError::invalid_input)
}

// ---------- 项目管理 ----------

async fn get_projects(State(state): State<Arc<AppState>>) -> Json<Vec<Project>> {
    Json(state.config.projects())
}

async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(project): Json<Project>,
) -> ApiResult<OpAck> {
    state
        .config
        .add_project(&project)
        .map_err(ServerError::invalid_input)?;
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
        .map_err(ServerError::invalid_input)?;
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
        .map_err(ServerError::invalid_input)?;
    log::info!("项目已更新: {name}");
    Ok(Json(OpAck { ok: true }))
}

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> ApiResult<OpAck> {
    state
        .config
        .delete_project(&name)
        .map_err(ServerError::invalid_input)?;
    log::info!("项目已删除: {name}");
    // 项目删除后，其下会话回到未归属（docs/PRD.md「项目管理」）
    let sessions = Arc::clone(&state.sessions);
    let workflows = Arc::clone(&state.workflows);
    let groups = Arc::clone(&state.groups);
    let project = name.clone();
    db::blocking(move || {
        sessions.unassign_project(&project);
        workflows.unassign_project(&project);
        groups.unassign_project(&project);
    })
    .await?;
    Ok(Json(OpAck { ok: true }))
}

async fn get_group_members(State(state): State<Arc<AppState>>) -> Json<Vec<GroupMemberTemplate>> {
    Json(state.config.group_members())
}

async fn put_group_members(
    State(state): State<Arc<AppState>>,
    Json(members): Json<Vec<GroupMemberTemplate>>,
) -> ApiResult<OpAck> {
    state
        .config
        .set_group_members(&members)
        .map_err(ServerError::invalid_input)?;
    log::info!("群聊预设成员配置已更新: {} 项", members.len());
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
        .map_err(ServerError::invalid_input)?;
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
        .map_err(ServerError::invalid_input)?;
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
    state
        .config
        .set_automation(&config)
        .map_err(ServerError::invalid_input)?;
    log::info!("自动化决策模型配置已更新: {}", config.model);
    Ok(Json(OpAck { ok: true }))
}

/// 通用操作应答。
#[derive(Debug, serde::Serialize)]
pub struct OpAck {
    pub ok: bool,
}

fn verify_github_signature(secret: &str, signature: &str, body: &[u8]) -> Result<(), ServerError> {
    let signature = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| ServerError::invalid_input("X-Hub-Signature-256 格式无效"))?;
    let signature = decode_hex(signature)
        .ok_or_else(|| ServerError::invalid_input("X-Hub-Signature-256 格式无效"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|error| ServerError::internal(format!("校验 Webhook 签名失败: {error}")))?;
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| ServerError::invalid_input("Webhook 签名校验失败"))
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

fn upload_attachment(
    state: &AppState,
    owner: AttachmentOwner<'_>,
    filename: &str,
    body: Bytes,
) -> ApiResult<Attachment> {
    if body.len() > MAX_ATTACHMENT_BYTES {
        return Err(ServerError::invalid_input("单个附件不得超过 20 MB"));
    }
    state.attachments.save(owner, filename, &body).map(Json)
}

fn download_attachment(
    state: &AppState,
    owner: AttachmentOwner<'_>,
    name: &str,
) -> Result<Response, ServerError> {
    let bytes = state.attachments.read(owner, name)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .body(Body::from(bytes))
        .map_err(|error| ServerError::internal(format!("构造下载响应失败: {error}")))
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
