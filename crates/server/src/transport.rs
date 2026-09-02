//! WebSocket 传输层：token 认证、连接管理、JSON-RPC 分发、
//! 通知广播（仅 `session.state_change`；多客户端同一份流、互不踢出）。
//! 终端输出帧走每连接专属通道直发，不进全局广播（见 terminal.rs）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use protocol::{method, notify, server_error, AuthParams, JsonRpcRequest, OpResult};

use crate::rpc::{Handlers, RpcError};
use crate::session::ServerNotification;
use crate::terminal::ConnScope;

pub struct TransportOptions {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub handlers: Arc<Handlers>,
    /// 会话通知流（server 级独占消费；每连接只收序列化后的帧）
    pub notifications: broadcast::Receiver<ServerNotification>,
}

/// 每连接共享的上下文（从 TransportOptions 派生；不含仅 server 级的 notifications）。
struct ConnectionCtx {
    token: String,
    handlers: Arc<Handlers>,
}

pub struct Transport {
    opts: TransportOptions,
}

impl Transport {
    pub fn new(opts: TransportOptions) -> Self {
        Transport { opts }
    }

    pub async fn run(self) -> Result<(), String> {
        let TransportOptions {
            host,
            port,
            token,
            handlers,
            notifications,
        } = self.opts;
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| format!("地址非法: {e}"))?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("监听失败: {e}"))?;
        log::info!("amux server listening on ws://{addr}");

        // 广播任务：把会话通知序列化为 JSON-RPC notification（仅 session.state_change）发给所有连接。
        // ServerNotification 接收端在此独占消费；每连接只订阅序列化后的帧通道。
        let (tx, _) = broadcast::channel::<String>(256);
        let tx_clone = tx.clone();
        let mut notify_rx = notifications;
        tokio::spawn(async move {
            loop {
                match notify_rx.recv().await {
                    Ok(n) => {
                        if let Some(frame) = notification_frame(&n) {
                            let _ = tx_clone.send(frame);
                        }
                    }
                    // A slow subscriber may miss old notifications. Keep consuming
                    // newer events; ending this task would disable state broadcasts
                    // for every connection until the server restarts.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        log::warn!("服务端通知积压，跳过 {missed} 条");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let ctx = ConnectionCtx { token, handlers };
        let next_conn_id = AtomicU64::new(1);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    log::info!("accept 失败: {e}");
                    continue;
                }
            };
            let conn = ConnectionCtx {
                token: ctx.token.clone(),
                handlers: ctx.handlers.clone(),
            };
            let notify_rx = tx.subscribe();
            let conn_id = next_conn_id.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                handle_connection(stream, peer, conn, notify_rx, conn_id).await;
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    opts: ConnectionCtx,
    mut notify_rx: tokio::sync::broadcast::Receiver<String>,
    conn_id: u64,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            log::info!("ws 握手失败 ({peer}): {e}");
            return;
        }
    };
    log::info!("连接: {peer} (#{conn_id})");

    let (mut sink, mut source) = ws.split();
    let handlers = opts.handlers.clone();
    // 每连接「已认证」标志：建连后必须先发 `auth` 消息。
    let authenticated = Arc::new(AtomicBool::new(false));
    let token = opts.token.clone();
    // 请求处理与通知发送解耦：dispatch 在独立任务，响应经通道回传
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<String>(64);
    // 终端输出帧专属通道：满时施加背压而非丢弃（与终端语义一致）
    let (term_tx, mut term_rx) = tokio::sync::mpsc::channel::<String>(256);
    let conn_scope = ConnScope {
        conn_id,
        frame_tx: term_tx.clone(),
    };

    loop {
        tokio::select! {
            n = notify_rx.recv() => {
                match n {
                    Ok(frame) => {
                        // Notifications are connection-scoped after authentication;
                        // do not leak session ids to unauthenticated peers.
                        if authenticated.load(Ordering::SeqCst)
                            && sink.send(Message::Text(frame.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    // Lagged：慢客户端积压超限，丢弃错过通知继续服务（断连代价更高）
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        log::info!("通知积压，跳过 {missed} 条");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            frame = term_rx.recv() => {
                let Some(frame) = frame else { break };
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            resp = resp_rx.recv() => {
                let Some(frame) = resp else { break };
                if sink.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            msg = source.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        log::info!("连接错误 ({peer}): {e}");
                        break;
                    }
                };
                let Message::Text(text) = msg else { continue };
                let req: JsonRpcRequest = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        let frame = serde_json::to_string(&parse_error_response()).unwrap_or_default();
                        if sink.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                // 未认证阶段的请求必须在读取循环内同步处理。否则，业务请求会被
                // spawn 后挂起，紧随其后的 auth 可能先把 authenticated 置为 true，
                // 导致这条实际上先到达的请求绕过认证检查。
                if !authenticated.load(Ordering::SeqCst) {
                    if let Some(resp) =
                        dispatch(&handlers, req, &token, &authenticated, &conn_scope).await
                    {
                        let Ok(frame) = serde_json::to_string(&resp) else {
                            break;
                        };
                        if sink.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    continue;
                }
                let handlers = handlers.clone();
                let resp_tx = resp_tx.clone();
                let authenticated = authenticated.clone();
                let token = token.clone();
                let conn_scope = conn_scope.clone();
                tokio::spawn(async move {
                    if let Some(resp) = dispatch(&handlers, req, &token, &authenticated, &conn_scope).await {
                        let frame = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = resp_tx.send(frame).await;
                    }
                });
            }
        }
    }
    // 断连清理：PTY 进程与连接绑定，连接关闭时一并释放其终端。
    handlers.terminals.release_conn(conn_id);
    log::info!("断开: {peer} (#{conn_id})");
}

/// 构造 JSON-RPC 错误响应；所有错误响应共享同一协议 envelope。
fn error_response(
    id: protocol::JsonRpcId,
    code: i32,
    message: impl Into<String>,
) -> protocol::JsonRpcResponse {
    protocol::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(protocol::JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

/// Parse error 响应（id 未知，恒为 Null）。
fn parse_error_response() -> protocol::JsonRpcResponse {
    error_response(
        protocol::JsonRpcId::Null,
        protocol::rpc_error::PARSE_ERROR,
        "Parse error",
    )
}

/// 分发一帧已解析的 JSON-RPC 请求，返回响应。
/// 未认证连接只接受 `auth` 方法，其余一律返回 AUTH_FAILED。
async fn dispatch(
    handlers: &Handlers,
    req: JsonRpcRequest,
    token: &str,
    authenticated: &Arc<AtomicBool>,
    conn: &ConnScope,
) -> Option<protocol::JsonRpcResponse> {
    if req.jsonrpc != "2.0" {
        return Some(error_response(
            req.id,
            protocol::rpc_error::INVALID_REQUEST,
            "jsonrpc 必须为 2.0",
        ));
    }
    // 无 id 的帧视为非法请求（文档样例 auth 带 id=1；auth 缺 id 返回 INVALID_REQUEST）
    let Some(id) = req.id.else_null() else {
        return Some(error_response(
            protocol::JsonRpcId::Null,
            protocol::rpc_error::INVALID_REQUEST,
            "请求缺少 id",
        ));
    };

    if req.method == method::AUTH {
        return Some(handle_auth(&req.params, token, authenticated, id));
    }
    if !authenticated.load(Ordering::SeqCst) {
        log::error!("未认证连接请求 {} → AUTH_FAILED", req.method);
        return Some(error_response(
            id,
            server_error::AUTH_FAILED,
            "未认证：请先发送 auth 消息",
        ));
    }

    let summary = amux_common::log::params_summary(
        req.params.as_ref().unwrap_or(&serde_json::Value::Null),
        &["sessionId", "agent", "cwd", "input"],
        60,
    );
    log::debug!("请求 {} {summary}", req.method);
    let started = std::time::Instant::now();
    let result = handlers.handle(&req.method, &req.params, conn).await;
    let response = match result {
        Ok(result) => protocol::JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        },
        Err(RpcError { code, message }) => {
            log::error!("请求 {} 失败 [{code}]: {message}", req.method);
            error_response(id, code, message)
        }
    };
    log::debug!("响应 {}（{}ms）", req.method, started.elapsed().as_millis());
    Some(response)
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
            log::debug!("认证通过");
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
            log::error!("认证失败：token 不匹配");
            error_response(id, server_error::AUTH_FAILED, "认证失败")
        }
    }
}

/// 会话状态变更通知 → JSON-RPC notification 帧。
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

    /// 状态变更通知帧：方法名与 camelCase 负载。
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
