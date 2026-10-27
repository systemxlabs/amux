//! ACP v2 客户端：每 (机器, agent) 一条连接，传输经 Daemon WebSocket 多路复用。
//!
//! Server 是 ACP client，Daemon 只是消息管道：本模块把 Daemon 的 `acp` 通知
//! （`raw` 为一条 ACP JSON-RPC 消息文本）桥接成官方 SDK 的 `Lines` 传输，
//! 因此 initialize、会话方法、通知路由与权限自动审批都走官方 SDK
//! （docs/DESIGN.md「ACP 通信」「ACP 多路复用」）。
//!
//! 通知一律翻译为 [`AcpEvent`] 交给会话层落盘与驱动工作流，本模块不碰存储。

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v2::{
    AvailableCommand, AvailableCommandInput, CancelSessionNotification, ClientCapabilities,
    CloseSessionRequest, CompactionStatus, DeleteSessionRequest, IdleStateUpdate, Implementation,
    InitializeRequest, NewSessionRequest, NoticeSeverity, PermissionOption, PermissionOptionId,
    PermissionOptionKind, PlanUpdateContent, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionConfigKind as AcpSessionConfigKind,
    SessionConfigOption as AcpSessionConfigOption,
    SessionConfigOptionValue as AcpSessionConfigOptionValue, SessionConfigSelectOptions,
    SessionUpdate, SetSessionConfigOptionRequest, StateUpdate, StopReason, ToolCallStatus,
    ToolCallUpdate, ToolKind, UpdateSessionNotification,
};
use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::{Agent, Client, ConnectTo, JsonRpcRequest, Lines, V2ConnectionTo};
use amux_common::api::{NANO_ERROR_META_KEY, NANO_ERROR_STOP_REASON};
use amux_common::domain::{
    ContentBlock, SessionConfigKind, SessionConfigOption, SessionConfigOptionValue,
    SessionConfigSelectEntry, SessionPlanEntry, SessionPlanPriority, SessionPlanStatus,
    SessionState, SlashCommand, StateChangeReason,
};
use futures_util::sink;
use futures_util::stream;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

/// ACP 通知 → 会话层事件。
#[derive(Debug, Clone)]
pub enum AcpEvent {
    /// 前台状态变更
    State {
        agent_session_id: String,
        state: SessionState,
        reason: StateChangeReason,
    },
    /// agent 消息流式更新
    Message {
        agent_session_id: String,
        message_id: String,
        update: MessageUpdate,
    },
    /// 思考活动流式更新
    Thinking {
        agent_session_id: String,
        message_id: String,
        update: ThinkingUpdate,
    },
    /// 上下文压缩活动更新
    Compaction {
        agent_session_id: String,
        compaction_id: String,
        status: CompactionStatus,
        update: CompactionPatch,
    },
    /// 工具调用：`Pending`/`InProgress` 开始实时展示，终态结束
    ToolCall {
        agent_session_id: String,
        tool_call_id: String,
        /// 缺省表示本次 update 未携带状态（字段补丁）
        status: Option<ToolCallStatus>,
        name: Option<String>,
        kind: Option<ToolKind>,
        title: Option<String>,
        parameters: Option<String>,
    },
    /// 会话通知活动（每次通知本地生成唯一 ID）
    Notice {
        agent_session_id: String,
        severity: NoticeSeverity,
        title: String,
        description: Option<String>,
    },
    /// 错误活动（本地生成 id）
    Error {
        agent_session_id: String,
        message: String,
    },
    /// 会话选项全量覆盖
    Options {
        agent_session_id: String,
        options: Vec<SessionConfigOption>,
    },
    /// 斜杠命令全量覆盖
    Commands {
        agent_session_id: String,
        commands: Vec<SlashCommand>,
    },
    /// 计划全量覆盖
    Plan {
        agent_session_id: String,
        entries: Vec<SessionPlanEntry>,
    },
    /// 会话用量
    Usage {
        agent_session_id: String,
        used: u64,
        size: u64,
    },
    /// Agent 被重启（生命周期步骤或用户手动）：其会话状态需回到空闲
    AgentRestarted { machine: String, agent: String },
}

#[derive(Debug, Clone)]
pub enum MessageUpdate {
    Chunk(Box<ContentBlock>),
    Snapshot(Option<Vec<ContentBlock>>),
}

#[derive(Debug, Clone)]
pub enum ThinkingUpdate {
    Chunk(String),
    Snapshot(Option<String>),
}

#[derive(Debug, Clone)]
pub enum CompactionPatch {
    Chunk(String),
    Patch {
        summary: MaybeUndefined<String>,
        error: MaybeUndefined<String>,
    },
}

/// 会话层发往 ACP 连接的调用。
enum Call {
    NewSession {
        cwd: String,
        reply: oneshot::Sender<Result<(String, Vec<SessionConfigOption>), String>>,
    },
    ResumeSession {
        agent_session_id: String,
        cwd: String,
        reply: oneshot::Sender<Result<Vec<SessionConfigOption>, String>>,
    },
    Prompt {
        agent_session_id: String,
        input: Vec<ContentBlock>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Cancel {
        agent_session_id: String,
    },
    Close {
        agent_session_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Delete {
        agent_session_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetConfigOption {
        agent_session_id: String,
        config_id: String,
        value: SessionConfigOptionValue,
        reply: oneshot::Sender<Result<Vec<SessionConfigOption>, String>>,
    },
}

/// 一条 ACP 连接（`(machine, agent)` 维度）。
pub struct AgentConnection {
    calls: mpsc::Sender<Call>,
    /// Agent 是否在 initialize 时声明支持 `session/delete`。
    supports_delete: bool,
    opened_sessions: Arc<Mutex<HashSet<String>>>,
}

impl AgentConnection {
    /// 连接任务是否仍在运行：calls 通道关闭说明连接已结束。
    pub fn is_alive(&self) -> bool {
        !self.calls.is_closed()
    }

    pub async fn new_session(
        &self,
        cwd: &str,
    ) -> Result<(String, Vec<SessionConfigOption>), String> {
        let (tx, rx) = oneshot::channel();
        self.send(Call::NewSession {
            cwd: cwd.to_string(),
            reply: tx,
        })?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    pub async fn resume_session(
        &self,
        agent_session_id: &str,
        cwd: &str,
    ) -> Result<Vec<SessionConfigOption>, String> {
        let (tx, rx) = oneshot::channel();
        self.send(Call::ResumeSession {
            agent_session_id: agent_session_id.to_string(),
            cwd: cwd.to_string(),
            reply: tx,
        })?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    pub async fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.send(Call::Prompt {
            agent_session_id: agent_session_id.to_string(),
            input,
            reply: tx,
        })?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    pub fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.send(Call::Cancel {
            agent_session_id: agent_session_id.to_string(),
        })
    }

    pub async fn close(&self, agent_session_id: &str) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.calls
            .send(Call::Close {
                agent_session_id: agent_session_id.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| "ACP 连接已关闭".to_string())?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    /// 关闭该 ACP 连接当前打开的全部会话。
    pub async fn close_all(&self) {
        let sessions: Vec<_> = self.opened_sessions.lock().iter().cloned().collect();
        for agent_session_id in sessions {
            if let Err(error) = self.close(&agent_session_id).await {
                log::warn!("session/close 失败（{agent_session_id}）: {error}");
            }
        }
    }

    pub async fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        if !self.supports_delete {
            log::debug!("Agent 不支持 session/delete，跳过: {agent_session_id}");
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.send(Call::Delete {
            agent_session_id: agent_session_id.to_string(),
            reply: tx,
        })?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    pub async fn set_config_option(
        &self,
        agent_session_id: &str,
        config_id: &str,
        value: SessionConfigOptionValue,
    ) -> Result<Vec<SessionConfigOption>, String> {
        let (tx, rx) = oneshot::channel();
        self.send(Call::SetConfigOption {
            agent_session_id: agent_session_id.to_string(),
            config_id: config_id.to_string(),
            value,
            reply: tx,
        })?;
        rx.await.map_err(|_| "ACP 连接已关闭".to_string())?
    }

    pub fn opened_session_count(&self) -> usize {
        self.opened_sessions.lock().len()
    }

    fn send(&self, call: Call) -> Result<(), String> {
        self.calls
            .try_send(call)
            .map_err(|_| "ACP 连接忙或已关闭".to_string())
    }
}

/// Agent 是否声明支持 `session/delete`（docs/DESIGN.md「普通会话删除」）。
fn supports_session_delete(
    capabilities: &agent_client_protocol::schema::v2::AgentCapabilities,
) -> bool {
    capabilities
        .session
        .as_ref()
        .and_then(|session| session.delete.as_ref())
        .is_some()
}

/// 建立与某 agent 的 ACP 连接：桥接 Daemon 通道、完成 initialize 后返回句柄。
pub async fn connect(
    machine: &str,
    agent: &str,
    outgoing: mpsc::Sender<String>,
    incoming: mpsc::Receiver<String>,
    events: mpsc::UnboundedSender<AcpEvent>,
    config: Option<amux_common::api::OrchestratorConfig>,
) -> Result<Arc<AgentConnection>, String> {
    let (calls_tx, mut calls_rx) = mpsc::channel::<Call>(64);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<bool, String>>();
    let opened_sessions = Arc::new(Mutex::new(HashSet::new()));
    let machine_name = machine.to_string();
    let agent_name = agent.to_string();
    let task_opened_sessions = Arc::clone(&opened_sessions);

    tokio::spawn(async move {
        let transport = DaemonTransport { outgoing, incoming };
        let notifications = {
            let events = events.clone();
            async move |notification: UpdateSessionNotification, _cx| {
                translate(&notification, &events);
                Ok(())
            }
        };
        let permissions = async move |request: RequestPermissionRequest,
                                      responder: agent_client_protocol::Responder<
            RequestPermissionResponse,
        >,
                                      _cx| {
            let outcome = pick_approve_option(&request.options)
                .map(|id| RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)))
                .unwrap_or(RequestPermissionOutcome::Cancelled);
            let _ = responder.respond(RequestPermissionResponse::new(outcome));
            Ok(())
        };

        let result = Client
            .v2()
            .name("amux-server")
            .on_receive_notification(
                notifications,
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(permissions, agent_client_protocol::on_receive_request!())
            .connect_with(transport, {
                let agent_name = agent_name.clone();
                let machine_name = machine_name.clone();
                move |cx: V2ConnectionTo<Agent>| async move {
                    let initialize = InitializeRequest::new(
                        ProtocolVersion::V2,
                        Implementation::new("amux-server", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(ClientCapabilities::default());
                    match cx.send_request(initialize).block_task().await {
                        Ok(response) => {
                            if agent_name == amux_common::api::NANO_AGENT {
                                let login = async {
                                    use amux_common::api::AMUX_AUTH_METHOD;
                                    if !response.auth_methods.iter().any(|method| {
                                        method.method_id().to_string() == AMUX_AUTH_METHOD
                                    }) {
                                        return Err("Nano 未声明配置认证方法".to_string());
                                    }
                                    let config = config.ok_or("内置智能体未配置")?;
                                    config.validate()?;
                                    request(
                                        &cx,
                                        agent_client_protocol::schema::v2::LoginAuthRequest::new(
                                            AMUX_AUTH_METHOD,
                                        )
                                        .meta(config.auth_meta()),
                                    )
                                    .await?;
                                    Ok::<_, String>(())
                                }
                                .await;
                                if let Err(error) = login {
                                    let _ = ready_tx.send(Err(error));
                                    return Ok(());
                                }
                            }
                            let supports_delete = supports_session_delete(&response.capabilities);
                            let _ = ready_tx.send(Ok(supports_delete));
                            log::info!("ACP 连接就绪: {agent_name}@{machine_name}");
                        }
                        Err(error) => {
                            let message = format!("initialize 失败: {error}");
                            let _ = ready_tx.send(Err(message.clone()));
                            return Err(agent_client_protocol::util::internal_error(message));
                        }
                    }

                    let mut calls = JoinSet::new();
                    loop {
                        tokio::select! {
                            call = calls_rx.recv() => {
                                let Some(call) = call else {
                                    break;
                                };
                                let cx = cx.clone();
                                let opened_sessions = Arc::clone(&task_opened_sessions);
                                calls.spawn(async move {
                                    // 所有调用独立并发，同一会话的冲突由 Agent 处理并将错误返回调用方。
                                    handle_call(call, &cx, &opened_sessions).await;
                                });
                            }
                            Some(result) = calls.join_next(), if !calls.is_empty() => {
                                if let Err(error) = result {
                                    log::warn!("ACP 调用任务异常结束: {error}");
                                }
                            }
                        }
                    }
                    // 连接结束时中止并回收在途调用，避免任务和请求内容滞留。
                    calls.shutdown().await;
                    Ok(())
                }
            })
            .await;
        log::info!("ACP 连接结束: {agent_name}@{machine_name} ({result:?})");
    });

    let supports_delete = ready_rx
        .await
        .map_err(|_| "ACP 连接启动失败".to_string())
        .and_then(|result| result)?;
    Ok(Arc::new(AgentConnection {
        calls: calls_tx,
        supports_delete,
        opened_sessions,
    }))
}

/// Daemon `acp` 通知 ↔ SDK `Lines` 传输的桥。
struct DaemonTransport {
    outgoing: mpsc::Sender<String>,
    incoming: mpsc::Receiver<String>,
}

impl ConnectTo<Client> for DaemonTransport {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let outgoing = sink::unfold(self.outgoing, |tx, line: String| async move {
            tx.send(line)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Daemon 连接已关闭"))?;
            Ok::<_, io::Error>(tx)
        });
        let mut incoming = self.incoming;
        let incoming = stream::poll_fn(move |cx| {
            incoming.poll_recv(cx).map(|item| {
                item.map(Ok::<_, io::Error>).or(Some(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Daemon 连接已关闭",
                ))))
            })
        });
        ConnectTo::<Client>::connect_to(Lines::new(outgoing, incoming), client).await
    }
}

async fn handle_call(
    call: Call,
    cx: &V2ConnectionTo<Agent>,
    opened_sessions: &Mutex<HashSet<String>>,
) {
    match call {
        Call::NewSession { cwd, reply } => match request(cx, NewSessionRequest::new(cwd)).await {
            Ok(response) => {
                let session_id = response.session_id.to_string();
                let options = config_options(response.config_options);
                opened_sessions.lock().insert(session_id.clone());
                if reply.send(Ok((session_id.clone(), options))).is_err() {
                    close_unclaimed_session(cx, opened_sessions, &session_id).await;
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        },
        Call::ResumeSession {
            agent_session_id,
            cwd,
            reply,
        } => match request(cx, ResumeSessionRequest::new(agent_session_id.clone(), cwd)).await {
            Ok(response) => {
                opened_sessions.lock().insert(agent_session_id.clone());
                if reply
                    .send(Ok(config_options(response.config_options)))
                    .is_err()
                {
                    close_unclaimed_session(cx, opened_sessions, &agent_session_id).await;
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        },
        Call::Prompt {
            agent_session_id,
            input,
            reply,
        } => {
            let result = request(cx, PromptRequest::new(agent_session_id, input))
                .await
                .map(|response| response.message_id.to_string());
            let _ = reply.send(result);
        }
        Call::Cancel { agent_session_id } => {
            if let Err(error) =
                cx.send_notification(CancelSessionNotification::new(agent_session_id))
            {
                log::warn!("session/cancel 发送失败: {error}");
            }
        }
        Call::Close {
            agent_session_id,
            reply,
        } => {
            let result = request(cx, CloseSessionRequest::new(agent_session_id.clone()))
                .await
                .map(|_| {
                    opened_sessions.lock().remove(&agent_session_id);
                })
                .map_err(|error| format!("session/close 失败: {error}"));
            let _ = reply.send(result);
        }
        Call::Delete {
            agent_session_id,
            reply,
        } => {
            let result = request(cx, DeleteSessionRequest::new(agent_session_id.clone()))
                .await
                .map(|_| {
                    opened_sessions.lock().remove(&agent_session_id);
                });
            let _ = reply.send(result);
        }
        Call::SetConfigOption {
            agent_session_id,
            config_id,
            value,
            reply,
        } => {
            let set_option = SetSessionConfigOptionRequest::new(
                agent_session_id,
                config_id,
                acp_config_value(&value),
            );
            let result = request(cx, set_option)
                .await
                .map(|response| config_options(response.config_options));
            let _ = reply.send(result);
        }
    }
}

/// 调用方已离开时关闭刚创建/恢复的会话，避免 Agent 侧和 `opened_sessions` 残留。
async fn close_unclaimed_session(
    cx: &V2ConnectionTo<Agent>,
    opened_sessions: &Mutex<HashSet<String>>,
    agent_session_id: &str,
) {
    match request(cx, CloseSessionRequest::new(agent_session_id.to_string())).await {
        Ok(_) => {
            opened_sessions.lock().remove(agent_session_id);
        }
        Err(error) => {
            log::warn!("未接管会话的 session/close 失败（{agent_session_id}）: {error}");
        }
    }
}

/// 单次 ACP 请求的等待上限：Daemon 断线期间连接保留（重连后复用），
/// 在途请求等不到应答，靠超时结束而不是永久挂起。
const ACP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn request<R: JsonRpcRequest>(
    cx: &V2ConnectionTo<Agent>,
    request: R,
) -> Result<R::Response, String> {
    tokio::time::timeout(ACP_REQUEST_TIMEOUT, cx.send_request(request).block_task())
        .await
        .map_err(|_| "ACP 请求超时".to_string())?
        .map_err(|error| error.to_string())
}

/// ACP 通知 → 事件。消息/活动保留增量或快照语义，由存储层读取磁盘后合并。
fn translate(notification: &UpdateSessionNotification, events: &mpsc::UnboundedSender<AcpEvent>) {
    let session_id = notification.session_id.to_string();
    let mut out: Vec<AcpEvent> = Vec::new();

    match &notification.update {
        // 用户消息由 Server 落盘（DESIGN：忽略 agent 回放的 user_message*）
        SessionUpdate::UserMessageChunk(_) | SessionUpdate::UserMessage(_) => {}
        SessionUpdate::AgentMessageChunk(chunk) => out.push(AcpEvent::Message {
            agent_session_id: session_id.clone(),
            message_id: chunk.message_id.to_string(),
            update: MessageUpdate::Chunk(Box::new(chunk.content.clone())),
        }),
        SessionUpdate::AgentMessage(message) => {
            if let Some(content) = snapshot_blocks(&message.content) {
                out.push(AcpEvent::Message {
                    agent_session_id: session_id.clone(),
                    message_id: message.message_id.to_string(),
                    update: MessageUpdate::Snapshot(content),
                });
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(text) = text_of(&chunk.content) {
                out.push(AcpEvent::Thinking {
                    agent_session_id: session_id.clone(),
                    message_id: chunk.message_id.to_string(),
                    update: ThinkingUpdate::Chunk(text),
                });
            }
        }
        SessionUpdate::AgentThought(thought) => {
            if let Some(text) = snapshot_text(&thought.content) {
                out.push(AcpEvent::Thinking {
                    agent_session_id: session_id.clone(),
                    message_id: thought.message_id.to_string(),
                    update: ThinkingUpdate::Snapshot(text),
                });
            }
        }
        SessionUpdate::CompactionUpdate(update) => out.push(AcpEvent::Compaction {
            agent_session_id: session_id.clone(),
            compaction_id: update.compaction_id.to_string(),
            status: update.status.clone(),
            update: CompactionPatch::Patch {
                summary: text_patch(&update.summary),
                error: update.error.clone(),
            },
        }),
        SessionUpdate::CompactionSummaryChunk(chunk) => out.push(AcpEvent::Compaction {
            agent_session_id: session_id.clone(),
            compaction_id: chunk.compaction_id.to_string(),
            status: CompactionStatus::InProgress,
            update: CompactionPatch::Chunk(text_of(&chunk.content).unwrap_or_default()),
        }),
        SessionUpdate::Notice(notice) => out.push(AcpEvent::Notice {
            agent_session_id: session_id.clone(),
            severity: notice.severity.clone(),
            title: notice.title.clone(),
            description: notice.description.clone(),
        }),
        SessionUpdate::StateUpdate(state) => match state {
            StateUpdate::Running(_) | StateUpdate::RequiresAction(_) => out.push(AcpEvent::State {
                agent_session_id: session_id.clone(),
                state: SessionState::Busy,
                reason: StateChangeReason::Completed,
            }),
            StateUpdate::Idle(idle) => {
                let reason = stop_reason_reason(idle.stop_reason.as_ref());
                if matches!(
                    idle.stop_reason.as_ref(),
                    Some(StopReason::Other(value)) if value == NANO_ERROR_STOP_REASON
                ) {
                    out.push(AcpEvent::Error {
                        agent_session_id: session_id.clone(),
                        message: nano_error_message(idle)
                            .unwrap_or_else(|| "agent 前台工作以错误结束".to_string()),
                    });
                }
                out.push(AcpEvent::State {
                    agent_session_id: session_id.clone(),
                    state: SessionState::Idle,
                    reason,
                });
            }
            _ => {}
        },
        SessionUpdate::ToolCallUpdate(update) => out.push(tool_call_event(&session_id, update)),
        SessionUpdate::UsageUpdate(update) => out.push(AcpEvent::Usage {
            agent_session_id: session_id.clone(),
            used: update.used,
            size: update.size,
        }),
        SessionUpdate::ConfigOptionUpdate(update) => out.push(AcpEvent::Options {
            agent_session_id: session_id.clone(),
            options: config_options(update.config_options.clone()),
        }),
        SessionUpdate::AvailableCommandsUpdate(update) => out.push(AcpEvent::Commands {
            agent_session_id: session_id.clone(),
            commands: slash_commands(&update.available_commands),
        }),
        SessionUpdate::PlanUpdate(update) => {
            if let PlanUpdateContent::Items(items) = &update.plan {
                out.push(AcpEvent::Plan {
                    agent_session_id: session_id.clone(),
                    entries: plan_entries(&items.entries),
                });
            }
        }
        _ => {}
    }

    for event in out {
        if events.send(event).is_err() {
            log::warn!("ACP 事件通道已关闭");
            break;
        }
    }
}

/// 工具调用生命周期由 status 决定（docs/DESIGN.md「普通会话实时活动」），
/// 即使本次 update 不带任何字段也要透传，不能只凭字段缺失丢弃。
fn tool_call_event(session_id: &str, update: &ToolCallUpdate) -> AcpEvent {
    // `MaybeUndefined`：Undefined 表示未携带（不修改），Null 表示显式清空。
    let name = update
        .name
        .as_opt_ref()
        .map(|value| value.cloned().unwrap_or_default());
    let kind = update.kind.as_opt_ref().and_then(|value| value.cloned());
    let title = update
        .title
        .as_opt_ref()
        .map(|value| value.cloned().unwrap_or_default());
    let parameters = update
        .raw_input
        .as_opt_ref()
        .map(|value| value.map(serde_json::Value::to_string).unwrap_or_default());
    AcpEvent::ToolCall {
        agent_session_id: session_id.to_string(),
        tool_call_id: update.tool_call_id.to_string(),
        status: update.status.as_opt_ref().and_then(|value| value.cloned()),
        name,
        kind,
        title,
        parameters,
    }
}

fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

fn join_text(blocks: &[ContentBlock]) -> Option<String> {
    let text: String = blocks.iter().filter_map(text_of).collect();
    (!text.is_empty()).then_some(text)
}

fn snapshot_blocks(
    content: &MaybeUndefined<Vec<ContentBlock>>,
) -> Option<Option<Vec<ContentBlock>>> {
    match content {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(blocks) => Some(Some(blocks.clone())),
    }
}

fn snapshot_text(content: &MaybeUndefined<Vec<ContentBlock>>) -> Option<Option<String>> {
    match content {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(blocks) => Some(join_text(blocks)),
    }
}

fn text_patch(content: &MaybeUndefined<Vec<ContentBlock>>) -> MaybeUndefined<String> {
    match content {
        MaybeUndefined::Undefined => MaybeUndefined::Undefined,
        MaybeUndefined::Null => MaybeUndefined::Null,
        MaybeUndefined::Value(blocks) => {
            MaybeUndefined::Value(join_text(blocks).unwrap_or_default())
        }
    }
}

fn stop_reason_reason(reason: Option<&StopReason>) -> StateChangeReason {
    match reason {
        Some(StopReason::Cancelled) => StateChangeReason::Cancelled,
        Some(StopReason::MaxTokens) => StateChangeReason::MaxTokens,
        Some(StopReason::MaxTurnRequests) => StateChangeReason::MaxTurnRequests,
        Some(StopReason::Refusal) => StateChangeReason::Refusal,
        Some(StopReason::Other(value)) if value == NANO_ERROR_STOP_REASON => {
            StateChangeReason::Aborted
        }
        _ => StateChangeReason::Completed,
    }
}

/// 从 Nano 的 idle `_meta` 中提取模型错误详情。
fn nano_error_message(idle: &IdleStateUpdate) -> Option<String> {
    idle.meta
        .as_ref()?
        .get(NANO_ERROR_META_KEY)?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// 权限自动审批：优先 allow 类选项（选项列表首项往往是「拒绝」）。
fn pick_approve_option(options: &[PermissionOption]) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        })
        .or_else(|| {
            options.iter().find(|option| {
                !matches!(
                    option.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                )
            })
        })
        .map(|option| option.option_id.clone())
}

fn acp_config_value(value: &SessionConfigOptionValue) -> AcpSessionConfigOptionValue {
    match value {
        SessionConfigOptionValue::ValueId { value } => AcpSessionConfigOptionValue::Id {
            value: agent_client_protocol::schema::v2::SessionConfigValueId::new(value.clone()),
        },
        SessionConfigOptionValue::Boolean { value } => {
            AcpSessionConfigOptionValue::Boolean { value: *value }
        }
    }
}

/// ACP 会话选项 → amux 投影（select 分组展平为平面列表）。
pub fn config_options(options: Vec<AcpSessionConfigOption>) -> Vec<SessionConfigOption> {
    options
        .into_iter()
        .map(|option| {
            let kind = match option.kind {
                AcpSessionConfigKind::Select(select) => {
                    let entries = match select.options {
                        SessionConfigSelectOptions::Ungrouped(list) => list
                            .into_iter()
                            .map(|entry| SessionConfigSelectEntry {
                                value: entry.value.0.to_string(),
                                name: entry.name,
                            })
                            .collect(),
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .into_iter()
                            .flat_map(|group| {
                                let prefix = format!("{} · ", group.name);
                                group.options.into_iter().map(move |entry| {
                                    SessionConfigSelectEntry {
                                        value: entry.value.0.to_string(),
                                        name: format!("{prefix}{}", entry.name),
                                    }
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    SessionConfigKind::Select {
                        current_value: select.current_value.0.to_string(),
                        options: entries,
                    }
                }
                AcpSessionConfigKind::Boolean(boolean) => SessionConfigKind::Boolean {
                    current_value: boolean.current_value,
                },
                _ => SessionConfigKind::Select {
                    current_value: String::new(),
                    options: Vec::new(),
                },
            };
            SessionConfigOption {
                id: option.config_id.0.to_string(),
                name: option.name,
                description: option.description,
                category: option
                    .category
                    .as_ref()
                    .and_then(|category| serde_json::to_value(category).ok())
                    .and_then(|value| value.as_str().map(str::to_string)),
                kind,
            }
        })
        .collect()
}

fn slash_commands(commands: &[AvailableCommand]) -> Vec<SlashCommand> {
    commands
        .iter()
        .map(|command| SlashCommand {
            name: command.name.clone(),
            description: command.description.clone(),
            hint: command.input.as_ref().and_then(|input| match input {
                AvailableCommandInput::Text(text) => Some(text.hint.clone()),
                _ => None,
            }),
        })
        .collect()
}

fn plan_entries(entries: &[agent_client_protocol::schema::v2::PlanEntry]) -> Vec<SessionPlanEntry> {
    use agent_client_protocol::schema::v2::{PlanEntryPriority, PlanEntryStatus};
    entries
        .iter()
        .map(|entry| SessionPlanEntry {
            content: entry.content.clone(),
            priority: match entry.priority {
                PlanEntryPriority::High => SessionPlanPriority::High,
                PlanEntryPriority::Low => SessionPlanPriority::Low,
                _ => SessionPlanPriority::Medium,
            },
            status: match entry.status {
                PlanEntryStatus::InProgress => SessionPlanStatus::InProgress,
                PlanEntryStatus::Completed => SessionPlanStatus::Completed,
                _ => SessionPlanStatus::Pending,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_mapping_covers_error_and_default() {
        assert_eq!(
            stop_reason_reason(Some(&StopReason::Cancelled)),
            StateChangeReason::Cancelled
        );
        assert_eq!(
            stop_reason_reason(Some(&StopReason::Other(NANO_ERROR_STOP_REASON.to_string()))),
            StateChangeReason::Aborted
        );
        assert_eq!(stop_reason_reason(None), StateChangeReason::Completed);
        assert_eq!(
            stop_reason_reason(Some(&StopReason::EndTurn)),
            StateChangeReason::Completed
        );
    }

    #[test]
    fn tool_call_event_uses_programmatic_name() {
        let update = ToolCallUpdate::new("call-1")
            .name("read_file")
            .title("读取配置")
            .kind(agent_client_protocol::schema::v2::ToolKind::Read);
        let AcpEvent::ToolCall { name, .. } = tool_call_event("s1", &update) else {
            panic!("应生成工具调用事件");
        };
        assert_eq!(name.as_deref(), Some("read_file"));
    }

    #[test]
    fn tool_call_event_preserves_name_clear() {
        let update = ToolCallUpdate::new("call-1").name(MaybeUndefined::Null);
        let AcpEvent::ToolCall { name, .. } = tool_call_event("s1", &update) else {
            panic!("清空名称也应生成工具调用事件");
        };
        assert_eq!(name.as_deref(), Some(""));
    }

    #[test]
    fn tool_call_event_carries_kind_for_fallback() {
        let update =
            ToolCallUpdate::new("call-1").kind(agent_client_protocol::schema::v2::ToolKind::Read);
        let AcpEvent::ToolCall { name, kind, .. } = tool_call_event("s1", &update) else {
            panic!("应生成工具调用事件");
        };
        assert!(name.is_none());
        assert_eq!(kind, Some(ToolKind::Read));
    }

    #[test]
    fn tool_call_event_preserves_typed_status() {
        let update = ToolCallUpdate::new("call-1").status(ToolCallStatus::InProgress);
        let AcpEvent::ToolCall { status, .. } = tool_call_event("s1", &update) else {
            panic!("应生成工具调用事件");
        };
        assert_eq!(status, Some(ToolCallStatus::InProgress));

        let update = ToolCallUpdate::new("call-1");
        let AcpEvent::ToolCall { status, .. } = tool_call_event("s1", &update) else {
            panic!("应生成工具调用事件");
        };
        assert_eq!(status, None, "未携带 status 的 update 不应改变生命周期");
    }

    #[test]
    fn session_delete_supported_only_when_advertised() {
        use agent_client_protocol::schema::v2::{
            AgentCapabilities, SessionCapabilities, SessionDeleteCapabilities,
        };
        assert!(!supports_session_delete(&AgentCapabilities::new()));

        let caps = AgentCapabilities::new()
            .session(SessionCapabilities::new().delete(SessionDeleteCapabilities::new()));
        assert!(supports_session_delete(&caps));
    }

    #[test]
    fn approve_option_prefers_allow_kinds() {
        let options = vec![
            permission(PermissionOptionKind::RejectOnce),
            permission(PermissionOptionKind::AllowOnce),
        ];
        let picked = pick_approve_option(&options).unwrap();
        assert_eq!(picked.0.to_string(), "allow-once");

        let options = vec![permission(PermissionOptionKind::AllowOnce)];
        assert!(pick_approve_option(&options).is_some());
        assert!(pick_approve_option(&[]).is_none());
    }

    fn permission(kind: PermissionOptionKind) -> PermissionOption {
        let id = serde_json::to_value(&kind)
            .ok()
            .and_then(|value| value.as_str().map(|text| text.replace('_', "-")))
            .unwrap_or_else(|| "x".to_string());
        PermissionOption::new(PermissionOptionId::new(id), "label", kind)
    }
}
