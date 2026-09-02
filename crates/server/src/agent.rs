//! Agent 驱动抽象：server 与 agent 的唯一接口。
//! 本模块提供：
//! - `AcpAgentDriver`：真实 ACP v1 对接（官方 SDK `agent-client-protocol`，
//!   `AcpAgent` stdio 传输 + typed 请求/通知，`grok agent`、`codex-acp` / `claude-acp` / `kimi acp`）
//! - `StubAgentDriver`：内存 Stub（演示/无需 agent 的测试）
//! - `AgentRegistry`：按 agent 名解析驱动——`--agent` 配置的驱动 + PATH 自动发现的
//!   agent（启动即拉起并复用；拉起失败标记不可用；
//!   运行期新发现的惰性拉起）
//!
//! ACP v1 语义：session/new、resume、prompt、cancel、close 等
//! 方法；session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpAgentDriver 使用**专用 exec 线程**承载全部异步 IO（官方 SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub use crate::acp::{
    AcpAgentDriver, AgentDriver, AgentEvent, AgentSessionCaps, LaunchSummary, SharedDriver,
};
use crate::discovery::{discover_acp_agents, DiscoveredAgent};
use protocol::AgentInfo;

/// agent 注册表（自动发现可执行路径，不要求手动指定）：
///
/// - `--agent` 指定的驱动（agent 名 = 可执行文件名，如 `mock_acp` / `kimi acp`）为显式覆盖
/// - 自动发现（无需 `--agent`）：
///   - 已知 CLI 的 `acp` 子命令探测（如 `kimi acp`，ACP 原生）
///   - 已知 CLI 的 `agent` 子命令探测（如 `grok agent --always-approve stdio`）
///   - 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///   - 发现的 agent 在 server 启动时**直接拉起**（`launch_discovered`，后续
///     `driver_for` 复用缓存驱动）；
///     **拉起失败的 agent 标记为不可用**（agent.list 的 available=false，使用时报明确错误）；
///     运行期新发现的 agent 仍走惰性拉起兜底
/// - 生产路径不提供内置 Stub；没有发现 agent 时 `agent.list` 为空，使用未知 agent 会报错。
pub struct AgentRegistry {
    /// 仅测试使用的 Stub 驱动。
    stub: Mutex<Option<SharedDriver>>,
    /// 测试强制 stub：跳过运行期发现（避免本机 PATH 干扰单测）
    force_stub: bool,
    /// 禁用运行期自动发现（`AMUX_NO_DISCOVERY=1`）：只使用 `--agent` 显式配置的 agent。
    /// 供受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    no_discovery: bool,
    /// 配置驱动：agent 名 + 驱动
    configured: Option<(String, SharedDriver)>,
    /// 显式配置 agent 的重启参数；驱动重启后仍复用同一注册表条目。
    configured_spec: Mutex<Option<DiscoveredAgent>>,
    /// 显式配置 agent 最近一次重启后的驱动。
    configured_override: Mutex<Option<SharedDriver>>,
    /// 自动发现的 agent（不含已配置的；可运行期刷新）
    discovered: Mutex<Vec<DiscoveredAgent>>,
    /// 已拉起的发现驱动（启动拉起 + 懒路径共用缓存；`driver_for` 不再二次 spawn）
    spawned: Mutex<HashMap<String, SharedDriver>>,
    /// 启动时拉起失败的 agent（标记为不可用：agent.list 的 available=false、driver_for 报错）
    unavailable: Mutex<HashSet<String>>,
}

impl AgentRegistry {
    /// 构建注册表（生产路径：自动发现本机 ACP agent）。
    /// - `configured`：`--agent` 显式指定的驱动，可为 None（由自动发现接管）
    pub fn new(configured: Option<(String, SharedDriver)>) -> Self {
        let no_discovery = std::env::var("AMUX_NO_DISCOVERY")
            .map(|v| v == "1")
            .unwrap_or(false);
        let registry = AgentRegistry {
            stub: Mutex::new(None),
            force_stub: false,
            no_discovery,
            configured,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(Vec::new()),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
        };
        if !no_discovery {
            registry.refresh_discovery();
        }
        registry
    }

    /// 重新扫描本机 ACP agent，供运行期安装的新 agent 刷新发现。
    /// 合并新发现的 agent，保留已配置/已发现条目。生产路径没有 Stub 兜底。
    fn refresh_discovery(&self) {
        if self.force_stub {
            return;
        }
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

    /// 测试构造：忽略本机 PATH 发现，强制 stub 演示模式（agent 任意）。
    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        AgentRegistry {
            stub: Mutex::new(Some(Arc::new(StubAgentDriver::new()))),
            force_stub: true,
            no_discovery: false,
            configured: None,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(Vec::new()),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
        }
    }
    #[cfg(test)]
    pub fn new_for_tests_with_driver(harness: &str, driver: SharedDriver) -> Self {
        AgentRegistry {
            stub: Mutex::new(None),
            force_stub: false,
            no_discovery: true,
            configured: Some((harness.to_string(), driver)),
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(Vec::new()),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
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
    pub fn list_agents(&self) -> Vec<AgentInfo> {
        self.refresh_discovery();
        let discovered = self.discovered.lock();
        let unavailable = self.unavailable.lock();
        let mut out: Vec<AgentInfo> = Vec::new();
        if let Some((name, _)) = &self.configured {
            out.push(AgentInfo {
                name: name.clone(),
                available: !unavailable.contains(name),
            });
        } else if let Some(spec) = self.configured_spec.lock().as_ref() {
            out.push(AgentInfo {
                name: spec.name.clone(),
                available: !unavailable.contains(&spec.name),
            });
        }
        for d in discovered.iter() {
            if out.iter().any(|agent| agent.name == d.name) {
                continue;
            }
            out.push(AgentInfo {
                name: d.name.clone(),
                available: !unavailable.contains(&d.name),
            });
        }
        out
    }
    pub fn driver_for(&self, harness: &str) -> Result<SharedDriver, String> {
        if let Some(stub) = &*self.stub.lock() {
            return Ok(stub.clone());
        }
        if self.unavailable.lock().contains(harness) {
            return Err(format!("agent 不可用（启动时拉起失败）: {harness}"));
        }
        if let Some((name, _)) = &self.configured {
            if name == harness {
                if let Some(driver) = self.configured_override.lock().as_ref() {
                    return Ok(driver.clone());
                }
                return Ok(self
                    .configured
                    .as_ref()
                    .expect("配置驱动刚刚存在")
                    .1
                    .clone());
            }
        }
        if let Some(spec) = self
            .configured_spec
            .lock()
            .as_ref()
            .filter(|spec| spec.name == harness)
            .cloned()
        {
            return self.spawn_and_cache(&spec);
        }
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .iter()
            .find(|d| d.name == harness)
            .cloned();
        if let Some(d) = found {
            return self.spawn_and_cache(&d);
        }
        Err(format!("本机未发现 agent: {harness}"))
    }
    fn spawn_and_cache(&self, d: &DiscoveredAgent) -> Result<SharedDriver, String> {
        if let Some(driver) = self.spawned.lock().get(&d.name).cloned() {
            return Ok(driver);
        }
        let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
        let driver = AcpAgentDriver::spawn(&d.bin, &args, &d.env)
            .map_err(|e| format!("启动 ACP agent ({}) 失败: {e}", d.bin))?;
        let driver: SharedDriver = Arc::new(driver);
        let mut spawned = self.spawned.lock();
        match spawned.get(&d.name) {
            Some(existing) => {
                let existing = existing.clone();
                drop(spawned);
                driver.shutdown();
                Ok(existing)
            }
            None => {
                spawned.insert(d.name.clone(), driver.clone());
                Ok(driver)
            }
        }
    }
    pub fn launch_discovered(&self) -> LaunchSummary {
        if self.force_stub || self.no_discovery {
            return LaunchSummary::default();
        }
        if self.stub.lock().is_some() {
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
    pub fn restart_agent(&self, harness: &str) -> Result<(), String> {
        let configured_name = self
            .configured
            .as_ref()
            .map(|(name, _)| name == harness)
            .unwrap_or(false);
        let explicit_spec = self
            .configured_spec
            .lock()
            .as_ref()
            .filter(|spec| spec.name == harness)
            .cloned();
        if configured_name || explicit_spec.is_some() {
            let spec = self
                .configured_spec
                .lock()
                .clone()
                .ok_or_else(|| format!("显式 agent 缺少重启配置: {harness}"))?;
            let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
            let driver = match AcpAgentDriver::spawn(&spec.bin, &args, &spec.env) {
                Ok(driver) => driver,
                Err(e) => {
                    self.unavailable.lock().insert(harness.to_string());
                    return Err(format!("重启 ACP agent ({}) 失败: {e}", spec.bin));
                }
            };
            self.unavailable.lock().remove(harness);
            if configured_name {
                let old =
                    self.configured_override.lock().take().unwrap_or_else(|| {
                        self.configured.as_ref().expect("配置驱动不存在").1.clone()
                    });
                old.shutdown();
                *self.configured_override.lock() = Some(Arc::new(driver));
            } else {
                self.spawned
                    .lock()
                    .insert(harness.to_string(), Arc::new(driver));
            }
            log::info!("手动重启成功：{}（agent={}）", spec.bin, harness);
            return Ok(());
        }
        self.unavailable.lock().remove(harness);
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .iter()
            .find(|d| d.name == harness)
            .cloned();
        let Some(d) = found else {
            return Err(format!("本机未发现 agent: {harness}"));
        };
        // `spawn_and_cache` intentionally reuses an existing driver for normal
        // lookups, but restart must evict and shut down that driver first.
        if let Some(old) = self.spawned.lock().remove(harness) {
            old.shutdown_and_join();
        }
        match self.spawn_and_cache(&d) {
            Ok(_) => {
                log::info!("手动重启成功：{}（agent={}）", d.bin, d.name);
                Ok(())
            }
            Err(e) => {
                self.unavailable.lock().insert(harness.to_string());
                log::error!("手动重启失败（agent={}）: {e}", d.name);
                Err(e)
            }
        }
    }
    /// 重新发现 agents（`agent.rediscover`）：重扫本机 ACP agent 并拉起未运行的。
    /// 已运行的复用现有驱动不重复拉起；此前拉起失败的在此重试。stub/no_discovery
    /// 模式下为 no-op。
    pub fn rediscover_agents(&self) -> LaunchSummary {
        self.refresh_discovery();
        self.launch_discovered()
    }

    pub fn shutdown_all(&self) {
        if let Some((_, d)) = &self.configured {
            if let Some(override_driver) = self.configured_override.lock().as_ref() {
                override_driver.shutdown_and_join();
            } else {
                d.shutdown_and_join();
            }
        }
        if let Some(stub) = &*self.stub.lock() {
            stub.shutdown_and_join();
        }
        let spawned = self.spawned.lock().clone();
        for (_, d) in spawned {
            d.shutdown_and_join();
        }
    }
}
#[cfg(test)]
mod stub {
    use super::*;
    use protocol::ContentBlock;
    use tokio::sync::mpsc;

    pub struct StubAgentDriver {
        sessions: Mutex<Vec<String>>,
        pub output_prefix: String,
    }

    impl StubAgentDriver {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl Default for StubAgentDriver {
        fn default() -> Self {
            StubAgentDriver {
                sessions: Mutex::new(Vec::new()),
                output_prefix: "模拟输出：".into(),
            }
        }
    }

    impl AgentDriver for StubAgentDriver {
        fn create_session(
            &self,
            cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            let id = format!("agent_{}", cwd.replace('/', "_"));
            self.sessions.lock().push(id.clone());
            Ok((id, Vec::new()))
        }

        fn resume_session(
            &self,
            _agent_session_id: &str,
            _cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn prompt(
            &self,
            _agent_session_id: &str,
            _input: Vec<ContentBlock>,
        ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
            let (tx, rx) = mpsc::channel(16);
            let prefix = self.output_prefix.clone();
            tokio::spawn(async move {
                tx.send(AgentEvent::Thinking("正在分析问题…".into()))
                    .await
                    .ok();
                tx.send(AgentEvent::ToolCall {
                    id: "tc1".into(),
                    name: Some("read_file".into()),
                    title: Some("读取 src/main.rs".into()),
                    content: None,
                })
                .await
                .ok();
                tx.send(AgentEvent::OutputChunk(format!("{prefix}完成")))
                    .await
                    .ok();
                tx.send(AgentEvent::TurnEnded(
                    protocol::StateChangeReason::Completed,
                ))
                .await
                .ok();
            });
            rx
        }

        fn cancel(&self, _agent_session_id: &str) -> Result<(), String> {
            Ok(())
        }

        fn close(&self, agent_session_id: &str) -> Result<(), String> {
            self.sessions.lock().retain(|s| s != agent_session_id);
            Ok(())
        }

        fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
            self.close(agent_session_id)
        }

        fn set_config_option(
            &self,
            _agent_session_id: &str,
            _config_id: &str,
            _value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn shutdown(&self) {}
    }
}

#[cfg(test)]
pub use stub::StubAgentDriver;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_discovery_skips_auto_discovery() {
        let mut reg = AgentRegistry::new_for_tests();
        reg.no_discovery = true;
        reg.force_stub = false;
        reg.refresh_discovery();
        assert!(reg.stub.lock().is_some());
        assert!(reg.discovered.lock().is_empty());
        reg.stub = Mutex::new(None);
        reg.configured = Some((
            "mock_acp".to_string(),
            Arc::new(StubAgentDriver::new()) as SharedDriver,
        ));
        reg.refresh_discovery();
        let agents = reg.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "mock_acp");
        assert!(reg.discovered.lock().is_empty());
    }
    #[cfg(test)]
    fn test_registry(
        discovered: Vec<DiscoveredAgent>,
        force_stub: bool,
        no_discovery: bool,
        stub: Option<SharedDriver>,
    ) -> AgentRegistry {
        AgentRegistry {
            stub: Mutex::new(stub),
            force_stub,
            no_discovery,
            configured: None,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: Mutex::new(discovered),
            spawned: Mutex::new(HashMap::new()),
            unavailable: Mutex::new(HashSet::new()),
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
            false,
            None,
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
        assert!(mock_info.available, "拉起成功的 agent 应 available=true");
        let broken_info = agents
            .iter()
            .find(|a| a.name == "broken")
            .expect("broken 在列表");
        assert!(
            !broken_info.available,
            "拉起失败的 agent 应 available=false"
        );
        let d1 = reg.driver_for("mock_acp").expect("已拉起驱动应直接返回");
        let d2 = reg.driver_for("mock_acp").expect("已拉起驱动应直接返回");
        assert!(Arc::ptr_eq(&d1, &d2), "driver_for 应复用同一缓存驱动");
        let err = match reg.driver_for("broken") {
            Err(e) => e,
            Ok(_) => panic!("不可用 agent 的 driver_for 应返回错误"),
        };
        assert!(err.contains("不可用"), "不可用 agent 的错误应明确: {err}");
        assert!(
            !reg.spawned.lock().contains_key("broken"),
            "不可用 agent 不应被再次拉起"
        );
    }

    #[test]
    fn configured_launch_failure_remains_visible_and_restartable() {
        let reg = test_registry(Vec::new(), false, true, None);
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
        assert!(!info.available);
        let error = reg
            .restart_agent("broken")
            .expect_err("重启不存在的 agent 应失败");
        assert!(error.contains("失败"));
        assert!(
            !reg.list_agents()
                .into_iter()
                .find(|agent| agent.name == "broken")
                .expect("失败 agent 应仍在列表")
                .available
        );
    }
    #[test]
    fn launch_discovered_skips_when_no_discovery_or_stub() {
        let mock = sibling_bin("mock_acp");
        let entry = DiscoveredAgent {
            name: "mock_acp".into(),
            bin: mock.display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let reg = test_registry(vec![entry.clone()], false, true, None);
        let summary = reg.launch_discovered();
        assert_eq!(
            summary.started + summary.failed,
            0,
            "AMUX_NO_DISCOVERY=1 不应拉起: {summary:?}"
        );
        assert!(reg.spawned.lock().is_empty());
        let reg = test_registry(vec![entry.clone()], true, false, None);
        let summary = reg.launch_discovered();
        assert_eq!(summary.started + summary.failed, 0);
        assert!(reg.spawned.lock().is_empty());
        let reg = test_registry(
            vec![entry.clone()],
            false,
            false,
            Some(Arc::new(StubAgentDriver::new())),
        );
        let summary = reg.launch_discovered();
        assert_eq!(summary.started + summary.failed, 0);
        assert!(reg.spawned.lock().is_empty());
    }

    #[test]
    fn rediscover_agents_noop_when_disabled_or_stub() {
        let entry = DiscoveredAgent {
            name: "mock_acp".into(),
            bin: sibling_bin("mock_acp").display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        // no_discovery / force_stub / stub 三种受限模式均为 no-op：
        // 重新发现不会拉起任何 agent（docs/DESIGN.md `agent.rediscover`）
        for reg in [
            test_registry(vec![entry.clone()], false, true, None),
            test_registry(vec![entry.clone()], true, false, None),
            test_registry(
                vec![entry],
                false,
                false,
                Some(Arc::new(StubAgentDriver::new())),
            ),
        ] {
            let summary = reg.rediscover_agents();
            assert_eq!(
                summary.started + summary.failed,
                0,
                "受限模式 rediscover 不应拉起: {summary:?}"
            );
            assert!(reg.spawned.lock().is_empty());
        }
    }
}
