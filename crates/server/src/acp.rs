//! ACP v1 驱动：官方 SDK `agent-client-protocol` 的 Client 角色，
//! 经 stdio 与 ACP server 子进程通信。
//!
//! `AcpAgentDriver` 使用**专用 exec 线程**承载全部异步 IO（SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨
//! runtime 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommandInput, BlobResourceContents, BooleanConfigOptionCapabilities,
    CancelNotification, ClientCapabilities, ClientSessionCapabilities, CloseSessionRequest,
    ContentBlock as AcpContentBlock, CreateTerminalRequest, DeleteSessionRequest, EmbeddedResource,
    EmbeddedResourceResource, InitializeRequest, KillTerminalRequest, NewSessionRequest,
    PermissionOption, PermissionOptionId, PermissionOptionKind, PromptRequest,
    ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResourceLink, ResumeSessionRequest, SelectedPermissionOutcome,
    SessionConfigOption as AcpSessionConfigOption,
    SessionConfigOptionValue as AcpSessionConfigOptionValue, SessionConfigOptionsCapabilities,
    SessionConfigSelectOptions, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    StopReason, TerminalOutputRequest, TextContent, TextResourceContents, ToolKind,
    WaitForTerminalExitRequest,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::AcpAgent;
use agent_client_protocol::ConnectionTo;
use tokio::sync::{mpsc, watch};

use protocol::ContentBlock;

use crate::acp_terminal;

/// 拉起的统计（server 启动日志用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct LaunchSummary {
    /// 成功拉起的 ACP server 数
    pub started: usize,
    /// 拉起失败的 agent 数（标记为**不可用**，agent.list 的 status=unavailable）
    pub failed: usize,
}

/// turn 过程中的 agent 事件，供 server 透传给 GUI 聚合。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 输出的增量片段
    OutputChunk(String),
    /// 思考片段
    Thinking(String),
    /// 工具调用（ACP `tool_call` / `tool_call_update`，按 `tool_call_id` 合并）
    ToolCall {
        id: String,
        name: Option<String>,
        title: Option<String>,
        parameters: Option<String>,
    },
    /// ACP 请求或传输失败
    Error(String),
    /// ACP `session/prompt` 响应为错误（turn 未开始）。与 `Error`（turn 内
    /// 错误）区分：调用方可据此把失败作为请求错误上报。
    PromptFailed(String),
    /// 会话上下文大小更新（ACP `usage_update`：当前上下文大小与窗口总大小，token）。
    UsageUpdate {
        /// 当前在上下文中的 token 数
        used: u64,
        /// 上下文窗口总大小（token）
        size: u64,
    },
    /// 会话配置选项更新（ACP `config_options_update`：完整的选项集合与当前值）。
    ConfigOptions(Vec<protocol::SessionConfigOption>),
    /// turn 完成（携带结束原因）。
    TurnEnded(protocol::StateChangeReason),
}

/// 与单个 agent 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id 与初始配置选项
    fn create_session(
        &self,
        cwd: &str,
    ) -> Result<(String, Vec<protocol::SessionConfigOption>), String>;
    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史；
    /// 同一进程内对同一会话幂等——已恢复过则直接成功），返回会话配置选项
    fn resume_session(
        &self,
        agent_session_id: &str,
        cwd: &str,
    ) -> Result<Vec<protocol::SessionConfigOption>, String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 关闭会话，释放 agent 侧资源。
    fn close(&self, agent_session_id: &str) -> Result<(), String>;
    /// 删除会话：close 之后若 ACP Server 支持会话删除，
    /// 则发送 `session/delete` 删除 agent 侧会话；不支持删除的 agent 返回错误，
    /// 调用方按「不支持」忽略）
    fn delete_session(&self, agent_session_id: &str) -> Result<(), String>;
    /// 设置会话配置选项（ACP `session/set_config_option`），返回更新后的完整选项集合。
    fn set_config_option(
        &self,
        agent_session_id: &str,
        config_id: &str,
        value: protocol::SessionConfigOptionValue,
    ) -> Result<Vec<protocol::SessionConfigOption>, String>;
    /// 会话当前斜杠命令（最近一次 ACP `available_commands_update` 通知的全量集合；
    /// 无通知则空）。默认空（不支持命令的驱动）。
    fn available_commands(&self, _agent_session_id: &str) -> Vec<protocol::SlashCommand> {
        Vec::new()
    }
    /// 会话当前计划（最近一次 ACP `plan` 通知的全量条目；无通知则空）。默认空
    /// （不支持计划的驱动）。
    fn session_plan(&self, _agent_session_id: &str) -> Vec<protocol::SessionPlanEntry> {
        Vec::new()
    }
    /// 会话建立时记录的 agent 侧能力。默认全不支持。
    fn session_caps(&self, _agent_session_id: &str) -> AgentSessionCaps {
        AgentSessionCaps::default()
    }
    /// 是否处于未认证状态（ACP `initialize` 响应携带非空 `authMethods`）。
    fn requires_auth(&self) -> bool {
        false
    }
    /// 关闭驱动自身，释放 ACP 子进程资源。
    fn shutdown(&self);
    /// 关闭并等待驱动后台线程退出（默认仅 shutdown、不等待；确定性退出路径使用，
    /// 避免 process::exit 抢在子进程清理之前）。
    fn shutdown_and_join(&self) {
        self.shutdown();
    }
}

/// 驱动的共享句柄（注册表缓存与调用方传递）。
pub type SharedDriver = Arc<dyn AgentDriver>;

/// 主线程 → exec 线程的 ACP 方法调用（强类型，替代裸 method 字符串 + Value 参数，
/// 消除 params 里 cwd 缺省回落 "/" 的魔法值）。
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

/// ACP 方法响应（进程内强类型投影：dispatch_call_inner 已拿到 SDK 类型化响应，
/// 不再经 serde_json::Value 往返）。
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
        /// 本次 prompt 的事件发送端：响应回调按通道身份回收自己的路由条目
        tx: mpsc::Sender<AgentEvent>,
    },
}

/// ACP v1 客户端（官方 SDK stdio 传输）。
pub struct AcpAgentDriver {
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
    /// `initialize` 响应是否携带非空 `authMethods`（标记未认证）。
    requires_auth: Arc<AtomicBool>,
}

/// 会话建立时 agent 侧声明的能力快照。能力由 initialize 握手的
/// agentCapabilities 声明，按 agent sessionId 存档于连接级缓存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSessionCaps {
    /// agent 是否支持 `session/delete`
    pub delete: bool,
}

/// initialize 响应的 agentCapabilities → 会话能力快照（纯函数，可单测）。
fn session_caps_from_agent_caps(
    caps: &agent_client_protocol::schema::v1::AgentCapabilities,
) -> AgentSessionCaps {
    AgentSessionCaps {
        delete: caps.session_capabilities.delete.is_some(),
    }
}

/// `session/update` 通知处理器共享的连接级状态（驱动与 exec 线程各持一份克隆）。
#[derive(Clone)]
struct SessionCaches {
    /// 会话事件路由：agent sessionId -> 该会话所有进行中 prompt 的事件接收端。
    /// 同一会话允许多个 prompt 并发在途（是否受理由 ACP server 决定），
    /// `session/update` 通知不区分来源，扇出给全部在途订阅者。
    routes: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>>,
    /// 会话斜杠命令：agent sessionId -> 最近一次 `available_commands_update`
    /// 的全量集合。缓存在驱动层而非事件流——通知可能出现在无 prompt 路由的
    /// 窗口（如 session/new 后 agent 立即下发）。
    commands: Arc<Mutex<HashMap<String, Vec<protocol::SlashCommand>>>>,
    /// 会话计划：agent sessionId -> 最近一次 `plan` 通知的全量条目（缓存理由同上）。
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

fn send_disconnect_events(tx: mpsc::Sender<AgentEvent>) {
    let error = AgentEvent::Error("ACP 连接已关闭".into());
    match tx.try_send(error) {
        Ok(()) => match tx.try_send(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted)) {
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
                    let _ = tx
                        .blocking_send(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted));
                }
            });
        }
    }
}

impl AcpAgentDriver {
    /// 启动 ACP agent 子进程（官方 SDK `AcpAgent` 管理 stdio 传输与进程生命周期）；
    /// `env` 为附加环境变量（经 from_args 的 `NAME=value` 前缀传入）；
    /// exec 线程承载全部异步 IO。
    ///
    /// **同步就绪握手**：阻塞等待 exec 线程完成「子进程拉起 + 连接建立 + initialize
    /// 握手」后才返回——二进制缺失 / 进程立即退出（如 npx 不可用、无网络）在此快速
    /// 失败并返回明确错误；健康 agent 在握手完成后立即返回。等待受
    /// `AMUX_ACP_SPAWN_TIMEOUT_MS` 限制（默认 30s；npx 首次按需下载可能较慢，
    /// 超时按失败处理，`driver_for` 兜底会重试）。
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
        let requires_auth = Arc::new(AtomicBool::new(false));
        let requires_auth2 = requires_auth.clone();
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
                requires_auth2,
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
        Ok(AcpAgentDriver {
            exec_tx,
            caches,
            resumed: Arc::new(Mutex::new(HashSet::new())),
            stop_tx,
            thread: Mutex::new(Some(thread)),
            requires_auth,
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

impl AgentDriver for AcpAgentDriver {
    fn requires_auth(&self) -> bool {
        self.requires_auth.load(Ordering::SeqCst)
    }

    fn create_session(
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

    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史——
    /// 历史以 server 本地日志为权威。
    /// 同一进程内对同一会话幂等（已恢复过则直接成功），返回会话配置选项。
    fn resume_session(
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

    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent> {
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
            let _ = tx.try_send(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted));
            return rx;
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
            let removed = {
                let mut routes = self.caches.routes.lock();
                routes
                    .get_mut(agent_session_id)
                    .and_then(|txs| {
                        txs.iter()
                            .position(|s| s.same_channel(&tx))
                            .map(|idx| txs.remove(idx))
                    })
                    .is_some()
            };
            if removed {
                let _ = tx.try_send(AgentEvent::Error(error));
                let _ = tx.try_send(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted));
            }
        }
        rx
    }

    fn shutdown(&self) {
        self.caches.alive.store(false, Ordering::SeqCst);
        let _ = self.stop_tx.send(true);
        let _ = self.exec_tx.lock().take();
        self.caches.clear_on_disconnect();
    }

    /// 关闭并等待 exec 线程退出：通道关闭 → 服务循环结束 → SDK 连接 drop（子进程
    /// 随之回收）。server 退出路径调用，保证清理先于进程退出完成。
    fn shutdown_and_join(&self) {
        self.shutdown();
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }

    fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.call(AcpCall::Cancel {
            sid: agent_session_id.to_string(),
        })
        .map(|_| ())
    }

    fn close(&self, agent_session_id: &str) -> Result<(), String> {
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
    fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
        // 会话建立时 agent 未声明 sessionCapabilities.delete：不发请求直接报不支持
        //（调用方按「不支持」忽略）。避免对 codex 这类声明语义缺失的 agent
        // 发出必然失败的 session/delete（"no rollout found"）。
        if !self.session_caps(agent_session_id).delete {
            self.caches.caps.lock().remove(agent_session_id);
            return Err(
                "agent 不支持 session/delete（initialize 未声明 sessionCapabilities.delete）"
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
    fn set_config_option(
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

    fn available_commands(&self, agent_session_id: &str) -> Vec<protocol::SlashCommand> {
        self.caches
            .commands
            .lock()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn session_plan(&self, agent_session_id: &str) -> Vec<protocol::SessionPlanEntry> {
        self.caches
            .plans
            .lock()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn session_caps(&self, agent_session_id: &str) -> AgentSessionCaps {
        self.caches
            .caps
            .lock()
            .get(agent_session_id)
            .copied()
            .unwrap_or(*self.caches.default_caps.lock())
    }
}

/// yolo 权限批准：从请求选项中选出要批准的选项（纯函数，可单测）。
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

/// exec 线程主循环：经官方 SDK 建立 ACP 连接，承载方法分发、通知路由与权限批准。
/// `ready_tx`：就绪握手——连接建立（子进程拉起）且 initialize 握手完成后发送结果；
/// 若连接在握手前就失败（二进制缺失 / 进程立即退出），在此补发 `Err` 供
/// `AcpAgentDriver::spawn` 同步快速失败，而非等满超时。
async fn exec_main(
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    control: ExecControl,
    caches: SessionCaches,
    requires_auth: Arc<AtomicBool>,
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
            requires_auth,
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
    // 之后的连接异常仅记录，不影响已缓存的驱动。
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
    requires_auth: Arc<AtomicBool>,
) -> agent_client_protocol::Result<()> {
    // 本连接内的 ACP 终端宿主：terminal/* 反向请求在此执行命令并回收进程
    let terminals: acp_terminal::SharedTerminals =
        std::sync::Arc::new(acp_terminal::TerminalRegistry::new());
    // 连接终止时回收剩余终端（service 循环结束时调用）
    let shutdown_terminals = terminals.clone();
    agent_client_protocol::Client
        .builder()
        .name("amux-server")
        .on_receive_notification(
            {
                let caches = caches.clone();
                async move |notif: SessionNotification, _cx| {
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
                // yolo：自动批准，避免额外的审批往返。
                // 必须选 allow 类选项：claude-acp 等包装器的选项列表**第一项往往是
                // 「Deny/reject」**，选第一个会被 agent 误判为用户拒绝
                // （"User refused permission to run tool"）。
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
        // ACP terminal/* 反向请求（初始化已声明 terminal 能力）
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: CreateTerminalRequest, responder, _cx| {
                    match terminals.create(&request) {
                        Ok(resp) => {
                            let _ = responder.respond(resp);
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            let _ = responder.respond_with_internal_error(e);
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: TerminalOutputRequest, responder, _cx| {
                    match terminals.output(&request) {
                        Ok(resp) => {
                            let _ = responder.respond(resp);
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            let _ = responder.respond_with_internal_error(e);
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: WaitForTerminalExitRequest, responder, _cx| {
                    match terminals.wait(&request).await {
                        Ok(resp) => {
                            let _ = responder.respond(resp);
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            let _ = responder.respond_with_internal_error(e);
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: KillTerminalRequest, responder, _cx| {
                    match terminals.kill(&request) {
                        Ok(resp) => {
                            let _ = responder.respond(resp);
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            let _ = responder.respond_with_internal_error(e);
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let terminals = terminals.clone();
                async move |request: ReleaseTerminalRequest, responder, _cx| {
                    match terminals.release(&request) {
                        Ok(resp) => {
                            let _ = responder.respond(resp);
                        }
                        Err(e) => {
                            log::warn!("{e}");
                            let _ = responder.respond_with_internal_error(e);
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, {
            let caches = caches.clone();
            move |cx: ConnectionTo<agent_client_protocol::Agent>| async move {
                let caches = caches;
                let req_rx = req_rx;
                // 初始化握手（版本协商）。失败需区分两种情形：
                // - **协议级失败**（agent 存活但不实现 initialize，如返回 method not
                //   found）：仅记录、连接保持可用，视为拉起成功；
                // - **传输层失败**（进程已退出 / 连接已死，如 npx 不可用、无网络）：拉起失败。
                // 二者用短窗口探测连接活性区分：incoming_closed 在传输层关闭后很快完成，
                // 超时则连接仍存活。
                // 声明客户端能力：会话配置选项由 ACP 会话提供，需客户端声明
                // configOptions 能力
                // agent 才会在 new/resume 响应中下发选项并接受 set_config_option）
                // 与 terminal/*（kimi acp 等将 shell 执行委托给客户端）。
                let init_request = InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                    ClientCapabilities::new()
                        .session(
                            ClientSessionCapabilities::new().config_options(
                                SessionConfigOptionsCapabilities::new()
                                    .boolean(BooleanConfigOptionCapabilities::new()),
                            ),
                        )
                        .terminal(true),
                );
                let init_result = match cx.send_request(init_request).block_task().await {
                    core::result::Result::Ok(resp) => {
                        // 记录 agent 侧声明的连接默认能力，供后续会话建立时复制。
                        *caches.default_caps.lock() =
                            session_caps_from_agent_caps(&resp.agent_capabilities);
                        // 非空 authMethods 表示 ACP server 需要认证：标记未认证。
                        if !resp.auth_methods.is_empty() {
                            requires_auth.store(true, Ordering::SeqCst);
                            log::warn!(
                                "ACP server 声明 {} 种认证方式，标记未认证",
                                resp.auth_methods.len()
                            );
                        }
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
                                let callback_sid = sid.clone();
                                let callback_routes = routes.clone();
                                let callback_tx = tx.clone();
                                let result = cx
                                    .send_request(PromptRequest::new(sid.clone(), blocks))
                                    .on_receiving_result(async move |result| {
                                        // turn 完成：按通道身份移除本次 prompt 的路由条目并发送
                                        // TurnEnded（在最后一批通知之后）。同会话其他在途 prompt
                                        // 的条目保留，继续接收后续通知。
                                        // 结束原因取自 ACP prompt 响应的 stopReason（权威归因）
                                        let route = {
                                            let mut routes = callback_routes.lock();
                                            let mut found = None;
                                            if let Some(txs) = routes.get_mut(&callback_sid) {
                                                if let Some(idx) = txs
                                                    .iter()
                                                    .position(|s| s.same_channel(&callback_tx))
                                                {
                                                    found = Some(txs.remove(idx));
                                                }
                                                if txs.is_empty() {
                                                    routes.remove(&callback_sid);
                                                }
                                            }
                                            found
                                        };
                                        if let Some(tx) = route {
                                            let reason = match &result {
                                                Ok(resp) => stop_reason_reason(resp.stop_reason),
                                                Err(_) => protocol::StateChangeReason::Aborted,
                                            };
                                            if let core::result::Result::Err(e) = &result {
                                                let _ = tx
                                                    .send(AgentEvent::PromptFailed(format!(
                                                        "ACP prompt 失败: {e}"
                                                    )))
                                                    .await;
                                            }
                                            let _ = tx.send(AgentEvent::TurnEnded(reason)).await;
                                        }
                                        core::result::Result::Ok(())
                                    });
                                if let Err(e) = result {
                                    log::error!("prompt 调用失败 {sid}: {e}");
                                    // 回调不会触发：按通道身份回收自己的路由条目并结束本次事件流
                                    let route = {
                                        let mut routes = routes.lock();
                                        let mut found = None;
                                        if let Some(txs) = routes.get_mut(&sid) {
                                            if let Some(idx) =
                                                txs.iter().position(|s| s.same_channel(&tx))
                                            {
                                                found = Some(txs.remove(idx));
                                            }
                                            if txs.is_empty() {
                                                routes.remove(&sid);
                                            }
                                        }
                                        found
                                    };
                                    if let Some(tx) = route {
                                        let _ = tx
                                            .send(AgentEvent::Error(format!(
                                                "ACP prompt 调用失败: {e}"
                                            )))
                                            .await;
                                        let _ = tx
                                            .send(AgentEvent::TurnEnded(
                                                protocol::StateChangeReason::Aborted,
                                            ))
                                            .await;
                                    }
                                }
                            });
                        }
                    }
                }
                // 服务循环结束（driver 已 shutdown）：回收本连接的终端子进程
                shutdown_terminals.terminate_all().await;
                core::result::Result::Ok(())
            }
        })
        .await
}

/// 分发 ACP v1 方法调用（强类型 AcpCall，经官方 SDK 传输）。
async fn dispatch_call(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
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
    cx: &ConnectionTo<agent_client_protocol::Agent>,
    call: &AcpCall,
) -> Result<AcpResponse, String> {
    match call {
        AcpCall::NewSession { cwd } => {
            let resp = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/new 失败: {e}"))?;
            Ok(AcpResponse::NewSession {
                session_id: resp.session_id.to_string(),
                config_options: acp_config_options(resp.config_options),
            })
        }
        AcpCall::Resume { sid, cwd } => {
            let resp = cx
                .send_request(ResumeSessionRequest::new(sid.clone(), cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/resume 失败: {e}"))?;
            Ok(AcpResponse::ConfigOptions(acp_config_options(
                resp.config_options,
            )))
        }
        AcpCall::Cancel { sid } => {
            cx.send_notification(CancelNotification::new(sid.clone()))
                .map_err(|e| format!("session/cancel 失败: {e}"))?;
            Ok(AcpResponse::Unit)
        }
        AcpCall::Close { sid } => {
            cx.send_request(CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await
                .map_err(|e| format!("session/close 失败: {e}"))?;
            Ok(AcpResponse::Unit)
        }
        AcpCall::Delete { sid } => {
            // 仅 agent 声明 sessionCapabilities.delete 时可用；不支持时返回错误，
            // 调用方（会话删除路径）按「不支持删除」忽略。
            cx.send_request(DeleteSessionRequest::new(sid.clone()))
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
            // 客户端已声明 configOptions 能力；不支持时 agent 返回错误并向上传播。
            let acp_value = match value {
                protocol::SessionConfigOptionValue::ValueId { value } => {
                    AcpSessionConfigOptionValue::value_id(
                        agent_client_protocol::schema::v1::SessionConfigValueId::new(
                            value.as_str(),
                        ),
                    )
                }
                protocol::SessionConfigOptionValue::Boolean { value } => {
                    AcpSessionConfigOptionValue::boolean(*value)
                }
            };
            let resp = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    config_id.clone(),
                    acp_value,
                ))
                .block_task()
                .await
                .map_err(|e| format!("session/set_config_option 失败: {e}"))?;
            Ok(AcpResponse::ConfigOptions(acp_config_options(Some(
                resp.config_options,
            ))))
        }
    }
}

/// ACP stopReason → 状态变更原因。
/// 未识别的新枚举值按正常结束处理（仅 cancelled 参与注入过滤）。
fn stop_reason_reason(reason: StopReason) -> protocol::StateChangeReason {
    match reason {
        StopReason::Cancelled => protocol::StateChangeReason::Cancelled,
        StopReason::MaxTokens => protocol::StateChangeReason::MaxTokens,
        StopReason::MaxTurnRequests => protocol::StateChangeReason::MaxTurnRequests,
        StopReason::Refusal => protocol::StateChangeReason::Refusal,
        _ => protocol::StateChangeReason::Completed,
    }
}

/// 把 ACP `session/update` 通知映射为 AgentEvent 并路由。
async fn route_update(
    routes: &Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>,
    commands: &Mutex<HashMap<String, Vec<protocol::SlashCommand>>>,
    plans: &Mutex<HashMap<String, Vec<protocol::SessionPlanEntry>>>,
    notif: &SessionNotification,
) {
    // 一次通知可能同时产生多个事件（如 tool_call_update 同时更新标题并携带结果）。
    let evs: Vec<AgentEvent> = match &notif.update {
        // 用户消息由 server 直接落盘，不重复放入活动流。
        SessionUpdate::UserMessageChunk(_) => Vec::new(),
        SessionUpdate::AgentMessageChunk(chunk) => text_of(&chunk.content)
            .map(AgentEvent::OutputChunk)
            .into_iter()
            .collect(),
        SessionUpdate::AgentThoughtChunk(chunk) => text_of(&chunk.content)
            .map(AgentEvent::Thinking)
            .into_iter()
            .collect(),
        SessionUpdate::ToolCall(tc) => vec![AgentEvent::ToolCall {
            id: tc.tool_call_id.0.to_string(),
            name: Some(tool_kind_str(&tc.kind)),
            title: Some(tc.title.clone()),
            parameters: tc.raw_input.as_ref().map(|v| v.to_string()),
        }],
        // ACP `tool_call_update`：`kind`/`title`/`raw_input` 按 `tool_call_id`
        // 合并到同一条活动；`content`（工具结果）不单独成条活动。
        // 仅当携带可合并字段时才发出调用事件，避免空更新产生无意义条目。
        SessionUpdate::ToolCallUpdate(tcu) => {
            let name = tcu.fields.kind.as_ref().map(tool_kind_str);
            let title = tcu.fields.title.clone();
            let parameters = tcu.fields.raw_input.as_ref().map(|v| v.to_string());
            if name.is_some() || title.is_some() || parameters.is_some() {
                vec![AgentEvent::ToolCall {
                    id: tcu.tool_call_id.0.to_string(),
                    name,
                    title,
                    parameters,
                }]
            } else {
                Vec::new()
            }
        }
        SessionUpdate::UsageUpdate(update) => vec![AgentEvent::UsageUpdate {
            used: update.used,
            size: update.size,
        }],
        // ACP `config_options_update`：会话配置选项变更（完整集合）。
        SessionUpdate::ConfigOptionUpdate(update) => vec![AgentEvent::ConfigOptions(
            acp_config_options(Some(update.config_options.clone())),
        )],
        // ACP `available_commands_update`：斜杠命令全量覆盖驱动内存缓存
        // 以 Agent 侧数据为权威。
        // 不产生事件流——通知可能出现在无 prompt 路由的窗口，缓存于驱动层。
        SessionUpdate::AvailableCommandsUpdate(update) => {
            commands.lock().insert(
                notif.session_id.to_string(),
                acp_slash_commands(update.available_commands.clone()),
            );
            Vec::new()
        }
        // ACP `plan`：agent 计划全量覆盖驱动内存缓存
        // 以 Agent 侧数据为权威。
        // 不产生事件流，理由同上。
        SessionUpdate::Plan(update) => {
            plans.lock().insert(
                notif.session_id.to_string(),
                acp_plan(update.entries.clone()),
            );
            Vec::new()
        }
        // SessionInfoUpdate（ACP v1 未携带状态字段）/ CurrentModeUpdate 等
        // 不产生 AgentEvent
        _ => Vec::new(),
    };
    if !evs.is_empty() {
        let txs = routes
            .lock()
            .get(notif.session_id.to_string().as_str())
            .cloned()
            .unwrap_or_default();
        for tx in txs {
            for ev in &evs {
                if tx.send(ev.clone()).await.is_err() {
                    log::warn!("agent 事件接收端已关闭");
                }
            }
        }
    }
}

/// ContentBlock → 文本（仅 text 类型；其他类型记 debug 日志后忽略——
/// 对话历史按 IM 式文本流处理，非文本块暂无落盘表示。
fn text_of(block: &AcpContentBlock) -> Option<String> {
    match block {
        AcpContentBlock::Text(t) => Some(t.text.clone()),
        other => {
            let kind = match other {
                AcpContentBlock::Image(_) => "image",
                AcpContentBlock::Audio(_) => "audio",
                AcpContentBlock::Resource(_) => "resource",
                AcpContentBlock::ResourceLink(_) => "resource_link",
                _ => "unknown",
            };
            log::debug!("忽略非文本内容块（{kind}）");
            None
        }
    }
}

/// ToolKind → 字符串（serde 序列化的 snake_case 名，如 `execute` / `read`）。
fn tool_kind_str(kind: &ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "tool_call".to_string())
}

/// ACP `SessionConfigOption` 列表 → amux 协议投影。
/// select 选项的分组展平为扁平 (value, name) 列表（group 名并入 name 前缀）。
fn acp_config_options(
    options: Option<Vec<AcpSessionConfigOption>>,
) -> Vec<protocol::SessionConfigOption> {
    options
        .unwrap_or_default()
        .into_iter()
        .map(|opt| {
            let kind = match opt.kind {
                agent_client_protocol::schema::v1::SessionConfigKind::Select(sel) => {
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
                                let prefix = format!("{} · ", g.group.0);
                                g.options.into_iter().map(move |o| {
                                    protocol::SessionConfigSelectEntry {
                                        value: o.value.0.to_string(),
                                        name: format!("{prefix}{}", o.name),
                                    }
                                })
                            })
                            .collect(),
                        // SDK 1.4.0 仅含 Ungrouped/Grouped；non_exhaustive 要求通配
                        _ => Vec::new(),
                    };
                    protocol::SessionConfigKind::Select {
                        current_value: sel.current_value.0.to_string(),
                        options: entries,
                    }
                }
                agent_client_protocol::schema::v1::SessionConfigKind::Boolean(b) => {
                    protocol::SessionConfigKind::Boolean {
                        current_value: b.current_value,
                    }
                }
                // SDK 1.4.0 仅含 Select/Boolean；non_exhaustive 要求通配
                _ => protocol::SessionConfigKind::Select {
                    current_value: String::new(),
                    options: Vec::new(),
                },
            };
            protocol::SessionConfigOption {
                id: opt.id.0.to_string(),
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

/// ACP `AvailableCommand` 列表 → amux 协议投影（input 仅支持 unstructured 提示）。
fn acp_slash_commands(
    commands: Vec<agent_client_protocol::schema::v1::AvailableCommand>,
) -> Vec<protocol::SlashCommand> {
    commands
        .into_iter()
        .map(|c| protocol::SlashCommand {
            name: c.name,
            description: c.description,
            hint: c.input.and_then(|input| match input {
                AvailableCommandInput::Unstructured(u) => Some(u.hint),
                // SDK 1.4.0 仅含 Unstructured；non_exhaustive 要求通配
                _ => None,
            }),
        })
        .collect()
}

/// ACP `PlanEntry` 列表 → amux 协议投影（priority/status 非穷尽枚举按通配回落 Medium/Pending）。
fn acp_plan(
    entries: Vec<agent_client_protocol::schema::v1::PlanEntry>,
) -> Vec<protocol::SessionPlanEntry> {
    use agent_client_protocol::schema::v1::{PlanEntryPriority, PlanEntryStatus};
    entries
        .into_iter()
        .map(|e| protocol::SessionPlanEntry {
            content: e.content,
            priority: match e.priority {
                PlanEntryPriority::High => protocol::SessionPlanPriority::High,
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
            let resource = if let Some(blob) = blob {
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new(blob.clone(), uri.clone())
                        .mime_type(mime_type.clone()),
                )
            } else {
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new(text.clone().unwrap_or_default(), uri.clone())
                        .mime_type(mime_type.clone()),
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
                .mime_type(mime_type.clone())
                .title(title.clone())
                .description(description.clone()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AvailableCommandsUpdate, ConfigOptionUpdate, ContentBlock as AcpContentBlock, ContentChunk,
        SessionConfigOption, SessionConfigSelectOption, SessionId, TextContent, ToolCall,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };
    use tokio::sync::mpsc;

    type Routes = Mutex<HashMap<String, Vec<mpsc::Sender<AgentEvent>>>>;

    fn route_with_channel() -> (Routes, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let routes = Mutex::new(HashMap::from([("s1".to_string(), vec![tx])]));
        (routes, rx)
    }

    #[tokio::test]
    async fn disconnect_wakes_turn_when_event_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(AgentEvent::OutputChunk("尚未处理".into()))
            .unwrap();
        let caches = SessionCaches {
            routes: Arc::new(Mutex::new(HashMap::from([("s1".into(), vec![tx])]))),
            ..SessionCaches::default()
        };

        caches.clear_on_disconnect();

        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::OutputChunk(text)) if text == "尚未处理"
        ));
        assert!(
            matches!(rx.recv().await, Some(AgentEvent::Error(message)) if message == "ACP 连接已关闭")
        );
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted))
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
            AcpAgentDriver::spawn_with_shutdown(
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
        let driver = AcpAgentDriver {
            exec_tx: Arc::new(Mutex::new(None)),
            caches,
            resumed: Arc::new(Mutex::new(HashSet::new())),
            stop_tx,
            thread: Mutex::new(None),
            requires_auth: Arc::new(AtomicBool::new(false)),
        };
        let mut rx = driver.prompt("s1", Vec::new());
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::Error(message)) if message == "agent 已关闭"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted))
        ));
    }

    #[test]
    fn session_caps_from_agent_capabilities() {
        use agent_client_protocol::schema::v1::{
            AgentCapabilities, SessionCapabilities, SessionDeleteCapabilities,
        };
        // 未声明 sessionCapabilities.delete：不支持删除
        let caps = session_caps_from_agent_caps(&AgentCapabilities::new());
        assert!(!caps.delete);
        // 声明 `{}`：支持删除
        let caps = session_caps_from_agent_caps(&AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new().delete(SessionDeleteCapabilities::new()),
        ));
        assert!(caps.delete);
    }

    #[tokio::test]
    async fn route_update_available_commands_overwrites_cache() {
        let routes = Mutex::new(HashMap::new());
        let commands: Mutex<HashMap<String, Vec<protocol::SlashCommand>>> =
            Mutex::new(HashMap::new());
        let notif = |cmds: Vec<agent_client_protocol::schema::v1::AvailableCommand>| {
            SessionNotification::new(
                SessionId::new("s1"),
                SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(cmds)),
            )
        };
        route_update(
            &routes,
            &commands,
            &Mutex::new(HashMap::new()),
            &notif(vec![
                agent_client_protocol::schema::v1::AvailableCommand::new("goal", "目标"),
            ]),
        )
        .await;
        assert_eq!(commands.lock()["s1"].len(), 1);
        // 新通知全量覆盖旧集合。
        route_update(
            &routes,
            &commands,
            &Mutex::new(HashMap::new()),
            &notif(Vec::new()),
        )
        .await;
        assert!(commands.lock()["s1"].is_empty());
        // 无 prompt 路由时通知仍被缓存（不产生事件流）
    }

    #[tokio::test]
    async fn route_update_plan_overwrites_cache() {
        use agent_client_protocol::schema::v1::{
            Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
        };
        let routes = Mutex::new(HashMap::new());
        let plans: Mutex<HashMap<String, Vec<protocol::SessionPlanEntry>>> =
            Mutex::new(HashMap::new());
        let notif = |entries: Vec<agent_client_protocol::schema::v1::PlanEntry>| {
            SessionNotification::new(
                SessionId::new("s1"),
                SessionUpdate::Plan(Plan::new(entries)),
            )
        };
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &plans,
            &notif(vec![
                PlanEntry::new(
                    "第一步",
                    PlanEntryPriority::High,
                    PlanEntryStatus::InProgress,
                ),
                PlanEntry::new("收尾", PlanEntryPriority::Low, PlanEntryStatus::Pending),
            ]),
        )
        .await;
        let cached = plans.lock()["s1"].clone();
        assert_eq!(
            cached,
            vec![
                protocol::SessionPlanEntry {
                    content: "第一步".to_string(),
                    priority: protocol::SessionPlanPriority::High,
                    status: protocol::SessionPlanStatus::InProgress,
                },
                protocol::SessionPlanEntry {
                    content: "收尾".to_string(),
                    priority: protocol::SessionPlanPriority::Low,
                    status: protocol::SessionPlanStatus::Pending,
                },
            ]
        );
        // 新通知全量覆盖旧计划。
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &plans,
            &notif(Vec::new()),
        )
        .await;
        assert!(plans.lock()["s1"].is_empty());
    }

    #[tokio::test]
    async fn route_update_user_message_chunk() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::UserMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("收到"),
            ))),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn route_update_agent_message_chunk() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("输出"),
            ))),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到事件");
        assert!(matches!(ev, AgentEvent::OutputChunk(s) if s == "输出"));
    }

    #[tokio::test]
    async fn route_update_thinking() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("思考中"),
            ))),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 thinking 事件");
        assert!(matches!(ev, AgentEvent::Thinking(s) if s == "思考中"));
    }

    #[tokio::test]
    async fn route_update_tool_call() {
        let (routes, mut rx) = route_with_channel();
        let tc = ToolCall::new("tc1", "运行 cargo test")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({"command": "cargo test"}));
        let notif = SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCall(tc));
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 tool_call 事件");
        match ev {
            AgentEvent::ToolCall {
                id,
                name,
                title,
                parameters,
            } => {
                assert_eq!(id, "tc1");
                assert_eq!(name.as_deref(), Some("execute"));
                assert_eq!(title.as_deref(), Some("运行 cargo test"));
                assert!(parameters.unwrap_or_default().contains("cargo test"));
            }
            other => panic!("应为 ToolCall，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_update_tool_call_update_with_fields() {
        let (routes, mut rx) = route_with_channel();
        let tcu = ToolCallUpdate::new(
            "tc1",
            ToolCallUpdateFields::new()
                .kind(ToolKind::Execute)
                .title("运行测试")
                .raw_input(serde_json::json!({"cmd": "cargo test"})),
        );
        let notif =
            SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCallUpdate(tcu));
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 tool_call_update 事件");
        match ev {
            AgentEvent::ToolCall {
                id,
                name,
                title,
                parameters,
            } => {
                assert_eq!(id, "tc1");
                assert_eq!(name.as_deref(), Some("execute"));
                assert_eq!(title.as_deref(), Some("运行测试"));
                assert!(parameters.unwrap_or_default().contains("cargo test"));
            }
            other => panic!("应为 ToolCall，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_update_tool_call_update_kind_missing_keeps_others() {
        let (routes, mut rx) = route_with_channel();
        // ACP tool_call_update 的 kind 可选，常缺失；只更新 title。
        let tcu = ToolCallUpdate::new("tc1", ToolCallUpdateFields::new().title("更新后的标题"));
        let notif =
            SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCallUpdate(tcu));
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 tool_call_update 事件");
        match ev {
            AgentEvent::ToolCall { name, title, .. } => {
                assert_eq!(
                    name, None,
                    "kind 缺失时 name 应为 None（沿用合并器中的同 id 名称）"
                );
                assert_eq!(title.as_deref(), Some("更新后的标题"));
            }
            other => panic!("应为 ToolCall，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_update_tool_call_update_empty_skipped() {
        let (routes, mut rx) = route_with_channel();
        // 仅 status 变化（无可合并字段）不产生事件，避免空条目。
        let tcu = ToolCallUpdate::new(
            "tc1",
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        );
        let notif =
            SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCallUpdate(tcu));
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        assert!(rx.try_recv().is_err(), "仅 status 的 update 不应产生事件");
    }

    #[tokio::test]
    async fn route_update_tool_call_update_content_not_recorded_as_activity() {
        let (routes, mut rx) = route_with_channel();
        // `tool_call_update` 携带 title 时仅合并到 tool_call 活动；
        // `content`（工具结果）不再单独成条活动（DESIGN 活动格式无 tool_result）。
        let tcu = ToolCallUpdate::new("tc1", ToolCallUpdateFields::new().title("读完了"));
        let notif =
            SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCallUpdate(tcu));
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 tool_call 事件");
        assert!(
            matches!(ev, AgentEvent::ToolCall { title, .. } if title.as_deref() == Some("读完了"))
        );
        assert!(rx.try_recv().is_err(), "content 不应单独产生工具结果活动");
    }

    #[tokio::test]
    async fn route_update_session_info() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new(),
            ),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn route_update_usage_update() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::UsageUpdate(agent_client_protocol::schema::v1::UsageUpdate::new(
                53_000, 200_000,
            )),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 usage 事件");
        match ev {
            AgentEvent::UsageUpdate { used, size } => {
                assert_eq!(used, 53_000);
                assert_eq!(size, 200_000);
            }
            other => panic!("应为 UsageUpdate，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_update_config_option_update() {
        let (routes, mut rx) = route_with_channel();
        let opt = SessionConfigOption::select(
            "model",
            "模型",
            "gpt-5",
            vec![SessionConfigSelectOption::new("gpt-5", "GPT-5")],
        );
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![opt])),
        );
        route_update(
            &routes,
            &Mutex::new(HashMap::new()),
            &Mutex::new(HashMap::new()),
            &notif,
        )
        .await;
        let ev = rx.try_recv().expect("应收到 config_options 事件");
        match ev {
            AgentEvent::ConfigOptions(opts) => {
                assert_eq!(opts.len(), 1);
                assert_eq!(opts[0].id, "model");
                assert_eq!(opts[0].name, "模型");
                match &opts[0].kind {
                    protocol::SessionConfigKind::Select {
                        current_value,
                        options,
                    } => {
                        assert_eq!(current_value, "gpt-5");
                        assert_eq!(options.len(), 1);
                        assert_eq!(options[0].value, "gpt-5");
                    }
                    other => panic!("应为 Select 选项，得到 {other:?}"),
                }
            }
            other => panic!("应为 ConfigOptions，得到 {other:?}"),
        }
    }

    #[test]
    fn pick_approve_option_prefers_allow() {
        fn opt(id: &str, kind: PermissionOptionKind) -> PermissionOption {
            PermissionOption::new(id.to_string(), id.to_string(), kind)
        }

        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("allow", PermissionOptionKind::AllowOnce),
            opt("allow_always", PermissionOptionKind::AllowAlways),
        ];
        let picked = pick_approve_option(&opts).expect("应批准");
        assert_eq!(
            picked.to_string(),
            "allow_always",
            "应优先选 AllowAlways（yolo 免重复询问）"
        );

        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("allow", PermissionOptionKind::AllowOnce),
        ];
        assert_eq!(pick_approve_option(&opts).unwrap().to_string(), "allow");

        let opts = vec![opt("allow-once", PermissionOptionKind::AllowOnce)];
        assert_eq!(
            pick_approve_option(&opts).unwrap().to_string(),
            "allow-once"
        );

        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("reject_all", PermissionOptionKind::RejectAlways),
        ];
        assert!(pick_approve_option(&opts).is_none());

        assert!(pick_approve_option(&[]).is_none());
    }
}
