//! amux server 常驻进程入口（docs/DESIGN.md §3）。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

mod agent;
mod config;
mod git;
mod rpc;
mod session;
mod transport;

use std::sync::Arc;

use crate::agent::{SharedDriver, StubAgentDriver};
use crate::config::load_config;
use crate::git::GitRunner;
use crate::rpc::Handlers;
use crate::session::SessionManager;
use crate::transport::{Transport, TransportOptions};

pub const SERVER_VERSION: &str = "0.1.0";

#[tokio::main]
async fn main() {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // 依赖组装：agent 驱动（当前为内存 Stub；真实 ACP stdio 对接见 docs/DESIGN.md §9，待接入）
    let driver: SharedDriver = Arc::new(StubAgentDriver::new());
    let (manager, notifications) = SessionManager::new(driver.clone(), 200);
    let manager = Arc::new(manager);

    // 数据目录（docs/DESIGN.md §5.5）
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        eprintln!("创建数据目录失败: {e}");
        std::process::exit(1);
    }

    let handlers = Arc::new(Handlers {
        manager: manager.clone(),
        git: GitRunner::new(),
        server_version: SERVER_VERSION.to_string(),
    });

    let transport = Transport::new(TransportOptions {
        host: cfg.host.clone(),
        port: cfg.port,
        token: cfg.token.clone(),
        handlers: handlers.clone(),
        notifications,
        logger: Some(Arc::new(|line| println!("{line}"))),
    });

    if let Err(e) = transport.run().await {
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }
}
