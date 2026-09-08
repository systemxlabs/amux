//! amux server 常驻进程入口。
//! 启动流程：配置 → token → 依赖组装 → 监听 WebSocket。

// server crate 的模块全部在 lib.rs 声明（main.rs 复用它，避免重复编译）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use amux_server::agent::{AcpAgentDriver, AgentDriver, AgentRegistry, SharedDriver};
use amux_server::config::load_config;
use amux_server::fs::FsBrowser;
use amux_server::git::GitRunner;
use amux_server::registry::SessionRegistry;
use amux_server::rpc::Handlers;
use amux_server::session::SessionManager;
use amux_server::transport::{Transport, TransportOptions};

/// `AMUX_AGENT_BIN` 可执行路径对应注册表 agent 名（可执行文件名；路径不含文件名时回落为原始串）。
fn configured_agent_name(bin: &str) -> String {
    std::path::Path::new(bin)
        .file_name()
        .and_then(|f| f.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| bin.to_string())
}

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
    amux_common::log::init_file_output(&log_path);

    // 依赖组装：ACP agent 驱动（AMUX_AGENT_BIN 指定 agent 可执行与子命令参数）；未指定时由
    // AgentRegistry 自动发现本机 ACP agent。单个显式 agent 拉起失败不阻止
    // Server 监听，其他已发现 agent 仍可用。
    //
    // 显式 agent 的 initialize 可能阻塞较久；放入 blocking 线程并在此阶段先监听退出信号，
    // 避免 SIGTERM 恰好落在启动握手期间时无法通知 ACP 线程回收子进程。
    let shutting_down = Arc::new(AtomicBool::new(false));
    let configured: Option<(String, SharedDriver)> = if let Some(bin) = cfg.agent_bin.clone() {
        let name = configured_agent_name(&bin);
        let log_bin = bin.clone();
        let args = cfg.agent_args.clone();
        let startup_shutdown = shutting_down.clone();
        let mut startup = tokio::task::spawn_blocking(move || {
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            AcpAgentDriver::spawn_with_shutdown(&bin, &args_ref, &[], Some(startup_shutdown))
        });
        let startup_result = tokio::select! {
            result = &mut startup => result,
            _ = tokio::signal::ctrl_c() => {
                shutting_down.store(true, Ordering::Release);
                if let Ok(Ok(driver)) = startup.await {
                    driver.shutdown_and_join();
                }
                std::process::exit(0);
            },
            _ = async {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )
                .expect("注册 SIGTERM 处理失败");
                sigterm.recv().await
            } => {
                shutting_down.store(true, Ordering::Release);
                if let Ok(Ok(driver)) = startup.await {
                    driver.shutdown_and_join();
                }
                std::process::exit(0);
            },
        };
        match startup_result {
            Ok(Ok(driver)) => Some((name, Arc::new(driver) as SharedDriver)),
            Ok(Err(e)) => {
                log::warn!("启动 ACP agent ({log_bin}) 失败，Server 将继续监听: {e}");
                None
            }
            Err(e) => {
                log::warn!("启动 ACP agent ({log_bin}) 任务失败，Server 将继续监听: {e}");
                None
            }
        }
    } else {
        None
    };

    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        log::error!("创建数据目录失败: {e}");
        if let Some((_, driver)) = &configured {
            driver.shutdown_and_join();
        }
        std::process::exit(1);
    }
    let configured_failed = cfg.agent_bin.is_some() && configured.is_none();
    let agents = Arc::new(AgentRegistry::with_shutdown(
        configured,
        shutting_down.clone(),
    ));
    if let Some(bin) = cfg.agent_bin.clone() {
        let name = configured_agent_name(&bin);
        agents.set_configured_spec(name.clone(), bin, cfg.agent_args.clone(), Vec::new());
        if configured_failed {
            agents.mark_configured_unavailable(&name);
        }
    }

    let registry = match SessionRegistry::open(&cfg.data_dir.join("session.sqlite")) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            log::error!("打开会话注册表失败: {e}");
            agents.shutdown_all();
            std::process::exit(1);
        }
    };

    // 保留 agents 引用用于退出时关闭 ACP 子进程。
    let shutdown_agents = agents.clone();

    let (manager, notifications) =
        SessionManager::new(agents.clone(), registry, cfg.data_dir.clone());
    let manager = Arc::new(manager);
    let terminals = Arc::new(amux_server::terminal::TerminalService::new());

    // 启动拉起：并行拉起已发现 agent。放在监听之前的独立线程中，避免
    // agent 握手（最坏 30s/个）阻塞 server 就绪；可用性经 agent.list 反映。
    let discovery_thread = Arc::new(std::sync::Mutex::new(Some({
        let launch_agents = agents.clone();
        std::thread::spawn(move || {
            let launch = launch_agents.launch_discovered();
            log::info!(
                "ACP server 启动完成：{} 个已拉起，{} 个失败（标记不可用）",
                launch.started,
                launch.failed
            );
        })
    })));

    // 退出信号（Ctrl+C 与 SIGTERM）：优雅关闭 PTY 与 ACP 子进程资源。
    // 看门狗：个别 agent 挂死时 join 可能不返回，5s 后强制退出兜底。
    let signal_agents = shutdown_agents.clone();
    let signal_terminals = terminals.clone();
    let signal_discovery = discovery_thread.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 处理失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        log::info!("收到退出信号，正在关闭 PTY 与 ACP 子进程…");
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            log::error!("关闭超时，强制退出");
            std::process::exit(0);
        });
        signal_terminals.shutdown_all();
        signal_agents.shutdown_all();
        if let Some(handle) = signal_discovery
            .lock()
            .expect("获取 agent 启动线程锁失败")
            .take()
        {
            let _ = handle.join();
        }
        log::info!("PTY 与 ACP 子进程已全部关闭");
        std::process::exit(0);
    });

    // 定时清理：关闭长时间无活动会话；清理超期不活跃会话的 worktree。
    let cleanup_manager = manager.clone();
    tokio::spawn(async move {
        const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
        const CLOSE_IDLE_SESSION_AFTER: std::time::Duration =
            std::time::Duration::from_secs(60 * 60);
        const CLEANUP_WORKTREE_AFTER: std::time::Duration =
            std::time::Duration::from_secs(7 * 24 * 60 * 60);
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            interval.tick().await;
            let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => duration.as_millis() as u64,
                Err(e) => {
                    log::error!("读取系统时间失败：{e}");
                    continue;
                }
            };
            match cleanup_manager
                .close_idle(now_ms, CLOSE_IDLE_SESSION_AFTER)
                .await
            {
                Ok(closed) if closed > 0 => {
                    log::info!("关闭 {closed} 个长时间无活动会话");
                }
                Ok(_) => {}
                Err(e) => log::error!("{}", e),
            }
            // 超过 7 天不活跃的会话自动清理其 worktree。
            match cleanup_manager
                .cleanup_idle_worktrees(now_ms, CLEANUP_WORKTREE_AFTER)
                .await
            {
                Ok(cleaned) if cleaned > 0 => {
                    log::info!("清理 {cleaned} 个超期会话的 worktree");
                }
                Ok(_) => {}
                Err(e) => log::error!("{}", e),
            }
        }
    });

    let handlers = Arc::new(Handlers {
        manager: manager.clone(),
        git: GitRunner::new(),
        fs: FsBrowser::new(),
        terminals: terminals.clone(),
    });

    let transport = Transport::new(TransportOptions {
        host: cfg.host.clone(),
        port: cfg.port,
        token: cfg.token.clone(),
        handlers: handlers.clone(),
        notifications,
    });

    if let Err(e) = transport.run().await {
        log::error!("server 出错: {e}");
        terminals.shutdown_all();
        shutdown_agents.shutdown_all();
        if let Some(handle) = discovery_thread
            .lock()
            .expect("获取 agent 启动线程锁失败")
            .take()
        {
            let _ = handle.join();
        }
        std::process::exit(1);
    }
}
