//! amux server 常驻进程入口。
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

    let log_path = cfg
        .data_dir
        .parent()
        .map(|p| p.join("logs").join("server.log"))
        .unwrap_or_else(|| cfg.data_dir.join("server.log"));
    protocol::log::init_file_output(&log_path);

    // 依赖组装：ACP agent 驱动（--agent 显式指定 agent 可执行与子命令参数）；未指定时由
    // AgentRegistry 自动发现本机 ACP agent。单个显式 agent 拉起失败不阻止
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
                protocol::log::warn(
                    "server.startup",
                    format!("启动 ACP agent ({bin}) 失败，Server 将继续监听: {e}"),
                );
                None
            }
        }
    });

    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        protocol::log::error("server.startup", format!("创建数据目录失败: {e}"));
        std::process::exit(1);
    }
    let configured_failed = cfg.agent_bin.is_some() && configured.is_none();
    let agents = Arc::new(AgentRegistry::new(configured));
    if let (Some(name), Some(bin)) = (configured_name, cfg.agent_bin.clone()) {
        agents.set_configured_spec(name.clone(), bin, cfg.agent_args.clone(), Vec::new());
        if configured_failed {
            agents.mark_configured_unavailable(&name);
        }
    }

    let registry = match SessionRegistry::open(&cfg.data_dir.join("session.sqlite")) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            protocol::log::error("server.startup", format!("打开会话注册表失败: {e}"));
            std::process::exit(1);
        }
    };

    // 保留 agents 引用用于退出时关闭 ACP 子进程。
    let shutdown_agents = agents.clone();

    let (manager, notifications) =
        SessionManager::new(agents.clone(), registry, cfg.data_dir.clone());
    let manager = Arc::new(manager);

    // 退出信号（Ctrl+C 与 SIGTERM）：优雅关闭 ACP 子进程资源。
    // 看门狗：个别 agent 挂死时 join 可能不返回，5s 后强制退出兜底。
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 处理失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        protocol::log::info("server.shutdown", "收到退出信号，正在关闭 ACP 子进程…");
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            protocol::log::error("server.shutdown", "关闭超时，强制退出");
            std::process::exit(0);
        });
        shutdown_agents.shutdown_all();
        protocol::log::info("server.shutdown", "ACP 子进程已全部关闭");
        std::process::exit(0);
    });

    // 定时清理长时间无活动会话（>1h）。
    let cleanup_manager = manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => duration.as_millis() as u64,
                Err(e) => {
                    protocol::log::error("server.cleanup", format!("读取系统时间失败：{e}"));
                    continue;
                }
            };
            match cleanup_manager.close_idle(now_ms, 3_600_000).await {
                Ok(closed) if closed > 0 => {
                    protocol::log::info(
                        "server.cleanup",
                        format!("关闭 {closed} 个长时间无活动会话"),
                    );
                }
                Ok(_) => {}
                Err(e) => protocol::log::error("server.cleanup", e.to_string()),
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
        logger: Some(Arc::new(|line| {
            protocol::log::info("server.transport", line)
        })),
    });

    // 启动拉起：并行拉起已发现 agent。放在监听之后
    // 后台执行——bind 失败路径不再遗留子进程，agent 握手（最坏 30s/个）不阻塞
    // server 就绪；可用性经 agent.list 反映。
    let launch_agents = agents.clone();
    std::thread::spawn(move || {
        let launch = launch_agents.launch_discovered();
        protocol::log::info(
            "server.startup",
            format!(
                "ACP server 启动完成：{} 个已拉起，{} 个失败（标记不可用）",
                launch.started, launch.failed
            ),
        );
    });

    if let Err(e) = transport.run().await {
        protocol::log::error("server", format!("server 出错: {e}"));
        std::process::exit(1);
    }
}
