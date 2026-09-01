//! GUI 的 WS 客户端：连接 amux server，处理 JSON-RPC 请求、响应和通知。
//!
//! 认证：应用连接后**先发 `auth`**（method=AUTH，params={token}），
//! 认证成功后才处理其它请求；认证完成前到达的请求一律返回认证失败（AUTH_FAILED）。
//! 数据为**拉取式**（request/response 按 id 匹配）；
//! 连接失败 / 认证失败 / 断开后**不自动重连**，保持离线或认证失败状态，
//! 由 UI 手动触发重连。仅保留本地 connected/disconnected 通知供 UI 标记在线/离线状态。

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use protocol::OpResult;

/// GPUI 环境无 Tokio runtime，这里维护一个独立的多线程 runtime 跑 WS 后台任务。
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// 全部活跃连接的关闭信号，应用退出时统一触发，确保连接确定性关闭。
static CLOSE_SIGNALS: OnceLock<Mutex<Vec<tokio::sync::watch::Sender<bool>>>> = OnceLock::new();

fn close_signals() -> &'static Mutex<Vec<tokio::sync::watch::Sender<bool>>> {
    CLOSE_SIGNALS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 关闭全部 WS 连接（幂等；on_app_quit 钩子调用）。
pub fn close_all() {
    let mut signals = close_signals().lock();
    for tx in signals.drain(..) {
        let _ = tx.send(true);
    }
}

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("初始化 tokio runtime 失败")
    })
}

/// GUI 全局 tokio runtime 句柄（WS 后台任务；也供需要 reactor 的编排引擎使用）。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    rt()
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC 错误 {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

struct ClientReq {
    method: String,
    params: Option<Value>,
    resp: oneshot::Sender<Result<Value, RpcError>>,
}

/// 通知（method + params）。
#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone)]
pub struct WsClient {
    req_tx: mpsc::Sender<ClientReq>,
    notify_tx: broadcast::Sender<Notification>,
}

impl WsClient {
    /// 连接 server（后台 task 持有连接并处理收发）。
    /// 认证 token 在首个 JSON-RPC `auth` 请求中发送，不放入 URL。
    pub fn connect_with_token(url: String, token: String) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<ClientReq>(64);
        let (notify_tx, _) = broadcast::channel::<Notification>(256);
        let (close_tx, close_rx) = tokio::sync::watch::channel(false);
        close_signals().lock().push(close_tx);
        let notify_for_task = notify_tx.clone();
        rt().spawn(run_loop(url, token, req_rx, close_rx, notify_for_task));
        WsClient { req_tx, notify_tx }
    }

    /// 发送 JSON-RPC 请求并按强类型反序列化响应结果。
    /// 响应键名以协议类型为准（camelCase），禁止手工取键——分页游标曾因
    /// GUI 读 snake_case 而整体失效，此处泛型化即为杜绝该类错配。
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<P>,
    ) -> Result<R, RpcError> {
        let params_value = params
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| RpcError {
                code: -1,
                message: format!("参数序列化失败: {e}"),
            })?;
        let (tx, rx) = oneshot::channel();
        self.req_tx
            .send(ClientReq {
                method: method.to_string(),
                params: params_value,
                resp: tx,
            })
            .await
            .map_err(|_| RpcError {
                code: -1,
                message: "连接已关闭".into(),
            })?;
        let value = rx.await.map_err(|_| RpcError {
            code: -1,
            message: "连接中断".into(),
        })??;
        serde_json::from_value(value).map_err(|e| RpcError {
            code: -1,
            message: format!("响应反序列化失败: {e}"),
        })
    }

    /// 无业务载荷的方法（cancel/delete/configure/restart/restore/prompt 等）：
    /// 响应恒为 `OpResult`，只关心成败。
    pub async fn request_ok<P: Serialize>(
        &self,
        method: &str,
        params: Option<P>,
    ) -> Result<(), RpcError> {
        let op: OpResult = self.request(method, params).await?;
        if !op.ok {
            return Err(RpcError {
                code: -1,
                message: op.message.unwrap_or_else(|| "操作失败".into()),
            });
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notify_tx.subscribe()
    }
}

/// 单次连接尝试。失败（连不上 / 认证失败 / 已连接后断开）即终止，
/// 不做自动重连：机器保持离线或认证失败状态，由用户手动触发重连。
async fn run_loop(
    url: String,
    token: String,
    mut req_rx: mpsc::Receiver<ClientReq>,
    close_rx: tokio::sync::watch::Receiver<bool>,
    notify_tx: broadcast::Sender<Notification>,
) {
    if *close_rx.borrow() {
        return;
    }
    match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => {
            log::info!("已连接 {url}");
            let _ = notify_tx.send(Notification {
                method: "connected".into(),
                params: Value::Null,
            });
            serve_connection(ws, token, &mut req_rx, &close_rx.clone(), &notify_tx).await;
        }
        Err(e) => {
            log::debug!("连接失败: {e}");
            // UI 需区分「连不上」与「正在连」：连接失败也广播（修复机器永远显示连接中）
            let _ = notify_tx.send(Notification {
                method: "connect_failed".into(),
                params: json!({ "error": e.to_string() }),
            });
        }
    }
}

/// 服务单条已建立的 WS 连接：auth 握手 → 请求/通知收发循环，直至断开、
/// 认证失败或收到显式关闭信号。信封统一用 protocol::jsonrpc 强类型，
/// 不手工拼帧/取键（响应键名以协议类型为准）。
async fn serve_connection(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    token: String,
    req_rx: &mut mpsc::Receiver<ClientReq>,
    close_rx: &tokio::sync::watch::Receiver<bool>,
    notify_tx: &broadcast::Sender<Notification>,
) {
    let mut close_rx = close_rx.clone();
    let (mut sink, mut source) = ws.split();
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, RpcError>>> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut authed = false;
    // 心跳：周期 ping，超过 HEARTBEAT_TIMEOUT 未见任何入站帧即判定半开断连。
    // TCP 半开（拔网线/server 假死）下 select 永远不会唤醒，请求将永久挂起、
    // 机器状态也永远显示在线；超时走既有断连路径（离线标记 + 在途请求报错）。
    const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = std::time::Instant::now();
    let auth_id = next_id;
    next_id += 1;
    let auth_frame = protocol::JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: auth_id.into(),
        method: protocol::method::AUTH.into(),
        params: Some(json!({ "token": token })),
    };
    let auth_frame = serde_json::to_string(&auth_frame).expect("auth 帧序列化失败");
    if sink.send(Message::Text(auth_frame.into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            _ = close_rx.changed() => {
                if *close_rx.borrow() {
                    log::info!("收到关闭信号");
                    return;
                }
            }
            req = req_rx.recv() => {
                let Some(req) = req else {
                    return;
                };
                if !authed {
                    let _ = req.resp.send(Err(RpcError {
                        code: protocol::server_error::AUTH_FAILED,
                        message: "未认证：请先完成连接认证".into(),
                    }));
                    continue;
                }
                let id = next_id;
                next_id += 1;
                let frame = protocol::JsonRpcRequest {
                    jsonrpc: "2.0".into(),
                    id: id.into(),
                    method: req.method,
                    params: req.params,
                };
                let Ok(frame) = serde_json::to_string(&frame) else { continue };
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
                pending.insert(id, req.resp);
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > HEARTBEAT_TIMEOUT {
                    log::warn!("连接无响应（心跳超时），判定断开");
                    break;
                }
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            msg = source.next() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { break };
                last_seen = std::time::Instant::now();
                let Message::Text(t) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                // 带 Number id 的帧是响应；其余（缺 id）按 server → GUI 通知处理
                let Ok(resp) = serde_json::from_value::<protocol::JsonRpcResponse>(v.clone()) else {
                    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                        // server → GUI 通知（唯一主动推送 session.state_change）
                        log::debug!("通知 {method}");
                        let _ = notify_tx.send(Notification {
                            method: method.to_string(),
                            params: v.get("params").cloned().unwrap_or(Value::Null),
                        });
                    }
                    continue;
                };
                let protocol::JsonRpcId::Number(id) = resp.id else { continue };
                if id == auth_id {
                    // auth 响应：无论成败，认证阶段结束；失败则按退避重连
                    if let Some(err) = resp.error {
                        log::warn!("认证失败 [{}]: {}", err.code, err.message);
                        let _ = notify_tx.send(Notification {
                            method: "auth_failed".into(),
                            params: json!({ "code": err.code, "message": err.message }),
                        });
                        return;
                    }
                    log::info!("认证成功");
                    authed = true;
                    let _ = notify_tx.send(Notification {
                        method: "auth_ok".into(),
                        params: Value::Null,
                    });
                    continue;
                }
                if let Some(resp_tx) = pending.remove(&id) {
                    if let Some(err) = resp.error {
                        log::warn!("请求 #{id} 失败 [{}]: {}", err.code, err.message);
                        let _ = resp_tx.send(Err(RpcError { code: err.code, message: err.message }));
                    } else {
                        let _ = resp_tx.send(Ok(resp.result.unwrap_or(Value::Null)));
                    }
                }
            }
        }
    }
    // 连接断开：通知 UI（真实离线状态），清空在途请求。
    // 不做自动重连（见 run_loop 文档）：机器转 Offline，等用户手动重连
    log::warn!("连接断开（server 重启或网络中断），等待手动重连");
    let _ = notify_tx.send(Notification {
        method: "disconnected".into(),
        params: Value::Null,
    });
    for (_, resp) in pending.drain() {
        let _ = resp.send(Err(RpcError {
            code: -1,
            message: "连接断开".into(),
        }));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn auth_failed_error_code_matches_protocol() {
        assert_eq!(
            protocol::server_error::AUTH_FAILED,
            -32000,
            "未认证请求应返回协议定义的认证失败码"
        );
    }

    #[test]
    fn method_and_notify_names_match_protocol() {
        // 防止方法名/通知名漂移（协议单一来源）
        assert_eq!(
            protocol::method::SESSION_LIST,
            "session.list",
            "guid 使用的会话列表方法名必须与协议一致"
        );
        assert_eq!(
            protocol::notify::SESSION_STATE_CHANGE,
            "session.state_change"
        );
    }
}
