//! 与 Server 的 WebSocket 连接：握手认证、请求分发、断线缓存重连。
//!
//! 连接生命周期（docs/DESIGN.md「Server-Daemon 通信」与「断线重连」）：
//! - 握手期携带 `Authorization: Bearer <token>` 与 `amux-machine: <machine_name>`，
//!   Server 校验失败则握手不成立
//! - 连接断开后 Agent 与终端继续运行，出站帧进入 [`Outbox`] 缓存，重连后补发
//! - 每隔 1 分钟尝试重连

use std::sync::Arc;
use std::time::Duration;

use amux_common::daemon::{
    header, method, notify, AcpForward, AgentParams, GitRepoParams, WorktreeListResult,
    WorktreePathParams, WorktreeResult,
};
use amux_common::domain::{
    FsListParams, FsReadParams, OpResult, TerminalIdParams, TerminalInputParams,
    TerminalOpenParams, TerminalOpenResult, TerminalResizeParams,
};
use amux_common::jsonrpc::{JsonRpcNotification, JsonRpcRequest};
use futures_util::stream::SplitStream;
use futures_util::{Sink, SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::agents::AgentRegistry;
use crate::frames;
use crate::fs::FsBrowser;
use crate::git::GitRunner;
use crate::machine;
use crate::outbox::Outbox;
use crate::rpc::{RpcError, RpcResult};
use crate::terminal::TerminalRegistry;

/// 断线重连间隔。
const RECONNECT_INTERVAL: Duration = Duration::from_secs(60);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Daemon {
    machine: String,
    agents: Arc<AgentRegistry>,
    terminals: Arc<TerminalRegistry>,
    fs: FsBrowser,
    git: GitRunner,
}

/// Daemon 主循环：连接 Server，断线则周期性重连；收到终止信号时关闭所有 Agent 与终端。
pub async fn run(machine_name: String, server: String, token: String) -> Result<(), String> {
    let daemon = Arc::new(Daemon {
        machine: machine_name,
        agents: AgentRegistry::new(),
        terminals: TerminalRegistry::new(),
        fs: FsBrowser::new(),
        git: GitRunner::new(),
    });
    let outbox = Arc::new(Outbox::new());

    tokio::select! {
        _ = reconnect_loop(&server, &token, &daemon, &outbox) => log::info!("连接循环结束"),
        _ = shutdown_signal() => log::info!("收到终止信号"),
    }

    log::info!("daemon 关闭：关闭所有 agent 与终端");
    daemon.agents.shutdown_all().await;
    daemon.terminals.shutdown_all();
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
        std::future::pending::<()>().await;
        return;
    };
    let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn reconnect_loop(server: &str, token: &str, daemon: &Arc<Daemon>, outbox: &Arc<Outbox>) {
    loop {
        match connect_once(server, token, daemon, outbox).await {
            Ok(()) => log::info!("与 Server 的连接已断开，{RECONNECT_INTERVAL:?} 后重连"),
            Err(error) => log::warn!("连接 Server 失败: {error}，{RECONNECT_INTERVAL:?} 后重连"),
        }
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

/// 建立一次连接并服务到断开。
async fn connect_once(
    server: &str,
    token: &str,
    daemon: &Arc<Daemon>,
    outbox: &Arc<Outbox>,
) -> Result<(), String> {
    let server = with_daemon_path(server);
    let mut request = server
        .as_str()
        .into_client_request()
        .map_err(|error| format!("Server 地址非法: {error}"))?;
    {
        let headers = request.headers_mut();
        headers.insert(
            HeaderName::from_static(header::AUTHORIZATION),
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| format!("token 含非法字符: {error}"))?,
        );
        headers.insert(
            HeaderName::from_static(header::MACHINE),
            HeaderValue::from_str(&header::encode_machine(&daemon.machine))
                .map_err(|error| format!("机器名编码失败: {error}"))?,
        );
    }

    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("握手失败: {error}"))?;
    log::info!("已连接 Server（machine={}）", daemon.machine);

    let (sink, mut stream) = ws.split();
    let drain = tokio::spawn(drain_outbox(Arc::clone(outbox), sink));
    let result = receive_loop(&mut stream, daemon, outbox).await;
    drain.abort();
    result
}

/// Server 的 Daemon 接入端点：`--server` 未指定路径时补 `/daemon`。
fn with_daemon_path(server: &str) -> String {
    let after_scheme = server
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(server);
    if after_scheme.contains('/') {
        server.to_string()
    } else {
        format!("{}/daemon", server.trim_end_matches('/'))
    }
}

/// 出站泵：把缓存中的帧按序发送，发送成功后才从缓存移除。
async fn drain_outbox<S>(outbox: Arc<Outbox>, mut sink: S) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
{
    loop {
        let (seq, frame) = outbox.peek().await;
        log::debug!("发送帧: {}", amux_common::text::truncate(&frame, 400));
        sink.send(Message::text(frame))
            .await
            .map_err(|_| "发送失败".to_string())?;
        outbox.ack(seq);
    }
}

async fn receive_loop(
    stream: &mut SplitStream<WsStream>,
    daemon: &Arc<Daemon>,
    outbox: &Arc<Outbox>,
) -> Result<(), String> {
    while let Some(message) = stream.next().await {
        let message = message.map_err(|error| format!("接收失败: {error}"))?;
        match message {
            Message::Text(text) => dispatch(&text, daemon, outbox),
            Message::Binary(_) => log::warn!("忽略二进制帧"),
            // 不使用心跳：Server 未约定 ping/pong 语义
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Frame(_) => {}
            Message::Close(_) => break,
        }
    }
    Ok(())
}

fn dispatch(text: &str, daemon: &Arc<Daemon>, outbox: &Arc<Outbox>) {
    log::debug!("收到帧: {}", amux_common::text::truncate(text, 400));
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        log::warn!("收到非法 JSON 帧");
        return;
    };
    if value.get("method").is_none() {
        // Daemon 不主动发起请求，不会收到响应帧
        log::warn!("忽略非请求帧: {text}");
        return;
    }
    match serde_json::from_value::<JsonRpcRequest>(value.clone()) {
        Ok(request) => {
            let daemon = Arc::clone(daemon);
            let outbox = Arc::clone(outbox);
            tokio::spawn(async move { handle_request(request, &daemon, &outbox).await });
        }
        Err(_) => match serde_json::from_value::<JsonRpcNotification>(value) {
            Ok(notification) => handle_notification(notification, daemon),
            Err(error) => log::warn!("无法解析帧: {error}"),
        },
    }
}

async fn handle_request(request: JsonRpcRequest, daemon: &Arc<Daemon>, outbox: &Arc<Outbox>) {
    let id = request.id.clone();
    let params = request.params.clone().unwrap_or(serde_json::Value::Null);
    let frame = match execute(&request.method, params, daemon, outbox).await {
        Ok(value) => frames::response(id, &value),
        Err(error) => {
            log::warn!("{} 失败: {}", request.method, error.message);
            frames::error_response(id, error.code, &error.message)
        }
    };
    log::debug!("应答帧已入队: {}", amux_common::text::truncate(&frame, 400));
    outbox.push(frame);
}

async fn execute(
    method: &str,
    params: serde_json::Value,
    daemon: &Arc<Daemon>,
    outbox: &Arc<Outbox>,
) -> RpcResult<serde_json::Value> {
    match method {
        method::MACHINE_INFO => to_value(machine::machine_info(&daemon.machine)),
        method::AGENT_LIST => to_value(daemon.agents.list()),
        method::AGENT_RESTART => {
            let params: AgentParams = decode(params)?;
            daemon
                .agents
                .restart(&params.agent, Arc::clone(outbox))
                .await
                .map_err(RpcError::agent_unavailable)?;
            to_value(OpResult::ok())
        }
        method::GIT_DIFF => {
            let params: GitRepoParams = decode(params)?;
            to_value(daemon.git.diff(&params.repo))
        }
        method::GIT_WORKTREE_NEW => {
            let params: GitRepoParams = decode(params)?;
            let dir = daemon
                .git
                .worktree_new(&params.repo)
                .map_err(RpcError::git)?;
            to_value(WorktreeResult { worktree_dir: dir })
        }
        method::GIT_WORKTREE_RESUME => {
            let params: WorktreePathParams = decode(params)?;
            let dir = daemon
                .git
                .worktree_resume(&params.repo, &params.path)
                .map_err(RpcError::git)?;
            to_value(WorktreeResult { worktree_dir: dir })
        }
        method::GIT_WORKTREE_LIST => {
            let params: GitRepoParams = decode(params)?;
            let worktrees = daemon
                .git
                .worktree_list(&params.repo)
                .map_err(RpcError::git)?;
            to_value(WorktreeListResult { worktrees })
        }
        method::GIT_WORKTREE_REMOVE => {
            let params: WorktreePathParams = decode(params)?;
            daemon.git.worktree_remove(&params.repo, &params.path);
            to_value(OpResult::ok())
        }
        method::FS_LIST => {
            let params: FsListParams = decode(params)?;
            let result = daemon
                .fs
                .list(
                    params.path.as_deref(),
                    params.limit,
                    params.offset,
                    params.dirs_only,
                )
                .map_err(RpcError::fs)?;
            to_value(result)
        }
        method::FS_READ => {
            let params: FsReadParams = decode(params)?;
            let result = daemon
                .fs
                .read(&params.path, params.offset, params.limit)
                .map_err(RpcError::fs)?;
            to_value(result)
        }
        method::TERMINAL_OPEN => {
            let params: TerminalOpenParams = decode(params)?;
            let terminal_id = daemon.terminals.open(params, Arc::clone(outbox))?;
            to_value(TerminalOpenResult { terminal_id })
        }
        method::TERMINAL_RESIZE => {
            let params: TerminalResizeParams = decode(params)?;
            daemon.terminals.resize(params)?;
            to_value(OpResult::ok())
        }
        method::TERMINAL_INPUT => {
            let params: TerminalInputParams = decode(params)?;
            daemon.terminals.input(params)?;
            to_value(OpResult::ok())
        }
        method::TERMINAL_CLOSE => {
            let params: TerminalIdParams = decode(params)?;
            daemon.terminals.close(params)?;
            to_value(OpResult::ok())
        }
        other => Err(RpcError::method_not_found(other)),
    }
}

fn handle_notification(notification: JsonRpcNotification, daemon: &Arc<Daemon>) {
    if notification.method != notify::ACP {
        log::warn!("忽略未知通知: {}", notification.method);
        return;
    }
    let Some(params) = notification.params else {
        log::warn!("acp 通知缺少参数");
        return;
    };
    match serde_json::from_value::<AcpForward>(params) {
        Ok(forward) => {
            if let Err(error) = daemon.agents.forward_to_agent(&forward.agent, &forward.raw) {
                log::warn!("{error}");
            }
        }
        Err(error) => log::warn!("acp 通知负载非法: {error}"),
    }
}

fn decode<T: DeserializeOwned>(params: serde_json::Value) -> RpcResult<T> {
    serde_json::from_value(params).map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn to_value<T: Serialize>(value: T) -> RpcResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|error| RpcError::internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_defaults_to_daemon_endpoint() {
        assert_eq!(
            with_daemon_path("ws://127.0.0.1:34567"),
            "ws://127.0.0.1:34567/daemon"
        );
        assert_eq!(
            with_daemon_path("wss://example.com:443/"),
            "wss://example.com:443/"
        );
        assert_eq!(
            with_daemon_path("ws://example.com/custom"),
            "ws://example.com/custom"
        );
    }
}
