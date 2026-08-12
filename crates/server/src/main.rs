//! amux server 常驻进程入口（docs/DESIGN.md §3）。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

mod agent;
mod config;
mod git;
mod rpc;
mod session;
mod transport;

use std::sync::Arc;

use crate::agent::{AcpAgentDriver, AgentRegistry, SharedDriver};
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

    // 依赖组装：ACP agent 驱动（--agent 显式指定 agent 可执行与子命令参数，如
    // `--agent "kimi acp"` 或 `--agent /path/codex-acp`）；未指定时由 AgentRegistry
    // 自动发现本机 ACP agent（PRD §3.3），仅当无任何发现时才回落内存 Stub。docs/DESIGN.md §9
    let configured: Option<(String, SharedDriver)> = match &cfg.agent_bin {
        Some(bin) => {
            let args: Vec<&str> = cfg.agent_args.iter().map(String::as_str).collect();
            match AcpAgentDriver::spawn(bin, &args, &[]) {
                Ok(d) => {
                    let name = std::path::Path::new(bin)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("agent")
                        .to_string();
                    println!(
                        "已连接 ACP agent: {bin} {}（harness: {name}）",
                        cfg.agent_args.join(" ")
                    );
                    Some((name, Arc::new(d)))
                }
                Err(e) => {
                    eprintln!("启动 ACP agent ({bin}) 失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            println!("未指定 --agent，将自动发现本机 ACP agent");
            None
        }
    };

    // 数据目录（docs/DESIGN.md §5.5）
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        eprintln!("创建数据目录失败: {e}");
        std::process::exit(1);
    }
    let agents = Arc::new(AgentRegistry::new(
        configured,
        cfg.data_dir.join("agent-models.json"),
    ));

    let (manager, notifications) = SessionManager::new(agents);
    let manager = Arc::new(manager);
    // 重启恢复（docs/DESIGN.md §4.1）：经 ACP `session/list` 从 agent 侧恢复会话列表。
    // server 无持久化状态；agent 子进程在注册表/上面的 --agent 路径已随注册表惰性/显式拉起。
    manager.recover().await;

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
