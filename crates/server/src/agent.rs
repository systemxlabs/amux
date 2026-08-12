//! Agent 驱动抽象（docs/DESIGN.md §9）：server 与 agent harness 的唯一接口。
//! 本模块提供：
//! - `AcpAgentDriver`：真实 ACP v1 对接（官方 SDK `agent-client-protocol`，
//!   `AcpAgent` stdio 传输 + typed 请求/通知，`codex-acp` / `claude-acp` / `kimi acp`）
//! - `StubAgentDriver`：内存 Stub（演示/无需 agent 的测试）
//! - `AgentRegistry`：按 harness 名解析驱动——`--agent` 配置的驱动 + PATH 自动发现的
//!   `*-acp` 可执行（惰性 spawn，PRD §3.3「可执行路径自动发现、不手动指定」）+ 默认模型配置
//!
//! ACP v1 语义（docs/DESIGN.md §9）：session/new、load、resume、prompt、cancel、close、
//! delete、list 等方法；session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpAgentDriver 使用**专用 exec 线程**承载全部异步 IO（官方 SDK 连接、子进程 stdio、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock as AcpContentBlock, DeleteSessionRequest, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent, ToolKind,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    AcpAgent, Agent, Client, ConnectionTo, JsonRpcRequest, JsonRpcResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use protocol::{ContentBlock, HarnessInfo, PassthroughEvent, SessionState};

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

/// 把 driver 的 turn 事件映射为透传事件（server 不聚合，原样透传）。
pub fn passthrough_event(ev: AgentEvent, ts: u64) -> PassthroughEvent {
    match ev {
        AgentEvent::OutputChunk(s) => PassthroughEvent::OutputChunk {
            text: s,
            timestamp: ts,
        },
        AgentEvent::UserMessage(c) => PassthroughEvent::UserMessage {
            content: vec![ContentBlock::Text { text: c }],
            timestamp: ts,
        },
        AgentEvent::Thinking(c) => PassthroughEvent::ThinkingChunk {
            content: c,
            timestamp: ts,
        },
        AgentEvent::ToolCall {
            name,
            title,
            content,
        } => PassthroughEvent::ToolCall {
            name,
            title,
            content,
            timestamp: ts,
        },
        AgentEvent::Compaction(d) => PassthroughEvent::Compaction {
            detail: d,
            timestamp: ts,
        },
        AgentEvent::SessionInfo { state } => PassthroughEvent::SessionInfo {
            state,
            timestamp: ts,
        },
        AgentEvent::TurnEnded => PassthroughEvent::TurnEnded { timestamp: ts },
    }
}

/// ACP `session/list` 返回的单条会话（id + cwd + 可选标题）。
/// server 重启后从 agent 侧恢复会话列表的数据来源（docs/DESIGN.md §4.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    /// agent 侧会话 id
    pub agent_session_id: String,
    /// 会话工作目录（ACP `SessionInfo.cwd`）
    pub cwd: String,
    /// agent 自报标题（ACP `SessionInfo.title`，可缺省）
    pub title: Option<String>,
}

/// 与单个 agent harness 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id
    fn create_session(&self, cwd: &str, model: Option<&str>) -> Result<String, String>;
    /// 加载会话（ACP `session/load` 全量重放；返回透传事件，GUI 聚合）
    fn load_session(&self, agent_session_id: &str) -> Result<Vec<PassthroughEvent>, String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 删除会话（历史一并移除）
    fn delete(&self, agent_session_id: &str) -> Result<(), String>;
    /// 列出 agent 侧全部会话（server 重启后从 agent 恢复会话列表，docs/DESIGN.md §4.1）
    #[allow(dead_code)]
    fn list_sessions(&self) -> Vec<ListedSession>;
    /// 该 agent 安装的 skills 列表（PRD §3.3；agent 不支持时返回空列表）
    #[allow(dead_code)]
    fn list_skills(&self) -> Vec<String>;
}

/// 当前时间戳（毫秒）。
pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(dead_code)]
pub type SharedDriver = Arc<dyn AgentDriver>;

// ---- AgentRegistry：harness 名 → 驱动 ----

/// 自动发现的 ACP agent（含 ACP 子命令参数 / npx 包装器参数与附加环境变量）。
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub bin: String,
    pub args: Vec<String>,
    /// 附加环境变量（如 codex 包装器的 INITIAL_AGENT_MODE）
    pub env: Vec<(String, String)>,
}

/// harness 注册表（PRD §3.3：agent 自动发现，可执行路径不手动指定）：
///
/// - `--agent` 指定的驱动（harness 名 = 可执行文件名，如 `mock_acp` / `kimi acp`）为显式覆盖
/// - 自动发现（无需 `--agent`，docs/DESIGN.md §9.1）：
///   - 已知 CLI 的 `acp` 子命令探测（如 `kimi acp`，ACP 原生）
///   - 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///   - 发现项惰性 spawn（按需拉起，不手动指定路径）
/// - 演示兜底：既无 `--agent` 又无任何发现时，用内存 Stub（接受任意 harness 名）
///
/// 默认模型按 harness 持久化到数据目录（`agent-models.json`），get_info 一并返回。
pub struct AgentRegistry {
    /// 演示模式：任意 harness 名都解析到同一个 Stub 驱动（内部可变，随发现刷新）
    stub: std::sync::Mutex<Option<SharedDriver>>,
    /// 测试强制 stub：跳过运行期发现（避免本机 PATH 干扰单测）
    force_stub: bool,
    /// 禁用运行期自动发现（`AMUX_NO_DISCOVERY=1`）：只使用 `--agent` 显式配置的 agent。
    /// 供受限环境与测试隔离（避免拉起本机未配置的 agent 并恢复其会话）。
    no_discovery: bool,
    /// 配置驱动：harness 名 + 驱动
    configured: Option<(String, SharedDriver)>,
    /// 自动发现的 agent（不含已配置的；可运行期刷新）
    discovered: std::sync::Mutex<Vec<DiscoveredAgent>>,
    /// 惰性 spawn 的发现驱动
    spawned: std::sync::Mutex<HashMap<String, SharedDriver>>,
    /// 按 harness 的默认模型配置
    models: std::sync::Mutex<HashMap<String, Option<String>>>,
    model_file: std::path::PathBuf,
}

impl AgentRegistry {
    /// 构建注册表（生产路径：自动发现本机 ACP agent）。
    /// - `configured`：`--agent` 显式指定的驱动，可为 None（由自动发现接管）
    /// - `model_file`：默认模型配置的落盘路径
    pub fn new(configured: Option<(String, SharedDriver)>, model_file: std::path::PathBuf) -> Self {
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
            models: std::sync::Mutex::new(load_models(&model_file)),
            model_file,
        };
        if !no_discovery {
            registry.refresh_discovery();
        }
        registry
    }

    /// 重新扫描本机 ACP agent（运行期安装的新 agent 经 get_info 刷新即可发现，PRD §3.3）。
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

    /// 测试构造：忽略本机 PATH 发现，强制 stub 演示模式（harness 任意）。
    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        AgentRegistry {
            stub: std::sync::Mutex::new(Some(Arc::new(StubAgentDriver::new()))),
            force_stub: true,
            no_discovery: false,
            configured: None,
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            models: std::sync::Mutex::new(HashMap::new()),
            model_file: std::path::PathBuf::new(),
        }
    }

    /// get_info 的 harness 列表（available + 默认模型）；先运行期刷新一次发现。
    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        self.refresh_discovery();
        let models = self
            .models
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        let discovered = self
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        let mut out: Vec<HarnessInfo> = Vec::new();
        if let Some((name, _)) = &self.configured {
            out.push(HarnessInfo {
                name: name.clone(),
                available: true,
                default_model: models.get(name).cloned().flatten(),
            });
        } else if self
            .stub
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_some()
        {
            out.push(HarnessInfo {
                name: "stub".into(),
                available: true,
                default_model: models.get("stub").cloned().flatten(),
            });
        }
        for d in discovered.iter() {
            out.push(HarnessInfo {
                name: d.name.clone(),
                available: true,
                default_model: models.get(&d.name).cloned().flatten(),
            });
        }
        out
    }

    /// 按 harness 名解析驱动；未知 harness 报错（HARNESS_UNAVAILABLE）。
    /// 未知 harness 时先运行期刷新一次发现（新装的 agent 无需重启即可用）。
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
        self.refresh_discovery();
        let found = self
            .discovered
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.name == harness)
            .cloned();
        if let Some(d) = found {
            let mut spawned = self
                .spawned
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）");
            if let Some(d) = spawned.get(harness) {
                return Ok(d.clone());
            }
            let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
            let env = d.env.clone();
            let driver = AcpAgentDriver::spawn(&d.bin, &args, &env)
                .map_err(|e| format!("启动 ACP agent ({}) 失败: {e}", d.bin))?;
            let driver: SharedDriver = Arc::new(driver);
            spawned.insert(harness.to_string(), driver.clone());
            return Ok(driver);
        }
        Err(format!("本机未发现 agent: {harness}"))
    }

    /// 查询默认模型（get_info 用）。
    #[allow(dead_code)]
    pub fn default_model(&self, harness: &str) -> Option<String> {
        self.models
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .get(harness)
            .cloned()
            .flatten()
    }

    /// 配置默认模型并落盘（PRD §3.3）。
    pub fn set_default_model(&self, harness: &str, model: Option<String>) {
        self.models
            .lock()
            .unwrap()
            .insert(harness.to_string(), model);
        self.save_models();
    }

    fn save_models(&self) {
        let models = self
            .models
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        let json = serde_json::to_string_pretty(&*models).unwrap_or_default();
        if let Some(parent) = self.model_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.model_file, json);
    }
}

/// 是否为 agent 内部的系统提醒（非真实用户消息）。
/// 典型内容：turn 被取消后追加的 "The previous turn was interrupted by the user
/// before completion; ..."，且通常以 `<system-reminder>` 包裹（kimi 等实现）。
fn is_internal_reminder(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("<system-reminder>") || s.contains("interrupted by the user before completion")
}

fn load_models(path: &std::path::Path) -> HashMap<String, Option<String>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, Option<String>>>(&s).ok())
        .unwrap_or_default()
}

/// 自动发现 ACP agent（PRD §3.3：可执行路径自动发现、不手动指定；docs/DESIGN.md §9.1）：
/// 1) 已知 CLI 的 `acp` 子命令探测（ACP 原生，如 `kimi acp`）
/// 2) 已知 CLI（`claude` / `codex`）经 npx 启动官方 ACP 包装器（`npx -y @agentclientprotocol/...`）
///
/// 不做任意 `*-acp` 扫描：只认已知 agent，避免无关可执行污染列表。
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

/// 单 CLI 的发现决策（纯逻辑，便于单测；docs/DESIGN.md §9.1）：
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
        // 全权限自主模式（docs/DESIGN.md §9.1）
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
    exec_tx: std::sync::mpsc::SyncSender<ExecReq>,
    /// 会话事件路由：agent sessionId -> prompt/load 的事件接收端
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    /// create 时记录的会话 cwd（load/resume 需要）
    cwds: Arc<Mutex<HashMap<String, String>>>,
    /// exec 线程句柄（连接由 SDK 管理，线程结束即子进程清理）
    _thread: std::thread::JoinHandle<()>,
}

impl AcpAgentDriver {
    /// 启动 ACP agent 子进程（官方 SDK `AcpAgent` 管理 stdio 传输与进程生命周期）；
    /// `env` 为附加环境变量（经 from_args 的 `NAME=value` 前缀传入）；
    /// exec 线程承载全部异步 IO。
    pub fn spawn(bin: &str, args: &[&str], env: &[(String, String)]) -> Result<Self, String> {
        let (exec_tx, exec_rx) = std::sync::mpsc::sync_channel::<ExecReq>(32);
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
            rt.block_on(exec_main(&bin, &args, &env, exec_rx, routes2));
        });
        Ok(AcpAgentDriver {
            exec_tx,
            routes,
            cwds: Arc::new(Mutex::new(HashMap::new())),
            _thread: thread,
        })
    }

    /// 同步方法调用：请求发往 exec 线程，阻塞等待响应。
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Value, String>>(1);
        self.exec_tx
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
    fn create_session(&self, cwd: &str, _model: Option<&str>) -> Result<String, String> {
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
        Ok(sid)
    }

    fn load_session(&self, agent_session_id: &str) -> Result<Vec<PassthroughEvent>, String> {
        let cwd = self
            .cwds
            .lock()
            .unwrap()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_else(|| "/tmp".into());
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        self.routes
            .lock()
            .unwrap()
            .insert(agent_session_id.to_string(), tx);
        let res = self.call(
            "session/load",
            json!({ "sessionId": agent_session_id, "cwd": cwd, "mcpServers": [] }),
        );
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::UserMessage(s) => {
                    // 过滤 agent 内部提醒：turn 被取消后 agent 会往会话历史追加一条
                    // 系统提示（"The previous turn was interrupted by the user..."），
                    // 不是真实用户消息，展示出来会污染会话历史（docs/DESIGN.md §5）
                    if is_internal_reminder(&s) {
                        continue;
                    }
                    events.push(passthrough_event(AgentEvent::UserMessage(s), now_ts()));
                }
                AgentEvent::TurnEnded => break,
                _ => events.push(passthrough_event(ev, now_ts())),
            }
        }
        self.routes
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(agent_session_id);
        res.map(|_| events)
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
        let _ = self.exec_tx.send(req);
        rx
    }

    fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.call("session/cancel", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        self.cwds
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .remove(agent_session_id);
        self.call("session/delete", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn list_sessions(&self) -> Vec<ListedSession> {
        match self.call("session/list", json!({})) {
            Ok(res) => res
                .get("sessions")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| {
                            let agent_session_id = s.get("id").and_then(|v| v.as_str())?.to_string();
                            let cwd = s
                                .get("cwd")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let title = s
                                .get("title")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            Some(ListedSession {
                                agent_session_id,
                                cwd,
                                title,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
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

/// exec 线程主循环：经官方 SDK 建立 ACP 连接，承载方法分发、通知路由与权限批准。
async fn exec_main(
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    exec_rx: std::sync::mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
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

    // SDK from_args 支持 `NAME=value` 前缀参数作为环境变量（docs/DESIGN.md §9.1）
    let mut cmd: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    cmd.push(bin.to_string());
    cmd.extend(args.iter().cloned());
    let agent = match AcpAgent::from_args(cmd) {
        Ok(a) => a,
        Err(e) => {
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

    let _ = Client
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
                // yolo：自动批准（选第一个 allow 选项；无选项则取消）
                match request.options.first() {
                    Some(opt) => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            opt.option_id.clone(),
                        )),
                    )),
                    None => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
            // 初始化握手（版本协商）。失败仅记录——部分 agent（如 mock_acp）不实现
            // initialize 也照常工作，连接保持。
            if let Err(e) = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await
            {
                protocol::log::error("acp", format!("initialize 失败（继续）: {e}"));
            } else {
                protocol::log::debug("acp", "initialize 完成");
            }

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
                                    if let Err(e) = result {
                                        protocol::log::error(
                                            "acp",
                                            format!("prompt 失败 {sid}: {e}"),
                                        );
                                    }
                                    Ok(())
                                });
                        });
                    }
                }
            }
            Ok(())
        })
        .await;
}

/// 按方法名分发 ACP v1 方法调用（typed 请求，经官方 SDK 传输）。
async fn dispatch_call(
    cx: &ConnectionTo<Agent>,
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
    cx: &ConnectionTo<Agent>,
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
        "session/load" => {
            let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or("/tmp");
            cx.send_request(LoadSessionRequest::new(sid.to_string(), cwd))
                .block_task()
                .await
                .map_err(|e| format!("session/load 失败: {e}"))?;
            Ok(Value::Null)
        }
        "session/cancel" => {
            cx.send_notification(CancelNotification::new(sid.to_string()))
                .map_err(|e| format!("session/cancel 失败: {e}"))?;
            Ok(Value::Null)
        }
        "session/delete" => {
            cx.send_request(DeleteSessionRequest::new(sid.to_string()))
                .block_task()
                .await
                .map_err(|e| format!("session/delete 失败: {e}"))?;
            Ok(Value::Null)
        }
        "session/list" => {
            let resp = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await
                .map_err(|e| format!("session/list 失败: {e}"))?;
            let sessions = resp
                .sessions
                .into_iter()
                .map(|s| {
                    json!({
                        "id": s.session_id,
                        "cwd": s.cwd,
                        "title": s.title,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "sessions": sessions }))
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
        SessionUpdate::ToolCallUpdate(tcu) => Some(AgentEvent::ToolCall {
            name: tcu
                .fields
                .kind
                .as_ref()
                .map(tool_kind_str)
                .unwrap_or_else(|| "tool_call".to_string()),
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

/// protocol::ContentBlock → SDK ContentBlock（MCP 兼容；无 SDK 等价的类型忽略）。
fn acp_content_block(b: &ContentBlock) -> Option<AcpContentBlock> {
    match b {
        ContentBlock::Text { text } => Some(AcpContentBlock::Text(TextContent::new(text.clone()))),
        ContentBlock::Resource { .. } | ContentBlock::ResourceLink { .. } => None,
    }
}

// ---- 内存 Stub（演示/无需 agent 的测试）----

pub struct StubAgentDriver {
    sessions: std::sync::Mutex<Vec<ListedSession>>,
    pub output_prefix: String,
}

impl StubAgentDriver {
    #[allow(dead_code)]
    pub fn new() -> Self {
        StubAgentDriver {
            sessions: std::sync::Mutex::new(Vec::new()),
            output_prefix: "模拟输出：".into(),
        }
    }
}

impl AgentDriver for StubAgentDriver {
    fn create_session(&self, cwd: &str, _model: Option<&str>) -> Result<String, String> {
        let id = format!("agent_{}", cwd.replace('/', "_"));
        self.sessions.lock().expect("Mutex 中毒（临界区内不应 panic）").push(ListedSession {
            agent_session_id: id.clone(),
            cwd: cwd.to_string(),
            title: None,
        });
        Ok(id)
    }

    fn load_session(&self, _agent_session_id: &str) -> Result<Vec<PassthroughEvent>, String> {
        Ok(Vec::new())
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

    fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        self.sessions
            .lock()
            .unwrap()
            .retain(|s| s.agent_session_id != agent_session_id);
        Ok(())
    }

    fn list_sessions(&self) -> Vec<ListedSession> {
        self.sessions
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone()
    }

    fn list_skills(&self) -> Vec<String> {
        Vec::new()
    }
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

    /// agent 内部提醒（turn 取消后的中断提示）应被识别并过滤，不污染会话历史。
    #[test]
    fn internal_reminder_detected() {
        assert!(is_internal_reminder(
            "<system-reminder>\nThe previous turn was interrupted by the user before \
             completion; any partial output shown above is incomplete. The user's next \
             message continues the conversation.\n</system-reminder>"
        ));
        assert!(is_internal_reminder(
            "The previous turn was interrupted by the user before completion; ..."
        ));
        assert!(!is_internal_reminder("请安装/更新以下 skill：OpenCLI"));
        assert!(!is_internal_reminder("好的，我来分析一下这个问题。"));
    }

    /// 自动发现：`acp` 子命令探测逻辑（输出含 acp 才算支持）。
    #[test]
    fn has_acp_subcommand_detects() {
        // 用当前测试二进制自身不可能触发，直接验证判别逻辑：
        // `kimi acp --help` 在装有 kimi 的机器上命中；此处只验证函数对
        // 不存在二进制的安全返回 false。
        assert!(!has_acp_subcommand("/nonexistent/bin/definitely-not-here"));
    }

    /// 发现决策：ACP 原生优先；无 acp 子命令时回落 npx 包装器（docs/DESIGN.md §9.1）。
    #[test]
    fn discover_for_cli_prefers_native_acp() {
        // kimi：ACP 原生（`kimi acp`），无附加 env
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

        // 有显式配置时：harnesses 只含配置的驱动，不扫描 PATH
        reg.stub = std::sync::Mutex::new(None);
        reg.configured = Some((
            "mock_acp".to_string(),
            Arc::new(StubAgentDriver::new()) as SharedDriver,
        ));
        reg.refresh_discovery();
        let hs = reg.harnesses();
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].name, "mock_acp");
        assert!(reg
            .discovered
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .is_empty());
    }
}
