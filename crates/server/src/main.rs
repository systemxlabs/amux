//! amux server 常驻进程入口（docs/DESIGN.md §3）。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

mod agent;
mod config;
mod git;
mod rpc;
mod session;
mod transport;

use std::sync::Arc;

use crate::agent::{AcpAgentDriver, SharedDriver, StubAgentDriver};
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

    // 依赖组装：ACP agent 驱动（--agent 指定真实 agent 可执行，如 codex-acp）；
    // 未指定时用内存 Stub（演示模式）。docs/DESIGN.md §9
    let driver: SharedDriver = match &cfg.agent_bin {
        Some(bin) => match AcpAgentDriver::spawn(bin, &[]) {
            Ok(d) => {
                println!("已连接 ACP agent: {bin}");
                Arc::new(d)
            }
            Err(e) => {
                eprintln!("启动 ACP agent ({bin}) 失败: {e}");
                std::process::exit(1);
            }
        },
        None => {
            println!("未指定 --agent，使用内存 Stub（演示模式）");
            Arc::new(StubAgentDriver::new())
        }
    };
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
