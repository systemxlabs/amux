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
    header, method, notify, AcpForward, AgentListResult, AgentParams, GitRepoParams,
    GitRestoreParams, MachineInfo, WorktreePathParams, WorktreeResult,
};
use amux_common::domain::{
    FsListParams, FsListResult, FsReadParams, FsReadResult, GitDiffResult, OpResult,
    TerminalExitNotification, TerminalIdParams, TerminalInputParams, TerminalOpenParams,
    TerminalOpenResult, TerminalOutputNotification, TerminalResizeParams,
};
use amux_common::jsonrpc::{JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::acp::{self, AcpEvent, AgentConnection};
use crate::frames;
use crate::terminals::TerminalCache;

/// Daemon 请求超时（worktree 创建等可能较慢）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct MachineHub {
    token: String,
    events: mpsc::Sender<AcpEvent>,
    terminals: Arc<TerminalCache>,
    machines: Arc<Mutex<HashMap<String, Arc<Machine>>>>,
}

struct Machine {
    name: String,
    info: Mutex<Option<MachineInfo>>,
    outgoing: mpsc::Sender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>,
    next_id: AtomicU64,
    agents: Mutex<HashMap<String, AgentSlot>>,
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
    ) -> Self {
        Self {
            token,
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
            .map(|agent| Agent {
                available: agent.running && machine.has_connection(&agent.name),
                name: agent.name,
            })
            .collect())
    }

    /// 重新发现 agents：发现、启动未运行者、建立 ACP 连接，再回报最新状态。
    pub async fn rediscover(&self, machine_name: &str) -> Result<Vec<Agent>, String> {
        let machine = self.machine(machine_name)?;
        self.start_agents(&machine).await?;
        self.agents(machine_name).await
    }

    /// 重启（或启动）指定 agent，并重建 ACP 连接。
    pub async fn restart_agent(&self, machine_name: &str, agent: &str) -> Result<(), String> {
        let machine = self.machine(machine_name)?;
        machine
            .request::<_, OpResult>(
                method::AGENT_RESTART,
                AgentParams {
                    agent: agent.to_string(),
                },
            )
            .await?;
        machine.drop_agent(agent);
        self.connect_agent(&machine, agent).await?;
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

    pub async fn git_diff(&self, machine: &str, repo: &str) -> Result<GitDiffResult, String> {
        self.machine(machine)?
            .request(
                method::GIT_DIFF,
                GitRepoParams {
                    repo: repo.to_string(),
                },
            )
            .await
    }

    pub async fn git_restore(
        &self,
        machine: &str,
        repo: &str,
        path: Option<String>,
        patch: Option<String>,
    ) -> Result<OpResult, String> {
        self.machine(machine)?
            .request(
                method::GIT_RESTORE,
                GitRestoreParams {
                    repo: repo.to_string(),
                    path,
                    patch,
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
    pub async fn acp(&self, machine: &str, agent: &str) -> Result<Arc<AgentConnection>, String> {
        let machine = self.machine(machine)?;
        if let Some(conn) = machine.connection(agent) {
            return Ok(conn);
        }
        self.connect_agent(&machine, agent).await
    }

    // ---------- Daemon 连接 ----------

    /// Daemon 握手与接入：校验 token、机器名与机器重名后升级为长连接
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
        let Some(machine_name) = headers
            .get(header::MACHINE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
        else {
            log::warn!("Daemon 握手被拒：缺少机器名");
            return StatusCode::BAD_REQUEST.into_response();
        };
        if self.machines.lock().contains_key(&machine_name) {
            log::warn!("Daemon 握手被拒：机器重名 {machine_name}");
            return StatusCode::CONFLICT.into_response();
        }

        let hub = self.clone();
        ws.on_upgrade(move |socket| async move {
            if let Err(error) = hub.serve(machine_name, socket).await {
                log::warn!("Daemon 连接结束: {error}");
            }
        })
    }

    async fn serve(self, machine_name: String, socket: WebSocket) -> Result<(), String> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<String>(256);
        let machine = Arc::new(Machine {
            name: machine_name.clone(),
            info: Mutex::new(None),
            outgoing: outgoing_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            agents: Mutex::new(HashMap::new()),
        });
        self.machines
            .lock()
            .insert(machine_name.clone(), Arc::clone(&machine));
        log::info!("Daemon 已接入: {machine_name}");

        let (mut sink, mut stream) = socket.split();
        let writer = tokio::spawn(async move {
            while let Some(frame) = outgoing_rx.recv().await {
                log::debug!("下行帧: {}", amux_common::text::truncate(&frame, 200));
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
                // Agent 生命周期：发现 → 启动/重启 → 建立 ACP 连接与 initialize
                if let Err(error) = hub.start_agents(&machine).await {
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

        writer.abort();
        self.machines.lock().remove(&machine_name);
        let pending: Vec<_> = machine.pending.lock().drain().map(|(_, tx)| tx).collect();
        for tx in pending {
            let _ = tx.send(Err("Daemon 连接已断开".to_string()));
        }
        log::info!("Daemon 连接断开: {machine_name}");
        Ok(())
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

    /// Agent 生命周期：未启动 → 启动；已启动但 Server 无活跃 ACP 连接 → 重启；
    /// 之后为需要连接的 agent 建立 ACP 连接并 initialize。
    async fn start_agents(&self, machine: &Arc<Machine>) -> Result<(), String> {
        let list: AgentListResult = machine.request(method::AGENT_LIST, ()).await?;
        for agent in list.agents {
            if agent.running && machine.has_connection(&agent.name) {
                continue;
            }
            if let Err(error) = machine
                .request::<_, OpResult>(
                    method::AGENT_RESTART,
                    AgentParams {
                        agent: agent.name.clone(),
                    },
                )
                .await
            {
                log::warn!(
                    "启动 agent 失败（{}@{}）: {error}",
                    agent.name,
                    machine.name
                );
                continue;
            }
            machine.drop_agent(&agent.name);
            if let Err(error) = self.connect_agent(machine, &agent.name).await {
                log::warn!(
                    "建立 ACP 连接失败（{}@{}）: {error}",
                    agent.name,
                    machine.name
                );
                continue;
            }
            self.notify_agent_restarted(machine, &agent.name);
        }
        Ok(())
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
            .cloned()
            .ok_or_else(|| "机器未连接".to_string())
    }
}

impl Machine {
    fn connection(&self, agent: &str) -> Option<Arc<AgentConnection>> {
        self.agents
            .lock()
            .get(agent)
            .and_then(|slot| slot.conn.clone())
    }

    fn has_connection(&self, agent: &str) -> bool {
        self.connection(agent).is_some()
    }

    fn drop_agent(&self, agent: &str) {
        self.agents.lock().remove(agent);
    }

    /// 该 agent 的出站通道：原始 ACP 消息 → `acp` 通知。
    fn agent_outgoing(&self, agent: &str) -> mpsc::Sender<String> {
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let outgoing = self.outgoing.clone();
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
                if outgoing.send(frame).await.is_err() {
                    break;
                }
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
        if self.outgoing.send(frame).await.is_err() {
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
