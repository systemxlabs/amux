//! Agent 驱动抽象（docs/DESIGN.md §7.2/§7.3）：server 与 agent 的唯一接口。
//! 本模块提供：
//! - `AcpAgentDriver`：真实 ACP v1 对接（官方 SDK `agent-client-protocol`，
//!   `AcpAgent` stdio 传输 + typed 请求/通知，`codex-acp` / `claude-acp` / `kimi acp`）
//! - `StubAgentDriver`：内存 Stub（演示/无需 agent 的测试）
//! - `AgentRegistry`：按 agent 名解析驱动——`--agent` 配置的驱动 + PATH 自动发现的
//!   agent（启动即拉起并复用，docs/DESIGN.md §4.1/§7.3；拉起失败标记不可用；
//!   运行期新发现的惰性拉起）
//!
//! ACP v1 语义（docs/DESIGN.md §7.2）：session/new、resume、prompt、cancel、close 等
//! 方法；session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpAgentDriver 使用**专用 exec 线程**承载全部异步 IO（官方 SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub use crate::acp::{AcpAgentDriver, AgentDriver, AgentEvent, LaunchSummary, SharedDriver};
use crate::discovery::{discover_acp_agents, DiscoveredAgent};
use protocol::AgentInfo;

// ---- AgentRegistry：agent 名 → 驱动 ----

/// agent 注册表（PRD §3.3：agent 自动发现，可执行路径不手动指定）：
///
/// - `--agent` 指定的驱动（agent 名 = 可执行文件名，如 `mock_acp` / `kimi acp`）为显式覆盖
/// - 自动发现（无需 `--agent`，docs/DESIGN.md §7.3）：
///   - 已知 CLI 的 `acp` 子命令探测（如 `kimi acp`，ACP 原生）
///   - 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///   - 发现的 agent 在 server 启动时**直接拉起**（`launch_discovered`，docs/DESIGN.md
///     §4.1/§7.3：ACP server 随 server 启动一起拉起，后续 `driver_for` 复用缓存驱动）；
///     **拉起失败的 agent 标记为不可用**（agent.list 的 available=false，使用时报明确错误）；
///     运行期新发现的 agent 仍走惰性拉起兜底
/// - 生产路径不提供内置 Stub；没有发现 agent 时 `agent.list` 为空，使用未知 agent 会报错。
pub struct AgentRegistry {
    /// 仅测试使用的 Stub 驱动。
    stub: std::sync::Mutex<Option<SharedDriver>>,
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
    discovered: std::sync::Mutex<Vec<DiscoveredAgent>>,
    /// 已拉起的发现驱动（启动拉起 + 懒路径共用缓存；`driver_for` 不再二次 spawn）
    spawned: std::sync::Mutex<HashMap<String, SharedDriver>>,
    /// 启动时拉起失败的 agent（标记为不可用：agent.list 的 available=false、driver_for 报错）
    unavailable: std::sync::Mutex<HashSet<String>>,
}

impl AgentRegistry {
    /// 构建注册表（生产路径：自动发现本机 ACP agent）。
    /// - `configured`：`--agent` 显式指定的驱动，可为 None（由自动发现接管）
    pub fn new(configured: Option<(String, SharedDriver)>) -> Self {
        let no_discovery = std::env::var("AMUX_NO_DISCOVERY")
            .map(|v| v == "1")
            .unwrap_or(false);
        let registry = AgentRegistry {
            stub: std::sync::Mutex::new(None),
            force_stub: false,
            no_discovery,
            configured,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(HashSet::new()),
        };
        if !no_discovery {
            registry.refresh_discovery();
        }
        registry
    }

    /// 重新扫描本机 ACP agent（运行期安装的新 agent 经 agent.list 刷新即可发现，PRD §3.3）。
    /// 合并新发现的 agent，保留已配置/已发现条目。生产路径没有 Stub 兜底。
    fn refresh_discovery(&self) {
        if self.force_stub {
            return;
        }
        if !self.no_discovery {
            let current = discover_acp_agents();
            let mut disc = self
                .discovered
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）");
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
            stub: std::sync::Mutex::new(Some(Arc::new(StubAgentDriver::new()))),
            force_stub: true,
            no_discovery: false,
            configured: None,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// 测试构造：指定单个配置驱动并跳过运行期发现（避免 PATH 上的真实 agent 干扰单测）。
    #[cfg(test)]
    pub fn new_for_tests_with_driver(harness: &str, driver: SharedDriver) -> Self {
        AgentRegistry {
            stub: std::sync::Mutex::new(None),
            force_stub: false,
            no_discovery: true,
            configured: Some((harness.to_string(), driver)),
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// 为显式配置的 agent 保存可重启命令。生产入口在首次拉起后调用。
    pub fn set_configured_spec(
        &self,
        name: String,
        bin: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        *self
            .configured_spec
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）") = Some(DiscoveredAgent {
            name,
            bin,
            args,
            env,
        });
    }

    /// 记录显式配置的 ACP agent 启动失败。即使驱动未创建成功，也要在
    /// `agent.list` 中保留该 agent 的不可用状态，并允许后续 `agent.restart` 重试。
    pub fn mark_configured_unavailable(&self, name: &str) {
        if self
            .configured_spec
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .as_ref()
            .is_some_and(|spec| spec.name == name)
        {
            self.unavailable
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .insert(name.to_string());
        }
    }

    /// `agent.list` 的 agent 列表（名称 + 可用性）。**启动时拉起失败的 agent 标记为不可用**。
    pub fn list_agents(&self) -> Vec<AgentInfo> {
        self.refresh_discovery();
        let discovered = self
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        let unavailable = self
            .unavailable
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        let mut out: Vec<AgentInfo> = Vec::new();
        if let Some((name, _)) = &self.configured {
            out.push(AgentInfo {
                name: name.clone(),
                available: !unavailable.contains(name),
            });
        } else if let Some(spec) = self
            .configured_spec
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .as_ref()
        {
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

    /// 按 agent 名解析驱动；未知 agent 报错（agent 不可用/未发现）。
    /// 未知 agent 时先运行期刷新一次发现（新装的 agent 无需重启即可用）。
    /// 启动时已拉起的驱动直接复用缓存（不再二次 spawn）；运行期新发现或未拉起的
    /// 走共享 spawn-and-cache 惰性拉起；**启动时拉起失败的 agent（不可用）直接报错**。
    pub fn driver_for(&self, harness: &str) -> Result<SharedDriver, String> {
        if let Some(stub) = &*self.stub.lock().expect("Mutex 中毒（临界区内不应 panic）")
        {
            return Ok(stub.clone());
        }
        // 不可用标记必须优先于显式驱动缓存：重启失败后，旧驱动也不能继续被新请求使用。
        if self
            .unavailable
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .contains(harness)
        {
            return Err(format!("agent 不可用（启动时拉起失败）: {harness}"));
        }
        if let Some((name, _)) = &self.configured {
            if name == harness {
                if let Some(driver) = self
                    .configured_override
                    .lock()
                    .expect("Mutex 中毒（临界区内不应 panic）")
                    .as_ref()
                {
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
            .expect("Mutex 中毒（临界区内不应 panic）")
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
            .unwrap()
            .iter()
            .find(|d| d.name == harness)
            .cloned();
        if let Some(d) = found {
            return self.spawn_and_cache(&d);
        }
        Err(format!("本机未发现 agent: {harness}"))
    }

    /// 共享 spawn-and-cache：按 `DiscoveredAgent` 拉起 ACP server 并存入 `spawned` 缓存。
    /// **启动拉起与 `driver_for` 懒路径共用同一实现**——已拉起的驱动直接复用，不重复
    /// spawn；拉起失败返回明确错误且不写缓存（调用方决定是否标记不可用）。
    /// 拉起发生在锁外（最长可达 30s），不阻塞其他 agent 的并发解析；并发重复拉起时
    /// 保留先到者、后到者立即关闭。
    fn spawn_and_cache(&self, d: &DiscoveredAgent) -> Result<SharedDriver, String> {
        if let Some(driver) = self
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .get(&d.name)
            .cloned()
        {
            return Ok(driver);
        }
        let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
        let driver = AcpAgentDriver::spawn(&d.bin, &args, &d.env)
            .map_err(|e| format!("启动 ACP agent ({}) 失败: {e}", d.bin))?;
        let driver: SharedDriver = Arc::new(driver);
        let mut spawned = self
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
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

    /// 启动拉起（docs/DESIGN.md §4.1/§7.3）：server 启动时发现本机 agent 并**同时拉起**
    /// （并行拉起，单个最坏 30s 的握手不串行累加），结果进 `spawned` 缓存，
    /// 后续 `driver_for` 直接复用、不再二次 spawn。
    ///
    /// 单 agent 拉起失败**不致命且标记为不可用**：只记录错误并把该 harness 记入
    /// `unavailable`（agent.list 的 available=false，`driver_for` 返回明确错误、不尝试
    /// 再次拉起），server 正常启动、其余 agent 正常使用；重启 server 后重新发现与拉起。
    /// 尊重 `AMUX_NO_DISCOVERY=1` 与 stub/force_stub 模式（无发现则无需拉起）。
    pub fn launch_discovered(&self) -> LaunchSummary {
        // 受限/演示模式：无发现可拉起（防御性检查——discovered 本就应为空）
        if self.force_stub || self.no_discovery {
            return LaunchSummary::default();
        }
        if self
            .stub
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_some()
        {
            return LaunchSummary::default();
        }
        let discovered = self
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone();
        let summary = std::sync::Mutex::new(LaunchSummary::default());
        std::thread::scope(|s| {
            for d in discovered {
                let summary = &summary;
                s.spawn(move || match self.spawn_and_cache(&d) {
                    Ok(_) => {
                        summary.lock().unwrap().started += 1;
                        protocol::log::info(
                            "server.launch",
                            format!("已拉起 ACP server: {}（agent={}）", d.bin, d.name),
                        );
                    }
                    Err(e) => {
                        summary.lock().unwrap().failed += 1;
                        self.unavailable
                            .lock()
                            .expect("Mutex 中毒（临界区内不应 panic）")
                            .insert(d.name.clone());
                        protocol::log::error(
                            "server.launch",
                            format!("ACP server 拉起失败（agent={}，已标记不可用）: {e}", d.name),
                        );
                    }
                });
            }
        });
        std::sync::Mutex::into_inner(summary).expect("LaunchSummary Mutex 中毒")
    }

    /// 手动重试拉起指定 agent（`agent.restart`，docs/DESIGN.md「ACP Server 生命周期」：
    /// 用户可从应用侧重启某一 ACP Server）：移除不可用标记 → 重新发现 → 尝试拉起；
    /// 再次失败则重新标记不可用。
    pub fn restart_agent(&self, harness: &str) -> Result<(), String> {
        let configured_name = self
            .configured
            .as_ref()
            .map(|(name, _)| name == harness)
            .unwrap_or(false);
        let explicit_spec = self
            .configured_spec
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .as_ref()
            .filter(|spec| spec.name == harness)
            .cloned();
        if configured_name || explicit_spec.is_some() {
            let spec = self
                .configured_spec
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .clone()
                .ok_or_else(|| format!("显式 agent 缺少重启配置: {harness}"))?;
            let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
            let driver = match AcpAgentDriver::spawn(&spec.bin, &args, &spec.env) {
                Ok(driver) => driver,
                Err(e) => {
                    self.unavailable
                        .lock()
                        .expect("Mutex 中毒（临界区内不应 panic）")
                        .insert(harness.to_string());
                    return Err(format!("重启 ACP agent ({}) 失败: {e}", spec.bin));
                }
            };
            self.unavailable
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .remove(harness);
            if configured_name {
                let old = self
                    .configured_override
                    .lock()
                    .expect("Mutex 中毒（临界区内不应 panic）")
                    .take()
                    .unwrap_or_else(|| self.configured.as_ref().expect("配置驱动不存在").1.clone());
                old.shutdown();
                *self
                    .configured_override
                    .lock()
                    .expect("Mutex 中毒（临界区内不应 panic）") = Some(Arc::new(driver));
            } else {
                self.spawned
                    .lock()
                    .expect("Mutex 中毒（临界区内不应 panic）")
                    .insert(harness.to_string(), Arc::new(driver));
            }
            protocol::log::info(
                "server.launch",
                format!("手动重启成功：{}（agent={}）", spec.bin, harness),
            );
            return Ok(());
        }
        self.unavailable
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(harness);
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .iter()
            .find(|d| d.name == harness)
            .cloned();
        let Some(d) = found else {
            return Err(format!("本机未发现 agent: {harness}"));
        };
        match self.spawn_and_cache(&d) {
            Ok(_) => {
                protocol::log::info(
                    "server.launch",
                    format!("手动重启成功：{}（agent={}）", d.bin, d.name),
                );
                Ok(())
            }
            Err(e) => {
                self.unavailable
                    .lock()
                    .expect("Mutex 中毒（临界区内不应 panic）")
                    .insert(harness.to_string());
                protocol::log::error(
                    "server.launch",
                    format!("手动重启失败（agent={}）: {e}", d.name),
                );
                Err(e)
            }
        }
    }

    /// 关闭所有已拉起的 ACP 驱动并等待其后台线程退出（docs/DESIGN.md「ACP Server 生命周期」：
    /// Server 关闭时释放 ACP 子进程资源）。调用方需配超时看门狗，防个别 agent 挂死拖住退出。
    pub fn shutdown_all(&self) {
        if let Some((_, d)) = &self.configured {
            if let Some(override_driver) = self
                .configured_override
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .as_ref()
            {
                override_driver.shutdown_and_join();
            } else {
                d.shutdown_and_join();
            }
        }
        if let Some(stub) = &*self.stub.lock().expect("Mutex 中毒（临界区内不应 panic）")
        {
            stub.shutdown_and_join();
        }
        let spawned = self
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone();
        for (_, d) in spawned {
            d.shutdown_and_join();
        }
    }
}

// ---- 内存 Stub（仅测试：演示/无需真实 agent 的单测）----
#[cfg(test)]
mod stub {
    use super::*;
    use protocol::ContentBlock;
    use tokio::sync::mpsc;

    pub struct StubAgentDriver {
        sessions: std::sync::Mutex<Vec<String>>,
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
                sessions: std::sync::Mutex::new(Vec::new()),
                output_prefix: "模拟输出：".into(),
            }
        }
    }

    impl AgentDriver for StubAgentDriver {
        fn create_session(&self, cwd: &str) -> Result<String, String> {
            let id = format!("agent_{}", cwd.replace('/', "_"));
            self.sessions
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .push(id.clone());
            Ok(id)
        }

        fn resume_session(&self, _agent_session_id: &str, _cwd: &str) -> Result<(), String> {
            Ok(())
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
                    name: "read_file".into(),
                    title: Some("读取 src/main.rs".into()),
                    content: None,
                })
                .await
                .ok();
                tx.send(AgentEvent::OutputChunk(format!("{prefix}完成")))
                    .await
                    .ok();
                tx.send(AgentEvent::TurnEnded).await.ok();
            });
            rx
        }

        fn cancel(&self, _agent_session_id: &str) -> Result<(), String> {
            Ok(())
        }

        fn close(&self, agent_session_id: &str) -> Result<(), String> {
            self.sessions
                .lock()
                .unwrap()
                .retain(|s| s != agent_session_id);
            Ok(())
        }

        fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
            self.close(agent_session_id)
        }

        fn list_skills(&self) -> Result<Vec<String>, String> {
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

    /// `AMUX_NO_DISCOVERY=1`：跳过运行期自动发现（只保留显式配置）。
    /// 用于受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    #[test]
    fn no_discovery_skips_auto_discovery() {
        let mut reg = AgentRegistry::new_for_tests();
        reg.no_discovery = true;
        reg.force_stub = false;
        reg.refresh_discovery();
        // 该测试构造显式注入 stub；生产构造不会注入它。
        assert!(reg
            .stub
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_some());
        assert!(reg
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());

        // 有显式配置时：agents 列表只含配置的驱动，不扫描 PATH
        reg.stub = std::sync::Mutex::new(None);
        reg.configured = Some((
            "mock_acp".to_string(),
            Arc::new(StubAgentDriver::new()) as SharedDriver,
        ));
        reg.refresh_discovery();
        let agents = reg.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "mock_acp");
        assert!(reg
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());
    }

    // ---- 启动拉起（docs/DESIGN.md §4.1/§7.3：server 启动发现 agent 并直接拉起，失败标记不可用）----

    /// 测试构造：给定 discovered 条目（跳过 PATH 扫描），可控制 force_stub/no_discovery/stub。
    #[cfg(test)]
    fn test_registry(
        discovered: Vec<DiscoveredAgent>,
        force_stub: bool,
        no_discovery: bool,
        stub: Option<SharedDriver>,
    ) -> AgentRegistry {
        AgentRegistry {
            stub: std::sync::Mutex::new(stub),
            force_stub,
            no_discovery,
            configured: None,
            configured_spec: Mutex::new(None),
            configured_override: Mutex::new(None),
            discovered: std::sync::Mutex::new(discovered),
            spawned: std::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// 定位同包兄弟 bin 的可执行：测试二进制在 `target/debug/deps/` 下，
    /// 兄弟 bin（如 mock_acp）在 `target/debug/` 下（`cargo test` 会先构建全部 bin）。
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

    /// 启动拉起：mock_acp 可执行（真实拉起）进入缓存；不存在的二进制拉起失败被标记为
    /// **不可用**（agent.list 的 available=false）且不阻断其余 agent；`driver_for` 复用缓存
    /// 驱动（不重复 spawn），对不可用 agent 返回明确错误（不尝试再次拉起）。
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

        // 启动拉起：成功者入缓存、失败者标记不可用（不 panic、不阻断其余 agent）
        let summary = reg.launch_discovered();
        assert_eq!(
            summary.started, 1,
            "mock_acp 应拉起成功（真实 spawn 路径）: {summary:?}"
        );
        assert_eq!(
            summary.failed, 1,
            "不存在的二进制应拉起失败并标记不可用: {summary:?}"
        );
        let spawned = reg
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        assert_eq!(spawned.len(), 1, "spawned 缓存应恰好含注入的成功条目");
        assert!(spawned.contains_key("mock_acp"));
        assert!(!spawned.contains_key("broken"), "失败条目不应入缓存");
        drop(spawned);

        // agent.list 的 available 反映不可用状态
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

        // driver_for 复用缓存驱动（同一 Arc，不二次 spawn）
        let d1 = reg.driver_for("mock_acp").expect("已拉起驱动应直接返回");
        let d2 = reg.driver_for("mock_acp").expect("已拉起驱动应直接返回");
        assert!(Arc::ptr_eq(&d1, &d2), "driver_for 应复用同一缓存驱动");

        // 不可用 agent：driver_for 返回明确错误（不尝试再次拉起、不污染缓存）
        let err = match reg.driver_for("broken") {
            Err(e) => e,
            Ok(_) => panic!("不可用 agent 的 driver_for 应返回错误"),
        };
        assert!(err.contains("不可用"), "不可用 agent 的错误应明确: {err}");
        assert!(
            !reg.spawned
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .contains_key("broken"),
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

    /// `AMUX_NO_DISCOVERY=1` / force_stub / stub 兜底模式：不启动拉起（即使有 discovered 条目）。
    #[test]
    fn launch_discovered_skips_when_no_discovery_or_stub() {
        let mock = sibling_bin("mock_acp");
        let entry = DiscoveredAgent {
            name: "mock_acp".into(),
            bin: mock.display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };

        // AMUX_NO_DISCOVERY=1（no_discovery=true）：即使有 discovered 条目也不拉起
        let reg = test_registry(vec![entry.clone()], false, true, None);
        let summary = reg.launch_discovered();
        assert_eq!(
            summary.started + summary.failed,
            0,
            "AMUX_NO_DISCOVERY=1 不应拉起: {summary:?}"
        );
        assert!(reg
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());

        // force_stub（测试强制 stub）：跳过启动拉起
        let reg = test_registry(vec![entry.clone()], true, false, None);
        let summary = reg.launch_discovered();
        assert_eq!(summary.started + summary.failed, 0);
        assert!(reg
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());

        // stub 兜底模式（无发现 → stub 存在）：跳过启动拉起
        let reg = test_registry(
            vec![entry.clone()],
            false,
            false,
            Some(Arc::new(StubAgentDriver::new())),
        );
        let summary = reg.launch_discovered();
        assert_eq!(summary.started + summary.failed, 0);
        assert!(reg
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());
    }
}
