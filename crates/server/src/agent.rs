//! Agent 驱动抽象（docs/DESIGN.md §9）：server 与 agent harness 的唯一接口。
//! 本模块提供：
//! - `AcpAgentDriver`：真实 ACP v1 对接（stdio JSON-RPC，`codex-acp` / `claude-acp` / `kimi acp`）
//! - `StubAgentDriver`：内存 Stub（演示/无需 agent 的测试）
//! - `AgentRegistry`：按 harness 名解析驱动——`--agent` 配置的驱动 + PATH 自动发现的
//!   `*-acp` 可执行（惰性 spawn，PRD §3.3「可执行路径自动发现、不手动指定」）+ 默认模型配置
//!
//! ACP v1 语义：session/new、load、resume、prompt、cancel、close、delete、list 等方法；
//! session/update 事件流聚合；session/request_permission 自动批准（yolo）。
//!
//! AcpAgentDriver 使用**专用 exec 线程**承载全部异步 IO（子进程 stdin/stdout 读写、
//! 通知路由、权限自动批准），主线程方法调用经 std 同步通道往返——避免跨线程/跨 runtime
//! 嵌套的 tokio 问题（调用方可能处于任意 tokio runtime 上下文）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use protocol::ContentBlock;
use protocol::HarnessInfo;

/// turn 过程中的 agent 事件（server 聚合为输出 + activities，docs/DESIGN.md §5）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 输出的增量片段（聚合为完整输出，非流式交付）
    OutputChunk(String),
    /// 用户消息回显（load 重放用）
    UserMessage(String),
    /// 思考活动
    #[allow(dead_code)]
    Thinking(String),
    /// 工具调用活动
    #[allow(dead_code)]
    ToolCall {
        name: String,
        title: Option<String>,
        content: Option<String>,
    },
    /// 上下文压缩活动（ACP 场景可能产生）
    #[allow(dead_code)]
    Compaction(String),
    /// turn 完成
    TurnEnded,
}

/// 与单个 agent harness 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id
    fn create_session(&self, cwd: &str, model: Option<&str>) -> Result<String, String>;
    /// 加载会话（ACP `session/load` 全量重放；返回对话内容）
    fn load_session(&self, agent_session_id: &str) -> Result<Vec<DialogRecord>, String>;
    /// 恢复会话上下文
    fn resume_session(&self, agent_session_id: &str) -> Result<(), String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 关闭会话（保留历史可恢复）
    #[allow(dead_code)]
    fn close(&self, agent_session_id: &str) -> Result<(), String>;
    /// 删除会话（历史一并移除）
    fn delete(&self, agent_session_id: &str) -> Result<(), String>;
    /// 列出 agent 侧全部会话 id（server 重启后从 agent 恢复会话列表，docs/DESIGN.md §3）
    #[allow(dead_code)]
    fn list_sessions(&self) -> Vec<String>;
    /// 该 agent 安装的 skills 列表（PRD §3.3；agent 不支持时返回空列表）
    #[allow(dead_code)]
    fn list_skills(&self) -> Vec<String>;
}

/// 对话内容条目（load 重放的产物）。
#[derive(Debug, Clone)]
pub enum DialogRecord {
    #[allow(dead_code)]
    UserMessage(Vec<ContentBlock>),
    AgentOutput(Vec<ContentBlock>),
}

#[allow(dead_code)]
pub type SharedDriver = Arc<dyn AgentDriver>;

// ---- AgentRegistry：harness 名 → 驱动 ----

/// 自动发现的 ACP agent（含 ACP 子命令参数，如 `kimi acp`）。
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub bin: String,
    pub args: Vec<String>,
}

/// harness 注册表（PRD §3.3：agent 自动发现，可执行路径不手动指定）：
///
/// - `--agent` 指定的驱动（harness 名 = 可执行文件名，如 `mock_acp` / `kimi acp`）为显式覆盖
/// - 自动发现（无需 `--agent`）：
///   - PATH 上的 `*-acp` 可执行（如 `codex-acp` / `claude-acp` / `kimi-acp`）
///   - 已知 agent CLI（`codex` / `claude` / `kimi`）的 `acp` 子命令探测（如 `kimi acp`）
///   - 发现项惰性 spawn（按需拉起，不手动指定路径）
/// - 演示兜底：既无 `--agent` 又无任何发现时，用内存 Stub（接受任意 harness 名）
///
/// 默认模型按 harness 持久化到数据目录（`agent-models.json`），get_info 一并返回。
pub struct AgentRegistry {
    /// 演示模式：任意 harness 名都解析到同一个 Stub 驱动（内部可变，随发现刷新）
    stub: std::sync::Mutex<Option<SharedDriver>>,
    /// 测试强制 stub：跳过运行期发现（避免本机 PATH 干扰单测）
    force_stub: bool,
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
        let registry = AgentRegistry {
            stub: std::sync::Mutex::new(None),
            force_stub: false,
            configured,
            discovered: std::sync::Mutex::new(Vec::new()),
            spawned: std::sync::Mutex::new(HashMap::new()),
            models: std::sync::Mutex::new(load_models(&model_file)),
            model_file,
        };
        registry.refresh_discovery();
        registry
    }

    /// 重新扫描本机 ACP agent（运行期安装的新 agent 经 get_info 刷新即可发现，PRD §3.3）。
    /// 合并新发现的 agent，保留已配置/已发现条目；无任何 agent 且无配置时启用 stub 兜底。
    fn refresh_discovery(&self) {
        if self.force_stub {
            return;
        }
        let current = discover_acp_agents();
        let mut disc = self.discovered.lock().unwrap();
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
        let need_stub = self.configured.is_none() && disc.is_empty();
        *self.stub.lock().unwrap() = if need_stub {
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
        let models = self.models.lock().unwrap();
        let discovered = self.discovered.lock().unwrap();
        let mut out: Vec<HarnessInfo> = Vec::new();
        if let Some((name, _)) = &self.configured {
            out.push(HarnessInfo {
                name: name.clone(),
                available: true,
                default_model: models.get(name).cloned().flatten(),
            });
        } else if self.stub.lock().unwrap().is_some() {
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
        if let Some(stub) = &*self.stub.lock().unwrap() {
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
            let mut spawned = self.spawned.lock().unwrap();
            if let Some(d) = spawned.get(harness) {
                return Ok(d.clone());
            }
            let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
            let driver = AcpAgentDriver::spawn(&d.bin, &args)
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
        self.models.lock().unwrap().get(harness).cloned().flatten()
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
        let models = self.models.lock().unwrap();
        let json = serde_json::to_string_pretty(&*models).unwrap_or_default();
        if let Some(parent) = self.model_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.model_file, json);
    }
}

fn load_models(path: &std::path::Path) -> HashMap<String, Option<String>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, Option<String>>>(&s).ok())
        .unwrap_or_default()
}

/// 自动发现 ACP agent（PRD §3.3：可执行路径自动发现、不手动指定）：
/// 1) PATH 上的 `*-acp` 可执行（独立 ACP server，如 `codex-acp` / `claude-acp` / `kimi-acp`）
/// 2) 已知 agent CLI（`codex` / `claude` / `kimi`）的 `acp` 子命令探测（如 `kimi acp`）
fn discover_acp_agents() -> Vec<DiscoveredAgent> {
    let mut found: Vec<DiscoveredAgent> = Vec::new();
    let path = std::env::var("PATH").unwrap_or_default();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // 1) `*-acp` 可执行
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(".exe").unwrap_or(&name).to_string();
            if stem.ends_with("-acp") && is_executable(&e.path()) && seen.insert(stem.clone()) {
                found.push(DiscoveredAgent {
                    name: stem,
                    bin: e.path().display().to_string(),
                    args: Vec::new(),
                });
            }
        }
    }

    // 2) 已知 CLI 的 `acp` 子命令探测（`<bin> acp --help` 退出 0 且输出含 acp）
    for cli in ["codex", "claude", "kimi"] {
        if !seen.insert(cli.to_string()) {
            continue;
        }
        let Some(bin) = find_on_path(cli) else {
            continue;
        };
        if has_acp_subcommand(&bin) {
            found.push(DiscoveredAgent {
                name: cli.to_string(),
                bin,
                args: vec!["acp".to_string()],
            });
        }
    }
    found
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

// ---- 真实 ACP v1 stdio 对接 ----

/// 请求的待处理动作（按请求 id 匹配响应）。
enum PendingAction {
    /// 同步方法调用：结果经 std 通道送回调用方
    Call(std::sync::mpsc::SyncSender<Result<Value, String>>),
    /// prompt：响应（turn 完成）时移除路由并推 TurnEnded
    Prompt {
        sid: String,
        routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    },
}

/// 主线程 → exec 线程的方法请求。
enum ExecReq {
    Call {
        method: String,
        params: Value,
        resp: std::sync::mpsc::SyncSender<Result<Value, String>>,
    },
    Prompt {
        sid: String,
        prompt: Vec<Value>,
        routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    },
}

/// ACP v1 客户端（stdio JSON-RPC，docs/DESIGN.md §9）。
pub struct AcpAgentDriver {
    exec_tx: std::sync::mpsc::SyncSender<ExecReq>,
    /// 会话事件路由：agent sessionId -> prompt/load 的事件接收端
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    /// create 时记录的会话 cwd（load/resume 需要）
    cwds: Arc<Mutex<HashMap<String, String>>>,
    /// 保活子进程句柄
    _child: Arc<Mutex<Option<tokio::process::Child>>>,
}

impl AcpAgentDriver {
    /// 启动 ACP agent 子进程（stdio JSON-RPC）；exec 线程承载全部 IO。
    pub fn spawn(bin: &str, args: &[&str]) -> Result<Self, String> {
        let (exec_tx, exec_rx) = std::sync::mpsc::sync_channel::<ExecReq>(32);
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let routes2 = routes.clone();
        let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let bin = bin.to_string();
        let child_holder: Arc<Mutex<Option<tokio::process::Child>>> = Arc::new(Mutex::new(None));
        let ch = child_holder.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("构建 tokio runtime 失败");
            rt.block_on(exec_main(&bin, &args, exec_rx, routes2, ch));
        });
        Ok(AcpAgentDriver {
            exec_tx,
            routes,
            cwds: Arc::new(Mutex::new(HashMap::new())),
            _child: child_holder,
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

/// exec 线程主循环：spawn 子进程，读写 stdin/stdout，路由通知，自动批准权限。
async fn exec_main(
    bin: &str,
    args: &[String],
    exec_rx: std::sync::mpsc::Receiver<ExecReq>,
    routes: Arc<Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>>,
    child_holder: Arc<Mutex<Option<tokio::process::Child>>>,
) {
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn ACP agent");
    let stdin = child.stdin.take().expect("agent 无 stdin");
    let stdout = child.stdout.take().expect("agent 无 stdout");
    *child_holder.lock().unwrap() = Some(child);

    // std exec_rx → tokio 通道（阻塞转发，供 select 使用）
    let (req_tx, mut req_rx) = mpsc::channel::<ExecReq>(32);
    tokio::task::spawn_blocking(move || {
        while let Ok(req) = exec_rx.recv() {
            if req_tx.blocking_send(req).is_err() {
                break;
            }
        }
    });

    let mut writer = stdin;
    let mut reader = BufReader::new(stdout).lines();
    let mut pending: HashMap<u64, PendingAction> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        tokio::select! {
            req = req_rx.recv() => {
                let Some(req) = req else { break };
                let id = next_id;
                next_id += 1;
                let (method, params, action) = match req {
                    ExecReq::Call { method, params, resp } => (method, params, PendingAction::Call(resp)),
                    ExecReq::Prompt { sid, prompt, routes: r } => (
                        "session/prompt".to_string(),
                        json!({ "sessionId": sid, "prompt": prompt }),
                        PendingAction::Prompt { sid, routes: r },
                    ),
                };
                let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                if writer.write_all(format!("{frame}\n").as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
                pending.insert(id, action);
            }
            line = reader.next_line() => {
                let Ok(Some(line)) = line else { break };
                if line.trim().is_empty() { continue; }
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v.get("id") {
                    if v.get("method").is_some() {
                        // agent 发来的请求（如 request_permission）：自动批准（yolo）
                        if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                            if method == "session/request_permission" {
                                let params = v.get("params").cloned().unwrap_or(Value::Null);
                                auto_approve(&mut writer, Some(id.clone()), &params).await;
                            }
                        }
                    } else if let Some(id) = id.as_u64() {
                        // 响应：匹配 pending
                        if let Some(action) = pending.remove(&id) {
                            let result = if let Some(err) = v.get("error") {
                                Err(err.get("message").and_then(|m| m.as_str()).unwrap_or("ACP 错误").to_string())
                            } else {
                                Ok(v.get("result").cloned().unwrap_or(Value::Null))
                            };
                            match action {
                                PendingAction::Call(resp) => {
                                    let _ = resp.send(result);
                                }
                                PendingAction::Prompt { sid, routes } => {
                                    let route_tx = { routes.lock().unwrap().remove(&sid) };
                                    if let Some(route_tx) = route_tx {
                                        let _ = route_tx.send(AgentEvent::TurnEnded).await;
                                    }
                                    if let Err(e) = result {
                                        eprintln!("[acp] prompt 失败: {e}");
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                    if method == "session/update" {
                        if let Some(params) = v.get("params") {
                            if let Some(sid) = params.get("sessionId").and_then(|s| s.as_str()) {
                                route_update(&routes, sid, params);
                            }
                        }
                    }
                }
            }
        }
    }
    // agent 退出：清空 pending
    for (_, action) in pending.drain() {
        if let PendingAction::Call(resp) = action {
            let _ = resp.send(Err("agent 连接断开".into()));
        }
    }
}

/// 读取事件字段：兼容两种 ACP 结构——扁平（`params.sessionUpdate`，mock_acp）与
/// 嵌套（`params.update.sessionUpdate`，kimi acp / ACP 规范）。
fn ev_field<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params
        .get(key)
        .or_else(|| params.get("update").and_then(|u| u.get(key)))
}

/// 把 session/update 通知映射为 AgentEvent 并路由。
fn route_update(
    routes: &Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
    sid: &str,
    params: &Value,
) {
    let Some(kind) = ev_field(params, "sessionUpdate").and_then(|v| v.as_str()) else {
        return;
    };
    let ev = match kind {
        "agent_message_chunk" => ev_field(params, "content")
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| AgentEvent::OutputChunk(s.to_string())),
        "user_message_chunk" => ev_field(params, "content")
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| AgentEvent::UserMessage(s.to_string())),
        "agent_thought_chunk" => ev_field(params, "content")
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| AgentEvent::Thinking(s.to_string())),
        "tool_call" | "tool_call_update" => Some(AgentEvent::ToolCall {
            name: ev_field(params, "kind")
                .and_then(|k| k.as_str())
                .unwrap_or("tool_call")
                .to_string(),
            title: ev_field(params, "title")
                .and_then(|t| t.as_str())
                .map(str::to_string),
            content: ev_field(params, "rawInput")
                .and_then(|r| r.as_str())
                .map(str::to_string),
        }),
        _ => None,
    };
    if let Some(ev) = ev {
        if let Some(tx) = routes.lock().unwrap().get(sid) {
            let _ = tx.try_send(ev);
        }
    }
}

/// yolo：自动批准 session/request_permission（选第一个 allow 选项）。
async fn auto_approve(writer: &mut tokio::process::ChildStdin, id: Option<Value>, params: &Value) {
    let option_id = params
        .get("options")
        .and_then(|o| o.as_array())
        .and_then(|opts| {
            opts.iter().find(|o| {
                o.get("kind")
                    .and_then(|k| k.as_str())
                    .map(|k| k.starts_with("allow"))
                    .unwrap_or(false)
            })
        })
        .and_then(|o| o.get("optionId").cloned())
        .unwrap_or_else(|| json!("allow-once"));
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
    });
    let _ = writer.write_all(format!("{frame}\n").as_bytes()).await;
    let _ = writer.flush().await;
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

    fn load_session(&self, agent_session_id: &str) -> Result<Vec<DialogRecord>, String> {
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
        let mut records = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::OutputChunk(s) => {
                    records.push(DialogRecord::AgentOutput(vec![ContentBlock::Text {
                        text: s,
                    }]));
                }
                AgentEvent::UserMessage(s) => {
                    records.push(DialogRecord::UserMessage(vec![ContentBlock::Text {
                        text: s,
                    }]));
                }
                AgentEvent::Thinking(_)
                | AgentEvent::ToolCall { .. }
                | AgentEvent::Compaction(_) => {}
                AgentEvent::TurnEnded => break,
            }
        }
        self.routes.lock().unwrap().remove(agent_session_id);
        res.map(|_| records)
    }

    fn resume_session(&self, agent_session_id: &str) -> Result<(), String> {
        let cwd = self
            .cwds
            .lock()
            .unwrap()
            .get(agent_session_id)
            .cloned()
            .unwrap_or_else(|| "/tmp".into());
        self.call(
            "session/resume",
            json!({ "sessionId": agent_session_id, "cwd": cwd }),
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
            prompt: input.iter().map(content_block_json).collect(),
            routes: self.routes.clone(),
        };
        let _ = self.exec_tx.send(req);
        rx
    }

    fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
        self.call("session/cancel", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn close(&self, agent_session_id: &str) -> Result<(), String> {
        self.call("session/close", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        self.cwds.lock().unwrap().remove(agent_session_id);
        self.call("session/delete", json!({ "sessionId": agent_session_id }))
            .map(|_| ())
    }

    fn list_sessions(&self) -> Vec<String> {
        match self.call("session/list", json!({})) {
            Ok(res) => res
                .get("sessions")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("id").and_then(|v| v.as_str()).map(str::to_string))
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

/// protocol::ContentBlock → ACP ContentBlock（{type, text} 等，MCP 兼容）。
fn content_block_json(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Resource { .. } => {
            json!({ "type": "resource", "mimeType": "text/plain", "text": "" })
        }
        ContentBlock::ResourceLink { name, uri, .. } => {
            json!({ "type": "resource_link", "name": name, "uri": uri })
        }
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
        StubAgentDriver {
            sessions: std::sync::Mutex::new(Vec::new()),
            output_prefix: "模拟输出：".into(),
        }
    }
}

impl AgentDriver for StubAgentDriver {
    fn create_session(&self, cwd: &str, _model: Option<&str>) -> Result<String, String> {
        let id = format!("agent_{}", cwd.replace('/', "_"));
        self.sessions.lock().unwrap().push(id.clone());
        Ok(id)
    }

    fn load_session(&self, _agent_session_id: &str) -> Result<Vec<DialogRecord>, String> {
        Ok(Vec::new())
    }

    fn resume_session(&self, _agent_session_id: &str) -> Result<(), String> {
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

    fn close(&self, _agent_session_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        self.sessions
            .lock()
            .unwrap()
            .retain(|s| s != agent_session_id);
        Ok(())
    }

    fn list_sessions(&self) -> Vec<String> {
        self.sessions.lock().unwrap().clone()
    }

    fn list_skills(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn route_with_channel() -> (
        Mutex<HashMap<String, mpsc::Sender<AgentEvent>>>,
        mpsc::Receiver<AgentEvent>,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let routes = Mutex::new(HashMap::from([("s1".to_string(), tx)]));
        (routes, rx)
    }

    /// mock_acp 的扁平事件格式（params.sessionUpdate / params.content）。
    #[test]
    fn route_update_flat_format() {
        let (routes, mut rx) = route_with_channel();
        let params = json!({
            "sessionId": "s1",
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "输出" }
        });
        route_update(&routes, "s1", &params);
        let ev = rx.try_recv().expect("应收到事件");
        assert!(matches!(ev, AgentEvent::OutputChunk(s) if s == "输出"));
    }

    /// 真实 agent（kimi acp / ACP 规范）的嵌套事件格式（params.update.sessionUpdate）。
    #[test]
    fn route_update_nested_format() {
        let (routes, mut rx) = route_with_channel();
        let params = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "收到" }
            }
        });
        route_update(&routes, "s1", &params);
        let ev = rx.try_recv().expect("应收到事件");
        assert!(matches!(ev, AgentEvent::OutputChunk(s) if s == "收到"));

        // thinking 事件（嵌套）
        let (routes, mut rx) = route_with_channel();
        let params = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "思考中" }
            }
        });
        route_update(&routes, "s1", &params);
        let ev = rx.try_recv().expect("应收到 thinking 事件");
        assert!(matches!(ev, AgentEvent::Thinking(s) if s == "思考中"));

        // 未知事件种类忽略
        let (routes, mut rx) = route_with_channel();
        let params = json!({
            "sessionId": "s1",
            "update": { "sessionUpdate": "available_commands_update", "availableCommands": [] }
        });
        route_update(&routes, "s1", &params);
        assert!(rx.try_recv().is_err(), "未知事件不应产生 AgentEvent");
    }

    /// 自动发现：`acp` 子命令探测逻辑（输出含 acp 才算支持）。
    #[test]
    fn has_acp_subcommand_detects() {
        // 用当前测试二进制自身不可能触发，直接验证判别逻辑：
        // `kimi acp --help` 在装有 kimi 的机器上命中；此处只验证函数对
        // 不存在二进制的安全返回 false。
        assert!(!has_acp_subcommand("/nonexistent/bin/definitely-not-here"));
    }
}
