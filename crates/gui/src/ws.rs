//! GUI 的 WS 客户端（docs/DESIGN.md §4）：连 amux server，JSON-RPC 请求/响应/通知。
//! 后台 task 持有连接（断线指数退避重连），请求经通道发送、响应按 id 匹配，
//! 通知经 broadcast 供 UI 订阅。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// GPUI 环境无 Tokio runtime，这里维护一个独立的多线程 runtime 跑 WS 后台任务。
static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("初始化 tokio runtime 失败")
    })
}

/// GUI 全局 tokio runtime 句柄（WS 后台任务；也供需要 reactor 的
/// 编排引擎等使用——rig/reqwest 的 LLM 调用必须在 tokio 上下文执行）。
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

#[derive(Clone)]
pub struct WsClient {
    req_tx: mpsc::Sender<ClientReq>,
    notify_tx: broadcast::Sender<Notification>,
}

impl WsClient {
    /// 连接 server（后台 task 持有连接并处理收发）。url 形如 `ws://host:port?token=xxx`。
    pub fn connect(url: String) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<ClientReq>(64);
        let (notify_tx, _) = broadcast::channel::<Notification>(256);
        let notify_for_task = notify_tx.clone();
        rt().spawn(run_loop(url, req_rx, notify_for_task));
        WsClient { req_tx, notify_tx }
    }

    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.req_tx
            .send(ClientReq {
                method: method.to_string(),
                params,
                resp: tx,
            })
            .await
            .map_err(|_| RpcError {
                code: -1,
                message: "连接已关闭".into(),
            })?;
        rx.await.map_err(|_| RpcError {
            code: -1,
            message: "连接中断".into(),
        })?
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notify_tx.subscribe()
    }
}

async fn run_loop(
    url: String,
    mut req_rx: mpsc::Receiver<ClientReq>,
    notify_tx: broadcast::Sender<Notification>,
) {
    let mut attempt: u32 = 0;
    loop {
        // 连接（指数退避）
        let mut connected = false;
        while !connected {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((ws, _)) => {
                    attempt = 0;
                    connected = true;
                    protocol::log::info("gui.ws", format!("已连接 {url}"));
                    let _ = notify_tx.send(Notification {
                        method: "connected".into(),
                        params: Value::Null,
                    });
                    let (mut sink, mut source) = ws.split();
                    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, RpcError>>> =
                        HashMap::new();
                    let mut next_id: u64 = 1;

                    loop {
                        tokio::select! {
                            req = req_rx.recv() => {
                                let Some(req) = req else { return };  // client 销毁
                                let id = next_id;
                                next_id += 1;
                                let frame = json!({
                                    "jsonrpc": "2.0", "id": id, "method": req.method,
                                    "params": req.params
                                });
                                protocol::log::debug(
                                    "gui.ws",
                                    format!("请求 #{id} {}", frame.get("method").and_then(|m| m.as_str()).unwrap_or("?")),
                                );
                                if sink.send(Message::Text(frame.to_string())).await.is_err() {
                                    let _ = req.resp.send(Err(RpcError { code: -1, message: "发送失败".into() }));
                                    break;
                                }
                                pending.insert(id, req.resp);
                            }
                            msg = source.next() => {
                                let Some(msg) = msg else { break };
                                let msg = match msg {
                                    Ok(m) => m,
                                    Err(_) => break,
                                };
                                let Message::Text(t) = msg else { continue };
                                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                                    if let Some(resp) = pending.remove(&id) {
                                        if let Some(err) = v.get("error") {
                                            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
                                            let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                                            protocol::log::warn("gui.ws", format!("请求 #{id} 失败 [{code}]: {message}"));
                                            let _ = resp.send(Err(RpcError { code, message }));
                                        } else {
                                            protocol::log::debug("gui.ws", format!("请求 #{id} 成功"));
                                            let _ = resp.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                                        }
                                    }
                                } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                                    protocol::log::debug("gui.ws", format!("通知 {method}"));
                                    let _ = notify_tx.send(Notification {
                                        method: method.to_string(),
                                        params: v.get("params").cloned().unwrap_or(Value::Null),
                                    });
                                }
                            }
                        }
                    }
                    // 连接断开：通知 UI（真实离线状态，PRD §3.3 在线状态），清 pending，退避后重连
                    protocol::log::warn("gui.ws", format!("连接断开 {url}，准备重连"));
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
                Err(e) => {
                    attempt += 1;
                    let delay = Duration::from_millis(500u64 * 2u64.pow(attempt.min(6)));
                    protocol::log::debug(
                        "gui.ws",
                        format!("连接失败（第 {attempt} 次），{delay:?} 后重试: {e}"),
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}
