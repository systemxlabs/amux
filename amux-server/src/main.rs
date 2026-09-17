//! amux Server：公网枢纽。
//!
//! 对外提供 Client HTTPS API 与 Daemon WebSocket 接入（docs/DESIGN.md「Server」）：
//! 作为 ACP client 经 Daemon 与各机器上的 Agents 通信，持有会话/工作流数据与工作流智能体。

mod acp;
mod api;
mod config_store;
mod events;
mod frames;
mod machines;
mod orchestrator;
mod sessions;
mod state;
mod store;
mod terminals;
mod timestamps;
mod web;
mod workflows;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use clap::Parser;
use tokio::sync::mpsc;

use crate::config_store::ConfigStore;
use crate::machines::MachineHub;
use crate::sessions::SessionService;
use crate::state::AppState;
use crate::store::Store;
use crate::terminals::TerminalCache;
use crate::workflows::WorkflowService;

/// 后台维护周期（关闭长时间无活动会话、清理过期 worktree）。
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(600);

/// ACP 事件队列容量。
const EVENT_QUEUE: usize = 4096;

#[derive(Debug, Parser)]
#[command(name = "amux-server", about = "公网枢纽：对接 Client 与各机器 Daemon")]
struct Args {
    /// 监听地址
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    /// 监听端口
    #[arg(long, default_value_t = 34567)]
    port: u16,
    /// 认证 token（Client 与 Daemon 共用）
    #[arg(long)]
    token: String,
    /// web 静态文件目录；未传则静态资源请求返回 404
    #[arg(long)]
    web: Option<String>,
}

#[tokio::main]
async fn main() {
    amux_common::log::init_file_output(&amux_common::log::log_path("server"));
    let args = Args::parse();
    if let Err(error) = run(args).await {
        log::error!("server 退出: {error}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let home = amux_common::paths::amux_home();
    let store = Arc::new(Store::open(&home)?);
    let config = Arc::new(ConfigStore::new(home.clone()));
    let terminals = Arc::new(TerminalCache::new());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);

    let machines = MachineHub::new(
        args.token.clone(),
        events_tx,
        Arc::clone(&terminals),
        Arc::clone(&config),
    );
    let sessions = Arc::new(SessionService::new(
        Arc::clone(&store),
        machines.clone(),
        Arc::clone(&terminals),
        Arc::clone(&config),
    ));
    let workflows = Arc::new(WorkflowService::new(
        Arc::clone(&store),
        Arc::clone(&sessions),
        machines.clone(),
        Arc::clone(&config),
        home.clone(),
    ));

    tokio::spawn(events::run(
        events_rx,
        Arc::clone(&sessions),
        Arc::clone(&workflows),
        Arc::clone(&store),
    ));
    tokio::spawn({
        let sessions = Arc::clone(&sessions);
        async move {
            loop {
                tokio::time::sleep(MAINTENANCE_INTERVAL).await;
                sessions.maintain().await;
            }
        }
    });

    let state = Arc::new(AppState {
        token: args.token.clone(),
        config,
        machines,
        sessions,
        workflows,
    });
    // 鉴权只对命中 API 路由的请求生效：浏览器加载页面时无法携带 Authorization 头，
    // 静态资源与未命中路径必须免鉴权，否则 Web 应用无法加载、
    // 未传 --web 时的 404 也会被鉴权中间件改写成 401（docs/DESIGN.md「Web 应用」）。
    let api = api::router(Arc::clone(&state)).route_layer(middleware::from_fn_with_state(
        Arc::clone(&state),
        authorize,
    ));
    let app = web::routes(api, args.web.as_deref());

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port))
        .await
        .map_err(|error| format!("监听 {}:{} 失败: {error}", args.host, args.port))?;
    log::info!("amux-server 监听 {}:{}", args.host, args.port);
    axum::serve(listener, app)
        .await
        .map_err(|error| format!("服务失败: {error}"))
}

/// 认证：所有请求（含 Daemon 握手）都必须携带 `Authorization: Bearer <token>`。
async fn authorize(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.token);
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}
