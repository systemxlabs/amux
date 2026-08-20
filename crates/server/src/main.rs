//! amux server 常驻进程入口（docs/DESIGN.md §3）。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

// server crate 的模块全部在 lib.rs 声明（main.rs 复用它，避免重复编译）。

use std::sync::Arc;

use server::agent::{AcpAgentDriver, AgentRegistry, SharedDriver};
use server::config::load_config;
use server::git::GitRunner;

use server::registry::SessionRegistry;
use server::rpc::Handlers;
use server::session::SessionManager;
use server::transport::{Transport, TransportOptions};

#[tokio::main]
async fn main() {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // 初始化文件日志（按天切片、保留 7 天，docs/DESIGN.md §8）
    let log_path = cfg
        .data_dir
        .parent()
        .map(|p| p.join("logs").join("server.log"))
        .unwrap_or_else(|| cfg.data_dir.join("server.log"));
    protocol::log::init_file_output(&log_path);

    // 依赖组装：ACP agent 驱动（--agent 显式指定 agent 可执行与子命令参数）；未指定时由
    // AgentRegistry 自动发现本机 ACP agent（PRD §3.3）。单个显式 agent 拉起失败不阻止
    // Server 监听，其他已发现 agent 仍可用。
    let configured_name = cfg.agent_bin.as_ref().map(|bin| {
        std::path::Path::new(bin)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(bin)
            .to_string()
    });
    let configured: Option<(String, SharedDriver)> = cfg.agent_bin.clone().and_then(|bin| {
        let name = configured_name.clone().expect("显式 agent 名称已计算");
        let args_ref: Vec<&str> = cfg.agent_args.iter().map(String::as_str).collect();
        match AcpAgentDriver::spawn(&bin, &args_ref, &[]) {
            Ok(driver) => Some((name, Arc::new(driver) as SharedDriver)),
            Err(e) => {
                eprintln!("启动 ACP agent ({bin}) 失败，Server 将继续监听: {e}");
                None
            }
        }
    });

    // 数据目录
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        eprintln!("创建数据目录失败: {e}");
        std::process::exit(1);
    }
    let agents = Arc::new(AgentRegistry::new(configured));
    if let (Some(name), Some(bin)) = (configured_name, cfg.agent_bin.clone()) {
        agents.set_configured_spec(name, bin, cfg.agent_args.clone(), Vec::new());
    }

    // 启动拉起（docs/DESIGN.md §4.1/§7.3）：server 启动时发现本机 agent 并直接拉起。
    let launch = agents.launch_discovered();
    protocol::log::info(
        "server.startup",
        format!(
            "ACP server 启动完成：{} 个已拉起，{} 个失败（标记不可用）",
            launch.started, launch.failed
        ),
    );

    // 会话注册表（SQLite，docs/DESIGN.md「普通会话存储」：session.sqlite）。
    let registry = match SessionRegistry::open(&cfg.data_dir.join("session.sqlite")) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("打开会话注册表失败: {e}");
            std::process::exit(1);
        }
    };

    // 保留 agents 引用用于退出时关闭 ACP 子进程（docs/DESIGN.md「ACP Server 生命周期」）
    let shutdown_agents = agents.clone();

    let (manager, notifications) = SessionManager::new(agents, registry, cfg.data_dir.clone());
    let manager = Arc::new(manager);

    // 捕获 Ctrl+C 等退出信号，优雅关闭 ACP 子进程资源
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        protocol::log::info("server.shutdown", "收到退出信号，正在关闭 ACP 子进程…");
        shutdown_agents.shutdown_all();
        std::process::exit(0);
    });

    // 定时清理长时间无活动会话（>1h，docs/DESIGN.md「主动关闭长时间无活动会话」）
    let cleanup_manager = manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let closed = cleanup_manager.close_idle(now_ms, 3_600_000).await;
            if closed > 0 {
                protocol::log::info(
                    "server.cleanup",
                    format!("关闭 {closed} 个长时间无活动会话"),
                );
            }
        }
    });

    let handlers = Arc::new(Handlers {
        manager: manager.clone(),
        git: GitRunner::new(),
    });

    let transport = Transport::new(TransportOptions {
        host: cfg.host.clone(),
        port: cfg.port,
        token: cfg.token.clone(),
        handlers: handlers.clone(),
        notifications,
        logger: Some(Arc::new(|line| eprintln!("{line}"))),
    });

    if let Err(e) = transport.run().await {
        eprintln!("server 出错: {e}");
        std::process::exit(1);
    }
}
