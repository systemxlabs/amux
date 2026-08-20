//! WebSocket 传输层（docs/DESIGN.md §4）：token 认证、连接管理、JSON-RPC 分发、
//! 通知广播（多客户端同一份流、互不踢出）。

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

use protocol::{method, notify, AuthParams, JsonRpcNotification, JsonRpcResponse};

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

        // 广播任务：把会话通知序列化为 JSON-RPC notification 发给所有连接
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
            tokio::spawn(handle_connection(stream, peer, opts, notify_rx));
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
    let (mut sink, mut source) = ws.split();
    log(&opts, format!("连接: {peer}"));

    let handlers = opts.handlers.clone();
    // 请求处理与通知发送解耦：dispatch（如 prompt 聚合整个 turn）在独立任务，
    // 响应经通道回传，避免阻塞本连接的会话通知（docs/DESIGN.md §5.1）
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut authenticated = false;
    loop {
        tokio::select! {
            n = notify_rx.recv() => {
                let Ok(frame) = n else { break };
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&frame) {
                    if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                        protocol::log::debug("server.transport", format!("通知 {m}"));
                    }
                }
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
                let value = match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        continue;
                    }
                };
                let request_method = value.get("method").and_then(|m| m.as_str());
                if !authenticated {
                    if request_method != Some(method::AUTH) {
                        let response = auth_error(value.get("id").cloned().unwrap_or(Value::Null));
                        if sink.send(Message::Text(response)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let valid = value
                        .get("params")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<AuthParams>(p).ok())
                        .map(|p| p.token == opts.token)
                        .unwrap_or(false);
                    let id = value.get("id").cloned().unwrap_or(Value::Null);
                    if !valid {
                        log(&opts, format!("认证失败 ({peer})"));
                        let response = auth_error(id);
                        let _ = sink.send(Message::Text(response)).await;
                        let _ = sink.close().await;
                        break;
                    }
                    authenticated = true;
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"authenticated": true}
                    })
                    .to_string();
                    if sink.send(Message::Text(response)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let handlers = handlers.clone();
                let resp_tx = resp_tx.clone();
                tokio::spawn(async move {
                    if let Some(resp) = dispatch(&handlers, &text).await {
                        let frame = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = resp_tx.send(frame).await;
                    }
                });
            }
        }
    }
    log(&opts, format!("断开: {peer}"));
}

fn auth_error(id: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": protocol::server_error::AUTH_REQUIRED,
            "message": "认证失败"
        }
    })
    .to_string()
}

/// 分发一帧 JSON-RPC 消息；请求返回响应，通知返回 None。
async fn dispatch(handlers: &Handlers, text: &str) -> Option<JsonRpcResponse> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(protocol::JsonRpcError {
                    code: protocol::rpc_error::PARSE_ERROR,
                    message: "Parse error".into(),
                    data: None,
                }),
            });
        }
    };
    let method = value.get("method").and_then(|m| m.as_str());
    let Some(method) = method else {
        protocol::log::error("server.transport", "收到非法请求（无 method）");
        return Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: value.get("id").cloned().unwrap_or(serde_json::Value::Null),
            result: None,
            error: Some(protocol::JsonRpcError {
                code: protocol::rpc_error::INVALID_REQUEST,
                message: "Invalid Request".into(),
                data: None,
            }),
        });
    };
    let params = value.get("params").cloned();

    if let Some(id) = value.get("id").cloned() {
        // 请求：记录方法 + 关键参数（长文本截断；docs/DESIGN.md §8 全链路日志）
        let summary = protocol::log::params_summary(
            params.as_ref().unwrap_or(&serde_json::Value::Null),
            &["sessionId", "harness", "cwd", "input", "session_id"],
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
        Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result,
            error,
        })
    } else {
        // 通知（当前无客户端发起的通知）
        None
    }
}

/// 会话通知 → JSON-RPC notification 帧。
fn notification_frame(n: &ServerNotification) -> Option<String> {
    let (method, params) = match n {
        ServerNotification::SessionStateChanged(s) => (
            notify::SESSION_STATE_CHANGE,
            serde_json::json!({
                "sessionId": s.id,
                "state": s.state,
            }),
        ),
        ServerNotification::SessionCreated(s) => {
            (notify::SESSION_CREATED, serde_json::json!({ "session": s }))
        }
        ServerNotification::SessionInterrupted(s) => (
            notify::SESSION_INTERRUPTED,
            serde_json::json!({ "session": s }),
        ),
        ServerNotification::SessionDeleted(s) => {
            (notify::SESSION_DELETED, serde_json::json!({ "session": s }))
        }
        ServerNotification::SessionUpdated(s) => {
            (notify::SESSION_UPDATED, serde_json::json!({ "session": s }))
        }
        ServerNotification::Passthrough { session_id, event } => (
            notify::PASSTHROUGH,
            serde_json::json!({ "sessionId": session_id, "event": event }),
        ),
    };
    let frame = JsonRpcNotification {
        jsonrpc: "2.0".into(),
        method: method.to_string(),
        params: Some(params),
    };
    serde_json::to_string(&frame).ok()
}
