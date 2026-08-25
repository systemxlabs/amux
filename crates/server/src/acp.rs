//! ACP v1 驱动（docs/DESIGN.md §9）：官方 SDK `agent-client-protocol` 的 Client 角色，
//! 经 stdio 与 ACP server 子进程通信。
//!
//! `AcpAgentDriver` 使用**专用 exec 线程**承载全部异步 IO（SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨
//! runtime 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    BlobResourceContents, CancelNotification, CloseSessionRequest, ContentBlock as AcpContentBlock,
    DeleteSessionRequest, EmbeddedResource, EmbeddedResourceResource, InitializeRequest,
    NewSessionRequest, PermissionOption, PermissionOptionId, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    StopReason, TextContent, TextResourceContents, ToolKind,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, ConnectionTo, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use protocol::ContentBlock;

/// 拉起的统计（server 启动日志用；docs/DESIGN.md §4.1/§7.3）。
#[derive(Debug, Default, Clone, Copy)]
pub struct LaunchSummary {
    /// 成功拉起的 ACP server 数
    pub started: usize,
    /// 拉起失败的 agent 数（标记为**不可用**，agent.list 的 available=false）
    pub failed: usize,
}

/// turn 过程中的 agent 事件（docs/DESIGN.md §5.1：server 透传，GUI 应用聚合）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 输出的增量片段
    OutputChunk(String),
    /// 思考片段
    Thinking(String),
    /// 工具调用
    ToolCall {
        name: String,
        title: Option<String>,
        content: Option<String>,
    },
    /// ACP 请求或传输失败
    Error(String),
    /// turn 完成（携带结束原因；docs/DESIGN.md §工作流会话驱动「变更原因」）
    TurnEnded(protocol::StateChangeReason),
}

/// 与单个 agent 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id
    fn create_session(&self, cwd: &str) -> Result<String, String>;
    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史；
    /// 同一进程内对同一会话幂等——已恢复过则直接成功）
    fn resume_session(&self, agent_session_id: &str, cwd: &str) -> Result<(), String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 关闭会话（删除/长时间无活动时释放 agent 侧资源，docs/DESIGN.md「ACP 生命周期」：
    /// server 经 ACP `session/close` 关闭 agent 侧会话）
    fn close(&self, agent_session_id: &str) -> Result<(), String>;
    /// 删除会话（docs/DESIGN.md「删除会话」：close 之后若 ACP Server 支持会话删除，
    /// 则发送 `session/delete` 删除 agent 侧会话；不支持删除的 agent 返回错误，
    /// 调用方按「不支持」忽略）
    fn delete_session(&self, agent_session_id: &str) -> Result<(), String>;
    /// 该 agent 安装的 skills 列表；查询失败必须显式返回错误。
    fn list_skills(&self) -> Result<Vec<String>, String>;
    /// 关闭驱动自身（server 退出时释放 ACP 子进程资源，docs/DESIGN.md「ACP Server 生命周期」）
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
    ListSkills,
}

/// 主线程 → exec 线程的方法请求。
enum ExecReq {
    Call {
        call: AcpCall,
        resp: std::sync::mpsc::SyncSender<Result<Value, String>>,
    },
    Prompt {
        sid: String,
        prompt: Vec<ContentBlock>,
        routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    },
}

/// ACP v1 客户端（官方 SDK stdio 传输，docs/DESIGN.md §9）。
pub struct AcpAgentDriver {
    /// 主线程 → exec 线程的请求发送端；shutdown 时置 None 以优雅结束 exec 线程
    exec_tx: Mutex<Option<std::sync::mpsc::SyncSender<ExecReq>>>,
    /// 会话事件路由：agent sessionId -> prompt 的事件接收端
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    /// 本进程内已 resume 过的会话（server 重启后从注册表恢复的会话首次交互前
    /// 经 ACP `session/resume` 恢复 agent 上下文，docs/DESIGN.md §7.2）
    resumed: Arc<Mutex<HashSet<String>>>,
    /// exec 线程句柄（Mutex 包装以便 `shutdown_and_join` 从 &self 取出并 join；
    /// 连接由 SDK 管理，线程结束即子进程清理）
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
        let (exec_tx, exec_rx) = std::sync::mpsc::sync_channel::<ExecReq>(32);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let routes2 = routes.clone();
        let bin = bin.to_string();
        let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let env = env.to_vec();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("构建 tokio runtime 失败");
            rt.block_on(exec_main(&bin, &args, &env, exec_rx, routes2, ready_tx));
        });
        let timeout_ms = std::env::var("AMUX_ACP_SPAWN_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000);
        match ready_rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(format!(
                    "ACP server 启动超时（{timeout_ms}ms 内未完成连接/initialize 握手）"
                ))
            }
        }
        Ok(AcpAgentDriver {
            exec_tx: Mutex::new(Some(exec_tx)),
            routes,
            resumed: Arc::new(Mutex::new(HashSet::new())),
            thread: Mutex::new(Some(thread)),
        })
    }

    fn sender(&self) -> Result<std::sync::mpsc::SyncSender<ExecReq>, String> {
        self.exec_tx
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone()
            .ok_or_else(|| "agent 已关闭".to_string())
    }

    /// 同步方法调用：请求发往 exec 线程，阻塞等待响应。
    fn call(&self, call: AcpCall) -> Result<Value, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Value, String>>(1);
        self.sender()?
            .send(ExecReq::Call { call, resp: tx })
            .map_err(|_| "agent 已关闭".to_string())?;
        rx.recv().map_err(|_| "ACP 调用执行失败".to_string())?
    }
}

impl AgentDriver for AcpAgentDriver {
    fn create_session(&self, cwd: &str) -> Result<String, String> {
        let res = self.call(AcpCall::NewSession {
            cwd: cwd.to_string(),
        })?;
        let sid = res
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "session/new 未返回 sessionId".to_string())?
            .to_string();
        // 新会话 agent 已在内存中持有，无需 resume
        self.resumed.lock().unwrap().insert(sid.clone());
        Ok(sid)
    }

    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史——
    /// 历史以 server 本地日志为权威，docs/DESIGN.md §7.2/§5.2）。
    /// 同一进程内对同一会话幂等（已恢复过则直接成功）。
    fn resume_session(&self, agent_session_id: &str, cwd: &str) -> Result<(), String> {
        {
            let resumed = self.resumed.lock().unwrap();
            if resumed.contains(agent_session_id) {
                return Ok(());
            }
        }
        let result = self.call(AcpCall::Resume {
            sid: agent_session_id.to_string(),
            cwd: cwd.to_string(),
        });
        if result.is_ok() {
            self.resumed
                .lock()
                .unwrap()
                .insert(agent_session_id.to_string());
        }
        result.map(|_| ())
    }

    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);
        self.routes
            .lock()
            .unwrap()
            .insert(agent_session_id.to_string(), tx.clone());
        let req = ExecReq::Prompt {
            sid: agent_session_id.to_string(),
            prompt: input,
            routes: self.routes.clone(),
        };
        let send_result = self
            .sender()
            .and_then(|sender| sender.send(req).map_err(|_| "agent 已关闭".to_string()));
        if let Err(error) = send_result {
            self.routes.lock().unwrap().remove(agent_session_id);
            let _ = tx.try_send(AgentEvent::Error(error));
            let _ = tx.try_send(AgentEvent::TurnEnded(protocol::StateChangeReason::Aborted));
        }
        rx
    }

    fn shutdown(&self) {
        let _ = self.exec_tx.lock().unwrap().take();
    }

    /// 关闭并等待 exec 线程退出：通道关闭 → 服务循环结束 → SDK 连接 drop（子进程
    /// 随之回收）。server 退出路径调用，保证清理先于进程退出完成。
    fn shutdown_and_join(&self) {
        self.shutdown();
        if let Some(handle) = self.thread.lock().unwrap().take() {
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
        self.resumed
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(agent_session_id);
        self.call(AcpCall::Close {
            sid: agent_session_id.to_string(),
        })
        .map(|_| ())
    }

    /// 删除 agent 侧会话（docs/DESIGN.md「删除会话」：close 之后，agent 支持
    /// 删除才调用；不支持删除的 agent 返回 METHOD_NOT_FOUND 类错误，调用方忽略）。
    fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
        self.call(AcpCall::Delete {
            sid: agent_session_id.to_string(),
        })
        .map(|_| ())
    }

    /// 经 ACP `skill/list` 查询该 agent 安装的 skills。
    fn list_skills(&self) -> Result<Vec<String>, String> {
        let res = self.call(AcpCall::ListSkills)?;
        res.get("skills")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("name").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| "skill/list 响应缺少 skills 数组".to_string())
    }
}

/// ACP `skill/list` 响应（SDK schema v1 未收录；typed 化，避免 Value 松散承载）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct SkillListResponse {
    skills: Vec<SkillInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillInfo {
    name: String,
}

/// 自定义请求：ACP `skill/list`（PRD §3.3；SDK schema v1 未收录该方法）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "skill/list", response = SkillListResponse)]
struct SkillListRequest {}

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

/// exec 线程主循环：经官方 SDK 建立 ACP 连接，承载方法分发、通知路由与权限批准。
/// `ready_tx`：就绪握手——连接建立（子进程拉起）且 initialize 握手完成后发送结果；
/// 若连接在握手前就失败（二进制缺失 / 进程立即退出），在此补发 `Err` 供
/// `AcpAgentDriver::spawn` 同步快速失败，而非等满超时。
async fn exec_main(
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    exec_rx: std::sync::mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
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

    // SDK from_args 支持 `NAME=value` 前缀参数作为环境变量（docs/DESIGN.md §7.3）
    let mut cmd: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    cmd.push(bin.to_string());
    cmd.extend(args.iter().cloned());
    let agent = match AcpAgent::from_args(cmd) {
        Ok(a) => a,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("解析 agent 命令失败 ({bin}): {e}")));
            protocol::log::error("acp", format!("解析 agent 命令失败 ({bin}): {e}"));
            return;
        }
    };
    protocol::log::info("acp", format!("已连接 ACP agent: {bin} {}", args.join(" ")));
    // trace 级：ACP 线上原始帧（GUI ↔ server ↔ ACP client ↔ agent 全链路，docs/DESIGN.md §8）
    let agent = if protocol::log::enabled_for(protocol::Level::Trace, "acp.wire") {
        agent.with_debug(|line, direction| {
            protocol::log::trace("acp.wire", format!("{direction:?} {line}"));
        })
    } else {
        agent
    };

    let result = connect_main(agent, &mut req_rx, routes, &ready_tx, ready_sent.clone()).await;

    // 连接异常结束：若就绪信号尚未发出（连接建立前传输层失败：二进制缺失 /
    // 进程立即退出 / npx 不可用 / 无网络），补报为 spawn 失败；若已报过就绪，
    // 之后的连接异常仅记录，不影响已缓存的驱动。
    if let core::result::Result::Err(e) = &result {
        protocol::log::error("acp", format!("ACP 连接异常结束: {e}"));
        if !ready_sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = ready_tx.send(Err(format!("ACP 连接失败: {e}")));
        }
    }
}

async fn connect_main(
    agent: AcpAgent,
    req_rx: &mut mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
    ready_sent: Arc<std::sync::atomic::AtomicBool>,
) -> agent_client_protocol::Result<()> {
    agent_client_protocol::Client
        .builder()
        .name("amux-server")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                route_update(&routes, &notif).await;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                // yolo：自动批准（docs/DESIGN.md §7.2，无审批往返）。
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
        .connect_with(
            agent,
            |cx: ConnectionTo<agent_client_protocol::Agent>| async move {
                let req_rx = req_rx;
                // 初始化握手（版本协商）。失败需区分两种情形：
                // - **协议级失败**（agent 存活但不实现 initialize，如返回 method not
                //   found）：仅记录、连接保持可用，视为拉起成功；
                // - **传输层失败**（进程已退出 / 连接已死，如 npx 不可用、无网络）：拉起失败。
                // 二者用短窗口探测连接活性区分：incoming_closed 在传输层关闭后很快完成，
                // 超时则连接仍存活。
                let init_result = match cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await
                {
                    core::result::Result::Ok(_) => {
                        protocol::log::debug("acp", "initialize 完成");
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
                            protocol::log::error("acp", format!("initialize 失败（继续）: {e}"));
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
                    init_result
                });

                // 服务循环：每个请求独立 spawn，支持并发（cancel 不必等 prompt 完成）
                loop {
                    let Some(req) = req_rx.recv().await else {
                        break;
                    };
                    match req {
                        ExecReq::Call { call, resp } => {
                            let cx = cx.clone();
                            tokio::spawn(async move {
                                let result = dispatch_call(&cx, &call).await;
                                let _ = resp.send(result);
                            });
                        }
                        ExecReq::Prompt {
                            sid,
                            prompt,
                            routes,
                        } => {
                            let cx = cx.clone();
                            tokio::spawn(async move {
                                let blocks = prompt
                                    .iter()
                                    .filter_map(acp_content_block)
                                    .collect::<Vec<_>>();
                                let callback_sid = sid.clone();
                                let callback_routes = routes.clone();
                                let result = cx
                                    .send_request(PromptRequest::new(sid.clone(), blocks))
                                    .on_receiving_result(async move |result| {
                                        // turn 完成：移除路由并发送 TurnEnded（在最后一批通知之后）。
                                        // 结束原因取自 ACP prompt 响应的 stopReason（权威归因）
                                        let route = callback_routes
                                            .lock()
                                            .expect("Mutex 中毒（临界区内不应 panic）")
                                            .remove(&callback_sid);
                                        if let Some(tx) = route {
                                            let reason = match &result {
                                                Ok(resp) => stop_reason_reason(resp.stop_reason),
                                                Err(_) => protocol::StateChangeReason::Aborted,
                                            };
                                            if let core::result::Result::Err(e) = &result {
                                                let _ = tx
                                                    .send(AgentEvent::Error(format!(
                                                        "ACP prompt 失败: {e}"
                                                    )))
                                                    .await;
                                            }
                                            let _ = tx.send(AgentEvent::TurnEnded(reason)).await;
                                        }
                                        core::result::Result::Ok(())
                                    });
                                if let Err(e) = result {
                                    protocol::log::error(
                                        "acp",
                                        format!("prompt 调用失败 {sid}: {e}"),
                                    );
                                    let route = routes
                                        .lock()
                                        .expect("Mutex 中毒（临界区内不应 panic）")
                                        .remove(&sid);
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
                core::result::Result::Ok(())
            },
        )
        .await
}

/// 分发 ACP v1 方法调用（强类型 AcpCall，经官方 SDK 传输）。
async fn dispatch_call(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
    call: &AcpCall,
) -> Result<Value, String> {
    let label = match call {
        AcpCall::NewSession { .. } => "session/new",
        AcpCall::Resume { .. } => "session/resume",
        AcpCall::Cancel { .. } => "session/cancel",
        AcpCall::Close { .. } => "session/close",
        AcpCall::Delete { .. } => "session/delete",
        AcpCall::ListSkills => "skill/list",
    };
    protocol::log::debug("acp", format!("调用 {label}"));
    let result = dispatch_call_inner(cx, call).await;
    match &result {
        Ok(_) => protocol::log::debug("acp", format!("{label} 成功")),
        Err(e) => protocol::log::error("acp", format!("{label} 失败: {e}")),
    }
    result
}

async fn dispatch_call_inner(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
    call: &AcpCall,
) -> Result<Value, String> {
    match call {
        AcpCall::NewSession { cwd } => {
            let resp = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/new 失败: {e}"))?;
            Ok(json!({ "sessionId": resp.session_id }))
        }
        AcpCall::Resume { sid, cwd } => {
            cx.send_request(ResumeSessionRequest::new(sid.clone(), cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/resume 失败: {e}"))?;
            Ok(Value::Null)
        }
        AcpCall::Cancel { sid } => {
            cx.send_notification(CancelNotification::new(sid.clone()))
                .map_err(|e| format!("session/cancel 失败: {e}"))?;
            Ok(Value::Null)
        }
        AcpCall::Close { sid } => {
            cx.send_request(CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await
                .map_err(|e| format!("session/close 失败: {e}"))?;
            Ok(Value::Null)
        }
        AcpCall::Delete { sid } => {
            // 仅 agent 声明 sessionCapabilities.delete 时可用；不支持时返回错误，
            // 调用方（会话删除路径）按「不支持删除」忽略。
            cx.send_request(DeleteSessionRequest::new(sid.clone()))
                .block_task()
                .await
                .map_err(|e| format!("session/delete 失败（agent 可能不支持删除）: {e}"))?;
            Ok(Value::Null)
        }
        AcpCall::ListSkills => {
            let resp = cx
                .send_request(SkillListRequest {})
                .block_task()
                .await
                .map_err(|e| format!("skill/list 失败: {e}"))?;
            serde_json::to_value(resp).map_err(|e| format!("skill/list 序列化失败: {e}"))
        }
    }
}

/// ACP stopReason → 状态变更原因（docs/DESIGN.md §工作流会话驱动「变更原因」）。
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

/// 把 ACP `session/update` 通知映射为 AgentEvent 并路由（docs/DESIGN.md §5 聚合）。
async fn route_update(
    routes: &Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
    notif: &SessionNotification,
) {
    let ev = match &notif.update {
        // 用户消息回显不进活动流（server 直接落盘用户输入，docs/DESIGN.md §5.2）
        SessionUpdate::UserMessageChunk(_) => None,
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(&chunk.content).map(AgentEvent::OutputChunk)
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            text_of(&chunk.content).map(AgentEvent::Thinking)
        }
        SessionUpdate::ToolCall(tc) => Some(AgentEvent::ToolCall {
            name: tool_kind_str(&tc.kind),
            title: Some(tc.title.clone()),
            content: tc.raw_input.as_ref().map(|v| v.to_string()),
        }),
        // ACP tool_call_update 的 kind 字段可选，常缺失；缺失时用标题作为展示名。
        // kind 与 title 都没有的更新没有可展示标识，直接跳过（避免「工具调用 工具」空条目）。
        SessionUpdate::ToolCallUpdate(tcu) => tcu
            .fields
            .kind
            .as_ref()
            .map(tool_kind_str)
            .or_else(|| tcu.fields.title.clone())
            .map(|name| AgentEvent::ToolCall {
                name,
                title: tcu.fields.title.clone(),
                content: tcu.fields.raw_input.as_ref().map(|v| v.to_string()),
            }),
        // SessionInfoUpdate（ACP v1 未携带状态字段）/ UsageUpdate /
        // AvailableCommandsUpdate / CurrentModeUpdate / ConfigOptionUpdate /
        // Plan 等不产生 AgentEvent
        _ => None,
    };
    if let Some(ev) = ev {
        let tx = routes
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .get(notif.session_id.to_string().as_str())
            .cloned();
        if let Some(tx) = tx {
            if tx.send(ev).await.is_err() {
                protocol::log::warn("acp", "agent 事件接收端已关闭");
            }
        }
    }
}

/// ContentBlock → 文本（仅 text 类型；其他类型记 debug 日志后忽略——
/// PRD 对话历史为 IM 式文本流，非文本块暂无落盘表示）。
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
            protocol::log::debug("acp", format!("忽略非文本内容块（{kind}）"));
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
        ContentBlock as AcpContentBlock, ContentChunk, SessionId, TextContent, ToolCall,
        ToolCallStatus, ToolKind,
    };
    use tokio::sync::mpsc;

    fn route_with_channel() -> (
        Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
        mpsc::Receiver<AgentEvent>,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let routes = Mutex::new(HashMap::from([("s1".to_string(), tx)]));
        (routes, rx)
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
        route_update(&routes, &notif).await;
        // 用户消息回显不进活动流（server 直接落盘用户输入），无事件产生
        assert!(rx.try_recv().is_err());
    }

    /// agent_message_chunk → OutputChunk。
    #[tokio::test]
    async fn route_update_agent_message_chunk() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("输出"),
            ))),
        );
        route_update(&routes, &notif).await;
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
        route_update(&routes, &notif).await;
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
        route_update(&routes, &notif).await;
        let ev = rx.try_recv().expect("应收到 tool_call 事件");
        match ev {
            AgentEvent::ToolCall {
                name,
                title,
                content,
            } => {
                assert_eq!(name, "execute");
                assert_eq!(title.as_deref(), Some("运行 cargo test"));
                assert!(content.unwrap_or_default().contains("cargo test"));
            }
            other => panic!("应为 ToolCall，得到 {other:?}"),
        }
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
        route_update(&routes, &notif).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pick_approve_option_prefers_allow() {
        fn opt(id: &str, kind: PermissionOptionKind) -> PermissionOption {
            PermissionOption::new(id.to_string(), id.to_string(), kind)
        }

        // claude-acp 实际选项顺序：Deny 在前
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

        // 无 AllowAlways：选 AllowOnce
        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("allow", PermissionOptionKind::AllowOnce),
        ];
        assert_eq!(pick_approve_option(&opts).unwrap().to_string(), "allow");

        // 仅一个 AllowOnce（mock 场景）
        let opts = vec![opt("allow-once", PermissionOptionKind::AllowOnce)];
        assert_eq!(
            pick_approve_option(&opts).unwrap().to_string(),
            "allow-once"
        );

        // 全为拒绝 → None（按取消处理）
        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("reject_all", PermissionOptionKind::RejectAlways),
        ];
        assert!(pick_approve_option(&opts).is_none());

        // 空列表 → None
        assert!(pick_approve_option(&[]).is_none());
    }
}
