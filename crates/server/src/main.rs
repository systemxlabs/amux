//! amux server 常驻进程入口（docs/DESIGN.md §3）。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

mod agent;
mod config;
mod git;
mod history;
mod registry;
mod rpc;
mod session;
mod transport;

use std::sync::Arc;

use crate::agent::{AcpAgentDriver, AgentRegistry, SharedDriver};
use crate::config::load_config;
use crate::git::GitRunner;
use crate::registry::SessionRegistry;
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

    // 启动拉起（docs/DESIGN.md §4.1/§7.3）：server 启动时发现本机 agent 并直接拉起
    // （kimi 原生 `kimi acp`，claude/codex 经 npx 包装器），后续 `driver_for` 复用缓存、
    // 不再二次 spawn。单 agent 拉起失败不致命：标记为**不可用**（get_info 的 available=
    // false，使用时报明确错误），server 照常启动、其余 agent 正常使用。
    let launch = agents.launch_discovered();
    protocol::log::info(
        "server.startup",
        format!(
            "ACP server 启动完成：{} 个已拉起，{} 个失败（标记不可用）",
            launch.started, launch.failed
        ),
    );

    // 会话注册表（SQLite，docs/DESIGN.md §4.3）：列表与历史权威 = server；
    // 重启后会话列表从本地库恢复（不依赖 ACP `session/list`，§4.1）。
    let registry = match SessionRegistry::open(&cfg.data_dir.join("amux.db")) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("打开会话注册表失败: {e}");
            std::process::exit(1);
        }
    };

    let (manager, notifications) = SessionManager::new(agents, registry, cfg.data_dir.clone());
    let manager = Arc::new(manager);

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
