//! Daemon 接入：WebSocket 握手认证、请求分发、Agent 生命周期与 ACP 连接管理。
//!
//! 一条机器一条长连接（docs/DESIGN.md「Server-Daemon 通信」）；Server 侧对 Daemon
//! 的调用都是 JSON-RPC 请求（id 关联应答），Daemon 上行的 `acp` / `terminal.*`
//! 通知按 agent / terminal 路由到 ACP 连接与终端缓存。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use amux_common::api::Agent;
use amux_common::daemon::{
    header, method, notify, AcpForward, AgentListResult, AgentParams, GitDiffParams, GitRepoParams,
    MachineInfo, WorktreePathParams, WorktreeResult,
};
use amux_common::domain::{
    FsListParams, FsListResult, FsReadParams, FsReadResult, GitBranchListResult, GitDiffResult,
    OpResult, TerminalExitNotification, TerminalIdParams, TerminalInputParams, TerminalOpenParams,
    TerminalOpenResult, TerminalOutputNotification, TerminalResizeParams,
};
use amux_common::jsonrpc::{JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::future::join_all;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::acp::{self, AcpEvent, AgentConnection};
use crate::frames;
use crate::terminals::TerminalCache;

/// Daemon 请求超时（worktree 创建等可能较慢）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct MachineHub {
    token: String,
    config: Arc<crate::config_store::ConfigStore>,
    events: mpsc::Sender<AcpEvent>,
    terminals: Arc<TerminalCache>,
    machines: Arc<Mutex<HashMap<String, Arc<Machine>>>>,
}

/// 一台机器的状态。生命周期跨越 WebSocket 重连：Daemon 断线期间保留 ACP 连接记录，
/// 重连后若 Daemon 未重启则直接复用（docs/DESIGN.md「Agent 生命周期」）。
struct Machine {
    name: String,
    /// 当前 WebSocket 的下行通道；未接入时为空
    ws: Mutex<Option<mpsc::Sender<String>>>,
    info: Mutex<Option<MachineInfo>>,
    /// 最近一次握手读取的 Daemon 启动时间
    daemon_boot_time: Mutex<Option<String>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>,
    next_id: AtomicU64,
    agents: Mutex<HashMap<String, AgentSlot>>,
    /// 按 agent 串行化连接建立：一个 agent 只有一条 stdio 连接，
    /// ACP v2 每条连接只允许一次 initialize，并发建立会让同一进程收到两次 initialize。
    connect_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

#[derive(Clone)]
struct AgentSlot {
    conn: Option<Arc<AgentConnection>>,
    /// 下行给该 agent 的 ACP 消息（连接任务的入站流）
    inbound: mpsc::Sender<String>,
}

impl MachineHub {
    pub fn new(
        token: String,
        events: mpsc::Sender<AcpEvent>,
        terminals: Arc<TerminalCache>,
        config: Arc<crate::config_store::ConfigStore>,
    ) -> Self {
        Self {
            token,
            config,
            events,
            terminals,
            machines: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ---------- 客户端 API 用到的方法 ----------

    pub fn machines(&self) -> Vec<MachineInfo> {
        self.machines
            .lock()
            .values()
            .filter(|machine| machine.is_connected())
            .filter_map(|machine| machine.info.lock().clone())
            .collect()
    }

    /// 查询机器上的 agents 与可用性（可用 = 已启动且 ACP 连接就绪）。
    pub async fn agents(&self, machine_name: &str) -> Result<Vec<Agent>, String> {
        let machine = self.machine(machine_name)?;
        let discovered: AgentListResult = machine.request(method::AGENT_LIST, ()).await?;
        Ok(discovered
            .agents
            .into_iter()
            .map(|agent| {
                let connection = machine.connection(&agent.name);
                Agent {
                    available: agent.running && connection.is_some(),
                    opened_sessions: connection
                        .as_ref()
                        .map_or(0, |connection| connection.opened_session_count()),
                    name: agent.name,
                }
            })
            .collect())
    }

    /// 重新发现 agents：发现、启动未运行者、建立 ACP 连接，再回报最新状态。
    /// 已启动且有活跃 ACP 连接记录的 agent 不重启。
    pub async fn rediscover(&self, machine_name: &str) -> Result<Vec<Agent>, String> {
        let machine = self.machine(machine_name)?;
        self.start_agents(&machine, false).await?;
        self.agents(machine_name).await
    }

    /// 重启（或启动）指定 agent，并重建 ACP 连接。
    pub async fn restart_agent(&self, machine_name: &str, agent: &str) -> Result<(), String> {
        let machine = self.machine(machine_name)?;
        let lock = machine.connect_lock(agent);
        let _guard = lock.lock().await;
        self.restart_and_connect(&machine, agent).await?;
        self.notify_agent_restarted(&machine, agent);
        Ok(())
    }

    pub async fn fs_list(
        &self,
        machine: &str,
        params: FsListParams,
    ) -> Result<FsListResult, String> {
        self.machine(machine)?
            .request(method::FS_LIST, params)
            .await
    }

    pub async fn fs_read(
        &self,
        machine: &str,
        params: FsReadParams,
    ) -> Result<FsReadResult, String> {
        self.machine(machine)?
            .request(method::FS_READ, params)
            .await
    }

    pub async fn git_diff(
        &self,
        machine: &str,
        repo: &str,
        base: &str,
    ) -> Result<GitDiffResult, String> {
        self.machine(machine)?
            .request(
                method::GIT_DIFF,
                GitDiffParams {
                    repo: repo.to_string(),
                    base: base.to_string(),
                },
            )
            .await
    }

    pub async fn git_branches(
        &self,
        machine: &str,
        repo: &str,
    ) -> Result<GitBranchListResult, String> {
        self.machine(machine)?
            .request(
                method::GIT_BRANCHES,
                GitRepoParams {
                    repo: repo.to_string(),
                },
            )
            .await
    }

    pub async fn worktree_new(&self, machine: &str, repo: &str) -> Result<String, String> {
        let result: WorktreeResult = self
            .machine(machine)?
            .request(
                method::GIT_WORKTREE_NEW,
                GitRepoParams {
                    repo: repo.to_string(),
                },
            )
            .await?;
        Ok(result.worktree_dir)
    }

    pub async fn worktree_resume(
        &self,
        machine: &str,
        repo: &str,
        path: &str,
    ) -> Result<String, String> {
        let result: WorktreeResult = self
            .machine(machine)?
            .request(
                method::GIT_WORKTREE_RESUME,
                WorktreePathParams {
                    repo: repo.to_string(),
                    path: path.to_string(),
                },
            )
            .await?;
        Ok(result.worktree_dir)
    }

    pub async fn worktree_remove(&self, machine: &str, repo: &str, path: &str) {
        let Ok(machine) = self.machine(machine) else {
            return;
        };
        let _ = machine
            .request::<_, OpResult>(
                method::GIT_WORKTREE_REMOVE,
                WorktreePathParams {
                    repo: repo.to_string(),
                    path: path.to_string(),
                },
            )
            .await;
    }

    pub async fn terminal_open(
        &self,
        machine: &str,
        params: TerminalOpenParams,
    ) -> Result<String, String> {
        let result: TerminalOpenResult = self
            .machine(machine)?
            .request(method::TERMINAL_OPEN, params)
            .await?;
        Ok(result.terminal_id)
    }

    pub async fn terminal_input(
        &self,
        machine: &str,
        params: TerminalInputParams,
    ) -> Result<(), String> {
        let _: OpResult = self
            .machine(machine)?
            .request(method::TERMINAL_INPUT, params)
            .await?;
        Ok(())
    }

    pub async fn terminal_resize(
        &self,
        machine: &str,
        params: TerminalResizeParams,
    ) -> Result<(), String> {
        let _: OpResult = self
            .machine(machine)?
            .request(method::TERMINAL_RESIZE, params)
            .await?;
        Ok(())
    }

    pub async fn terminal_close(&self, machine: &str, terminal_id: &str) {
        let Ok(machine) = self.machine(machine) else {
            return;
        };
        let _ = machine
            .request::<_, OpResult>(
                method::TERMINAL_CLOSE,
                TerminalIdParams {
                    terminal_id: terminal_id.to_string(),
                },
            )
            .await;
    }

    /// 取（必要时建立）某 agent 的 ACP 连接。
    ///
    /// 本地没有活跃连接记录时必须先让 Daemon 重启 agent（docs/DESIGN.md「Agent 生命周期」）：
    /// ACP v2 每条连接只允许一次 initialize，向已初始化的 agent 进程再发 initialize 会被拒绝，
    /// 而该拒绝不会随重试消失，只能靠重启进程解决。
    pub async fn acp(&self, machine: &str, agent: &str) -> Result<Arc<AgentConnection>, String> {
        let machine = self.machine(machine)?;
        if let Some(conn) = machine.connection(agent) {
            return Ok(conn);
        }
        let lock = machine.connect_lock(agent);
        let _guard = lock.lock().await;
        // 等锁期间可能已有其它调用建立了连接
        if let Some(conn) = machine.connection(agent) {
            return Ok(conn);
        }
        let conn = self.restart_and_connect(&machine, agent).await?;
        self.notify_agent_restarted(&machine, agent);
        Ok(conn)
    }

    /// 关闭所有已打开会话，供 Server 退出时清理 Agent 侧资源。
    pub async fn close_all_sessions(&self) {
        let machines: Vec<_> = self.machines.lock().values().cloned().collect();
        let connections: Vec<_> = machines
            .into_iter()
            .flat_map(|machine| {
                machine
                    .agents
                    .lock()
                    .values()
                    .filter_map(|slot| slot.conn.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        join_all(connections.iter().map(|connection| connection.close_all())).await;
    }

    // ---------- Daemon 连接 ----------

    /// Daemon 握手与接入：校验 token、机器名、机器重名与 Daemon 启动时间后升级为长连接
    /// （docs/DESIGN.md「认证」：任一不满足即握手失败）。
    pub fn upgrade(&self, headers: &HeaderMap, ws: WebSocketUpgrade) -> Response {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        if token != self.token {
            log::warn!("Daemon 握手被拒：token 不正确");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let Some(machine_name) = machine_from_headers(headers) else {
            log::warn!("Daemon 握手被拒：缺少机器名或机器名编码非法");
            return StatusCode::BAD_REQUEST.into_response();
        };
        // 重名只针对当前已接入的机器：断线的机器会保留状态等待重连
        if self
            .machines
            .lock()
            .get(&machine_name)
            .is_some_and(|machine| machine.is_connected())
        {
            log::warn!("Daemon 握手被拒：机器重名 {machine_name}");
            return StatusCode::CONFLICT.into_response();
        }
        let Some(boot_time) = daemon_boot_time(headers) else {
            log::warn!("Daemon 握手被拒：缺少 Daemon 启动时间");
            return StatusCode::BAD_REQUEST.into_response();
        };

        let hub = self.clone();
        ws.on_upgrade(move |socket| async move {
            if let Err(error) = hub.serve(machine_name, boot_time, socket).await {
                log::warn!("Daemon 连接结束: {error}");
            }
        })
    }

    async fn serve(
        self,
        machine_name: String,
        boot_time: String,
        socket: WebSocket,
    ) -> Result<(), String> {
        let machine = self.machine_slot(&machine_name);
        let (outgoing, mut frames) = mpsc::channel::<String>(256);
        *machine.ws.lock() = Some(outgoing.clone());
        log::info!("Daemon 已接入: {machine_name}");

        let (mut sink, mut stream) = socket.split();
        let writer = tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                // ACP auth/login 帧包含模型 API Key，不记录下行载荷。
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
        });

        // 就绪流程必须在读循环之外单独跑：请求的应答要由读循环投递，
        // 在读循环启动前 await 请求会自锁（直到请求超时）。
        tokio::spawn({
            let hub = self.clone();
            let machine = Arc::clone(&machine);
            async move {
                match machine
                    .request::<_, MachineInfo>(method::MACHINE_INFO, ())
                    .await
                {
                    Ok(info) => *machine.info.lock() = Some(info),
                    Err(error) => log::warn!("machine.info 失败（{}）: {error}", machine.name),
                }
                // Agent 生命周期：判定是否重建 ACP 连接 → 发现/启动/重启 → 建立连接与初始化
                let rebuild = {
                    let mut last = machine.daemon_boot_time.lock();
                    let rebuild = last.as_deref() != Some(boot_time.as_str());
                    *last = Some(boot_time.clone());
                    rebuild
                };
                if let Err(error) = hub.start_agents(&machine, rebuild).await {
                    log::warn!("Agent 生命周期处理失败（{}）: {error}", machine.name);
                }
            }
        });

        while let Some(message) = stream.next().await {
            let Ok(message) = message else { break };
            match message {
                Message::Text(text) => self.dispatch(&machine, &text.to_string()),
                Message::Close(_) => break,
                _ => {}
            }
        }

        // 断线只解绑 WebSocket：ACP 连接记录保留，重连后按「Agent 生命周期」判定复用。
        // 仅解绑自己绑定过的通道，避免旧连接退出时覆盖新连接。
        writer.abort();
        {
            let mut ws = machine.ws.lock();
            if ws
                .as_ref()
                .is_some_and(|current| current.same_channel(&outgoing))
            {
                *ws = None;
                *machine.info.lock() = None;
            }
        }
        let pending: Vec<_> = machine.pending.lock().drain().map(|(_, tx)| tx).collect();
        for tx in pending {
            let _ = tx.send(Err("Daemon 连接已断开".to_string()));
        }
        log::info!("Daemon 连接断开: {machine_name}");
        Ok(())
    }

    /// 取（必要时创建）机器状态；不校验是否已接入。
    fn machine_slot(&self, name: &str) -> Arc<Machine> {
        self.machines
            .lock()
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Machine {
                    name: name.to_string(),
                    ws: Mutex::new(None),
                    info: Mutex::new(None),
                    daemon_boot_time: Mutex::new(None),
                    pending: Mutex::new(HashMap::new()),
                    next_id: AtomicU64::new(0),
                    agents: Mutex::new(HashMap::new()),
                    connect_locks: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    fn dispatch(&self, machine: &Arc<Machine>, text: &str) {
        log::debug!("上行帧: {}", amux_common::text::truncate(text, 200));
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            log::warn!("收到非法 JSON 帧");
            return;
        };
        if value.get("method").is_none() {
            if let Ok(response) = serde_json::from_value::<JsonRpcResponse>(value) {
                let id = match response.id {
                    JsonRpcId::Number(id) => id,
                    JsonRpcId::String(id) => id.parse().unwrap_or_default(),
                };
                let Some(tx) = machine.pending.lock().remove(&id) else {
                    return;
                };
                let result = match (response.result, response.error) {
                    (Some(result), _) => Ok(result),
                    (None, Some(error)) => Err(error.message),
                    (None, None) => Ok(serde_json::Value::Null),
                };
                let _ = tx.send(result);
            }
            return;
        }
        let Ok(notification) = serde_json::from_value::<JsonRpcNotification>(value) else {
            return;
        };
        match notification.method.as_str() {
            notify::ACP => self.route_acp(machine, notification.params),
            notify::TERMINAL_OUTPUT => {
                if let Some(output) = decode::<TerminalOutputNotification>(notification.params) {
                    if let Ok(bytes) =
                        base64::engine::general_purpose::STANDARD.decode(&output.data)
                    {
                        self.terminals.output(&output.terminal_id, &bytes);
                    }
                }
            }
            notify::TERMINAL_EXIT => {
                if let Some(exit) = decode::<TerminalExitNotification>(notification.params) {
                    self.terminals.exit(&exit.terminal_id);
                }
            }
            other => log::debug!("忽略未知通知: {other}"),
        }
    }

    fn route_acp(&self, machine: &Arc<Machine>, params: Option<serde_json::Value>) {
        let Some(forward) = decode::<AcpForward>(params) else {
            return;
        };
        let slot = machine.agents.lock().get(&forward.agent).cloned();
        match slot {
            Some(slot) => {
                if slot.inbound.try_send(forward.raw).is_err() {
                    log::warn!("ACP 上行积压，丢弃一条（{}）", forward.agent);
                }
            }
            None => log::debug!("忽略未连接 agent 的 ACP 消息: {}", forward.agent),
        }
    }

    /// Agent 生命周期（docs/DESIGN.md「Agent 生命周期」）：`rebuild` 时逐个重启 agent 并初始化；
    /// 不重建时未启动的 agent 启动并初始化，已启动且 Server 持有活跃 ACP 连接记录的 agent 直接复用。
    async fn start_agents(&self, machine: &Arc<Machine>, rebuild: bool) -> Result<(), String> {
        let list: AgentListResult = machine.request(method::AGENT_LIST, ()).await?;
        for agent in list.agents {
            // 复用仅在不重建、agent 已启动且确有活跃连接记录时成立；
            // 已启动但无记录（例如上次建立失败）只能换一个新进程，否则 initialize 会被拒。
            if !rebuild && agent.running && machine.has_connection(&agent.name) {
                continue;
            }
            // 同一 agent 的连接建立要串行化，避免与惰性连接（`acp`）同时 initialize
            let lock = machine.connect_lock(&agent.name);
            let _guard = lock.lock().await;
            if !rebuild && agent.running && machine.has_connection(&agent.name) {
                continue;
            }
            if let Err(error) = self.restart_and_connect(machine, &agent.name).await {
                log::warn!(
                    "重建 agent 连接失败（{}@{}）: {error}",
                    agent.name,
                    machine.name
                );
                continue;
            }
            self.notify_agent_restarted(machine, &agent.name);
        }
        Ok(())
    }

    /// 让 Daemon 关闭并重新启动 agent，随后重建 ACP 连接与 initialize。
    ///
    /// 调用方必须持有该 agent 的连接锁（见 `Machine::connect_lock`）。
    async fn restart_and_connect(
        &self,
        machine: &Arc<Machine>,
        agent: &str,
    ) -> Result<Arc<AgentConnection>, String> {
        machine
            .request::<_, OpResult>(
                method::AGENT_RESTART,
                AgentParams {
                    agent: agent.to_string(),
                },
            )
            .await?;
        machine.drop_agent(agent);
        self.connect_agent(machine, agent).await
    }

    async fn connect_agent(
        &self,
        machine: &Arc<Machine>,
        agent: &str,
    ) -> Result<Arc<AgentConnection>, String> {
        let (inbound_tx, inbound_rx) = mpsc::channel::<String>(256);
        // 先登记入站通道：initialize 的应答可能在连接建立完成前就到达，
        // 未登记就会被当作「未连接 agent 的消息」丢弃，握手随之超时。
        machine.agents.lock().insert(
            agent.to_string(),
            AgentSlot {
                conn: None,
                inbound: inbound_tx.clone(),
            },
        );
        let outgoing = machine.agent_outgoing(agent);
        let conn = match acp::connect(
            &machine.name,
            agent,
            outgoing,
            inbound_rx,
            self.events.clone(),
            if agent == amux_common::api::NANO_AGENT {
                self.config.orchestrator()
            } else {
                None
            },
        )
        .await
        {
            Ok(conn) => conn,
            Err(error) => {
                machine.drop_agent(agent);
                return Err(error);
            }
        };
        machine.agents.lock().insert(
            agent.to_string(),
            AgentSlot {
                conn: Some(Arc::clone(&conn)),
                inbound: inbound_tx,
            },
        );
        Ok(conn)
    }

    fn notify_agent_restarted(&self, machine: &Arc<Machine>, agent: &str) {
        let _ = self.events.try_send(AcpEvent::AgentRestarted {
            machine: machine.name.clone(),
            agent: agent.to_string(),
        });
    }

    fn machine(&self, name: &str) -> Result<Arc<Machine>, String> {
        self.machines
            .lock()
            .get(name)
            .filter(|machine| machine.is_connected())
            .cloned()
            .ok_or_else(|| "机器未连接".to_string())
    }
}

impl Machine {
    /// Daemon 是否已接入（WebSocket 已绑定）。
    fn is_connected(&self) -> bool {
        self.ws.lock().is_some()
    }

    /// 活跃 ACP 连接：连接任务已结束（calls 通道关闭）的不算，
    /// 否则会挡住「无活跃连接 → 重启 agent」的恢复路径。
    fn connection(&self, agent: &str) -> Option<Arc<AgentConnection>> {
        self.agents
            .lock()
            .get(agent)
            .and_then(|slot| slot.conn.clone())
            .filter(|conn| conn.is_alive())
    }

    fn has_connection(&self, agent: &str) -> bool {
        self.connection(agent).is_some()
    }

    fn drop_agent(&self, agent: &str) {
        self.agents.lock().remove(agent);
    }

    /// 该 agent 的连接建立锁：同一 agent 的「重启 → initialize」必须互斥。
    fn connect_lock(&self, agent: &str) -> Arc<AsyncMutex<()>> {
        self.connect_locks
            .lock()
            .entry(agent.to_string())
            .or_default()
            .clone()
    }

    /// 向当前 WebSocket 发一帧；未接入时丢弃这一帧。
    ///
    /// Daemon 断线期间 ACP 连接保留（重连后可复用），因此这里不能报错断开连接，
    /// 否则连接任务会随之结束、复用无从谈起。
    async fn send_to_daemon(&self, frame: String) -> bool {
        let Some(ws) = self.ws.lock().clone() else {
            return true;
        };
        ws.send(frame).await.is_ok()
    }

    /// 该 agent 的出站通道：原始 ACP 消息 → `acp` 通知。
    fn agent_outgoing(self: &Arc<Self>, agent: &str) -> mpsc::Sender<String> {
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let machine = Arc::clone(self);
        let agent = agent.to_string();
        tokio::spawn(async move {
            while let Some(raw) = rx.recv().await {
                let frame = frames::notification(
                    notify::ACP,
                    &AcpForward {
                        agent: agent.clone(),
                        raw,
                    },
                );
                machine.send_to_daemon(frame).await;
            }
        });
        tx
    }

    /// 向 Daemon 发一次请求并等待应答。
    async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let frame = serde_json::to_string(&JsonRpcRequest::new(id, method, params))
            .map_err(|error| format!("请求序列化失败: {error}"))?;
        let Some(ws) = self.ws.lock().clone() else {
            self.pending.lock().remove(&id);
            return Err("Daemon 连接已断开".to_string());
        };
        if ws.send(frame).await.is_err() {
            self.pending.lock().remove(&id);
            return Err("Daemon 连接已关闭".to_string());
        }
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err("Daemon 连接已关闭".to_string()),
            Err(_) => {
                self.pending.lock().remove(&id);
                return Err(format!("Daemon 请求超时: {method}"));
            }
        }?;
        serde_json::from_value(response)
            .map_err(|error| format!("应答解析失败（{method}）: {error}"))
    }
}

fn decode<T: DeserializeOwned>(params: Option<serde_json::Value>) -> Option<T> {
    let params = params?;
    match serde_json::from_value(params) {
        Ok(value) => Some(value),
        Err(error) => {
            log::warn!("通知负载非法: {error}");
            None
        }
    }
}

/// 握手请求中的机器名：`amux-machine` 头值为 URL 编码（docs/DESIGN.md「认证」）。
fn machine_from_headers(headers: &HeaderMap) -> Option<String> {
    header::decode_machine(headers.get(header::MACHINE)?.to_str().ok()?)
}

/// 握手请求中的 Daemon 启动时间：用于判断是否需要重建 ACP 连接。
fn daemon_boot_time(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::DAEMON_BOOT_TIME)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2::{Implementation, InitializeResponse};
    use agent_client_protocol::schema::ProtocolVersion;

    #[test]
    fn machine_name_is_decoded_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::MACHINE,
            header::encode_machine("开发机 A").parse().unwrap(),
        );
        assert_eq!(machine_from_headers(&headers).as_deref(), Some("开发机 A"));
        assert_eq!(machine_from_headers(&HeaderMap::new()), None);
    }

    #[test]
    fn daemon_boot_time_is_read_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::DAEMON_BOOT_TIME, "1790588263791".parse().unwrap());
        assert_eq!(daemon_boot_time(&headers).as_deref(), Some("1790588263791"));
        assert_eq!(daemon_boot_time(&HeaderMap::new()), None);
        headers.insert(header::DAEMON_BOOT_TIME, "".parse().unwrap());
        assert_eq!(daemon_boot_time(&headers), None);
    }

    const AGENT: &str = "codex";

    /// 伪 Daemon：应答 Server 的请求，把 `initialize` 的应答写回连接入站流，
    /// 并按发生顺序记录每次「agent 重启」与「agent 初始化」。
    struct FakeDaemon {
        hub: Arc<MachineHub>,
        machine: Arc<Machine>,
        seen: Arc<Mutex<Vec<String>>>,
        /// 伪 Daemon 上报的 agent 启动状态
        running: bool,
    }

    impl FakeDaemon {
        fn new(running: bool) -> Self {
            let (events, _events_rx) = mpsc::channel(8);
            let hub = Arc::new(MachineHub::new(
                "token".to_string(),
                events,
                Arc::new(TerminalCache::new()),
                Arc::new(crate::config_store::ConfigStore::new(
                    std::env::temp_dir().join("amux-machines-test"),
                )),
            ));
            let machine = hub.machine_slot("m");
            Self {
                hub,
                machine,
                seen: Arc::new(Mutex::new(Vec::new())),
                running,
            }
        }

        /// 模拟一次 WebSocket 接入：绑定下行通道并开始处理 Server 的帧。
        fn attach(&self) {
            let (outgoing, mut frames) = mpsc::channel::<String>(16);
            *self.machine.ws.lock() = Some(outgoing);
            let hub = Arc::clone(&self.hub);
            let machine = Arc::clone(&self.machine);
            let seen = Arc::clone(&self.seen);
            let running = self.running;
            tokio::spawn(async move {
                while let Some(frame) = frames.recv().await {
                    respond(&hub, &machine, &seen, running, &frame).await;
                }
            });
        }

        /// 模拟 Daemon 断线：解绑 WebSocket，ACP 连接记录保留。
        fn detach(&self) {
            *self.machine.ws.lock() = None;
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().clone()
        }
    }

    /// 处理一帧 Server → Daemon 的消息：请求直接应答，ACP 下行按 agent 送回连接入站流。
    async fn respond(
        hub: &Arc<MachineHub>,
        machine: &Arc<Machine>,
        seen: &Arc<Mutex<Vec<String>>>,
        running: bool,
        frame: &str,
    ) {
        let value: serde_json::Value = serde_json::from_str(frame).unwrap();
        let result = match value["method"].as_str().unwrap_or_default() {
            method::MACHINE_INFO => serde_json::to_value(MachineInfo {
                name: "m".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                hostname: "fake".to_string(),
                temp_dir: "/tmp".to_string(),
                version: "0".to_string(),
            })
            .unwrap(),
            method::AGENT_LIST => serde_json::to_value(AgentListResult {
                agents: vec![amux_common::daemon::DiscoveredAgent {
                    name: AGENT.to_string(),
                    running,
                }],
            })
            .unwrap(),
            method::AGENT_RESTART => {
                seen.lock().push("agent.restart".to_string());
                serde_json::to_value(OpResult::ok()).unwrap()
            }
            notify::ACP => {
                let raw = value["params"]["raw"].as_str().unwrap();
                let request: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(request["method"], "initialize", "伪 Daemon 只应答初始化");
                seen.lock().push("initialize".to_string());
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": serde_json::to_value(InitializeResponse::new(
                        ProtocolVersion::V2,
                        Implementation::new("fake-daemon", "0"),
                    ))
                    .unwrap(),
                });
                let inbound = machine
                    .agents
                    .lock()
                    .get(AGENT)
                    .map(|slot| slot.inbound.clone())
                    .expect("连接入站流未登记");
                inbound.send(response.to_string()).await.unwrap();
                return;
            }
            other => panic!("伪 Daemon 收到未预期的帧: {other}"),
        };
        let response = serde_json::json!({"jsonrpc": "2.0", "id": value["id"], "result": result});
        hub.dispatch(machine, &response.to_string());
    }

    /// 重建：Daemon 首次接入或已重启时，逐个重启 agent 并 initialize。
    #[tokio::test]
    async fn rebuild_restarts_agent_and_initializes() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, true)
            .await
            .unwrap();
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// 不重建且 agent 已启动：复用活跃 ACP 连接，不重启也不重新 initialize。
    #[tokio::test]
    async fn reuse_keeps_existing_connection() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, true)
            .await
            .unwrap();
        let before = daemon.machine.connection(AGENT).expect("应持有连接");

        daemon
            .hub
            .start_agents(&daemon.machine, false)
            .await
            .unwrap();
        let after = daemon.machine.connection(AGENT).expect("应复用连接");
        assert!(Arc::ptr_eq(&before, &after), "复用应是同一条 ACP 连接");
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// Daemon 重启（启动时间变化）：即使 Server 还留着连接记录也要重建。
    #[tokio::test]
    async fn daemon_restart_rebuilds_connection() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, true)
            .await
            .unwrap();
        daemon
            .hub
            .start_agents(&daemon.machine, true)
            .await
            .unwrap();
        assert_eq!(
            daemon.seen(),
            vec!["agent.restart", "initialize", "agent.restart", "initialize"]
        );
    }

    /// 不重建但 agent 未启动：启动 agent 并 initialize。
    #[tokio::test]
    async fn stopped_agent_is_started() {
        let daemon = FakeDaemon::new(false);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, false)
            .await
            .unwrap();
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// 不重建、agent 已启动但没有活跃连接记录：只能换一个新进程再 initialize。
    #[tokio::test]
    async fn running_agent_without_record_is_restarted() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, false)
            .await
            .unwrap();
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// WebSocket 断线不解绑 ACP 连接：机器从列表消失，但连接记录保留，重连后直接复用。
    #[tokio::test]
    async fn websocket_detach_keeps_connection_for_reuse() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, true)
            .await
            .unwrap();
        let before = daemon.machine.connection(AGENT).expect("应持有连接");

        daemon.detach();
        assert!(daemon.hub.machines().is_empty(), "断线后不列入机器列表");
        assert!(daemon.machine.connection(AGENT).is_some(), "连接记录应保留");

        daemon.attach();
        daemon
            .hub
            .start_agents(&daemon.machine, false)
            .await
            .unwrap();
        let after = daemon.machine.connection(AGENT).expect("重连后应复用连接");
        assert!(Arc::ptr_eq(&before, &after), "复用应是同一条 ACP 连接");
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// 并发取连接只建立一次 ACP 连接：ACP v2 每条连接只允许一次 initialize，
    /// 向同一 agent 进程重复 initialize 会被拒绝且无法靠重试恢复。
    #[tokio::test]
    async fn concurrent_acp_opens_connection_once() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let hub = Arc::clone(&daemon.hub);
            tasks.push(tokio::spawn(async move {
                hub.acp("m", AGENT).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.unwrap().expect("并发取连接应成功");
        }
        assert_eq!(daemon.seen(), vec!["agent.restart", "initialize"]);
    }

    /// 本地无活跃连接记录时必须先重启 agent 再 initialize（docs/DESIGN.md「Agent 生命周期」）：
    /// agent 进程只接受一次 initialize，不换进程的重试永远失败。
    #[tokio::test]
    async fn missing_connection_restarts_agent_before_initialize() {
        let daemon = FakeDaemon::new(true);
        daemon.attach();
        daemon.hub.acp("m", AGENT).await.expect("首次连接应成功");
        daemon.machine.drop_agent(AGENT);
        daemon
            .hub
            .acp("m", AGENT)
            .await
            .expect("无记录时应重启并重建连接");
        assert_eq!(
            daemon.seen(),
            vec!["agent.restart", "initialize", "agent.restart", "initialize"]
        );
    }
}
