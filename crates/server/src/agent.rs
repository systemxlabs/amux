//! Agent 驱动抽象（docs/DESIGN.md §7.2/§7.3）：server 与 agent 的唯一接口。
//! 本模块提供：
//! - `AcpAgentDriver`：真实 ACP v1 对接（官方 SDK `agent-client-protocol`，
//!   `AcpAgent` stdio 传输 + typed 请求/通知，`codex-acp` / `claude-acp` / `kimi acp`）
//! - `StubAgentDriver`：内存 Stub（演示/无需 agent 的测试）
//! - `AgentRegistry`：按 agent 名解析驱动——`--agent` 配置的驱动 + PATH 自动发现的
//!   agent（启动即拉起并复用，docs/DESIGN.md §4.1/§7.3；拉起失败标记不可用；
//!   运行期新发现的兜底惰性拉起）
//!
//! ACP v1 语义（docs/DESIGN.md §7.2）：session/new、resume、prompt、cancel、close 等
//! 方法；session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpAgentDriver 使用**专用 exec 线程**承载全部异步 IO（官方 SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    BlobResourceContents, CancelNotification, CloseSessionRequest, ContentBlock as AcpContentBlock,
    EmbeddedResource, EmbeddedResourceResource, InitializeRequest, NewSessionRequest,
    PermissionOption, PermissionOptionId, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent, TextResourceContents, ToolKind,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, ConnectionTo, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use protocol::{AgentInfo, ContentBlock, SessionState};

/// turn 过程中的 agent 事件（docs/DESIGN.md §5.1：server 透传，GUI 应用聚合）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 输出的增量片段
    OutputChunk(String),
    /// 用户消息回显（load 重放用）
    UserMessage(String),
    /// 思考片段
    Thinking(String),
    /// 工具调用
    ToolCall {
        name: String,
        title: Option<String>,
        content: Option<String>,
    },
    /// 上下文压缩
    #[allow(dead_code)]
    Compaction(String),
    /// agent 自报状态（ACP `session_info_update` 透传；ACP 未携带状态时为 None）
    SessionInfo { state: Option<SessionState> },
    /// turn 完成
    TurnEnded,
}

/// 拉起的统计（server 启动日志用；docs/DESIGN.md §4.1/§7.3）。
#[derive(Debug, Default, Clone, Copy)]
pub struct LaunchSummary {
    /// 成功拉起的 ACP server 数
    pub started: usize,
    /// 拉起失败的 agent 数（标记为**不可用**，agent.list 的 available=false）
    pub failed: usize,
}

/// 与单个 agent 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id
    fn create_session(&self, cwd: &str) -> Result<String, String>;
    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史；
    /// 同一进程内对同一会话幂等——已恢复过则直接成功）
    fn resume_session(&self, agent_session_id: &str, cwd: &str) -> Result<(), String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 关闭会话（删除/长时间无活动时释放 agent 侧资源，docs/DESIGN.md「ACP 生命周期」：
    /// server 经 ACP `session/close` 关闭 agent 侧会话）
    fn close(&self, agent_session_id: &str) -> Result<(), String>;
    /// 该 agent 安装的 skills 列表（agent 不支持时返回空列表）
    fn list_skills(&self) -> Vec<String>;
    /// 关闭驱动自身（server 退出时释放 ACP 子进程资源，docs/DESIGN.md「ACP Server 生命周期」）
    fn shutdown(&self);
}

#[allow(dead_code)]
pub type SharedDriver = Arc<dyn AgentDriver>;

// ---- AgentRegistry：agent 名 → 驱动 ----

/// 自动发现的 ACP agent（含 ACP 子命令参数 / npx 包装器参数与附加环境变量）。
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub bin: String,
    pub args: Vec<String>,
    /// 附加环境变量（如 codex 包装器的 INITIAL_AGENT_MODE）
    pub env: Vec<(String, String)>,
}

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
/// - 演示兜底：既无 `--agent` 又无任何发现时，用内存 Stub（接受任意 agent 名）
pub struct AgentRegistry {
    /// 演示模式：任意 agent 名都解析到同一个 Stub 驱动（内部可变，随发现刷新）
    stub: std::sync::Mutex<Option<SharedDriver>>,
    /// 测试强制 stub：跳过运行期发现（避免本机 PATH 干扰单测）
    force_stub: bool,
    /// 禁用运行期自动发现（`AMUX_NO_DISCOVERY=1`）：只使用 `--agent` 显式配置的 agent。
    /// 供受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    no_discovery: bool,
    /// 配置驱动：agent 名 + 驱动
    configured: Option<(String, SharedDriver)>,
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
    /// 合并新发现的 agent，保留已配置/已发现条目；无任何 agent 且无配置时启用 stub 兜底。
    /// `AMUX_NO_DISCOVERY=1` 时跳过扫描（仅 stub 兜底逻辑仍生效）。
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
        let need_stub = self.configured.is_none() && {
            let disc = self
                .discovered
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）");
            disc.is_empty()
        };
        *self.stub.lock().expect("Mutex 中毒（临界区内不应 panic）") = if need_stub {
            Some(Arc::new(StubAgentDriver::new()))
        } else {
            None
        };
    }

    /// 测试构造：忽略本机 PATH 发现，强制 stub 演示模式（agent 任意）。
    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        AgentRegistry {
            stub: std::sync::Mutex::new(Some(Arc::new(StubAgentDriver::new()))),
            force_stub: true,
            no_discovery: false,
            configured: None,
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
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(HashSet::new()),
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
                available: true,
            });
        } else if self
            .stub
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_some()
        {
            out.push(AgentInfo {
                name: "stub".into(),
                available: true,
            });
        }
        for d in discovered.iter() {
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
        if let Some((name, d)) = &self.configured {
            if name == harness {
                return Ok(d.clone());
            }
        }
        // 启动时拉起失败 = 不可用：直接返回明确错误，不尝试再次拉起
        if self
            .unavailable
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .contains(harness)
        {
            return Err(format!("agent 不可用（启动时拉起失败）: {harness}"));
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
    fn spawn_and_cache(&self, d: &DiscoveredAgent) -> Result<SharedDriver, String> {
        let mut spawned = self
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        if let Some(driver) = spawned.get(&d.name) {
            return Ok(driver.clone());
        }
        let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
        let env = d.env.clone();
        let driver = AcpAgentDriver::spawn(&d.bin, &args, &env)
            .map_err(|e| format!("启动 ACP agent ({}) 失败: {e}", d.bin))?;
        let driver: SharedDriver = Arc::new(driver);
        spawned.insert(d.name.clone(), driver.clone());
        Ok(driver)
    }

    /// 启动拉起（docs/DESIGN.md §4.1/§7.3）：server 启动时发现本机 agent 并**直接拉起**
    /// （kimi 走原生 `kimi acp`，claude/codex 走 npx 包装器），结果进 `spawned` 缓存，
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
        let mut summary = LaunchSummary::default();
        for d in discovered {
            match self.spawn_and_cache(&d) {
                Ok(_) => {
                    summary.started += 1;
                    protocol::log::info(
                        "server.launch",
                        format!("已拉起 ACP server: {}（agent={}）", d.bin, d.name),
                    );
                }
                Err(e) => {
                    summary.failed += 1;
                    self.unavailable
                        .lock()
                        .expect("Mutex 中毒（临界区内不应 panic）")
                        .insert(d.name.clone());
                    protocol::log::error(
                        "server.launch",
                        format!("ACP server 拉起失败（agent={}，已标记不可用）: {e}", d.name),
                    );
                }
            }
        }
        summary
    }

    /// 手动重试拉起指定 agent（`agent.restart`，docs/DESIGN.md「ACP Server 生命周期」：
    /// 用户可从应用侧重启某一 ACP Server）：移除不可用标记 → 重新发现 → 尝试拉起；
    /// 再次失败则重新标记不可用。
    pub fn restart_agent(&self, harness: &str) -> Result<(), String> {
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

    /// 关闭所有已拉起的 ACP 驱动（docs/DESIGN.md「ACP Server 生命周期」：
    /// Server 关闭时释放 ACP 子进程资源）。
    pub fn shutdown_all(&self) {
        if let Some((_, d)) = &self.configured {
            d.shutdown();
        }
        if let Some(stub) = &*self.stub.lock().expect("Mutex 中毒（临界区内不应 panic）")
        {
            stub.shutdown();
        }
        let spawned = self
            .spawned
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone();
        for (_, d) in spawned {
            d.shutdown();
        }
    }
}

/// 自动发现 ACP agent（PRD §3.3：可执行路径自动发现、不手动指定；docs/DESIGN.md §7.3）：
/// 1) 已知 CLI 的 `acp` 子命令探测（ACP 原生，如 `kimi acp`）
/// 2) 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///
/// 不做任意 `*-acp` 扫描：只认已知 agent，避免无关可执行污染列表。
/// 发现的 agent 由 `launch_discovered` 在 server 启动时拉起（docs/DESIGN.md §4.1/§7.3）。
fn discover_acp_agents() -> Vec<DiscoveredAgent> {
    let mut found: Vec<DiscoveredAgent> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let npx = find_on_path("npx");

    // 已知 CLI：优先 ACP 原生（`acp` 子命令），否则回落 npx 官方包装器
    for (cli, pkg) in [
        ("kimi", None),
        ("claude", Some("@agentclientprotocol/claude-agent-acp")),
        ("codex", Some("@agentclientprotocol/codex-acp")),
    ] {
        if !seen.insert(cli.to_string()) {
            continue;
        }
        let cli_bin = find_on_path(cli);
        let acp_supported = cli_bin.as_deref().map(has_acp_subcommand).unwrap_or(false);
        if let Some(d) = discover_for_cli(cli, pkg, cli_bin, acp_supported, npx.clone()) {
            found.push(d);
        }
    }
    found
}

/// 单 CLI 的发现决策（纯逻辑，便于单测；docs/DESIGN.md §7.3）：
/// 已知 CLI 优先 ACP 原生（`acp` 子命令），否则回落 npx 官方包装器。
fn discover_for_cli(
    cli: &str,
    pkg: Option<&str>,
    cli_bin: Option<String>,
    acp_supported: bool,
    npx_bin: Option<String>,
) -> Option<DiscoveredAgent> {
    let cli_bin = cli_bin?;
    if acp_supported {
        return Some(DiscoveredAgent {
            name: cli.to_string(),
            bin: cli_bin,
            args: vec!["acp".to_string()],
            env: Vec::new(),
        });
    }
    let pkg = pkg?;
    let npx = npx_bin?;
    let mut env = Vec::new();
    if cli == "codex" {
        // 全权限自主模式（docs/DESIGN.md §7.3）
        env.push(("INITIAL_AGENT_MODE".into(), "agent-full-access".into()));
    }
    Some(DiscoveredAgent {
        name: cli.to_string(),
        bin: npx,
        args: vec!["-y".to_string(), pkg.to_string()],
        env,
    })
}

/// 在 PATH 上查找可执行文件（含 `.exe` 后缀剥离）。
fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        for candidate in [name, &format!("{name}.exe")] {
            let p = std::path::Path::new(dir).join(candidate);
            if p.is_file() && is_executable(&p) {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

/// 探测 `<bin> acp --help`：退出码 0 且输出提及 acp（区分"有 acp 子命令"与
/// "未知子命令回落通用帮助"——codex 0.137 退出 0 但输出不含 acp，故被排除）。
fn has_acp_subcommand(bin: &str) -> bool {
    use std::process::Command;
    let Ok(out) = Command::new(bin).args(["acp", "--help"]).output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    text.contains("acp")
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &std::path::Path) -> bool {
    true
}

// ---- 真实 ACP v1 stdio 对接（官方 SDK agent-client-protocol）----

/// 主线程 → exec 线程的方法请求。
enum ExecReq {
    Call {
        method: String,
        params: Value,
        resp: std::sync::mpsc::SyncSender<Result<Value, String>>,
    },
    Prompt {
        sid: String,
        prompt: Vec<ContentBlock>,
        routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    },
}

/// ACP v1 客户端（官方 SDK stdio 传输，docs/DESIGN.md §9）。
pub struct AcpAgentDriver {
    /// 主线程 → exec 线程的请求发送端；shutdown 时置 None 以优雅结束 exec 线程
    exec_tx: Mutex<Option<std::sync::mpsc::SyncSender<ExecReq>>>,
    /// 会话事件路由：agent sessionId -> prompt 的事件接收端
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    /// create 时记录的会话 cwd（resume 需要）
    cwds: Arc<Mutex<HashMap<String, String>>>,
    /// 本进程内已 resume 过的会话（server 重启后从注册表恢复的会话首次交互前
    /// 经 ACP `session/resume` 恢复 agent 上下文，docs/DESIGN.md §7.2）
    resumed: Arc<Mutex<HashSet<String>>>,
    /// exec 线程句柄（连接由 SDK 管理，线程结束即子进程清理）
    _thread: std::thread::JoinHandle<()>,
}

impl AcpAgentDriver {
    /// 启动 ACP agent 子进程（官方 SDK `AcpAgent` 管理 stdio 传输与进程生命周期）；
    /// `env` 为附加环境变量（经 from_args 的 `NAME=value` 前缀传入）；
    /// exec 线程承载全部异步 IO。
    ///
    /// **同步就绪握手**：阻塞等待 exec 线程完成「子进程拉起 + 连接建立 + initialize
    /// 握手」后才返回——二进制缺失 / 进程立即退出（如 npx 不可用、无网络）在此快速
    /// 失败并返回明确错误；健康 agent 在握手完成后立即返回。等待受
    /// `AMUX_ACP_SPAWN_TIMEOUT_MS` 限制（默认 30s；npx 首次按需下载可能较慢，
    /// 超时按失败处理，`driver_for` 兜底会重试）。
    pub fn spawn(bin: &str, args: &[&str], env: &[(String, String)]) -> Result<Self, String> {
        let (exec_tx, exec_rx) = std::sync::mpsc::sync_channel::<ExecReq>(32);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let routes2 = routes.clone();
        let bin = bin.to_string();
        let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let env = env.to_vec();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("构建 tokio runtime 失败");
            rt.block_on(exec_main(&bin, &args, &env, exec_rx, routes2, ready_tx));
        });
        let timeout_ms = std::env::var("AMUX_ACP_SPAWN_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000);
        match ready_rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(format!(
                    "ACP server 启动超时（{timeout_ms}ms 内未完成连接/initialize 握手）"
                ))
            }
        }
        Ok(AcpAgentDriver {
            exec_tx: Mutex::new(Some(exec_tx)),
            routes,
            cwds: Arc::new(Mutex::new(HashMap::new())),
            resumed: Arc::new(Mutex::new(HashSet::new())),
            _thread: thread,
        })
    }

    fn sender(&self) -> Result<std::sync::mpsc::SyncSender<ExecReq>, String> {
        self.exec_tx
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone()
            .ok_or_else(|| "agent 已关闭".to_string())
    }

    /// 同步方法调用：请求发往 exec 线程，阻塞等待响应。
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Value, String>>(1);
        self.sender()?
            .send(ExecReq::Call {
                method: method.to_string(),
                params,
                resp: tx,
            })
            .map_err(|_| "agent 已关闭".to_string())?;
        rx.recv().map_err(|_| "ACP 调用执行失败".to_string())?
    }
}

impl AgentDriver for AcpAgentDriver {
    fn create_session(&self, cwd: &str) -> Result<String, String> {
        let res = self.call("session/new", json!({ "cwd": cwd, "mcpServers": [] }))?;
        let sid = res
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "session/new 未返回 sessionId".to_string())?
            .to_string();
        self.cwds
            .lock()
            .unwrap()
            .insert(sid.clone(), cwd.to_string());
        // 新会话 agent 已在内存中持有，无需 resume
        self.resumed.lock().unwrap().insert(sid.clone());
        Ok(sid)
    }

    /// 恢复 agent 自身上下文（ACP `session/resume`，不向客户端重放历史——
    /// 历史以 server 本地日志为权威，docs/DESIGN.md §7.2/§5.2）。
    /// 同一进程内对同一会话幂等（已恢复过则直接成功）。
    fn resume_session(&self, agent_session_id: &str, cwd: &str) -> Result<(), String> {
        {
            let mut resumed = self.resumed.lock().unwrap();
            if resumed.contains(agent_session_id) {
                return Ok(());
            }
            // 提前插入：并发 prompt 场景只发起一次 resume
            resumed.insert(agent_session_id.to_string());
        }
        self.cwds
            .lock()
            .unwrap()
            .insert(agent_session_id.to_string(), cwd.to_string());
        self.call(
            "session/resume",
            json!({ "sessionId": agent_session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .map(|_| ())
    }

    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);
        self.routes
            .lock()
            .unwrap()
            .insert(agent_session_id.to_string(), tx.clone());
        let req = ExecReq::Prompt {
            sid: agent_session_id.to_string(),
            prompt: input,
            routes: self.routes.clone(),
        };
        if let Ok(sender) = self.sender() {
            let _ = sender.send(req);
        }
        rx
    }

    fn shutdown(&self) {
        let _ = self.exec_tx.lock().unwrap().take();
    }

    fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.call("session/cancel", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn close(&self, agent_session_id: &str) -> Result<(), String> {
        self.cwds
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(agent_session_id);
        self.resumed
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(agent_session_id);
        self.call("session/close", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    /// 经 ACP `skill/list` 查询该 agent 安装的 skills（agent 不支持时返回空列表）。
    fn list_skills(&self) -> Vec<String> {
        match self.call("skill/list", json!({})) {
            Ok(res) => res
                .get("skills")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("name").and_then(|v| v.as_str()).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
}

/// ACP `skill/list` 响应（SDK schema v1 未收录；typed 化，避免 Value 松散承载）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
struct SkillListResponse {
    skills: Vec<SkillInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillInfo {
    name: String,
}

/// 自定义请求：ACP `skill/list`（PRD §3.3；SDK schema v1 未收录该方法）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "skill/list", response = SkillListResponse)]
struct SkillListRequest {}

/// yolo 权限批准：从请求选项中选出要批准的选项（纯函数，可单测）。
/// 优先 `AllowAlways` > `AllowOnce` > 任意非拒绝选项；
/// 全为拒绝选项或列表为空 → `None`（无批准项，按取消处理）。
fn pick_approve_option(options: &[PermissionOption]) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
        })
        .or_else(|| {
            options.iter().find(|o| {
                !matches!(
                    o.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                )
            })
        })
        .map(|o| o.option_id.clone())
}

/// exec 线程主循环：经官方 SDK 建立 ACP 连接，承载方法分发、通知路由与权限批准。
/// `ready_tx`：就绪握手——连接建立（子进程拉起）且 initialize 握手完成（成功或
/// 协议级失败）后发送 `Ok`；若连接在建立前就失败（二进制缺失 / 进程立即退出），
/// 在 connect_with 结束后补发 `Err`，供 `AcpAgentDriver::spawn` 同步失败。
async fn exec_main(
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    exec_rx: std::sync::mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // std exec_rx → tokio 通道（阻塞转发，供 select 使用）
    let (req_tx, mut req_rx) = mpsc::channel::<ExecReq>(32);
    tokio::task::spawn_blocking(move || {
        while let Ok(req) = exec_rx.recv() {
            if req_tx.blocking_send(req).is_err() {
                break;
            }
        }
    });

    // SDK from_args 支持 `NAME=value` 前缀参数作为环境变量（docs/DESIGN.md §7.3）
    let mut cmd: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    cmd.push(bin.to_string());
    cmd.extend(args.iter().cloned());
    let agent = match AcpAgent::from_args(cmd) {
        Ok(a) => a,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("解析 agent 命令失败 ({bin}): {e}")));
            protocol::log::error("acp", format!("解析 agent 命令失败 ({bin}): {e}"));
            return;
        }
    };
    protocol::log::info("acp", format!("已连接 ACP agent: {bin} {}", args.join(" ")));
    // trace 级：ACP 线上原始帧（GUI ↔ server ↔ ACP client ↔ agent 全链路，docs/DESIGN.md §8）
    let agent = if protocol::log::enabled(protocol::Level::Trace) {
        agent.with_debug(|line, direction| {
            protocol::log::trace("acp.wire", format!("{direction:?} {line}"));
        })
    } else {
        agent
    };

    // 就绪信号：main_fn 启动（子进程已拉起、连接已建立）后完成 initialize 握手即报告
    // Ok——协议级失败（agent 存活但不实现 initialize）不致命、仍视为拉起成功；若握手失败
    // 且连接已关闭（进程立即退出），在 connect_with 结束后补报为 spawn 失败。
    let result = connect_main(agent, &mut req_rx, routes, &ready_tx).await;

    // 若 main_fn 从未报告就绪（连接建立前传输层失败：二进制缺失 / 进程立即退出 /
    // npx 不可用 / 无网络），把 connect_with 的结果补报为 spawn 失败；若已报过就绪，
    // 之后的连接异常仅记录，不影响已缓存的驱动。
    if let core::result::Result::Err(e) = &result {
        protocol::log::error("acp", format!("ACP 连接异常结束: {e}"));
    }
}

async fn connect_main(
    agent: AcpAgent,
    req_rx: &mut mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> agent_client_protocol::Result<()> {
    agent_client_protocol::Client
        .builder()
        .name("amux-server")
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                route_update(&routes, &notif);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                // yolo：自动批准（docs/DESIGN.md §7.2，无审批往返）。
                // 必须选 allow 类选项：claude-acp 等包装器的选项列表**第一项往往是
                // 「Deny/reject」**，选第一个会被 agent 误判为用户拒绝
                // （"User refused permission to run tool"）。
                if let Some(id) = pick_approve_option(&request.options) {
                    let _ = responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ));
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            agent,
            |cx: ConnectionTo<agent_client_protocol::Agent>| async move {
                let req_rx = req_rx;
                // 初始化握手（版本协商）。失败需区分两种情形：
                // - **协议级失败**（agent 存活但不实现 initialize，如返回 method not
                //   found）：仅记录、连接保持可用，视为拉起成功；
                // - **传输层失败**（进程已退出 / 连接已死，如 npx 不可用、无网络）：拉起失败。
                // 二者用短窗口探测连接活性区分：incoming_closed 在传输层关闭后很快完成，
                // 超时则连接仍存活。
                let init_result = match cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await
                {
                    core::result::Result::Ok(_) => {
                        protocol::log::debug("acp", "initialize 完成");
                        core::result::Result::Ok(())
                    }
                    core::result::Result::Err(e) => {
                        let alive = tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            cx.incoming_closed(),
                        )
                        .await
                        .is_err();
                        if alive {
                            protocol::log::error("acp", format!("initialize 失败（继续）: {e}"));
                            core::result::Result::Ok(())
                        } else {
                            core::result::Result::Err(format!(
                                "initialize 握手失败（连接已关闭）: {e}"
                            ))
                        }
                    }
                };
                let _ = ready_tx.send(init_result);

                // 服务循环：每个请求独立 spawn，支持并发（cancel 不必等 prompt 完成）
                loop {
                    let Some(req) = req_rx.recv().await else {
                        break;
                    };
                    match req {
                        ExecReq::Call {
                            method,
                            params,
                            resp,
                        } => {
                            let cx = cx.clone();
                            tokio::spawn(async move {
                                let result = dispatch_call(&cx, &method, &params).await;
                                let _ = resp.send(result);
                            });
                        }
                        ExecReq::Prompt {
                            sid,
                            prompt,
                            routes,
                        } => {
                            let cx = cx.clone();
                            tokio::spawn(async move {
                                let blocks = prompt
                                    .iter()
                                    .filter_map(acp_content_block)
                                    .collect::<Vec<_>>();
                                let _ = cx
                                    .send_request(PromptRequest::new(sid.clone(), blocks))
                                    .on_receiving_result(async move |result| {
                                        // turn 完成：移除路由并发送 TurnEnded（在最后一批通知之后）
                                        if let Some(tx) = routes
                                            .lock()
                                            .expect("Mutex 中毒（临界区内不应 panic）")
                                            .remove(&sid)
                                        {
                                            let _ = tx.try_send(AgentEvent::TurnEnded);
                                        }
                                        if let core::result::Result::Err(e) = result {
                                            protocol::log::error(
                                                "acp",
                                                format!("prompt 失败 {sid}: {e}"),
                                            );
                                        }
                                        core::result::Result::Ok(())
                                    });
                            });
                        }
                    }
                }
                core::result::Result::Ok(())
            },
        )
        .await
}

/// 按方法名分发 ACP v1 方法调用（typed 请求，经官方 SDK 传输）。
async fn dispatch_call(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let sid = params
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    protocol::log::debug(
        "acp",
        format!(
            "调用 {method} {}",
            protocol::log::params_summary(params, &["sessionId", "cwd"], 60)
        ),
    );
    let result = dispatch_call_inner(cx, method, params, &sid).await;
    match &result {
        Ok(_) => protocol::log::debug("acp", format!("{method} 成功")),
        Err(e) => protocol::log::error("acp", format!("{method} 失败: {e}")),
    }
    result
}

async fn dispatch_call_inner(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
    method: &str,
    params: &Value,
    sid: &str,
) -> Result<Value, String> {
    match method {
        "session/new" => {
            let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or("/");
            let resp = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/new 失败: {e}"))?;
            Ok(json!({ "sessionId": resp.session_id }))
        }
        "session/resume" => {
            let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or("/tmp");
            cx.send_request(ResumeSessionRequest::new(sid.to_string(), cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/resume 失败: {e}"))?;
            Ok(Value::Null)
        }
        "session/cancel" => {
            cx.send_notification(CancelNotification::new(sid.to_string()))
                .map_err(|e| format!("session/cancel 失败: {e}"))?;
            Ok(Value::Null)
        }
        "session/close" => {
            cx.send_request(CloseSessionRequest::new(sid.to_string()))
                .block_task()
                .await
                .map_err(|e| format!("session/close 失败: {e}"))?;
            Ok(Value::Null)
        }
        "skill/list" => {
            let resp = cx
                .send_request(SkillListRequest {})
                .block_task()
                .await
                .map_err(|e| format!("skill/list 失败: {e}"))?;
            serde_json::to_value(resp).map_err(|e| format!("skill/list 序列化失败: {e}"))
        }
        _ => Err(format!("未知 ACP 方法: {method}")),
    }
}

/// 把 ACP `session/update` 通知映射为 AgentEvent 并路由（docs/DESIGN.md §5 聚合）。
fn route_update(
    routes: &Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
    notif: &SessionNotification,
) {
    let ev = match &notif.update {
        SessionUpdate::UserMessageChunk(chunk) => {
            text_of(&chunk.content).map(AgentEvent::UserMessage)
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            text_of(&chunk.content).map(AgentEvent::OutputChunk)
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            text_of(&chunk.content).map(AgentEvent::Thinking)
        }
        SessionUpdate::ToolCall(tc) => Some(AgentEvent::ToolCall {
            name: tool_kind_str(&tc.kind),
            title: Some(tc.title.clone()),
            content: tc.raw_input.as_ref().map(|v| v.to_string()),
        }),
        // ACP tool_call_update 的 kind 字段可选，常缺失；缺失时用标题作为展示名。
        // kind 与 title 都没有的更新没有可展示标识，直接跳过（避免「工具调用 工具」空条目）。
        SessionUpdate::ToolCallUpdate(tcu) => tcu
            .fields
            .kind
            .as_ref()
            .map(tool_kind_str)
            .or_else(|| tcu.fields.title.clone())
            .map(|name| AgentEvent::ToolCall {
                name,
                title: tcu.fields.title.clone(),
                content: tcu.fields.raw_input.as_ref().map(|v| v.to_string()),
            }),
        // agent 自报状态透传（ACP v1 `session_info_update` 未携带状态字段，state=None）
        SessionUpdate::SessionInfoUpdate(_) => Some(AgentEvent::SessionInfo { state: None }),
        // UsageUpdate / AvailableCommandsUpdate / CurrentModeUpdate /
        // ConfigOptionUpdate / Plan 等不产生 AgentEvent
        _ => None,
    };
    if let Some(ev) = ev {
        if let Some(tx) = routes
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .get(notif.session_id.to_string().as_str())
        {
            let _ = tx.try_send(ev);
        }
    }
}

/// ContentBlock → 文本（仅 text 类型；其他类型忽略）。
fn text_of(block: &AcpContentBlock) -> Option<String> {
    match block {
        AcpContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// ToolKind → 字符串（serde 序列化的 snake_case 名，如 `execute` / `read`）。
fn tool_kind_str(kind: &ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "tool_call".to_string())
}

/// protocol::ContentBlock → SDK ContentBlock（MCP 兼容）。
fn acp_content_block(b: &ContentBlock) -> Option<AcpContentBlock> {
    match b {
        ContentBlock::Text { text } => Some(AcpContentBlock::Text(TextContent::new(text.clone()))),
        ContentBlock::Resource {
            mime_type,
            uri,
            text,
            blob,
        } => {
            let uri = uri.clone().unwrap_or_default();
            let resource = if let Some(blob) = blob {
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new(blob.clone(), uri.clone())
                        .mime_type(mime_type.clone()),
                )
            } else {
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new(text.clone().unwrap_or_default(), uri.clone())
                        .mime_type(mime_type.clone()),
                )
            };
            Some(AcpContentBlock::Resource(EmbeddedResource::new(resource)))
        }
        ContentBlock::ResourceLink {
            uri,
            name,
            mime_type,
            title,
            description,
        } => Some(AcpContentBlock::ResourceLink(
            ResourceLink::new(name.clone(), uri.clone())
                .mime_type(mime_type.clone())
                .title(title.clone())
                .description(description.clone()),
        )),
    }
}

// ---- 内存 Stub（演示/无需 agent 的测试）----

pub struct StubAgentDriver {
    sessions: std::sync::Mutex<Vec<String>>,
    pub output_prefix: String,
}

impl StubAgentDriver {
    #[allow(dead_code)]
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
    ) -> mpsc::Receiver<AgentEvent> {
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

    fn list_skills(&self) -> Vec<String> {
        Vec::new()
    }

    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentBlock as AcpContentBlock, ContentChunk, SessionId, TextContent, ToolCall,
        ToolCallStatus, ToolKind,
    };
    use tokio::sync::mpsc;

    fn route_with_channel() -> (
        Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
        mpsc::Receiver<AgentEvent>,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let routes = Mutex::new(HashMap::from([("s1".to_string(), tx)]));
        (routes, rx)
    }

    /// ACP 规范嵌套格式（params.update.sessionUpdate）的 agent_message_chunk。
    #[test]
    fn route_update_agent_message_chunk() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("输出"),
            ))),
        );
        route_update(&routes, &notif);
        let ev = rx.try_recv().expect("应收到事件");
        assert!(matches!(ev, AgentEvent::OutputChunk(s) if s == "输出"));
    }

    /// user_message_chunk → UserMessage。
    #[test]
    fn route_update_user_message_chunk() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::UserMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("收到"),
            ))),
        );
        route_update(&routes, &notif);
        let ev = rx.try_recv().expect("应收到事件");
        assert!(matches!(ev, AgentEvent::UserMessage(s) if s == "收到"));
    }

    /// agent_thought_chunk → Thinking。
    #[test]
    fn route_update_thinking() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new("思考中"),
            ))),
        );
        route_update(&routes, &notif);
        let ev = rx.try_recv().expect("应收到 thinking 事件");
        assert!(matches!(ev, AgentEvent::Thinking(s) if s == "思考中"));
    }

    /// tool_call → ToolCall 活动（kind / title / rawInput）。
    #[test]
    fn route_update_tool_call() {
        let (routes, mut rx) = route_with_channel();
        let tc = ToolCall::new("tc1", "运行 cargo test")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .raw_input(serde_json::json!({"command": "cargo test"}));
        let notif = SessionNotification::new(SessionId::new("s1"), SessionUpdate::ToolCall(tc));
        route_update(&routes, &notif);
        let ev = rx.try_recv().expect("应收到 tool_call 事件");
        match ev {
            AgentEvent::ToolCall {
                name,
                title,
                content,
            } => {
                assert_eq!(name, "execute");
                assert_eq!(title.as_deref(), Some("运行 cargo test"));
                assert!(content.unwrap_or_default().contains("cargo test"));
            }
            other => panic!("应为 ToolCall，得到 {other:?}"),
        }
    }

    /// `session_info_update` → SessionInfo 事件（agent 自报状态透传，docs/DESIGN.md §5.1）。
    #[test]
    fn route_update_session_info() {
        let (routes, mut rx) = route_with_channel();
        let notif = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new(),
            ),
        );
        route_update(&routes, &notif);
        let ev = rx
            .try_recv()
            .expect("session_info_update 应产生 SessionInfo 事件");
        assert!(matches!(ev, AgentEvent::SessionInfo { state: None }));
    }

    /// yolo 权限批准选项选择（docs/DESIGN.md §7.2）：必须选 allow 类选项——
    /// claude-acp 等包装器把「Deny」放在选项第一项，选第一个会被误判为用户拒绝。
    #[test]
    fn pick_approve_option_prefers_allow() {
        fn opt(id: &str, kind: PermissionOptionKind) -> PermissionOption {
            PermissionOption::new(id.to_string(), id.to_string(), kind)
        }

        // claude-acp 实际选项顺序：Deny 在前
        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("allow", PermissionOptionKind::AllowOnce),
            opt("allow_always", PermissionOptionKind::AllowAlways),
        ];
        let picked = pick_approve_option(&opts).expect("应批准");
        assert_eq!(
            picked.to_string(),
            "allow_always",
            "应优先选 AllowAlways（yolo 免重复询问）"
        );

        // 无 AllowAlways：选 AllowOnce
        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("allow", PermissionOptionKind::AllowOnce),
        ];
        assert_eq!(pick_approve_option(&opts).unwrap().to_string(), "allow");

        // 仅一个 AllowOnce（mock 场景）
        let opts = vec![opt("allow-once", PermissionOptionKind::AllowOnce)];
        assert_eq!(
            pick_approve_option(&opts).unwrap().to_string(),
            "allow-once"
        );

        // 全为拒绝 → None（按取消处理）
        let opts = vec![
            opt("reject", PermissionOptionKind::RejectOnce),
            opt("reject_all", PermissionOptionKind::RejectAlways),
        ];
        assert!(pick_approve_option(&opts).is_none());

        // 空列表 → None
        assert!(pick_approve_option(&[]).is_none());
    }

    /// 自动发现：`acp` 子命令探测逻辑（输出含 acp 才算支持）。
    #[test]
    fn has_acp_subcommand_detects() {
        assert!(!has_acp_subcommand("/nonexistent/bin/definitely-not-here"));
    }

    /// 发现决策：ACP 原生优先；无 acp 子命令时回落 npx 包装器（docs/DESIGN.md §9.1）。
    #[test]
    fn discover_for_cli_prefers_native_acp() {
        let d = discover_for_cli(
            "kimi",
            None,
            Some("/usr/bin/kimi".into()),
            true,
            Some("/usr/bin/npx".into()),
        )
        .unwrap();
        assert_eq!(d.name, "kimi");
        assert_eq!(d.bin, "/usr/bin/kimi");
        assert_eq!(d.args, vec!["acp"]);
        assert!(d.env.is_empty());
    }

    /// 发现决策：claude 无 acp 子命令 → npx 包装器，无附加 env。
    #[test]
    fn discover_for_cli_claude_via_npx() {
        let d = discover_for_cli(
            "claude",
            Some("@agentclientprotocol/claude-agent-acp"),
            Some("/usr/bin/claude".into()),
            false,
            Some("/usr/bin/npx".into()),
        )
        .unwrap();
        assert_eq!(d.name, "claude");
        assert_eq!(d.bin, "/usr/bin/npx");
        assert_eq!(d.args, vec!["-y", "@agentclientprotocol/claude-agent-acp"]);
        assert!(d.env.is_empty());
    }

    /// 发现决策：codex 无 acp 子命令 → npx 包装器，带 INITIAL_AGENT_MODE 环境变量。
    #[test]
    fn discover_for_cli_codex_via_npx_with_env() {
        let d = discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            Some("/usr/bin/codex".into()),
            false,
            Some("/usr/bin/npx".into()),
        )
        .unwrap();
        assert_eq!(d.name, "codex");
        assert_eq!(d.bin, "/usr/bin/npx");
        assert_eq!(d.args, vec!["-y", "@agentclientprotocol/codex-acp"]);
        assert_eq!(
            d.env,
            vec![(
                "INITIAL_AGENT_MODE".to_string(),
                "agent-full-access".to_string()
            )]
        );
    }

    /// 发现决策：CLI 未安装或无 npx 时不发现（懒加载，避免误报）。
    #[test]
    fn discover_for_cli_missing_prereqs() {
        assert!(discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            None,
            false,
            Some("/usr/bin/npx".into()),
        )
        .is_none());
        assert!(discover_for_cli(
            "codex",
            Some("@agentclientprotocol/codex-acp"),
            Some("/usr/bin/codex".into()),
            false,
            None,
        )
        .is_none());
    }

    /// `AMUX_NO_DISCOVERY=1`：跳过运行期自动发现（只保留显式配置 / stub 兜底）。
    /// 用于受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    #[test]
    fn no_discovery_skips_auto_discovery() {
        let mut reg = AgentRegistry::new_for_tests();
        reg.no_discovery = true;
        reg.force_stub = false;
        reg.refresh_discovery();
        // 无配置、无发现 → stub 兜底仍生效（演示模式可用）
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
