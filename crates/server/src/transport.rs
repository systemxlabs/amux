//! WebSocket 传输层（docs/DESIGN.md §4）：token 认证、连接管理、JSON-RPC 分发、
//! 通知广播（仅 `session.state_change`；多客户端同一份流、互不踢出）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use protocol::{method, notify, server_error, AuthParams, JsonRpcRequest, OpResult};

use crate::rpc::{Handlers, RpcError};
use crate::session::ServerNotification;

pub struct TransportOptions {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub handlers: Arc<Handlers>,
    /// 会话通知流（server 级独占消费；每连接只收序列化后的帧）
    pub notifications: broadcast::Receiver<ServerNotification>,
    pub logger: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// 每连接共享的上下文（从 TransportOptions 派生；不含仅 server 级的 notifications）。
struct ConnectionCtx {
    token: String,
    handlers: Arc<Handlers>,
    logger: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl ConnectionCtx {
    fn log(&self, line: String) {
        if let Some(l) = &self.logger {
            l(line);
        }
    }
}

pub struct Transport {
    opts: TransportOptions,
}

impl Transport {
    pub fn new(opts: TransportOptions) -> Self {
        Transport { opts }
    }

    pub async fn run(self) -> Result<(), String> {
        // 解构后 notifications 独占消费，其余字段进入连接上下文
        let TransportOptions {
            host,
            port,
            token,
            handlers,
            notifications,
            logger,
        } = self.opts;
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| format!("地址非法: {e}"))?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("监听失败: {e}"))?;
        if let Some(l) = &logger {
            l(format!("amux server listening on ws://{addr}"));
        }

        // 广播任务：把会话通知序列化为 JSON-RPC notification（仅 session.state_change）发给所有连接。
        // ServerNotification 接收端在此独占消费；每连接只订阅序列化后的帧通道。
        let (tx, _) = broadcast::channel::<String>(256);
        let tx_clone = tx.clone();
        let mut notify_rx = notifications;
        tokio::spawn(async move {
            while let Ok(n) = notify_rx.recv().await {
                if let Some(frame) = notification_frame(&n) {
                    let _ = tx_clone.send(frame);
                }
            }
        });

        let ctx = ConnectionCtx {
            token,
            handlers,
            logger,
        };
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    ctx.log(format!("accept 失败: {e}"));
                    continue;
                }
            };
            let conn = ConnectionCtx {
                token: ctx.token.clone(),
                handlers: ctx.handlers.clone(),
                logger: ctx.logger.clone(),
            };
            let notify_rx = tx.subscribe();
            tokio::spawn(async move {
                handle_connection(stream, peer, conn, notify_rx).await;
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    opts: ConnectionCtx,
    mut notify_rx: tokio::sync::broadcast::Receiver<String>,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            opts.log(format!("ws 握手失败 ({peer}): {e}"));
            return;
        }
    };
    opts.log(format!("连接: {peer}"));

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
                match n {
                    Ok(frame) => {
                        if sink.send(Message::Text(frame)).await.is_err() {
                            break;
                        }
                    }
                    // Lagged：慢客户端积压超限，丢弃错过通知继续服务（断连代价更高）
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        opts.log(format!("通知积压，跳过 {missed} 条"));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                        opts.log(format!("连接错误 ({peer}): {e}"));
                        break;
                    }
                };
                let Message::Text(text) = msg else { continue };
                // 入站只解析一次：信封在此解析为 JsonRpcRequest，dispatch 只做语义分发。
                let req: JsonRpcRequest = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        let frame = serde_json::to_string(&parse_error_response()).unwrap_or_default();
                        if sink.send(Message::Text(frame)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                // 认证请求在连接任务内串行完成；因此紧随其后的业务请求只有在
                // auth 响应已处理后才会被派发，不会与认证发生竞态。
                let is_auth = req.method == method::AUTH;
                if is_auth && !authenticated.load(Ordering::SeqCst) {
                    if let Some(resp) = dispatch(&handlers, req, &token, &authenticated).await {
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
                    if let Some(resp) = dispatch(&handlers, req, &token, &authenticated).await {
                        let frame = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = resp_tx.send(frame).await;
                    }
                });
            }
        }
    }
    opts.log(format!("断开: {peer}"));
}

/// Parse error 响应（id 未知，恒为 Null）。
fn parse_error_response() -> protocol::JsonRpcResponse {
    protocol::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: protocol::JsonRpcId::Null,
        result: None,
        error: Some(protocol::JsonRpcError {
            code: protocol::rpc_error::PARSE_ERROR,
            message: "Parse error".into(),
            data: None,
        }),
    }
}

/// 分发一帧已解析的 JSON-RPC 请求，返回响应。
/// 未认证连接只接受 `auth` 方法，其余一律返回 AUTH_FAILED（docs/DESIGN.md「认证」）。
async fn dispatch(
    handlers: &Handlers,
    req: JsonRpcRequest,
    token: &str,
    authenticated: &Arc<AtomicBool>,
) -> Option<protocol::JsonRpcResponse> {
    if req.jsonrpc != "2.0" {
        return Some(protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id,
            result: None,
            error: Some(protocol::JsonRpcError {
                code: protocol::rpc_error::INVALID_REQUEST,
                message: "jsonrpc 必须为 2.0".into(),
                data: None,
            }),
        });
    }
    // 无 id 的帧视为非法请求（文档样例 auth 带 id=1；auth 缺 id 返回 INVALID_REQUEST）
    let Some(id) = req.id.else_null() else {
        return Some(protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: protocol::JsonRpcId::Null,
            result: None,
            error: Some(protocol::JsonRpcError {
                code: protocol::rpc_error::INVALID_REQUEST,
                message: "请求缺少 id".into(),
                data: None,
            }),
        });
    };

    if req.method == method::AUTH {
        return Some(handle_auth(&req.params, token, authenticated, id));
    }
    if !authenticated.load(Ordering::SeqCst) {
        protocol::log::error(
            "server.transport",
            format!("未认证连接请求 {} → AUTH_FAILED", req.method),
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
        req.params.as_ref().unwrap_or(&serde_json::Value::Null),
        &["sessionId", "agent", "cwd", "input"],
        60,
    );
    protocol::log::debug("server.transport", format!("请求 {} {summary}", req.method));
    let started = std::time::Instant::now();
    let result = handlers.handle(&req.method, &req.params).await;
    let (result, error) = match result {
        Ok(v) => (Some(v), None),
        Err(RpcError { code, message }) => {
            protocol::log::error(
                "server.transport",
                format!("请求 {} 失败 [{code}]: {message}", req.method),
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
        format!("响应 {}（{}ms）", req.method, started.elapsed().as_millis()),
    );
    Some(protocol::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result,
        error,
    })
}

/// 常数时间字符串比较（token 校验）：避免逐字节短路泄漏前缀匹配长度。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 处理 `auth` 消息：比较 token，匹配则标记认证通过并返回结果，否则 AUTH_FAILED。
fn handle_auth(
    params: &Option<serde_json::Value>,
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
        Ok(params) if constant_time_eq(&params.token, token) => {
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
            reason: protocol::StateChangeReason::Completed,
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
