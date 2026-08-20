//! WebSocket 传输层（docs/DESIGN.md §4）：token 认证、连接管理、JSON-RPC 分发、
//! 通知广播（仅 `session.state_change`；多客户端同一份流、互不踢出）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use protocol::{method, notify, server_error, AuthParams, OpResult};

use crate::rpc::{Handlers, RpcError};
use crate::session::ServerNotification;

pub struct TransportOptions {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub handlers: Arc<Handlers>,
    pub notifications: broadcast::Receiver<ServerNotification>,
    pub logger: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

pub struct Transport {
    opts: TransportOptions,
}

fn log(opts: &TransportOptions, line: String) {
    if let Some(l) = &opts.logger {
        l(line);
    }
}

impl Transport {
    pub fn new(opts: TransportOptions) -> Self {
        Transport { opts }
    }

    pub async fn run(self) -> Result<(), String> {
        let addr: SocketAddr = format!("{}:{}", self.opts.host, self.opts.port)
            .parse()
            .map_err(|e| format!("地址非法: {e}"))?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("监听失败: {e}"))?;
        log(&self.opts, format!("amux server listening on ws://{addr}"));

        // 广播任务：把会话通知序列化为 JSON-RPC notification（仅 session.state_change）发给所有连接
        let (tx, _) = broadcast::channel::<String>(256);
        let tx_clone = tx.clone();
        let mut notify_rx = self.opts.notifications.resubscribe();
        tokio::spawn(async move {
            while let Ok(n) = notify_rx.recv().await {
                if let Some(frame) = notification_frame(&n) {
                    let _ = tx_clone.send(frame);
                }
            }
        });

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    log(&self.opts, format!("accept 失败: {e}"));
                    continue;
                }
            };
            let opts = TransportOptions {
                host: self.opts.host.clone(),
                port: self.opts.port,
                token: self.opts.token.clone(),
                handlers: self.opts.handlers.clone(),
                notifications: self.opts.notifications.resubscribe(),
                logger: self.opts.logger.clone(),
            };
            let notify_rx = tx.subscribe();
            tokio::spawn(async move {
                handle_connection(stream, peer, opts, notify_rx).await;
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    opts: TransportOptions,
    mut notify_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            log(&opts, format!("ws 握手失败 ({peer}): {e}"));
            return;
        }
    };
    log(&opts, format!("连接: {peer}"));

    let (mut sink, mut source) = ws.split();
    let handlers = opts.handlers.clone();
    // 每连接「已认证」标志：建连后必须先发 `auth` 消息（docs/DESIGN.md「认证」）
    let authenticated = Arc::new(AtomicBool::new(false));
    let token = opts.token.clone();
    // 请求处理与通知发送解耦：dispatch 在独立任务，响应经通道回传
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<String>(64);

    loop {
        tokio::select! {
            n = notify_rx.recv() => {
                let Ok(frame) = n else { break };
                if sink.send(Message::Text(frame)).await.is_err() {
                    break;
                }
            }
            resp = resp_rx.recv() => {
                let Some(frame) = resp else { break };
                if sink.send(Message::Text(frame)).await.is_err() {
                    break;
                }
            }
            msg = source.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        log(&opts, format!("连接错误 ({peer}): {e}"));
                        break;
                    }
                };
                let Message::Text(text) = msg else { continue };
                // 认证请求在连接任务内串行完成；因此紧随其后的业务请求只有在
                // auth 响应已处理后才会被派发，不会与认证发生竞态。
                let is_auth = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(|method| method.as_str())
                            .map(|method| method == method::AUTH)
                    })
                    .unwrap_or(false);
                if is_auth && !authenticated.load(Ordering::SeqCst) {
                    if let Some(resp) = dispatch(&handlers, &text, &token, &authenticated).await {
                        let Ok(frame) = serde_json::to_string(&resp) else {
                            break;
                        };
                        if sink.send(Message::Text(frame)).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                let handlers = handlers.clone();
                let resp_tx = resp_tx.clone();
                let authenticated = authenticated.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    if let Some(resp) = dispatch(&handlers, &text, &token, &authenticated).await {
                        let frame = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = resp_tx.send(frame).await;
                    }
                });
            }
        }
    }
    log(&opts, format!("断开: {peer}"));
}

/// 分发一帧 JSON-RPC 消息；请求返回响应，通知返回 None。
/// 未认证连接只接受 `auth` 方法，其余一律返回 AUTH_FAILED（docs/DESIGN.md「认证」）。
async fn dispatch(
    handlers: &Handlers,
    text: &str,
    token: &str,
    authenticated: &Arc<AtomicBool>,
) -> Option<protocol::JsonRpcResponse> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return Some(protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: protocol::JsonRpcId::Null,
                result: None,
                error: Some(protocol::JsonRpcError {
                    code: protocol::rpc_error::PARSE_ERROR,
                    message: "Parse error".into(),
                    data: None,
                }),
            });
        }
    };
    if value.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: value
                .get("id")
                .cloned()
                .unwrap_or(protocol::JsonRpcId::Null),
            result: None,
            error: Some(protocol::JsonRpcError {
                code: protocol::rpc_error::INVALID_REQUEST,
                message: "jsonrpc 必须为 2.0".into(),
                data: None,
            }),
        });
    }
    let method = value.get("method").and_then(|m| m.as_str());
    let Some(method) = method else {
        return Some(protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: value
                .get("id")
                .cloned()
                .unwrap_or(protocol::JsonRpcId::Null),
            result: None,
            error: Some(protocol::JsonRpcError {
                code: protocol::rpc_error::INVALID_REQUEST,
                message: "Invalid Request".into(),
                data: None,
            }),
        });
    };
    let id = value.get("id").cloned();
    let params = value.get("params").cloned();

    let Some(id) = id else {
        // 客户端发起的通知：当前无 client→server 通知
        return None;
    };

    if method == method::AUTH {
        return Some(handle_auth(&params, token, authenticated, id));
    }
    if !authenticated.load(Ordering::SeqCst) {
        protocol::log::error(
            "server.transport",
            format!("未认证连接请求 {method} → AUTH_FAILED"),
        );
        return Some(protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(protocol::JsonRpcError {
                code: server_error::AUTH_FAILED,
                message: "未认证：请先发送 auth 消息".into(),
                data: None,
            }),
        });
    }

    let summary = protocol::log::params_summary(
        params.as_ref().unwrap_or(&protocol::JsonRpcId::Null),
        &["sessionId", "agent", "cwd", "input"],
        60,
    );
    protocol::log::debug("server.transport", format!("请求 {method} {summary}"));
    let started = std::time::Instant::now();
    let result = handlers.handle(method, &params).await;
    let (result, error) = match result {
        Ok(v) => (Some(v), None),
        Err(RpcError { code, message }) => {
            protocol::log::error(
                "server.transport",
                format!("请求 {method} 失败 [{code}]: {message}"),
            );
            (
                None,
                Some(protocol::JsonRpcError {
                    code,
                    message,
                    data: None,
                }),
            )
        }
    };
    protocol::log::debug(
        "server.transport",
        format!("响应 {method}（{}ms）", started.elapsed().as_millis()),
    );
    Some(protocol::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result,
        error,
    })
}

/// 处理 `auth` 消息：比较 token，匹配则标记认证通过并返回结果，否则 AUTH_FAILED。
fn handle_auth(
    params: &Option<protocol::JsonRpcId>,
    token: &str,
    authenticated: &Arc<AtomicBool>,
    id: protocol::JsonRpcId,
) -> protocol::JsonRpcResponse {
    let auth: Result<AuthParams, _> =
        params
            .clone()
            .map(serde_json::from_value)
            .unwrap_or(Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "missing params",
            ))));
    match auth {
        Ok(params) if params.token == token => {
            authenticated.store(true, Ordering::SeqCst);
            protocol::log::debug("server.transport", "认证通过");
            protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: serde_json::to_value(OpResult {
                    ok: true,
                    message: None,
                })
                .ok(),
                error: None,
            }
        }
        _ => {
            protocol::log::error("server.transport", "认证失败：token 不匹配");
            protocol::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(protocol::JsonRpcError {
                    code: server_error::AUTH_FAILED,
                    message: "认证失败".into(),
                    data: None,
                }),
            }
        }
    }
}

/// 会话状态变更通知 → JSON-RPC notification 帧（docs/DESIGN.md 唯一主动推送）。
fn notification_frame(n: &ServerNotification) -> Option<String> {
    let frame = match n {
        ServerNotification::StateChange(state_change) => protocol::JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: notify::SESSION_STATE_CHANGE.to_string(),
            params: Some(serde_json::to_value(state_change.clone()).ok()?),
        },
    };
    serde_json::to_string(&frame).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{SessionState, SessionStateChange};

    /// 状态变更通知帧：方法名与 camelCase 负载（docs/DESIGN.md 唯一主动推送）。
    #[test]
    fn state_change_frame() {
        let n = ServerNotification::StateChange(SessionStateChange {
            session_id: "s1".into(),
            old_state: SessionState::Idle,
            new_state: SessionState::Busy,
        });
        let frame = notification_frame(&n).expect("应序列化");
        assert!(
            frame.contains(&format!("\"method\":\"{}\"", notify::SESSION_STATE_CHANGE)),
            "{frame}"
        );
        assert!(frame.contains("\"sessionId\":\"s1\""), "{frame}");
        assert!(frame.contains("\"newState\":\"busy\""), "{frame}");
    }
}
