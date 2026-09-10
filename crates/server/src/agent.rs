//! Agent 连接：server 与 agent 的唯一接口。
//! `AcpConnection` 经官方 SDK `agent-client-protocol` 对接真实 ACP v1
//! （`AcpAgent` stdio 传输 + typed 请求/通知，`grok agent`、`codex-acp` /
//! `claude-acp` / `kimi acp`）；测试经 `mock_acp` 子进程走同一真实路径。
//! `AgentRegistry` 按 agent 名解析连接——`AMUX_AGENT_BIN` 配置的连接 +
//! PATH 自动发现的 agent（启动即拉起并复用；拉起失败标记不可用；
//! 运行期新发现的惰性拉起）。
//!
//! ACP v1 语义：session/new、resume、prompt、cancel、close 等
//! 方法；session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpConnection 使用**专用 exec 线程**承载全部异步 IO（官方 SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use crate::acp::{AcpConnection, AgentEvent, AgentSessionCaps, LaunchSummary};
use crate::discovery::{discover_acp_agents, DiscoveredAgent};
use protocol::AgentInfo;

/// agent 注册表（自动发现可执行路径，不要求手动指定）：
///
/// - `AMUX_AGENT_BIN` 指定的连接（agent 名 = 可执行文件名，如 `mock_acp` / `kimi acp`）为显式覆盖
/// - 自动发现（无需显式配置）：
///   - 已知 CLI 的 `acp` 子命令探测（如 `kimi acp`，ACP 原生）
///   - 已知 CLI 的 `agent` 子命令探测（如 `grok agent --always-approve stdio`）
///   - 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///   - 发现的 agent 在 server 启动时**直接拉起**（`launch_discovered`，后续
///     `connection_for` 复用缓存连接）；
///     **拉起失败的 agent 标记为不可用**（agent.list 的 status 反映；使用时报明确错误）；
///     运行期新发现的 agent 仍走惰性拉起兜底
/// - 没有发现 agent 时 `agent.list` 为空，使用未知 agent 会报错。
pub struct AgentRegistry {
    /// 禁用运行期自动发现（`AMUX_NO_DISCOVERY=1`）：只使用 `AMUX_AGENT_BIN` 显式配置的 agent。
    /// 供受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    no_discovery: bool,
    /// 配置连接：agent 名 + 连接
    configured: Option<(String, Arc<AcpConnection>)>,
    /// 显式配置 agent 的重启参数；连接重启后仍复用同一注册表条目。
    configured_spec: Mutex<Option<DiscoveredAgent>>,
    /// 显式配置 agent 最近一次重启后的连接。
    configured_override: Mutex<Option<Arc<AcpConnection>>>,
    /// 自动发现的 agent（不含已配置的；可运行期刷新）
    discovered: Mutex<Vec<DiscoveredAgent>>,
    /// 已拉起的发现连接（启动拉起 + 懒路径共用缓存；`connection_for` 不再二次 spawn）
    spawned: Mutex<HashMap<String, Arc<AcpConnection>>>,
    /// 启动时拉起失败的 agent（标记为不可用：agent.list 的 status=unavailable、connection_for 报错）
    unavailable: Mutex<HashSet<String>>,
    /// server 退出后阻止新的 ACP 连接启动或进入缓存。
    shutting_down: Arc<AtomicBool>,
    /// 线性化显式连接的替换与 server 关闭，避免新连接发布在关闭快照之后。
    lifecycle: Mutex<()>,
}

impl AgentRegistry {
    /// 构建注册表（生产路径：自动发现本机 ACP agent）。
    /// - `configured`：`AMUX_AGENT_BIN` 显式指定的连接，可为 None（由自动发现接管）
    pub fn new(configured: Option<(String, Arc<AcpConnection>)>) -> Self {
        Self::with_shutdown(configured, Arc::new(AtomicBool::new(false)))
    }

    /// 使用 server 统一的关闭标志构建注册表，使启动中的 ACP 握手也能响应退出信号。
    pub fn with_shutdown(
        configured: Option<(String, Arc<AcpConnection>)>,
        shutting_down: Arc<AtomicBool>,
    ) -> Self {
        let no_discovery = std::env::var("AMUX_NO_DISCOVERY")
            .map(|v| v == "1")
            .unwrap_or(false);
        let registry = AgentRegistry {
            no_discovery,
            configured,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(Vec::new()),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
            shutting_down,
            lifecycle: Mutex::new(()),
        };
        if !no_discovery {
            registry.refresh_discovery();
        }
        registry
    }

    /// 重新扫描本机 ACP agent，供运行期安装的新 agent 刷新发现。
    /// 合并新发现的 agent，保留已配置/已发现条目。
    fn refresh_discovery(&self) {
        if !self.no_discovery {
            let current = discover_acp_agents();
            let mut disc = self.discovered.lock();
            for d in current {
                let dup = disc.iter().any(|x| x.name == d.name)
                    || self
                        .configured
                        .as_ref()
                        .map(|(c, _)| c == &d.name)
                        .unwrap_or(false);
                if !dup {
                    disc.push(d);
                }
            }
        }
    }

    pub fn set_configured_spec(
        &self,
        name: String,
        bin: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        *self.configured_spec.lock() = Some(DiscoveredAgent {
            name,
            bin,
            args,
            env,
        });
    }
    pub fn mark_configured_unavailable(&self, name: &str) {
        if self
            .configured_spec
            .lock()
            .as_ref()
            .is_some_and(|spec| spec.name == name)
        {
            self.unavailable.lock().insert(name.to_string());
        }
    }
    fn status_for(name: &str, unavailable: &HashSet<String>) -> protocol::AgentStatus {
        if unavailable.contains(name) {
            protocol::AgentStatus::Unavailable
        } else {
            protocol::AgentStatus::Available
        }
    }

    pub fn list_agents(&self) -> Vec<AgentInfo> {
        self.refresh_discovery();
        let discovered = self.discovered.lock();
        let unavailable = self.unavailable.lock();
        let mut out: Vec<AgentInfo> = Vec::new();
        if let Some((name, _)) = &self.configured {
            out.push(AgentInfo {
                name: name.clone(),
                status: Self::status_for(name, &unavailable),
            });
        } else if let Some(spec) = self.configured_spec.lock().as_ref() {
            out.push(AgentInfo {
                name: spec.name.clone(),
                status: Self::status_for(&spec.name, &unavailable),
            });
        }
        for d in discovered.iter() {
            if out.iter().any(|agent| agent.name == d.name) {
                continue;
            }
            out.push(AgentInfo {
                name: d.name.clone(),
                status: Self::status_for(&d.name, &unavailable),
            });
        }
        out
    }
    pub fn connection_for(&self, agent: &str) -> Result<Arc<AcpConnection>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("agent registry 正在关闭".into());
        }
        if self.unavailable.lock().contains(agent) {
            return Err(format!("agent 不可用（启动时拉起失败）: {agent}"));
        }
        if let Some((name, _)) = &self.configured {
            if name == agent {
                if let Some(connection) = self.configured_override.lock().as_ref() {
                    return Ok(connection.clone());
                }
                return Ok(self
                    .configured
                    .as_ref()
                    .expect("配置连接刚刚存在")
                    .1
                    .clone());
            }
        }
        if let Some(spec) = self
            .configured_spec
            .lock()
            .as_ref()
            .filter(|spec| spec.name == agent)
            .cloned()
        {
            return self.spawn_and_cache(&spec);
        }
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .iter()
            .find(|d| d.name == agent)
            .cloned();
        if let Some(d) = found {
            return self.spawn_and_cache(&d);
        }
        Err(format!("本机未发现 agent: {agent}"))
    }
    /// 连接成功拉起即视为可用：清除此前记录的不可用标记。
    fn clear_unavailable(&self, name: &str) {
        self.unavailable.lock().remove(name);
    }

    fn spawn_and_cache(&self, d: &DiscoveredAgent) -> Result<Arc<AcpConnection>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("agent registry 正在关闭".into());
        }
        if let Some(connection) = self.spawned.lock().get(&d.name).cloned() {
            self.clear_unavailable(&d.name);
            return Ok(connection);
        }
        let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
        let connection = AcpConnection::spawn_with_shutdown(
            &d.bin,
            &args,
            &d.env,
            Some(self.shutting_down.clone()),
        )
        .map_err(|e| format!("启动 ACP agent ({}) 失败: {e}", d.bin))?;
        let connection: Arc<AcpConnection> = Arc::new(connection);
        let mut spawned = self.spawned.lock();
        if self.shutting_down.load(Ordering::Acquire) {
            drop(spawned);
            connection.shutdown_and_join();
            return Err("agent registry 正在关闭".into());
        }
        match spawned.get(&d.name) {
            Some(existing) => {
                let existing = existing.clone();
                drop(spawned);
                connection.shutdown_and_join();
                Ok(existing)
            }
            None => {
                spawned.insert(d.name.clone(), connection.clone());
                self.clear_unavailable(&d.name);
                Ok(connection)
            }
        }
    }
    pub fn launch_discovered(&self) -> LaunchSummary {
        if self.no_discovery {
            return LaunchSummary::default();
        }
        let discovered = self.discovered.lock().clone();
        let summary = Mutex::new(LaunchSummary::default());
        std::thread::scope(|s| {
            for d in discovered {
                let summary = &summary;
                s.spawn(move || match self.spawn_and_cache(&d) {
                    Ok(_) => {
                        summary.lock().started += 1;
                        log::info!("已拉起 ACP server: {}（agent={}）", d.bin, d.name);
                    }
                    Err(e) => {
                        summary.lock().failed += 1;
                        self.unavailable.lock().insert(d.name.clone());
                        log::error!("ACP server 拉起失败（agent={}，已标记不可用）: {e}", d.name);
                    }
                });
            }
        });
        Mutex::into_inner(summary)
    }
    pub fn restart_agent(&self, agent: &str) -> Result<(), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("agent registry 正在关闭".into());
        }
        let configured_name = self
            .configured
            .as_ref()
            .map(|(name, _)| name == agent)
            .unwrap_or(false);
        let explicit_spec = self
            .configured_spec
            .lock()
            .as_ref()
            .filter(|spec| spec.name == agent)
            .cloned();
        if configured_name || explicit_spec.is_some() {
            let spec = self
                .configured_spec
                .lock()
                .clone()
                .ok_or_else(|| format!("显式 agent 缺少重启配置: {agent}"))?;
            let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
            let connection = match AcpConnection::spawn_with_shutdown(
                &spec.bin,
                &args,
                &spec.env,
                Some(self.shutting_down.clone()),
            ) {
                Ok(connection) => connection,
                Err(e) => {
                    self.unavailable.lock().insert(agent.to_string());
                    return Err(format!("重启 ACP agent ({}) 失败: {e}", spec.bin));
                }
            };
            let new_connection: Arc<AcpConnection> = Arc::new(connection);
            self.clear_unavailable(agent);
            if configured_name {
                let _lifecycle = self.lifecycle.lock();
                if self.shutting_down.load(Ordering::Acquire) {
                    drop(_lifecycle);
                    new_connection.shutdown_and_join();
                    return Err("agent registry 正在关闭".into());
                }
                let old =
                    self.configured_override.lock().take().unwrap_or_else(|| {
                        self.configured.as_ref().expect("配置连接不存在").1.clone()
                    });
                *self.configured_override.lock() = Some(new_connection);
                drop(_lifecycle);
                old.shutdown_and_join();
            } else {
                let mut spawned = self.spawned.lock();
                if self.shutting_down.load(Ordering::Acquire) {
                    drop(spawned);
                    new_connection.shutdown_and_join();
                    return Err("agent registry 正在关闭".into());
                }
                let old = spawned.insert(agent.to_string(), new_connection);
                drop(spawned);
                if let Some(old) = old {
                    old.shutdown_and_join();
                }
            }
            log::info!("手动重启成功：{}（agent={}）", spec.bin, agent);
            return Ok(());
        }
        self.unavailable.lock().remove(agent);
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .iter()
            .find(|d| d.name == agent)
            .cloned();
        let Some(d) = found else {
            return Err(format!("本机未发现 agent: {agent}"));
        };
        // `spawn_and_cache` intentionally reuses an existing connection for normal
        // lookups, but restart must evict and shut down that connection first.
        if let Some(old) = self.spawned.lock().remove(agent) {
            old.shutdown_and_join();
        }
        match self.spawn_and_cache(&d) {
            Ok(_) => {
                log::info!("手动重启成功：{}（agent={}）", d.bin, d.name);
                Ok(())
            }
            Err(e) => {
                self.unavailable.lock().insert(agent.to_string());
                log::error!("手动重启失败（agent={}）: {e}", d.name);
                Err(e)
            }
        }
    }
    /// 重新发现 agents（`agent.rediscover`）：重扫本机 ACP agent 并拉起未运行的。
    /// 已运行的复用现有连接不重复拉起；此前拉起失败的在此重试。
    /// no_discovery 模式下为 no-op。
    pub fn rediscover_agents(&self) -> LaunchSummary {
        self.refresh_discovery();
        self.launch_discovered()
    }

    pub fn shutdown_all(&self) {
        // Close the admission gate before detaching cached connections. A concurrent
        // spawn either observes this gate before starting or observes it while
        // publishing and reclaims the newly-created connection itself.
        self.shutting_down.store(true, Ordering::Release);

        let configured_connection = {
            let _lifecycle = self.lifecycle.lock();
            self.configured_override.lock().take().or_else(|| {
                self.configured
                    .as_ref()
                    .map(|(_, connection)| connection.clone())
            })
        };
        if let Some(connection) = configured_connection {
            connection.shutdown_and_join();
        }
        let spawned = std::mem::take(&mut *self.spawned.lock());
        for (_, d) in spawned {
            d.shutdown_and_join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_discovery_skips_auto_discovery() {
        let reg = test_registry(Vec::new(), true);
        reg.refresh_discovery();
        assert!(reg.discovered.lock().is_empty());
        // no_discovery 下仅显式配置（configured_spec）进入列表
        reg.set_configured_spec("mock_acp".into(), "mock_acp".into(), Vec::new(), Vec::new());
        reg.refresh_discovery();
        let agents = reg.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "mock_acp");
        assert!(reg.discovered.lock().is_empty());
    }
    #[cfg(test)]
    fn test_registry(discovered: Vec<DiscoveredAgent>, no_discovery: bool) -> AgentRegistry {
        AgentRegistry {
            no_discovery,
            configured: None,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(discovered),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            lifecycle: Mutex::new(()),
        }
    }
    fn sibling_bin(name: &str) -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("当前测试可执行路径");
        let dir = exe.parent().expect("可执行所在目录");
        let bin_dir = if dir.ends_with("deps") {
            dir.parent().unwrap_or(dir)
        } else {
            dir
        };
        bin_dir.join(name)
    }
    #[test]
    fn launch_discovered_spawns_and_marks_unavailable() {
        let mock = sibling_bin("mock_acp");
        assert!(mock.exists(), "mock_acp 应已构建: {}", mock.display());
        let reg = test_registry(
            vec![
                DiscoveredAgent {
                    name: "mock_acp".into(),
                    bin: mock.display().to_string(),
                    args: Vec::new(),
                    env: Vec::new(),
                },
                DiscoveredAgent {
                    name: "broken".into(),
                    bin: "/nonexistent/bin/definitely-not-here".into(),
                    args: vec!["acp".into()],
                    env: Vec::new(),
                },
            ],
            false,
        );
        let summary = reg.launch_discovered();
        assert_eq!(
            summary.started, 1,
            "mock_acp 应拉起成功（真实 spawn 路径）: {summary:?}"
        );
        assert_eq!(
            summary.failed, 1,
            "不存在的二进制应拉起失败并标记不可用: {summary:?}"
        );
        let spawned = reg.spawned.lock();
        assert_eq!(spawned.len(), 1, "spawned 缓存应恰好含注入的成功条目");
        assert!(spawned.contains_key("mock_acp"));
        assert!(!spawned.contains_key("broken"), "失败条目不应入缓存");
        drop(spawned);
        let agents = reg.list_agents();
        let mock_info = agents
            .iter()
            .find(|a| a.name == "mock_acp")
            .expect("mock_acp 在列表");
        assert_eq!(
            mock_info.status,
            protocol::AgentStatus::Available,
            "拉起成功的 agent 应 available"
        );
        let broken_info = agents
            .iter()
            .find(|a| a.name == "broken")
            .expect("broken 在列表");
        assert_eq!(
            broken_info.status,
            protocol::AgentStatus::Unavailable,
            "拉起失败的 agent 应 unavailable"
        );
        let d1 = reg
            .connection_for("mock_acp")
            .expect("已拉起连接应直接返回");
        let d2 = reg
            .connection_for("mock_acp")
            .expect("已拉起连接应直接返回");
        assert!(Arc::ptr_eq(&d1, &d2), "connection_for 应复用同一缓存连接");
        let err = match reg.connection_for("broken") {
            Err(e) => e,
            Ok(_) => panic!("不可用 agent 的 connection_for 应返回错误"),
        };
        assert!(err.contains("不可用"), "不可用 agent 的错误应明确: {err}");
        assert!(
            !reg.spawned.lock().contains_key("broken"),
            "不可用 agent 不应被再次拉起"
        );
    }

    #[test]
    fn configured_launch_failure_remains_visible_and_restartable() {
        let reg = test_registry(Vec::new(), true);
        reg.set_configured_spec(
            "broken".into(),
            "/nonexistent/bin/definitely-not-here".into(),
            Vec::new(),
            Vec::new(),
        );
        reg.mark_configured_unavailable("broken");

        let info = reg
            .list_agents()
            .into_iter()
            .find(|agent| agent.name == "broken")
            .expect("显式配置的失败 agent 应保留在列表");
        assert!(matches!(info.status, protocol::AgentStatus::Unavailable));
        let error = reg
            .restart_agent("broken")
            .expect_err("重启不存在的 agent 应失败");
        assert!(error.contains("失败"));
        assert_eq!(
            reg.list_agents()
                .into_iter()
                .find(|agent| agent.name == "broken")
                .expect("失败 agent 应仍在列表")
                .status,
            protocol::AgentStatus::Unavailable
        );
    }
    #[test]
    fn launch_discovered_skips_when_no_discovery() {
        let entry = DiscoveredAgent {
            name: "mock_acp".into(),
            bin: sibling_bin("mock_acp").display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let reg = test_registry(vec![entry], true);
        let summary = reg.launch_discovered();
        assert_eq!(
            summary.started + summary.failed,
            0,
            "AMUX_NO_DISCOVERY=1 不应拉起: {summary:?}"
        );
        assert!(reg.spawned.lock().is_empty());
    }

    #[test]
    fn rediscover_agents_noop_when_disabled() {
        let entry = DiscoveredAgent {
            name: "mock_acp".into(),
            bin: sibling_bin("mock_acp").display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        // no_discovery 受限模式下重新发现为 no-op，不拉起任何 agent。
        let reg = test_registry(vec![entry], true);
        let summary = reg.rediscover_agents();
        assert_eq!(
            summary.started + summary.failed,
            0,
            "受限模式 rediscover 不应拉起: {summary:?}"
        );
        assert!(reg.spawned.lock().is_empty());
    }
}
