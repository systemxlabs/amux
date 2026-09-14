//! ACP v2 连接：官方 SDK `agent-client-protocol` 的 Client 角色（v2 surface），
//! 经 stdio 与 ACP server 子进程通信。
//!
//! `AcpConnection` 使用**专用 exec 线程**承载全部异步 IO（SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨
//! runtime 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。
//!
//! v2 语义要点：
//! - `session/prompt` 响应只表示**已受理**，前台工作结束由 `state_update` 的 `idle` 报告；
//! - `session/cancel` 是通知；
//! - 消息、工具调用、思考均按 `messageId` / `toolCallId` 的 upsert 语义增量下发。

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v2::{
    AvailableCommand, AvailableCommandInput, BlobResourceContents, CancelSessionNotification,
    ClientCapabilities, CloseSessionRequest, ContentBlock as AcpContentBlock, DeleteSessionRequest,
    EmbeddedResource, EmbeddedResourceResource, Implementation, InitializeRequest, MediaType,
    NewSessionRequest, PermissionOption, PermissionOptionId, PermissionOptionKind,
    PlanUpdateContent, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResourceLink, ResumeSessionRequest, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption as AcpSessionConfigOption,
    SessionConfigOptionValue as AcpSessionConfigOptionValue, SessionConfigSelectOptions,
    SessionUpdate, SetSessionConfigOptionRequest, StateUpdate, StopReason, TextContent,
    TextResourceContents, ToolCallUpdate, UpdateSessionNotification,
};
use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::AcpAgent;
use agent_client_protocol::V2ConnectionTo;
use tokio::sync::{mpsc, watch};

use protocol::{ContentBlock, SessionState};

/// 拉起的统计（server 启动日志用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct LaunchSummary {
    /// 成功拉起的 ACP server 数
    pub started: usize,
    /// 拉起失败的 agent 数（标记为**不可用**，agent.list 的 status=unavailable）
    pub failed: usize,
}

/// turn 过程中的 agent 事件，供 server 透传给 GUI 聚合与落盘。
///
/// v2 的流式内容带 `messageId`（消息/思考）或 `toolCallId`（工具调用），
/// 落盘按这些 id upsert，因此事件本身也是增量/替换语义。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 消息增量（`agent_message_chunk`：追加到同 `message_id` 的消息）
    AgentMessageChunk { message_id: String, text: String },
    /// agent 消息整条更新（`agent_message`）：全量替换同 `message_id` 的内容，
    /// `None` 表示清空。
    AgentMessageSnapshot {
        message_id: String,
        text: Option<String>,
    },
    /// 思考增量（`agent_thought_chunk`）
    ThinkingChunk { message_id: String, text: String },
    /// 思考整条更新（`agent_thought`）：`None` 表示清空
    ThinkingSnapshot {
        message_id: String,
        text: Option<String>,
    },
    /// 工具调用 upsert（`tool_call_update`，按 `tool_call_id` 合并；
    /// 未携带的字段保持不变）
    ToolCall {
        id: String,
        name: Option<String>,
        title: Option<String>,
        parameters: Option<String>,
    },
    /// 会话前台状态变更（`state_update`）。`idle` 表示前台工作结束，
    /// 携带结束原因；`running` / `requires_action` 表示工作中。
    StateUpdate {
        state: SessionState,
        reason: protocol::StateChangeReason,
    },
    /// ACP `session/prompt` 已受理（v2 的 prompt 响应只表示受理，
    /// 前台工作结束另由 `state_update(idle)` 报告）
    PromptAccepted,
    /// ACP 请求或传输失败
    Error(String),
    /// ACP `session/prompt` 请求被拒绝（前台工作未开始）
    PromptFailed(String),
    /// 会话上下文大小更新（`usage_update`）
    UsageUpdate {
        /// 当前在上下文中的 token 数
        used: u64,
        /// 上下文窗口总大小（token）
        size: u64,
    },
    /// 会话配置选项更新（`config_option_update`：完整的选项集合与当前值）
    ConfigOptions(Vec<protocol::SessionConfigOption>),
}

/// 主线程 → exec 线程的 ACP 方法调用。
#[derive(Debug, Clone)]
enum AcpCall {
    NewSession {
        cwd: String,
    },
    Resume {
        sid: String,
        cwd: String,
    },
    Cancel {
        sid: String,
    },
    Close {
        sid: String,
    },
    /// 删除 agent 侧会话（agent 不支持时返回错误，调用方按「不支持」忽略）
    Delete {
        sid: String,
    },
    /// 设置会话配置选项（ACP `session/set_config_option`）
    SetConfigOption {
        sid: String,
        config_id: String,
        value: protocol::SessionConfigOptionValue,
    },
}

/// ACP 方法响应（进程内强类型投影）。
#[derive(Debug, Clone)]
enum AcpResponse {
    NewSession {
        session_id: String,
        config_options: Vec<protocol::SessionConfigOption>,
    },
    /// resume / set_config_option 响应中的完整会话选项集合
    ConfigOptions(Vec<protocol::SessionConfigOption>),
    /// cancel / close / delete 无业务载荷
    Unit,
}

/// 一次 prompt 的事件流（含自身路由条目的注销能力）。
///
/// 路由条目按**通道身份**注销：同会话并发的多个 prompt 互不影响；注销由该 turn
/// 自己完成，而不是由 `state_update(idle)` 广播式回收。
pub struct PromptStream {
    rx: mpsc::Receiver<AgentEvent>,
    tx: mpsc::Sender<AgentEvent>,
    routes: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>>,
    agent_session_id: String,
}

impl PromptStream {
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.rx.recv().await
    }

    /// 注销本 turn 的路由条目（turn 结束时调用；幂等）。
    pub fn close(self) {
        remove_route(&self.routes, &self.agent_session_id, &self.tx);
    }
}

/// 主线程 → exec 线程的方法请求。
enum ExecReq {
    Call {
        call: AcpCall,
        resp: std::sync::mpsc::SyncSender<Result<AcpResponse, String>>,
    },
    Prompt {
        sid: String,
        prompt: Vec<ContentBlock>,
        routes: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>>,
        /// 本次 prompt 的事件发送端：state_update(idle) 按通道身份回收自己的路由条目
        tx: mpsc::Sender<AgentEvent>,
    },
}

/// ACP v2 客户端（官方 SDK stdio 传输）。
pub struct AcpConnection {
    /// 主线程 → exec 线程的请求发送端；连接结束或 shutdown 时置 None
    exec_tx: Arc<Mutex<Option<std::sync::mpsc::SyncSender<ExecReq>>>>,
    /// 连接级缓存与事件路由（与 exec 线程的通知处理器共享）
    caches: SessionCaches,
    /// 本进程内已 resume 过的会话（server 重启后从注册表恢复的会话首次交互前
    /// 经 ACP `session/resume` 恢复 agent 上下文。
    resumed: Arc<Mutex<HashSet<String>>>,
    /// 请求 exec 线程取消 ACP 连接并回收子进程
    stop_tx: watch::Sender<bool>,
    /// exec 线程句柄（Mutex 包装以便 `shutdown_and_join` 从 &self 取出并 join；
    /// 连接由 SDK 管理，线程结束即子进程清理）
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// 会话建立时 agent 侧声明的能力快照。能力由 initialize 握手的
/// `capabilities.session` 声明，按 agent sessionId 存档于连接级缓存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSessionCaps {
    /// agent 是否支持 `session/delete`
    pub delete: bool,
}

/// initialize 响应的 `capabilities` → 会话能力快照（纯函数，可单测）。
fn session_caps_from_agent_caps(
    caps: &agent_client_protocol::schema::v2::AgentCapabilities,
) -> AgentSessionCaps {
    AgentSessionCaps {
        delete: caps
            .session
            .as_ref()
            .and_then(|session| session.delete.as_ref())
            .is_some(),
    }
}

/// `session/update` 通知处理器共享的连接级状态（连接与 exec 线程各持一份克隆）。
#[derive(Clone)]
struct SessionCaches {
    /// 会话事件路由：agent sessionId -> 该会话进行中 prompt 的事件接收端。
    /// `state_update(idle)` 到达时回收该会话全部路由（前台工作已结束）。
    routes: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>>,
    /// 会话斜杠命令：agent sessionId -> 最近一次 `available_commands_update`
    /// 的全量集合。缓存在连接层而非事件流——通知可能出现在无 prompt 路由的
    /// 窗口（如 session/new 后 agent 立即下发）。
    commands: Arc<Mutex<HashMap<String, Vec<protocol::SlashCommand>>>>,
    /// 会话计划：agent sessionId -> 最近一次 `plan_update` 通知的全量条目（缓存理由同上）。
    plans: Arc<Mutex<HashMap<String, Vec<protocol::SessionPlanEntry>>>>,
    /// ACP 连接是否仍可接受新的调用；断连时先标记失效，再清理事件路由。
    alive: Arc<AtomicBool>,
    /// initialize 握手声明的连接默认能力（会话建立时按 sessionId 落档）
    default_caps: Arc<Mutex<AgentSessionCaps>>,
    /// 会话能力：agent sessionId -> 建立时 agent 侧声明的快照
    caps: Arc<Mutex<HashMap<String, AgentSessionCaps>>>,
}

impl Default for SessionCaches {
    fn default() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
            commands: Arc::new(Mutex::new(HashMap::new())),
            plans: Arc::new(Mutex::new(HashMap::new())),
            alive: Arc::new(AtomicBool::new(true)),
            default_caps: Arc::new(Mutex::new(AgentSessionCaps::default())),
            caps: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SessionCaches {
    /// 清除已关闭会话的连接级缓存。`caps` 由调用方在
    /// `session/delete` 判断能力后再清除。
    fn remove_closed_session(&self, session_id: &str) {
        self.routes.lock().remove(session_id);
        self.commands.lock().remove(session_id);
        self.plans.lock().remove(session_id);
    }

    /// ACP 连接结束时释放全部会话缓存，并唤醒仍等待 agent 事件的 turn。
    fn clear_on_disconnect(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let routes = std::mem::take(&mut *self.routes.lock());
        for (_, txs) in routes {
            for tx in txs {
                send_disconnect_events(tx);
            }
        }
        self.commands.lock().clear();
        self.plans.lock().clear();
        self.caps.lock().clear();
        *self.default_caps.lock() = AgentSessionCaps::default();
    }
}

/// 按通道身份从会话路由表中移除本次 turn 的发送端，返回被移除的 sender。
/// 与 `state_update(idle)` 广播式回收不同：同会话并发的多个 prompt 互不影响；
/// 移除后该会话路由为空时删除整条路由（幂等）。
fn remove_route(
    routes: &Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>,
    session_id: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Option<mpsc::Sender<AgentEvent>> {
    let mut routes = routes.lock();
    let txs = routes.get_mut(session_id)?;
    let idx = txs.iter().position(|s| s.same_channel(tx))?;
    let removed = txs.remove(idx);
    if txs.is_empty() {
        routes.remove(session_id);
    }
    Some(removed)
}

fn send_disconnect_events(tx: mpsc::Sender<AgentEvent>) {
    let error = AgentEvent::Error("ACP 连接已关闭".into());
    let ended = AgentEvent::StateUpdate {
        state: SessionState::Idle,
        reason: protocol::StateChangeReason::Aborted,
    };
    match tx.try_send(error) {
        Ok(()) => match tx.try_send(ended) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                std::thread::spawn(move || {
                    let _ = tx.blocking_send(event);
                });
            }
        },
        Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(error)) => {
            std::thread::spawn(move || {
                if tx.blocking_send(error).is_ok() {
                    let _ = tx.blocking_send(ended);
                }
            });
        }
    }
}

impl AcpConnection {
    /// 启动 ACP agent 子进程（官方 SDK `AcpAgent` 管理 stdio 传输与进程生命周期）；
    /// `env` 为附加环境变量（经 from_args 的 `NAME=value` 前缀传入）；
    /// exec 线程承载全部异步 IO。
    ///
    /// **同步就绪握手**：阻塞等待 exec 线程完成「子进程拉起 + 连接建立 + initialize
    /// 握手」后才返回——二进制缺失 / 进程立即退出（如 npx 不可用、无网络）在此快速
    /// 失败并返回明确错误；健康 agent 在握手完成后立即返回。等待受
    /// `AMUX_ACP_SPAWN_TIMEOUT_MS` 限制（默认 30s；npx 首次按需下载可能较慢，
    /// 超时按失败处理，`connection_for` 兜底会重试）。
    pub fn spawn(bin: &str, args: &[&str], env: &[(String, String)]) -> Result<Self, String> {
        Self::spawn_with_shutdown(bin, args, env, None)
    }

    /// 启动 ACP agent，并在 registry 关闭时中断尚未完成的 initialize 握手。
    pub fn spawn_with_shutdown(
        bin: &str,
        args: &[&str],
        env: &[(String, String)],
        shutdown: Option<Arc<AtomicBool>>,
    ) -> Result<Self, String> {
        let (exec_sender, exec_rx) = std::sync::mpsc::sync_channel::<ExecReq>(32);
        let exec_tx = Arc::new(Mutex::new(Some(exec_sender)));
        let exec_tx2 = exec_tx.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = watch::channel(false);
        let caches = SessionCaches::default();
        let caches2 = caches.clone();
        let bin = bin.to_string();
        let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let env = env.to_vec();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("构建 tokio runtime 失败");
            rt.block_on(exec_main(
                &bin,
                &args,
                &env,
                ExecControl {
                    exec_rx,
                    exec_tx: exec_tx2,
                    stop_rx,
                    ready_tx,
                },
                caches2,
            ));
        });
        let timeout_ms = std::env::var("AMUX_ACP_SPAWN_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000);
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(timeout_ms))
            .unwrap_or_else(std::time::Instant::now);
        let startup_error = loop {
            if shutdown
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                break Some("ACP server 启动已取消：agent registry 正在关闭".to_string());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Some(format!(
                    "ACP server 启动超时（{timeout_ms}ms 内未完成连接/initialize 握手）"
                ));
            }
            match ready_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(100))) {
                Ok(Ok(())) => break None,
                Ok(Err(e)) => break Some(e),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Some("ACP 启动线程提前退出".to_string())
                }
            }
        };
        if let Some(error) = startup_error {
            let _ = exec_tx.lock().take();
            let _ = stop_tx.send(true);
            let _ = thread.join();
            return Err(error);
        }
        Ok(AcpConnection {
            exec_tx,
            caches,
            resumed: Arc::new(Mutex::new(HashSet::new())),
            stop_tx,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn sender(&self) -> Result<std::sync::mpsc::SyncSender<ExecReq>, String> {
        if !self.caches.alive.load(Ordering::SeqCst) {
            return Err("agent 已关闭".to_string());
        }
        self.exec_tx
            .lock()
            .clone()
            .ok_or_else(|| "agent 已关闭".to_string())
    }

    /// 同步方法调用：请求发往 exec 线程，阻塞等待响应。
    fn call(&self, call: AcpCall) -> Result<AcpResponse, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<AcpResponse, String>>(1);
        self.sender()?
            .send(ExecReq::Call { call, resp: tx })
            .map_err(|_| "agent 已关闭".to_string())?;
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !self.caches.alive.load(Ordering::SeqCst) {
                        return Err("agent 已关闭".to_string());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("ACP 调用执行失败".to_string());
                }
            }
        }
    }
}

impl AcpConnection {
    /// 新建会话，返回 agent 侧会话 id 与初始配置选项。
    pub fn create_session(
        &self,
        cwd: &str,
    ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
        let res = self.call(AcpCall::NewSession {
            cwd: cwd.to_string(),
        })?;
        let AcpResponse::NewSession {
            session_id: sid,
            config_options: options,
        } = res
        else {
            return Err("session/new 响应类型不符".to_string());
        };
        // 新会话 agent 已在内存中持有，无需 resume
        self.resumed.lock().insert(sid.clone());
        Ok((sid, options))
    }

    /// 恢复 agent 自身上下文（ACP `session/resume`，不带 `replayFrom`：只恢复上下文、
    /// 不重放历史——历史以 server 本地存储为权威。
    /// 同一进程内对同一会话幂等（已恢复过则直接成功），返回会话配置选项。
    pub fn resume_session(
        &self,
        agent_session_id: &str,
        cwd: &str,
    ) -> Result<Vec<protocol::SessionConfigOption>, String> {
        {
            let resumed = self.resumed.lock();
            if resumed.contains(agent_session_id) {
                return Ok(Vec::new());
            }
        }
        let result = self.call(AcpCall::Resume {
            sid: agent_session_id.to_string(),
            cwd: cwd.to_string(),
        });
        if result.is_ok() {
            self.resumed.lock().insert(agent_session_id.to_string());
        }
        result.map(|res| match res {
            AcpResponse::ConfigOptions(options) => options,
            _ => Vec::new(),
        })
    }

    pub fn prompt(&self, agent_session_id: &str, input: Vec<ContentBlock>) -> PromptStream {
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);
        let registered = {
            let mut routes = self.caches.routes.lock();
            if self.caches.alive.load(Ordering::SeqCst) {
                routes
                    .entry(agent_session_id.to_string())
                    .or_default()
                    .push(tx.clone());
                true
            } else {
                false
            }
        };
        if !registered {
            let _ = tx.try_send(AgentEvent::Error("agent 已关闭".into()));
            let _ = tx.try_send(AgentEvent::StateUpdate {
                state: SessionState::Idle,
                reason: protocol::StateChangeReason::Aborted,
            });
            return PromptStream {
                rx,
                tx,
                routes: self.caches.routes.clone(),
                agent_session_id: agent_session_id.to_string(),
            };
        }
        let req = ExecReq::Prompt {
            sid: agent_session_id.to_string(),
            prompt: input,
            routes: self.caches.routes.clone(),
            tx: tx.clone(),
        };
        if let Err(error) = self
            .sender()
            .and_then(|sender| sender.send(req).map_err(|_| "agent 已关闭".to_string()))
        {
            // 只回收本次 prompt 自己的路由条目；同会话其他在途 prompt 不受影响
            if let Some(tx) = remove_route(&self.caches.routes, agent_session_id, &tx) {
                let _ = tx.try_send(AgentEvent::Error(error));
                let _ = tx.try_send(AgentEvent::StateUpdate {
                    state: SessionState::Idle,
                    reason: protocol::StateChangeReason::Aborted,
                });
            }
        }
        PromptStream {
            rx,
            tx,
            routes: self.caches.routes.clone(),
            agent_session_id: agent_session_id.to_string(),
        }
    }

    pub fn shutdown(&self) {
        self.caches.alive.store(false, Ordering::SeqCst);
        let _ = self.stop_tx.send(true);
        let _ = self.exec_tx.lock().take();
        self.caches.clear_on_disconnect();
    }

    /// 关闭并等待 exec 线程退出：通道关闭 → 服务循环结束 → SDK 连接 drop（子进程
    /// 随之回收）。server 退出路径调用，保证清理先于进程退出完成。
    pub fn shutdown_and_join(&self) {
        self.shutdown();
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }

    /// 取消会话前台工作：ACP v2 中 `session/cancel` 是**通知**，
    /// 结束以随后到达的 `state_update(idle)` 为准。
    pub fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.call(AcpCall::Cancel {
            sid: agent_session_id.to_string(),
        })
        .map(|_| ())
    }

    pub fn close(&self, agent_session_id: &str) -> Result<(), String> {
        self.resumed.lock().remove(agent_session_id);
        // Keep the capability snapshot until delete_session() has checked it:
        // close is intentionally followed by session/delete on supported agents.
        self.caches.remove_closed_session(agent_session_id);
        self.call(AcpCall::Close {
            sid: agent_session_id.to_string(),
        })
        .map(|_| ())
    }

    /// 删除 agent 侧会话：close 之后，agent 支持
    /// 删除才调用；不支持删除的 agent 返回 METHOD_NOT_FOUND 类错误，调用方忽略）。
    pub fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
        // 会话建立时 agent 未声明 `capabilities.session.delete`：不发请求直接报不支持
        //（调用方按「不支持」忽略）。
        if !self.session_caps(agent_session_id).delete {
            self.caches.caps.lock().remove(agent_session_id);
            return Err(
                "agent 不支持 session/delete（initialize 未声明 capabilities.session.delete）"
                    .into(),
            );
        }
        let result = self
            .call(AcpCall::Delete {
                sid: agent_session_id.to_string(),
            })
            .map(|_| ());
        // 本地会话已经删除，无论 agent 是否接受 delete，都不能继续保留
        // 连接级缓存；否则每个失败的删除都会永久占用一份能力快照。
        self.caches.caps.lock().remove(agent_session_id);
        result
    }

    /// 设置会话配置选项（ACP `session/set_config_option`），返回更新后的完整选项集合。
    pub fn set_config_option(
        &self,
        agent_session_id: &str,
        config_id: &str,
        value: protocol::SessionConfigOptionValue,
    ) -> Result<Vec<protocol::SessionConfigOption>, String> {
        self.call(AcpCall::SetConfigOption {
            sid: agent_session_id.to_string(),
            config_id: config_id.to_string(),
            value,
        })
        .map(|res| match res {
            AcpResponse::ConfigOptions(options) => options,
            _ => Vec::new(),
        })
    }

    pub fn available_commands(&self, agent_session_id: &str) -> Vec<protocol::SlashCommand> {
        self.caches
            .commands
            .lock()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn session_plan(&self, agent_session_id: &str) -> Vec<protocol::SessionPlanEntry> {
        self.caches
            .plans
            .lock()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn session_caps(&self, agent_session_id: &str) -> AgentSessionCaps {
        self.caches
            .caps
            .lock()
            .get(agent_session_id)
            .copied()
            .unwrap_or(*self.caches.default_caps.lock())
    }
}

/// 权限自动审批：从请求选项中选出要批准的选项（纯函数，可单测）。
/// 优先 `AllowAlways` > `AllowOnce` > 任意非拒绝选项；
/// 全为拒绝选项或列表为空 → `None`（无批准项，按取消处理）。
fn pick_approve_option(options: &[PermissionOption]) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
        })
        .or_else(|| {
            options.iter().find(|o| {
                !matches!(
                    o.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                )
            })
        })
        .map(|o| o.option_id.clone())
}

struct ExecControl {
    exec_rx: std::sync::mpsc::Receiver<ExecReq>,
    exec_tx: Arc<Mutex<Option<std::sync::mpsc::SyncSender<ExecReq>>>>,
    stop_rx: watch::Receiver<bool>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
}

/// exec 线程主循环：经官方 SDK 建立 ACP v2 连接，承载方法分发、通知路由与权限批准。
/// `ready_tx`：就绪握手——连接建立（子进程拉起）且 initialize 握手完成后发送结果；
/// 若连接在握手前就失败（二进制缺失 / 进程立即退出），在此补发 `Err` 供
/// `AcpConnection::spawn` 同步快速失败，而非等满超时。
async fn exec_main(
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    control: ExecControl,
    caches: SessionCaches,
) {
    let ExecControl {
        exec_rx,
        exec_tx,
        mut stop_rx,
        ready_tx,
    } = control;
    // std exec_rx → tokio 通道（阻塞转发，供 select 使用）
    let (req_tx, mut req_rx) = mpsc::channel::<ExecReq>(32);
    tokio::task::spawn_blocking(move || {
        while let Ok(req) = exec_rx.recv() {
            if req_tx.blocking_send(req).is_err() {
                break;
            }
        }
    });

    // 就绪信号只发一次：connect_main 内的握手完成发一次；连接在握手前夭折时
    // 由下方补发失败（swap 保证不重复发送）。
    let ready_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // SDK from_args 支持 `NAME=value` 前缀参数作为环境变量。
    let mut cmd: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    cmd.push(bin.to_string());
    cmd.extend(args.iter().cloned());
    let agent = match AcpAgent::from_args(cmd) {
        Ok(a) => a,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("解析 agent 命令失败 ({bin}): {e}")));
            log::error!("解析 agent 命令失败 ({bin}): {e}");
            return;
        }
    };
    log::info!("已连接 ACP agent: {bin} {}", args.join(" "));
    // trace 级记录 ACP 原始帧，便于排查跨进程协议问题。
    let agent = if log::log_enabled!(log::Level::Trace) {
        agent.with_debug(|line, direction| {
            log::trace!("{direction:?} {line}");
        })
    } else {
        agent
    };

    let result = tokio::select! {
        result = connect_main(
            agent,
            &mut req_rx,
            caches.clone(),
            &ready_tx,
            ready_sent.clone(),
        ) => result,
        changed = stop_rx.changed() => {
            let _ = changed;
            Err(agent_client_protocol::util::internal_error(
                "ACP 连接已停止",
            ))
        }
    };
    // ACP 连接关闭后，先阻止新的调用进入队列，再唤醒已有 turn。
    caches.alive.store(false, Ordering::SeqCst);
    exec_tx.lock().take();
    caches.clear_on_disconnect();

    // 连接异常结束：若就绪信号尚未发出（连接建立前传输层失败：二进制缺失 /
    // 进程立即退出 / npx 不可用 / 无网络），补报为 spawn 失败；若已报过就绪，
    // 之后的连接异常仅记录，不影响已缓存的连接。
    if let core::result::Result::Err(e) = &result {
        log::error!("ACP 连接异常结束: {e}");
        if !ready_sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = ready_tx.send(Err(format!("ACP 连接失败: {e}")));
        }
    }
}

async fn connect_main(
    agent: AcpAgent,
    req_rx: &mut mpsc::Receiver<ExecReq>,
    caches: SessionCaches,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
    ready_sent: Arc<std::sync::atomic::AtomicBool>,
) -> agent_client_protocol::Result<()> {
    agent_client_protocol::Client
        .v2()
        .name("amux-server")
        .on_receive_notification(
            {
                let caches = caches.clone();
                async move |notif: UpdateSessionNotification, _cx| {
                    let SessionCaches {
                        routes,
                        commands,
                        plans,
                        ..
                    } = caches.clone();
                    route_update(&routes, &commands, &plans, &notif).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                // 自动审批，避免额外的审批往返。
                // 必须选 allow 类选项：选项列表**第一项往往是「拒绝」**，
                // 选第一个会被 agent 误判为用户拒绝。
                let outcome = pick_approve_option(&request.options)
                    .map(|id| {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                    })
                    .unwrap_or(RequestPermissionOutcome::Cancelled);
                let _ = responder.respond(RequestPermissionResponse::new(outcome));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, {
            let caches = caches.clone();
            move |cx: V2ConnectionTo<agent_client_protocol::Agent>| async move {
                let caches = caches;
                let req_rx = req_rx;
                // 初始化握手（版本协商）。失败需区分两种情形：
                // - **协议级失败**（agent 存活但不实现 initialize）：仅记录、连接保持可用；
                // - **传输层失败**（进程已退出 / 连接已死，如 npx 不可用、无网络）：拉起失败。
                // 二者用短窗口探测连接活性区分：incoming_closed 在传输层关闭后很快完成，
                // 超时则连接仍存活。
                // 客户端能力为空：不使用文件系统/终端反向能力，会话选项由 ACP 会话提供。
                let init_request = InitializeRequest::new(
                    ProtocolVersion::V2,
                    Implementation::new("amux-server", env!("CARGO_PKG_VERSION")),
                )
                .capabilities(ClientCapabilities::default());
                let init_result = match cx.send_request(init_request).block_task().await {
                    core::result::Result::Ok(resp) => {
                        // 记录 agent 侧声明的连接默认能力，供后续会话建立时复制。
                        *caches.default_caps.lock() =
                            session_caps_from_agent_caps(&resp.capabilities);
                        log::debug!("initialize 完成");
                        core::result::Result::Ok(())
                    }
                    core::result::Result::Err(e) => {
                        let alive = tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            cx.incoming_closed(),
                        )
                        .await
                        .is_err();
                        if alive {
                            log::error!("initialize 失败（继续）: {e}");
                            core::result::Result::Ok(())
                        } else {
                            core::result::Result::Err(format!(
                                "initialize 握手失败（连接已关闭）: {e}"
                            ))
                        }
                    }
                };
                let _ = ready_tx.send({
                    ready_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                    init_result.clone()
                });
                if let Err(e) = init_result {
                    return Err(agent_client_protocol::util::internal_error(e));
                }

                // 服务循环：每个请求独立 spawn，支持并发（cancel 不必等 prompt 完成）
                loop {
                    let Some(req) = req_rx.recv().await else {
                        break;
                    };
                    match req {
                        ExecReq::Call { call, resp } => {
                            let cx = cx.clone();
                            let caches = caches.clone();
                            tokio::spawn(async move {
                                let result = dispatch_call(&cx, &call).await;
                                // 会话建立成功：按 sessionId 落档建立时的 agent 侧能力
                                if let Ok(AcpResponse::NewSession { session_id, .. }) = &result {
                                    let caps = *caches.default_caps.lock();
                                    match call {
                                        AcpCall::NewSession { .. } => {
                                            caches.caps.lock().insert(session_id.clone(), caps);
                                        }
                                        AcpCall::Resume { sid, .. } => {
                                            caches.caps.lock().entry(sid).or_insert(caps);
                                        }
                                        _ => {}
                                    }
                                }
                                let _ = resp.send(result);
                            });
                        }
                        ExecReq::Prompt {
                            sid,
                            prompt,
                            routes,
                            tx,
                        } => {
                            let cx = cx.clone();
                            tokio::spawn(async move {
                                let blocks = prompt
                                    .iter()
                                    .filter_map(acp_content_block)
                                    .collect::<Vec<_>>();
                                // v2 的 prompt 响应只表示受理；前台工作结束由
                                // `state_update(idle)` 报告（通知路由据此回收路由）。
                                match cx
                                    .send_request(PromptRequest::new(sid.clone(), blocks))
                                    .block_task()
                                    .await
                                {
                                    Ok(_) => {
                                        log::debug!("prompt 已受理 {sid}");
                                        let _ = tx.send(AgentEvent::PromptAccepted).await;
                                    }
                                    Err(e) => {
                                        log::error!("prompt 调用失败 {sid}: {e}");
                                        // 路由回收沿用它自己的通道身份：同会话其他在途 prompt 不受影响
                                        let removed = remove_route(&routes, &sid, &tx);
                                        if let Some(tx) = removed {
                                            let _ = tx
                                                .send(AgentEvent::PromptFailed(format!(
                                                    "ACP prompt 失败: {e}"
                                                )))
                                                .await;
                                            let _ = tx
                                                .send(AgentEvent::StateUpdate {
                                                    state: SessionState::Idle,
                                                    reason: protocol::StateChangeReason::Aborted,
                                                })
                                                .await;
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
                core::result::Result::Ok(())
            }
        })
        .await
}

/// 分发 ACP v2 方法调用（强类型 AcpCall，经官方 SDK 传输）。
async fn dispatch_call(
    cx: &V2ConnectionTo<agent_client_protocol::Agent>,
    call: &AcpCall,
) -> Result<AcpResponse, String> {
    let label = match call {
        AcpCall::NewSession { .. } => "session/new",
        AcpCall::Resume { .. } => "session/resume",
        AcpCall::Cancel { .. } => "session/cancel",
        AcpCall::Close { .. } => "session/close",
        AcpCall::Delete { .. } => "session/delete",
        AcpCall::SetConfigOption { .. } => "session/set_config_option",
    };
    log::debug!("调用 {label}");
    let result = dispatch_call_inner(cx, call).await;
    match &result {
        Ok(_) => log::debug!("{label} 成功"),
        Err(e) => log::error!("{label} 失败: {e}"),
    }
    result
}

async fn dispatch_call_inner(
    cx: &V2ConnectionTo<agent_client_protocol::Agent>,
    call: &AcpCall,
) -> Result<AcpResponse, String> {
    match call {
        AcpCall::NewSession { cwd } => {
            let resp = cx
                .send_request(NewSessionRequest::new(cwd.as_str()))
                .block_task()
                .await
                .map_err(|e| format!("session/new 失败: {e}"))?;
            Ok(AcpResponse::NewSession {
                session_id: resp.session_id.to_string(),
                config_options: acp_config_options(resp.config_options),
            })
        }
        AcpCall::Resume { sid, cwd } => {
            // 不带 replayFrom：只恢复 agent 上下文，历史以 server 本地存储为权威
            let resp = cx
                .send_request(ResumeSessionRequest::new(sid.as_str(), cwd.as_str()))
                .block_task()
                .await
                .map_err(|e| format!("session/resume 失败: {e}"))?;
            Ok(AcpResponse::ConfigOptions(acp_config_options(
                resp.config_options,
            )))
        }
        AcpCall::Cancel { sid } => {
            cx.send_notification(CancelSessionNotification::new(sid.as_str()))
                .map_err(|e| format!("session/cancel 失败: {e}"))?;
            Ok(AcpResponse::Unit)
        }
        AcpCall::Close { sid } => {
            cx.send_request(CloseSessionRequest::new(sid.as_str()))
                .block_task()
                .await
                .map_err(|e| format!("session/close 失败: {e}"))?;
            Ok(AcpResponse::Unit)
        }
        AcpCall::Delete { sid } => {
            // 仅 agent 声明 `capabilities.session.delete` 时可用；不支持时返回错误，
            // 调用方（会话删除路径）按「不支持删除」忽略。
            cx.send_request(DeleteSessionRequest::new(sid.as_str()))
                .block_task()
                .await
                .map_err(|e| format!("session/delete 失败（agent 可能不支持删除）: {e}"))?;
            Ok(AcpResponse::Unit)
        }
        AcpCall::SetConfigOption {
            sid,
            config_id,
            value,
        } => {
            let acp_value = match value {
                protocol::SessionConfigOptionValue::ValueId { value } => {
                    AcpSessionConfigOptionValue::Id {
                        value: agent_client_protocol::schema::v2::SessionConfigValueId::new(
                            value.as_str(),
                        ),
                    }
                }
                protocol::SessionConfigOptionValue::Boolean { value } => {
                    AcpSessionConfigOptionValue::Boolean { value: *value }
                }
            };
            let resp = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    sid.as_str(),
                    config_id.as_str(),
                    acp_value,
                ))
                .block_task()
                .await
                .map_err(|e| format!("session/set_config_option 失败: {e}"))?;
            Ok(AcpResponse::ConfigOptions(acp_config_options(
                resp.config_options,
            )))
        }
    }
}

/// ACP stopReason → 状态变更原因。
/// 未识别的新枚举值按正常结束处理（仅 cancelled 参与注入过滤）。
fn stop_reason_reason(reason: Option<&StopReason>) -> protocol::StateChangeReason {
    match reason {
        Some(StopReason::Cancelled) => protocol::StateChangeReason::Cancelled,
        Some(StopReason::MaxTokens) => protocol::StateChangeReason::MaxTokens,
        Some(StopReason::MaxTurnRequests) => protocol::StateChangeReason::MaxTurnRequests,
        Some(StopReason::Refusal) => protocol::StateChangeReason::Refusal,
        // 实现自定义的 `_error`：前台工作以错误结束
        Some(StopReason::Other(value)) if value == ERROR_STOP_REASON => {
            protocol::StateChangeReason::Aborted
        }
        None => protocol::StateChangeReason::Completed,
        Some(_) => protocol::StateChangeReason::Completed,
    }
}

/// agent 以错误结束前台工作时上报的 stopReason（实现自定义值）。
const ERROR_STOP_REASON: &str = "_error";

/// 把 ACP `session/update` 通知映射为 AgentEvent 并路由。
/// `state_update(idle)` 表示前台工作结束：事件送达后回收该会话的全部路由，
/// 使各在途 turn 的事件流随之结束。
async fn route_update(
    routes: &Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>,
    commands: &Mutex<HashMap<String, Vec<protocol::SlashCommand>>>,
    plans: &Mutex<HashMap<String, Vec<protocol::SessionPlanEntry>>>,
    notif: &UpdateSessionNotification,
) {
    let session_id = notif.session_id.to_string();
    // 一次通知可能同时产生多个事件。
    let mut evs: Vec<AgentEvent> = Vec::new();
    let mut idle = false;
    match &notif.update {
        // 用户消息由 server 直接落盘，不重复进入历史与活动流。
        SessionUpdate::UserMessageChunk(_) | SessionUpdate::UserMessage(_) => {}
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let Some(text) = text_of(&chunk.content) {
                evs.push(InlineEventKind::Message.chunk(&chunk.message_id, text));
            }
        }
        SessionUpdate::AgentMessage(message) => {
            if let Some(text) = snapshot_text(&message.content) {
                evs.push(InlineEventKind::Message.snapshot(&message.message_id, text));
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(text) = text_of(&chunk.content) {
                evs.push(InlineEventKind::Thinking.chunk(&chunk.message_id, text));
            }
        }
        SessionUpdate::AgentThought(thought) => {
            if let Some(text) = snapshot_text(&thought.content) {
                evs.push(InlineEventKind::Thinking.snapshot(&thought.message_id, text));
            }
        }
        SessionUpdate::StateUpdate(state) => match state {
            StateUpdate::Running(_) | StateUpdate::RequiresAction(_) => {
                evs.push(AgentEvent::StateUpdate {
                    state: SessionState::Busy,
                    reason: protocol::StateChangeReason::Completed,
                });
            }
            StateUpdate::Idle(idle_state) => {
                idle = true;
                let reason = stop_reason_reason(idle_state.stop_reason.as_ref());
                // 错误结束的前台工作：先记一条错误活动，再以上报状态结束本轮
                if matches!(
                    idle_state.stop_reason.as_ref(),
                    Some(StopReason::Other(value)) if value == ERROR_STOP_REASON
                ) {
                    evs.push(AgentEvent::Error(
                        "agent 前台工作以错误结束（stopReason=_error）".to_string(),
                    ));
                }
                evs.push(AgentEvent::StateUpdate {
                    state: SessionState::Idle,
                    reason,
                });
            }
            _ => {}
        },
        SessionUpdate::ToolCallUpdate(update) => {
            if let Some(event) = tool_call_event(update) {
                evs.push(event);
            }
        }
        SessionUpdate::UsageUpdate(update) => evs.push(AgentEvent::UsageUpdate {
            used: update.used,
            size: update.size,
        }),
        // `config_option_update`：会话配置选项变更（完整集合）。
        SessionUpdate::ConfigOptionUpdate(update) => evs.push(AgentEvent::ConfigOptions(
            acp_config_options(update.config_options.clone()),
        )),
        // `available_commands_update`：斜杠命令全量覆盖连接内存缓存
        // 以 Agent 侧数据为权威。不产生事件流——通知可能出现在无 prompt 路由的窗口。
        SessionUpdate::AvailableCommandsUpdate(update) => {
            commands.lock().insert(
                session_id.clone(),
                acp_slash_commands(&update.available_commands),
            );
        }
        // `plan_update`：agent 计划全量覆盖连接内存缓存（仅 items 型计划有对应投影）。
        SessionUpdate::PlanUpdate(update) => {
            if let PlanUpdateContent::Items(items) = &update.plan {
                plans
                    .lock()
                    .insert(session_id.clone(), acp_plan(&items.entries));
            }
        }
        // 工具调用内容流、agent 自有终端、会话元信息等不进入 amux 的活动模型
        _ => {}
    }
    let _ = idle;
    if evs.is_empty() {
        return;
    }
    // 路由回收由各 turn 自己完成（见 `PromptStream::close`）：一条提前到达的
    // idle 不能注销尚未观察到 running 的 turn，否则它会收不到自己的 running。
    let txs = routes.lock().get(&session_id).cloned().unwrap_or_default();
    for tx in txs {
        for ev in &evs {
            if tx.send(ev.clone()).await.is_err() {
                log::warn!("agent 事件接收端已关闭");
            }
        }
    }
}

/// `tool_call_update` → 工具调用事件（`name`/`title`/`raw_input` 均可缺省，
/// 缺省字段表示保持不变）。
fn tool_call_event(update: &ToolCallUpdate) -> Option<AgentEvent> {
    let name = match &update.kind {
        MaybeUndefined::Value(kind) => Some(tool_kind_str(kind)),
        MaybeUndefined::Null => Some(String::new()),
        MaybeUndefined::Undefined => None,
    };
    let title = match &update.title {
        MaybeUndefined::Value(title) => Some(title.clone()),
        MaybeUndefined::Null => Some(String::new()),
        MaybeUndefined::Undefined => None,
    };
    let parameters = match &update.raw_input {
        MaybeUndefined::Value(value) => Some(value.to_string()),
        // `null` 清除参数：以空字符串表示「已清空」，与「未携带」区分
        MaybeUndefined::Null => Some(String::new()),
        MaybeUndefined::Undefined => None,
    };
    if name.is_none() && title.is_none() && parameters.is_none() {
        return None;
    }
    Some(AgentEvent::ToolCall {
        id: update.tool_call_id.to_string(),
        name: name.filter(|s| !s.is_empty()),
        title: title.filter(|s| !s.is_empty()),
        parameters: parameters.filter(|s| !s.is_empty()),
    })
}

/// 内容块 → 文本（仅 text 类型；其他类型记 debug 日志后忽略——
/// 对话历史按 IM 式文本流处理，非文本块暂无落盘表示）。
fn text_of(block: &AcpContentBlock) -> Option<String> {
    match block {
        AcpContentBlock::Text(t) => Some(t.text.clone()),
        other => {
            log::debug!("忽略非文本内容块（{}）", content_kind(other));
            None
        }
    }
}

/// 内容块列表 → 文本（多块直接拼接；非文本块忽略）。
fn join_text(blocks: &[AcpContentBlock]) -> Option<String> {
    let text: String = blocks.iter().filter_map(text_of).collect();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 消息/思考的整条内容：`Undefined` 表示不上报；`Null` 表示清空（`Some(None)`）；
/// `Value` 拼接多块文本。
fn snapshot_text(content: &MaybeUndefined<Vec<AcpContentBlock>>) -> Option<Option<String>> {
    match content {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(blocks) => Some(join_text(blocks)),
    }
}

/// 消息/思考的 chunk/snapshot 事件构造复用（两者字段一致，仅变体不同）。
#[derive(Clone, Copy)]
enum InlineEventKind {
    Message,
    Thinking,
}

impl InlineEventKind {
    fn chunk(self, message_id: impl ToString, text: String) -> AgentEvent {
        match self {
            InlineEventKind::Message => AgentEvent::AgentMessageChunk {
                message_id: message_id.to_string(),
                text,
            },
            InlineEventKind::Thinking => AgentEvent::ThinkingChunk {
                message_id: message_id.to_string(),
                text,
            },
        }
    }

    /// 整条快照事件（`text: None` 表示清空）。
    fn snapshot(self, message_id: impl ToString, text: Option<String>) -> AgentEvent {
        match self {
            InlineEventKind::Message => AgentEvent::AgentMessageSnapshot {
                message_id: message_id.to_string(),
                text,
            },
            InlineEventKind::Thinking => AgentEvent::ThinkingSnapshot {
                message_id: message_id.to_string(),
                text,
            },
        }
    }
}

fn content_kind(block: &AcpContentBlock) -> &'static str {
    match block {
        AcpContentBlock::Text(_) => "text",
        AcpContentBlock::Image(_) => "image",
        AcpContentBlock::Audio(_) => "audio",
        AcpContentBlock::Resource(_) => "resource",
        AcpContentBlock::ResourceLink(_) => "resource_link",
        _ => "unknown",
    }
}

/// ToolKind → 字符串（serde 序列化的 snake_case 名，如 `execute` / `read`）。
fn tool_kind_str(kind: &agent_client_protocol::schema::v2::ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "tool_call".to_string())
}

/// ACP `SessionConfigOption` 列表 → amux 协议投影。
/// select 选项的分组展平为扁平 (value, name) 列表（group 名并入 name 前缀）。
pub(crate) fn acp_config_options(
    options: Vec<AcpSessionConfigOption>,
) -> Vec<protocol::SessionConfigOption> {
    options
        .into_iter()
        .map(|opt| {
            let kind = match opt.kind {
                SessionConfigKind::Select(sel) => {
                    let entries: Vec<protocol::SessionConfigSelectEntry> = match sel.options {
                        SessionConfigSelectOptions::Ungrouped(list) => list
                            .into_iter()
                            .map(|o| protocol::SessionConfigSelectEntry {
                                value: o.value.0.to_string(),
                                name: o.name,
                            })
                            .collect(),
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .into_iter()
                            .flat_map(|g| {
                                let prefix = format!("{} · ", g.name);
                                g.options.into_iter().map(move |o| {
                                    protocol::SessionConfigSelectEntry {
                                        value: o.value.0.to_string(),
                                        name: format!("{prefix}{}", o.name),
                                    }
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    protocol::SessionConfigKind::Select {
                        current_value: sel.current_value.0.to_string(),
                        options: entries,
                    }
                }
                SessionConfigKind::Boolean(b) => protocol::SessionConfigKind::Boolean {
                    current_value: b.current_value,
                },
                // 未知/未来类型：降级为空 select（保留选项本身）
                _ => protocol::SessionConfigKind::Select {
                    current_value: String::new(),
                    options: Vec::new(),
                },
            };
            protocol::SessionConfigOption {
                id: opt.config_id.0.to_string(),
                name: opt.name,
                description: opt.description,
                // category 序列化为不带 JSON 引号的 snake_case 字符串（如 model）
                category: opt
                    .category
                    .as_ref()
                    .and_then(|c| serde_json::to_value(c).ok())
                    .and_then(|v| v.as_str().map(str::to_string)),
                kind,
            }
        })
        .collect()
}

/// ACP `AvailableCommand` 列表 → amux 协议投影（input 仅支持文本提示）。
fn acp_slash_commands(commands: &[AvailableCommand]) -> Vec<protocol::SlashCommand> {
    commands
        .iter()
        .map(|c| protocol::SlashCommand {
            name: c.name.clone(),
            description: c.description.clone(),
            hint: c.input.as_ref().and_then(|input| match input {
                AvailableCommandInput::Text(text) => Some(text.hint.clone()),
                _ => None,
            }),
        })
        .collect()
}

/// ACP `PlanEntry` 列表 → amux 协议投影（priority/status 非穷尽枚举按通配回落）。
fn acp_plan(
    entries: &[agent_client_protocol::schema::v2::PlanEntry],
) -> Vec<protocol::SessionPlanEntry> {
    use agent_client_protocol::schema::v2::{PlanEntryPriority, PlanEntryStatus};
    entries
        .iter()
        .map(|e| protocol::SessionPlanEntry {
            content: e.content.clone(),
            priority: match e.priority {
                PlanEntryPriority::High => protocol::SessionPlanPriority::High,
                PlanEntryPriority::Medium => protocol::SessionPlanPriority::Medium,
                PlanEntryPriority::Low => protocol::SessionPlanPriority::Low,
                _ => protocol::SessionPlanPriority::Medium,
            },
            status: match e.status {
                PlanEntryStatus::InProgress => protocol::SessionPlanStatus::InProgress,
                PlanEntryStatus::Completed => protocol::SessionPlanStatus::Completed,
                _ => protocol::SessionPlanStatus::Pending,
            },
        })
        .collect()
}

/// protocol::ContentBlock → SDK ContentBlock。
fn acp_content_block(b: &ContentBlock) -> Option<AcpContentBlock> {
    match b {
        ContentBlock::Text { text } => Some(AcpContentBlock::Text(TextContent::new(text.clone()))),
        ContentBlock::Resource {
            mime_type,
            uri,
            text,
            blob,
        } => {
            let uri = uri.clone().unwrap_or_default();
            let media_type = (!mime_type.is_empty()).then(|| MediaType::new(mime_type.clone()));
            let resource = if let Some(blob) = blob {
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new(blob.clone(), uri.clone()).mime_type(media_type),
                )
            } else {
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new(text.clone().unwrap_or_default(), uri.clone())
                        .mime_type(media_type),
                )
            };
            Some(AcpContentBlock::Resource(EmbeddedResource::new(resource)))
        }
        ContentBlock::ResourceLink {
            uri,
            name,
            mime_type,
            title,
            description,
        } => Some(AcpContentBlock::ResourceLink(
            ResourceLink::new(name.clone(), uri.clone())
                .mime_type(mime_type.clone().map(MediaType::new))
                .title(title.clone())
                .description(description.clone()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    type Routes = Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>;

    fn routes_with(tx: mpsc::Sender<AgentEvent>) -> Routes {
        Mutex::new(HashMap::from([("s1".to_string(), vec![tx])]))
    }

    #[tokio::test]
    async fn disconnect_wakes_turn_when_event_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(AgentEvent::Error("尚未处理".into())).unwrap();
        let caches = SessionCaches {
            routes: Arc::new(Mutex::new(HashMap::from([("s1".into(), vec![tx])]))),
            ..SessionCaches::default()
        };

        caches.clear_on_disconnect();

        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::Error(message)) if message == "尚未处理"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::Error(message)) if message == "ACP 连接已关闭"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::StateUpdate {
                state: SessionState::Idle,
                reason: protocol::StateChangeReason::Aborted
            })
        ));
        assert!(caches.routes.lock().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn startup_cancellation_interrupts_initialize_wait() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let child_shutdown = shutdown.clone();
        let started = std::time::Instant::now();
        let task = std::thread::spawn(move || {
            AcpConnection::spawn_with_shutdown(
                "/bin/sh",
                &["-c", "sleep 30"],
                &[],
                Some(child_shutdown),
            )
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        shutdown.store(true, Ordering::SeqCst);
        let result = task.join().expect("ACP 启动线程不应 panic");

        let error = match result {
            Ok(_) => panic!("关闭闸门应取消未完成的 ACP 启动"),
            Err(error) => error,
        };
        assert!(error.contains("启动已取消"), "应返回启动取消错误: {error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "启动取消不应等待完整握手超时"
        );
    }

    #[tokio::test]
    async fn prompt_after_disconnect_finishes_immediately() {
        let caches = SessionCaches::default();
        caches.clear_on_disconnect();
        let (stop_tx, _stop_rx) = watch::channel(false);
        let connection = AcpConnection {
            exec_tx: Arc::new(Mutex::new(None)),
            caches,
            resumed: Arc::new(Mutex::new(HashSet::new())),
            stop_tx,
            thread: Mutex::new(None),
        };
        let mut rx = connection.prompt("s1", Vec::new());
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::Error(message)) if message == "agent 已关闭"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::StateUpdate {
                state: SessionState::Idle,
                reason: protocol::StateChangeReason::Aborted
            })
        ));
    }

    #[test]
    fn session_caps_from_agent_capabilities() {
        use agent_client_protocol::schema::v2::{
            AgentCapabilities, SessionCapabilities, SessionDeleteCapabilities,
        };
        // 未声明 capabilities.session.delete：不支持删除
        let caps = session_caps_from_agent_caps(&AgentCapabilities::default());
        assert!(!caps.delete);
        // 声明 `{}`：支持删除
        let caps =
            session_caps_from_agent_caps(&AgentCapabilities::default().session(
                SessionCapabilities::default().delete(SessionDeleteCapabilities::default()),
            ));
        assert!(caps.delete);
    }

    #[test]
    fn idle_state_update_is_delivered_and_keeps_routes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            let routes = routes_with(tx);
            let commands = Mutex::new(HashMap::new());
            let plans = Mutex::new(HashMap::new());
            let notif = UpdateSessionNotification::new(
                "s1",
                SessionUpdate::StateUpdate(StateUpdate::Idle(
                    agent_client_protocol::schema::v2::IdleStateUpdate::default()
                        .stop_reason(StopReason::EndTurn),
                )),
            );
            route_update(&routes, &commands, &plans, &notif).await;

            assert!(matches!(
                rx.recv().await,
                Some(AgentEvent::StateUpdate {
                    state: SessionState::Idle,
                    reason: protocol::StateChangeReason::Completed
                })
            ));
            // idle 不回收路由：回收由各 turn 自己完成（否则一条先到的 idle 会让
            // 尚未观察到 running 的在途 turn 收不到自己的 running）
            assert_eq!(routes.lock().len(), 1, "idle 不应注销路由");
        });
    }

    #[test]
    fn idle_with_error_stop_reason_reports_error_activity() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            let routes = routes_with(tx);
            let commands = Mutex::new(HashMap::new());
            let plans = Mutex::new(HashMap::new());
            let notif = UpdateSessionNotification::new(
                "s1",
                SessionUpdate::StateUpdate(StateUpdate::Idle(
                    agent_client_protocol::schema::v2::IdleStateUpdate::default()
                        .stop_reason(StopReason::Other("_error".to_string())),
                )),
            );
            route_update(&routes, &commands, &plans, &notif).await;

            assert!(matches!(rx.recv().await, Some(AgentEvent::Error(_))));
            assert!(matches!(
                rx.recv().await,
                Some(AgentEvent::StateUpdate {
                    state: SessionState::Idle,
                    reason: protocol::StateChangeReason::Aborted
                })
            ));
        });
    }

    /// 注销只影响自己的条目：同会话并发的其他 prompt 路由保持不动。
    #[tokio::test]
    async fn prompt_stream_close_removes_only_its_own_route() {
        let (tx_a, _rx_a) = mpsc::channel(8);
        let (tx_b, _rx_b) = mpsc::channel(8);
        let routes = Arc::new(Mutex::new(HashMap::from([(
            "s1".to_string(),
            vec![tx_a.clone(), tx_b.clone()],
        )])));
        let stream = PromptStream {
            rx: mpsc::channel(1).1,
            tx: tx_a,
            routes: routes.clone(),
            agent_session_id: "s1".to_string(),
        };
        stream.close();
        let remaining = routes.lock().get("s1").cloned().unwrap_or_default();
        assert_eq!(remaining.len(), 1, "只应注销自己的条目");
        assert!(remaining[0].same_channel(&tx_b));
    }

    #[test]
    fn running_state_update_maps_to_busy() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            let routes = routes_with(tx);
            let commands = Mutex::new(HashMap::new());
            let plans = Mutex::new(HashMap::new());
            let notif = UpdateSessionNotification::new(
                "s1",
                SessionUpdate::StateUpdate(StateUpdate::Running(
                    agent_client_protocol::schema::v2::RunningStateUpdate::default(),
                )),
            );
            route_update(&routes, &commands, &plans, &notif).await;

            assert!(matches!(
                rx.recv().await,
                Some(AgentEvent::StateUpdate {
                    state: SessionState::Busy,
                    ..
                })
            ));
            assert_eq!(routes.lock().len(), 1, "running 不回收路由");
        });
    }
}
