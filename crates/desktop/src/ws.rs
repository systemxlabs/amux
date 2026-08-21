//! GUI 的 WS 客户端（docs/DESIGN.md §4）：连 amux server，JSON-RPC 请求/响应/通知。
//!
//! 认证（docs/DESIGN.md「机器连接」）：应用连接后**先发 `auth`**（method=AUTH，params={token}），
//! 认证成功后才处理其它请求；认证完成前到达的请求一律返回认证失败（AUTH_FAILED）。
//! 数据为**拉取式**（request/response 按 id 匹配），断线指数退避重连；
//! 仅保留本地 connected/disconnected 通知供 UI 标记在线/离线状态。

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
        // 建连后**先发 `auth`**，之后再处理其它请求。
        let (req_tx, req_rx) = mpsc::channel::<ClientReq>(64);
        let (notify_tx, _) = broadcast::channel::<Notification>(256);
        let notify_for_task = notify_tx.clone();
        rt().spawn(run_loop(url, token, req_rx, notify_for_task));
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
    token: String,
    mut req_rx: mpsc::Receiver<ClientReq>,
    notify_tx: broadcast::Sender<Notification>,
) {
    let mut attempt: u32 = 0;
    loop {
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
                    // 认证成功后才放行普通请求（docs/DESIGN.md「认证」）。
                    let mut authed = false;
                    // 认证：建连后首个消息必须是 auth（docs/DESIGN.md「认证」）。
                    // token 由连接配置显式传入；认证响应到达前其余请求一律返回认证失败。
                    let auth_id = next_id;
                    next_id += 1;
                    let auth_frame = json!({
                        "jsonrpc": "2.0", "id": auth_id,
                        "method": protocol::method::AUTH,
                        "params": { "token": token },
                    });
                    {
                        // auth 只有成功才放行后续请求；失败则整个连接按失败重连。
                        let _ = notify_tx.send(Notification {
                            method: "auth_sent".into(),
                            params: Value::Null,
                        });
                        if sink
                            .send(Message::Text(auth_frame.to_string()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }

                    loop {
                        tokio::select! {
                            req = req_rx.recv() => {
                                let Some(req) = req else { return };  // client 销毁
                                // 认证成功后才放行普通请求，否则一律返回认证失败（docs/DESIGN.md「认证」）
                                if !authed {
                                    let _ = req.resp.send(Err(RpcError {
                                        code: protocol::server_error::AUTH_FAILED,
                                        message: "未认证：请先完成连接认证".into(),
                                    }));
                                    continue;
                                }
                                let id = next_id;
                                next_id += 1;
                                let frame = json!({
                                    "jsonrpc": "2.0", "id": id,
                                    "method": req.method,
                                    "params": req.params.unwrap_or(Value::Null),
                                });
                                if sink.send(Message::Text(frame.to_string())).await.is_err() {
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
                                    if id == auth_id {
                                        // auth 响应：无论成败，认证阶段结束；失败则断开重连
                                        if v.get("error").is_some() {
                                            let code = v.get("error").and_then(|e| e.get("code"))
                                                .and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
                                            let message = v.get("error").and_then(|e| e.get("message"))
                                                .and_then(|m| m.as_str()).unwrap_or("").to_string();
                                            protocol::log::warn("gui.ws", format!("认证失败 [{code}]: {message}"));
                                            let _ = notify_tx.send(Notification {
                                                method: "auth_failed".into(),
                                                params: json!({ "code": code, "message": message }),
                                            });
                                            break;
                                        }
                                        protocol::log::info("gui.ws", "认证成功");
                                        authed = true;
                                        let _ = notify_tx.send(Notification {
                                            method: "auth_ok".into(),
                                            params: Value::Null,
                                        });
                                        continue;
                                    }
                                    if let Some(resp) = pending.remove(&id) {
                                        if let Some(err) = v.get("error") {
                                            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
                                            let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                                            protocol::log::warn("gui.ws", format!("请求 #{id} 失败 [{code}]: {message}"));
                                            let _ = resp.send(Err(RpcError { code, message }));
                                        } else {
                                            let _ = resp.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                                        }
                                    }
                                } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                                    // server → GUI 通知（唯一主动推送 session.state_change）
                                    protocol::log::debug("gui.ws", format!("通知 {method}"));
                                    let _ = notify_tx.send(Notification {
                                        method: method.to_string(),
                                        params: v.get("params").cloned().unwrap_or(Value::Null),
                                    });
                                }
                            }
                        }
                    }
                    // 连接断开：通知 UI（真实离线状态），清 pending，退避后重连
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
