//! 工作流引擎（docs/DESIGN.md §10）：GUI 本地编排 agent 会话 + rig 单 turn 编排。
//!
//! 架构（纯逻辑与 IO 分离，本模块不依赖 GPUI，可被单元测试直接驱动）：
//! - `OrcSession`：编排 agent 会话状态（标题/描述/对话历史/子会话/暂停），可序列化持久化
//! - `OrcBackend`：单 turn 决策器——输入工作流上下文，输出 `Decision`（步骤动作 + 摘要）。
//!   真实实现 `RigBackend` 用 rig `Agent::prompt`（单 turn）；会话操作定义为 rig 工具，
//!   工具只记录动作（planning），由引擎统一执行——保证"会话操作经真实 WsClient"只有一条路径
//! - `WorkflowEngine`：状态机——首 turn 拆解计划并创建/复用子会话下发指令；子会话 idle
//!   通知触发自动推进（向编排会话注入完成情况）；暂停/继续/介入均为向编排会话发送指令
//! - 持久化：OrcSession 落盘（GUI 数据目录 workflows/）；重开后恢复并按子会话当前状态续跑

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rig::client::CompletionClient;
use rig::completion::Prompt;

use serde::{Deserialize, Serialize};

use protocol::{generate_title, ContentBlock, DialogItem, SessionState};

use crate::config::OrchestratorConfig;
use crate::ws::WsClient;

// ---- 编排会话状态（可持久化）----

/// 编排会话中的一条消息（对话历史）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrcMsg {
    /// 用户：计划 / 介入指令
    User { text: String },
    /// 编排 agent 输出（决策摘要）
    Orc { text: String },
    /// 系统事件（子会话完成、创建、错误等）
    System { text: String },
}

/// 子会话（工作流驱动的 agent 会话，由各机器 server 持久化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSession {
    pub id: String,
    /// app.machines 下标（机器归属）
    pub machine_idx: usize,
    pub machine_name: String,
    pub harness: String,
    pub step_desc: String,
    pub state: SessionState,
    pub last_output: String,
}

/// 编排 agent 会话（GUI 本地状态，docs/DESIGN.md §10「持久化与恢复」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrcSession {
    pub id: String,
    pub title: String,
    /// 用户自然语言计划（含 @ 引用展开的上下文）
    pub description: String,
    /// 模板/系统指令（内置进编排 agent 的系统提示词，不进入会话历史；PRD §3.7）
    #[serde(default)]
    pub preamble: String,
    pub state: SessionState,
    pub paused: bool,
    pub done: bool,
    pub transcript: Vec<OrcMsg>,
    pub children: Vec<ChildSession>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前毫秒时间戳（GUI 语音附件命名等用）。
pub fn now_ts() -> u64 {
    now()
}

// ---- 机器信息（供编排上下文与动作解析）----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineSummary {
    pub name: String,
    pub harnesses: Vec<String>,
}

// ---- 编排决策 ----

/// 子会话状态快照（发给编排 agent 的完成情况）。
#[derive(Debug, Clone)]
pub struct ChildStatus {
    pub session_id: String,
    pub machine_name: String,
    pub harness: String,
    pub state: SessionState,
    pub step_desc: String,
    pub last_output: String,
}

/// 编排上下文（每次 decide 的输入）。
#[derive(Debug, Clone)]
pub struct OrcContext {
    pub plan: String,
    /// 模板/系统指令（内置进编排系统提示词，不进入会话历史）
    pub preamble: String,
    /// 对话历史（用户计划/介入、编排输出、系统事件）文本化
    pub transcript: Vec<String>,
    pub children: Vec<ChildStatus>,
    pub machines: Vec<MachineSummary>,
}

/// 编排动作（引擎统一执行；会话操作经真实 WsClient，docs/DESIGN.md §10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrcAction {
    /// 创建/复用子会话并下发指令
    Run {
        machine: String,
        harness: String,
        cwd: String,
        prompt: String,
        /// 复用已存在的子会话 id
        reuse: Option<String>,
    },
    /// 向已有子会话追加指令（介入）
    Steer { session: String, prompt: String },
    /// 向已有子会话发重试指令
    Retry { session: String, prompt: String },
}

/// 单 turn 决策结果。
#[derive(Debug, Clone)]
pub struct Decision {
    pub summary: String,
    pub actions: Vec<OrcAction>,
    /// 编排是否结束（conclude）
    pub done: bool,
    pub conclusion: Option<String>,
}

/// 单 turn 决策器（rig 单 turn 模式，docs/DESIGN.md §10）。
pub trait OrcBackend: Send + Sync {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>>;
}

// ---- rig 工具的规划动作记录 ----

/// rig 工具记录的动作（规划接口；由引擎统一执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOp {
    Create {
        machine: String,
        harness: String,
        cwd: String,
        prompt: String,
    },
    Prompt {
        session: String,
        prompt: String,
    },
}

/// 工具共享状态：机器名列表（工具校验）+ 动作记录。
#[derive(Clone)]
pub struct ToolState {
    machine_names: Vec<String>,
    ops: Arc<Mutex<Vec<ToolOp>>>,
}

impl ToolState {
    pub fn new(machine_names: Vec<String>) -> Self {
        ToolState {
            machine_names,
            ops: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn record(&self, op: ToolOp) {
        self.ops
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .push(op);
    }

    pub fn take_ops(&self) -> Vec<ToolOp> {
        std::mem::take(&mut *self.ops.lock().expect("Mutex 中毒（临界区内不应 panic）"))
    }

    pub fn resolve_machine(&self, name: &str) -> Result<usize, String> {
        self.machine_names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| format!("机器不存在: {name}"))
    }
}

/// 规划动作 → 引擎动作（纯逻辑，可单测）。
pub fn ops_to_actions(ops: Vec<ToolOp>, known_sessions: &[String]) -> Vec<OrcAction> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            ToolOp::Create {
                machine,
                harness,
                cwd,
                prompt,
            } => out.push(OrcAction::Run {
                machine,
                harness,
                cwd,
                prompt,
                reuse: None,
            }),
            ToolOp::Prompt { session, prompt } => {
                if known_sessions.contains(&session) {
                    out.push(OrcAction::Retry { session, prompt });
                } else {
                    out.push(OrcAction::Steer { session, prompt });
                }
            }
        }
    }
    out
}

// ---- WorkflowEngine：状态机 ----

#[derive(Clone)]
pub struct WorkflowEngine {
    pub session: OrcSession,
    backend: Arc<dyn OrcBackend>,
    clients: Vec<WsClient>,
    machines: Vec<MachineSummary>,
    /// 防重入：一次 advance 进行中
    advancing: bool,
    /// advance 期间收到子会话 idle → 稍后补一次
    pending_advance: bool,
}

impl WorkflowEngine {
    /// 新建编排会话并执行首个决策 turn。
    /// - `context`：@ 引用展开的上下文文本（附加到计划后）
    /// - `preamble`：模板/系统指令，内置进编排 agent 的系统提示词（不进入会话历史）
    pub fn new(
        description: &str,
        context: &str,
        preamble: &str,
        backend: Arc<dyn OrcBackend>,
        clients: Vec<WsClient>,
        machines: Vec<MachineSummary>,
    ) -> Self {
        let full = if context.trim().is_empty() {
            description.to_string()
        } else {
            format!("{description}\n\n[上下文]\n{context}")
        };
        // 模板不进会话历史：仅当用户确实有描述/目标时才写入用户消息
        let mut transcript = Vec::new();
        if !description.trim().is_empty() {
            transcript.push(OrcMsg::User {
                text: description.to_string(),
            });
        }
        if !context.trim().is_empty() {
            transcript.push(OrcMsg::System {
                text: "已附加 @ 引用的上下文".into(),
            });
        }
        let t = now();
        let session = OrcSession {
            id: format!("orc_{}", uuid::Uuid::new_v4()),
            title: generate_title(description),
            description: full,
            preamble: preamble.to_string(),
            state: SessionState::Idle,
            paused: false,
            done: false,
            transcript,
            children: Vec::new(),
            created_at: t,
            updated_at: t,
        };
        WorkflowEngine {
            session,
            backend,
            clients,
            machines,
            advancing: false,
            pending_advance: false,
        }
    }

    /// 从持久化状态恢复（GUI 重开）。
    pub fn restore(
        session: OrcSession,
        backend: Arc<dyn OrcBackend>,
        clients: Vec<WsClient>,
        machines: Vec<MachineSummary>,
    ) -> Self {
        WorkflowEngine {
            session,
            backend,
            clients,
            machines,
            advancing: false,
            pending_advance: false,
        }
    }

    /// 首个 turn：拆解计划 → 创建/复用子会话下发指令。
    pub async fn start(&mut self) -> Result<(), String> {
        self.advance().await
    }

    /// 单 turn 决策循环（docs/DESIGN.md §10：每轮一次 Agent::prompt，不阻塞等待子会话）。
    pub async fn advance(&mut self) -> Result<(), String> {
        loop {
            if self.advancing {
                self.pending_advance = true;
                return Ok(());
            }
            if self.session.paused || self.session.done {
                return Ok(());
            }
            self.advancing = true;
            self.session.state = SessionState::Busy;
            self.session.updated_at = now();
            let result = self.do_advance().await;
            self.session.state = SessionState::Idle;
            self.session.updated_at = now();
            self.advancing = false;
            if !self.pending_advance {
                return result;
            }
            // advance 期间收到子会话 idle：补一次推进（循环代替递归避免 async 递归）
            self.pending_advance = false;
            result.as_ref()?;
        }
    }

    async fn do_advance(&mut self) -> Result<(), String> {
        let ctx = self.build_context();
        let decision = match self.backend.decide(&ctx).await {
            Ok(d) => d,
            Err(e) => {
                // 运行期 LLM 调用失败（未配置 API / 网络 / 鉴权等）：
                // 写入编排会话对话历史（System 消息），GUI 可见且随持久化保留
                self.session.transcript.push(OrcMsg::System {
                    text: format!("编排 agent 调用失败：{e}"),
                });
                return Err(e);
            }
        };
        self.session.transcript.push(OrcMsg::Orc {
            text: decision.summary.clone(),
        });
        if let Some(c) = &decision.conclusion {
            self.session.transcript.push(OrcMsg::System {
                text: format!("编排结束：{c}"),
            });
            self.session.done = true;
        }
        self.session.done = self.session.done || decision.done;
        self.apply_actions(decision.actions).await
    }

    fn build_context(&self) -> OrcContext {
        OrcContext {
            plan: self.session.description.clone(),
            preamble: self.session.preamble.clone(),
            transcript: self
                .session
                .transcript
                .iter()
                .map(|m| match m {
                    OrcMsg::User { text } => format!("用户：{text}"),
                    OrcMsg::Orc { text } => format!("编排：{text}"),
                    OrcMsg::System { text } => format!("系统：{text}"),
                })
                .collect(),
            children: self
                .session
                .children
                .iter()
                .map(|c| ChildStatus {
                    session_id: c.id.clone(),
                    machine_name: c.machine_name.clone(),
                    harness: c.harness.clone(),
                    state: c.state,
                    step_desc: c.step_desc.clone(),
                    last_output: c.last_output.clone(),
                })
                .collect(),
            machines: self.machines.clone(),
        }
    }

    /// 执行动作（会话操作经真实 WsClient，docs/DESIGN.md §10）。
    async fn apply_actions(&mut self, actions: Vec<OrcAction>) -> Result<(), String> {
        for action in actions {
            match action {
                OrcAction::Run {
                    machine,
                    harness,
                    cwd,
                    prompt,
                    reuse,
                } => {
                    let m_idx = self.resolve_machine(&machine);
                    let Ok(m_idx) = m_idx else {
                        self.session.transcript.push(OrcMsg::System {
                            text: format!("跳过步骤：{machine} 不可用（{m_idx:?}）"),
                        });
                        continue;
                    };
                    let session_id = match reuse {
                        Some(id) if self.session.children.iter().any(|c| c.id == id) => id,
                        Some(id) => {
                            self.session.transcript.push(OrcMsg::System {
                                text: format!("跳过：复用的子会话不存在 {id}"),
                            });
                            continue;
                        }
                        None => {
                            let res = self
                                .clients
                                .get(m_idx)
                                .ok_or_else(|| "机器连接已失效".to_string())?
                                .request(
                                    protocol::method::CREATE_SESSION,
                                    Some(serde_json::json!({
                                        "harness": harness,
                                        "cwd": cwd,
                                    })),
                                )
                                .await;
                            match res {
                                Ok(v) => v
                                    .get("session")
                                    .and_then(|s| s.get("id"))
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                Err(e) => {
                                    self.session.transcript.push(OrcMsg::System {
                                        text: format!("创建子会话失败（{machine}/{harness}）：{e}"),
                                    });
                                    continue;
                                }
                            }
                        }
                    };
                    if session_id.is_empty() {
                        self.session.transcript.push(OrcMsg::System {
                            text: "创建子会话失败：未返回 session id".into(),
                        });
                        continue;
                    }
                    // 记录子会话（或更新已存在的）
                    if !self.session.children.iter().any(|c| c.id == session_id) {
                        let step_desc = first_line(&prompt);
                        self.session.children.push(ChildSession {
                            id: session_id.clone(),
                            machine_idx: m_idx,
                            machine_name: self.machines[m_idx].name.clone(),
                            harness: harness.clone(),
                            step_desc,
                            state: SessionState::Busy,
                            last_output: String::new(),
                        });
                        self.session.transcript.push(OrcMsg::System {
                            text: format!(
                                "步骤：在 {} 用 {} 创建子会话 {session_id} 并下发指令",
                                self.machines[m_idx].name, harness
                            ),
                        });
                    }
                    let _ = self.prompt_child(&session_id, &prompt).await;
                }
                OrcAction::Steer { session, prompt } | OrcAction::Retry { session, prompt } => {
                    self.session.transcript.push(OrcMsg::System {
                        text: format!("介入子会话 {session}"),
                    });
                    let _ = self.prompt_child(&session, &prompt).await;
                }
            }
        }
        Ok(())
    }

    async fn prompt_child(&mut self, session_id: &str, text: &str) -> Result<(), String> {
        let Some(child) = self.session.children.iter().find(|c| c.id == session_id) else {
            self.session.transcript.push(OrcMsg::System {
                text: format!("子会话不存在：{session_id}"),
            });
            return Err(format!("子会话不存在: {session_id}"));
        };
        let client = self
            .clients
            .get(child.machine_idx)
            .cloned()
            .ok_or_else(|| "机器连接已失效".to_string())?;
        let res = client
            .request(
                protocol::method::PROMPT,
                Some(serde_json::json!({
                    "sessionId": session_id,
                    "input": [{ "type": "text", "text": text }],
                })),
            )
            .await;
        if let Some(c) = self
            .session
            .children
            .iter_mut()
            .find(|c| c.id == session_id)
        {
            c.state = SessionState::Busy;
        }
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                self.session.transcript.push(OrcMsg::System {
                    text: format!("下发指令失败：{e}"),
                });
                Err(e.to_string())
            }
        }
    }

    /// 机器名 → 下标（未知机器回退第一个可用）。
    fn resolve_machine(&self, name: &str) -> Result<usize, String> {
        if let Some(i) = self.machines.iter().position(|m| m.name == name) {
            return Ok(i);
        }
        if !self.machines.is_empty() {
            return Ok(0);
        }
        Err("没有可用机器".into())
    }

    /// 子会话状态变更（GUI 收到 server 通知时调用）。
    /// 子会话变 idle → 向编排会话注入完成情况并推进（docs/DESIGN.md §10 自动推进）。
    /// `output_excerpt`：子会话最近一次完整输出摘要（GUI 本地对话缓存）。
    /// 返回是否触发了推进。
    pub async fn on_child_state(
        &mut self,
        session_id: &str,
        state: SessionState,
        output_excerpt: Option<String>,
    ) -> bool {
        let Some(child) = self
            .session
            .children
            .iter_mut()
            .find(|c| c.id == session_id)
        else {
            return false;
        };
        if let Some(o) = output_excerpt {
            child.last_output = o;
        }
        child.state = state;
        if state == SessionState::Idle {
            let step = child.step_desc.clone();
            let last = child.last_output.clone();
            self.session.transcript.push(OrcMsg::System {
                text: format!(
                    "子会话 {session_id} 完成（{step}）：{}",
                    excerpt(&last, 120)
                ),
            });
            if !self.session.paused && !self.session.done {
                let _ = self.advance().await;
                return true;
            }
        }
        false
    }

    // ---- 异步任务与 GUI 的交接（引擎保持原位，克隆体执行异步推进）----
    // GUI 发起异步编排任务时，引擎始终留在 self.workflows 中（会话行与对话历史
    // 立即可见）；异步推进在克隆体上执行，完成后原位换回。以下方法维护防重入
    // 标记与同步可见的状态。

    /// 是否正在推进（异步推进进行中；GUI 据此跳过并发的自动推进/介入推进）。
    pub fn is_advancing(&self) -> bool {
        self.advancing
    }

    /// 异步任务开始前调用（GUI 主线程同步执行）：标记忙并上防重入锁，
    /// 使会话行/对话历史立即显示工作状态。
    pub fn begin_busy(&mut self) {
        self.advancing = true;
        self.pending_advance = false;
        self.session.state = SessionState::Busy;
        self.session.updated_at = now();
    }

    /// 克隆体开始推进前调用：解除 begin_busy 的防重入标记
    /// （advance 自身会重新置位，保证克隆体内一次只跑一轮）。
    pub fn start_advance(&mut self) {
        self.advancing = false;
        self.pending_advance = false;
    }

    /// 异步任务中断（oneshot 失效）时复位，避免永久卡在 Busy/防重入。
    pub fn abort_busy(&mut self) {
        self.advancing = false;
        self.pending_advance = false;
        if self.session.state == SessionState::Busy {
            self.session.state = SessionState::Idle;
        }
        self.session.updated_at = now();
    }

    /// 同步记录用户介入指令（立即在 GUI 可见，不等待 LLM）。
    /// 返回是否应触发推进：未暂停、未完成且没有推进在进行中。
    pub fn record_user(&mut self, text: &str) -> bool {
        self.session.transcript.push(OrcMsg::User {
            text: text.to_string(),
        });
        self.session.updated_at = now();
        !self.session.paused && !self.session.done && !self.advancing
    }

    /// 暂停/继续。继续时返回 true（调用方应触发一次推进）。
    pub fn set_paused(&mut self, paused: bool) -> bool {
        self.session.paused = paused;
        self.session.transcript.push(OrcMsg::System {
            text: if paused {
                "已暂停".into()
            } else {
                "已继续".into()
            },
        });
        !paused && !self.session.done
    }

    // ---- 持久化 ----

    pub fn persist(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", self.session.id));
        let json = serde_json::to_string_pretty(&self.session)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(path, json)
    }

    /// 加载目录下全部编排会话状态（GUI 重开恢复）。
    pub fn load_all(dir: &Path) -> Vec<OrcSession> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<OrcSession>(&s).ok())
            .collect()
    }

    /// 删除编排会话（连同持久化文件）。
    pub fn remove(dir: &Path, id: &str) {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

impl OrcSession {
    /// 编排会话对话流：用户消息与编排输出（系统事件不进入对话流，PRD §4.1.3）。
    pub fn to_dialog_items(&self) -> Vec<DialogItem> {
        self.transcript
            .iter()
            .filter_map(|m| match m {
                OrcMsg::User { text } => Some(DialogItem::UserMessage {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: self.updated_at,
                }),
                OrcMsg::Orc { text } => Some(DialogItem::AgentOutput {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: self.updated_at,
                }),
                OrcMsg::System { .. } => None,
            })
            .collect()
    }
}

fn excerpt(s: &str, max: usize) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

// ---- RigBackend：真实 rig 单 turn 编排（docs/DESIGN.md §10）----

pub struct RigBackend {
    cfg: OrchestratorConfig,
    tool_state: ToolState,
}

impl RigBackend {
    pub fn new(cfg: OrchestratorConfig, machine_names: Vec<String>) -> Self {
        RigBackend {
            cfg,
            tool_state: ToolState::new(machine_names),
        }
    }

    /// 构建 rig agent（单 turn：preamble + 规划工具）。
    /// 泛型于模型（chat_completions / messages 两个 API Backend，PRD §4.3）。
    /// 暴露为 pub 供测试验证 rig 装配（工具注册、模型构建）真实可用。
    pub fn build_agent<M>(&self, model: M, preamble: &str) -> rig::Agent<M>
    where
        M: rig::completion::CompletionModel + 'static,
    {
        rig::AgentBuilder::new(model)
            .preamble(preamble)
            .tool(PlanCreateSession)
            .tool(PlanPromptSession)
            .build()
    }

    fn preamble(&self) -> String {
        "你是 amux 的编排 agent。你的职责是把用户的自然语言工作流拆解为步骤，\
         为每步选择机器与 agent（或沿用用户指定），创建/复用子会话并下发指令，\
         依据各步结果决定后续（分支/重试/汇总）。你只做编排、不做具体实现。\n\
         规则：\n\
         1. 每个步骤用 create_session 工具规划（创建子会话并下发该步指令）；\
         需要继续推进已有子会话时用 prompt_session 工具。\n\
         2. 不要阻塞等待子会话结果：规划完本轮步骤后，用一段中文文本总结你的决策\
         并结束 turn。子会话完成后系统会再次调用你评估结果并推进。\n\
         3. 机器与 agent 必须从上下文中给出的列表里选择。\n\
         4. 全部步骤完成后，输出总结论并结束。"
            .to_string()
    }
}

/// 运行一次单 turn 编排（rig `Agent::prompt`，docs/DESIGN.md §10 单 turn 模式）。
async fn run_orc_turn<M>(
    agent: rig::Agent<M>,
    input: String,
    tool_ctx: rig::tool::ToolContext,
) -> Result<String, String>
where
    M: rig::completion::CompletionModel + 'static,
{
    agent
        .prompt(input)
        .tool_context(tool_ctx)
        .await
        .map_err(|e| format!("编排 agent 调用失败: {e}"))
}

impl OrcBackend for RigBackend {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>> {
        Box::pin(async move {
            // 配置校验：Base URL / API key 缺失时给出可操作的提示（PRD §4.3）
            if !self.cfg.is_configured() {
                return Err(
                    "未配置编排 agent API（Base URL / API key / 模型）。请在设置 → 编排 agent 中配置后再创建工作流"
                        .to_string(),
                );
            }
            // 模板/系统指令内置进编排系统提示词（不进入会话历史，PRD §3.7）
            let mut preamble = self.preamble();
            if !ctx.preamble.trim().is_empty() {
                preamble.push_str("\n\n");
                preamble.push_str("【工作流模板/执行要求】\n");
                preamble.push_str(ctx.preamble.trim());
            }
            let client = rig::providers::openai::Client::builder()
                .api_key(self.cfg.api_key.clone())
                .base_url(self.cfg.base_url.clone())
                .build()
                .map_err(|e| format!("构建 OpenAI client 失败: {e}"))?;
            let model = self.cfg.model.clone();
            let mut tool_ctx = rig::tool::ToolContext::new();
            tool_ctx.insert(self.tool_state.clone());
            let machine_list = ctx
                .machines
                .iter()
                .map(|m| format!("{}（agent: {}）", m.name, m.harnesses.join(", ")))
                .collect::<Vec<_>>()
                .join("；");
            let children_list = if ctx.children.is_empty() {
                "（尚无子会话）".to_string()
            } else {
                ctx.children
                    .iter()
                    .map(|c| {
                        format!(
                            "{} [{}@{}]（步骤：{}）{}：{}",
                            c.session_id,
                            c.harness,
                            c.machine_name,
                            c.step_desc,
                            state_label(c.state),
                            excerpt(&c.last_output, 200)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let transcript_text = if ctx.transcript.is_empty() {
                "（无）".to_string()
            } else {
                ctx.transcript.join("\n")
            };
            let input = format!(
                "工作流计划：\n{}\n\n可用机器：{}\n\n对话历史：\n{}\n\n子会话当前状态：\n{}\n\n\
                 请评估当前进展并决定本轮动作（继续创建/推进步骤，或结束）。",
                ctx.plan, machine_list, transcript_text, children_list
            );
            // API Backend（PRD §4.3）：chat_completions / messages 两种模型类型分别装配
            let text = match self.cfg.api_backend.as_str() {
                "messages" => {
                    let agent = self.build_agent(client.completion_model(model), &preamble);
                    run_orc_turn(agent, input, tool_ctx).await?
                }
                _ => {
                    let agent = self
                        .build_agent(client.completions_api().completion_model(model), &preamble);
                    run_orc_turn(agent, input, tool_ctx).await?
                }
            };
            let ops = self.tool_state.take_ops();
            let known: Vec<String> = ctx.children.iter().map(|c| c.session_id.clone()).collect();
            let actions = ops_to_actions(ops, &known);
            Ok(Decision {
                summary: text,
                actions,
                done: false,
                conclusion: None,
            })
        })
    }
}

fn state_label(s: SessionState) -> &'static str {
    match s {
        SessionState::Idle => "idle",
        SessionState::Busy => "busy",
    }
}

/// 脚本化测试后端（决策序列；用于驱动引擎的纯逻辑/集成测试）。
#[doc(hidden)]
#[allow(dead_code)]
pub struct FakeBackend {
    decisions: Mutex<VecDeque<Decision>>,
}

#[doc(hidden)]
#[allow(dead_code, clippy::new_ret_no_self)]
impl FakeBackend {
    pub fn new(decisions: Vec<Decision>) -> Arc<dyn OrcBackend> {
        Arc::new(FakeBackend {
            decisions: Mutex::new(VecDeque::from(decisions)),
        })
    }

    /// 空决策后端（仅用于构造引擎，不触发推进）。
    pub fn new_for_tests() -> Arc<dyn OrcBackend> {
        Self::new(vec![])
    }
}

impl OrcBackend for FakeBackend {
    fn decide<'a>(
        &'a self,
        _ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>> {
        Box::pin(async move {
            self.decisions
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "决策用尽".to_string())
        })
    }
}

// ---- rig 规划工具 ----

/// 规划工具：create_session（在指定机器创建子会话并下发一条步骤指令）。
/// 工具只记录规划动作（ToolOp），由引擎统一经真实 WsClient 执行（docs/DESIGN.md §10）。
struct PlanCreateSession;
impl rig::tool::Tool for PlanCreateSession {
    const NAME: &'static str = "create_session";
    type Args = CreateSessionArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "在指定机器上规划创建子会话并下发一条步骤指令（机器与 agent 必须取自上下文列表）".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "machine": { "type": "string", "description": "机器名" },
                "harness": { "type": "string", "description": "agent 名" },
                "cwd": { "type": "string", "description": "工作目录" },
                "prompt": { "type": "string", "description": "该步指令" }
            },
            "required": ["machine", "harness", "cwd", "prompt"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: CreateSessionArgs,
    ) -> Result<Self::Output, Self::Error> {
        let state = context
            .get::<ToolState>()
            .ok_or_else(|| rig::tool::ToolExecutionError::other("缺少工具状态"))?;
        state
            .resolve_machine(&args.machine)
            .map_err(rig::tool::ToolExecutionError::other)?;
        state.record(ToolOp::Create {
            machine: args.machine,
            harness: args.harness,
            cwd: args.cwd,
            prompt: args.prompt,
        });
        Ok("已规划：创建子会话并下发指令（引擎将在本轮结束后执行）".into())
    }
}

#[derive(serde::Deserialize)]
struct CreateSessionArgs {
    machine: String,
    harness: String,
    cwd: String,
    prompt: String,
}

/// 规划工具：prompt_session（向已有子会话发送指令：介入/重试/追加）。
struct PlanPromptSession;
impl rig::tool::Tool for PlanPromptSession {
    const NAME: &'static str = "prompt_session";
    type Args = PromptSessionArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "向已存在的子会话发送一条指令（介入/重试/追加要求）".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string", "description": "子会话 id" },
                "prompt": { "type": "string", "description": "指令内容" }
            },
            "required": ["session", "prompt"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: PromptSessionArgs,
    ) -> Result<Self::Output, Self::Error> {
        let state = context
            .get::<ToolState>()
            .ok_or_else(|| rig::tool::ToolExecutionError::other("缺少工具状态"))?;
        state.record(ToolOp::Prompt {
            session: args.session,
            prompt: args.prompt,
        });
        Ok("已规划：向子会话发送指令（引擎将在本轮结束后执行）".into())
    }
}

#[derive(serde::Deserialize)]
struct PromptSessionArgs {
    session: String,
    prompt: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::tool::Tool as _;

    fn run(machine: &str, harness: &str, prompt: &str) -> OrcAction {
        OrcAction::Run {
            machine: machine.into(),
            harness: harness.into(),
            cwd: "/tmp/work".into(),
            prompt: prompt.into(),
            reuse: None,
        }
    }

    fn machines() -> Vec<MachineSummary> {
        vec![MachineSummary {
            name: "测试机".into(),
            harnesses: vec!["mock_acp".into()],
        }]
    }

    fn clients_with_machines() -> (Vec<WsClient>, MachineSummary) {
        // 无 server 的测试用：客户端连接失败不影响（引擎只在其上执行动作）
        let m = machines().remove(0);
        let c = WsClient::connect("ws://127.0.0.1:1/?token=unused".into());
        (vec![c], m)
    }

    // ---- 纯逻辑 ----

    #[test]
    fn ops_to_actions_maps_create_and_prompt() {
        let ops = vec![
            ToolOp::Create {
                machine: "本机".into(),
                harness: "codex".into(),
                cwd: "/p".into(),
                prompt: "实现功能".into(),
            },
            ToolOp::Prompt {
                session: "s_known".into(),
                prompt: "重试".into(),
            },
            ToolOp::Prompt {
                session: "s_unknown".into(),
                prompt: "介入".into(),
            },
        ];
        let actions = ops_to_actions(ops, &["s_known".to_string()]);
        assert_eq!(actions.len(), 3);
        match &actions[0] {
            OrcAction::Run {
                machine,
                prompt,
                reuse,
                ..
            } => {
                assert_eq!(machine, "本机");
                assert_eq!(prompt, "实现功能");
                assert!(reuse.is_none());
            }
            _ => panic!("Create 应映射为 Run"),
        }
        match &actions[1] {
            OrcAction::Retry { session, .. } => assert_eq!(session, "s_known"),
            _ => panic!("已知会话应映射为 Retry"),
        }
        match &actions[2] {
            OrcAction::Steer { session, .. } => assert_eq!(session, "s_unknown"),
            _ => panic!("未知会话应映射为 Steer（引擎跳过并告警）"),
        }
    }

    #[test]
    fn title_generated_from_description() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new(
            "实现登录功能\n然后写测试",
            "",
            "",
            backend,
            clients,
            vec![m],
        );
        assert_eq!(engine.session.title, "实现登录功能");
        assert!(!engine.session.id.is_empty());
        assert!(engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text } if text == "实现登录功能\n然后写测试")));
    }

    #[test]
    fn context_appended_to_description() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new(
            "实现功能",
            "@src/main.rs 的内容……",
            "",
            backend,
            clients,
            vec![m],
        );
        assert!(engine.session.description.contains("[上下文]"));
        assert!(engine.session.description.contains("src/main.rs"));
    }

    // ---- 状态机：暂停/继续/介入（不依赖 server 的部分）----

    #[tokio::test]
    async fn pause_blocks_advance_steer_records() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "无动作".into(),
            actions: vec![],
            done: false,
            conclusion: None,
        }]);
        let mut engine = WorkflowEngine::new("计划", "", "", backend, clients, vec![m]);
        engine.set_paused(true);
        assert!(engine.session.paused);
        // 暂停时不推进（决策用尽也不会被消费）
        engine.advance().await.unwrap();
        assert!(!engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { .. })));
        // 介入指令在暂停时仅记录
        engine.record_user("先做 A");
        assert!(engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text } if text == "先做 A")));
        // 继续后推进
        let should_advance = engine.set_paused(false);
        assert!(should_advance);
        engine.advance().await.unwrap();
        assert!(engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { text } if text == "无动作")));
    }

    #[tokio::test]
    async fn conclude_marks_done() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "完成".into(),
            actions: vec![],
            done: true,
            conclusion: Some("全部步骤完成".into()),
        }]);
        let mut engine = WorkflowEngine::new("计划", "", "", backend, clients, vec![m]);
        engine.start().await.unwrap();
        assert!(engine.session.done);
        assert!(engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::System { text } if text.contains("编排结束"))));
    }

    // ---- 持久化往返 ----

    #[tokio::test]
    async fn persistence_roundtrip_and_restore() {
        let dir = std::env::temp_dir().join(format!("amux-wf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new("计划A", "", "", backend, clients, vec![m]);
        let id = engine.session.id.clone();
        engine.persist(&dir).unwrap();

        // 重新加载：会话状态往返
        let sessions = WorkflowEngine::load_all(&dir);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].title, "计划A");

        // restore：模拟 GUI 重开后恢复引擎并继续推进
        let backend2 = FakeBackend::new(vec![Decision {
            summary: "恢复后推进".into(),
            actions: vec![],
            done: true,
            conclusion: None,
        }]);
        let (clients2, m2) = clients_with_machines();
        let mut engine2 =
            WorkflowEngine::restore(sessions[0].clone(), backend2, clients2, vec![m2]);
        engine2.start().await.unwrap();
        assert!(engine2
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { text } if text == "恢复后推进")));

        // 删除
        WorkflowEngine::remove(&dir, &id);
        assert!(WorkflowEngine::load_all(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 真实 server 集成（test-server/mock_acp + 真实 WsClient）----

    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

    fn test_server_bin() -> std::path::PathBuf {
        // 优先 CARGO_MANIFEST_DIR 相对路径；回退当前可执行同目录
        let via_manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/test-server");
        if via_manifest.exists() {
            return via_manifest;
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("test-server")))
            .unwrap_or_default()
    }

    struct ServerGuard {
        child: tokio::process::Child,
    }
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
        }
    }

    async fn spawn_test_server() -> (u16, ServerGuard, MachineSummary) {
        let bin = test_server_bin();
        assert!(bin.exists(), "test-server 不存在: {}", bin.display());
        let port =
            38000 + (std::process::id() % 400) as u16 + NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        // 唯一 mock 状态 + 关闭自动发现：隔离本机真实 agent（避免重启恢复拉入其会话、
        // 拖慢启动），保证测试确定性（AMUX_NO_DISCOVERY=1）
        let state = std::env::temp_dir().join(format!(
            "amux-wf-mock-{}-{}.state",
            std::process::id(),
            NEXT_PORT.load(Ordering::SeqCst)
        ));
        let child = tokio::process::Command::new(bin)
            .args(["--token", "test-token", "--port", &port.to_string()])
            .env("AMUX_MOCK_STATE", &state)
            .env("AMUX_NO_DISCOVERY", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn test-server");
        // 等待端口就绪（SQLite 打开 + mock 拉起负载下放宽到 20s）
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return (
                    port,
                    ServerGuard { child },
                    MachineSummary {
                        name: "测试机".into(),
                        harnesses: vec!["mock_acp".into()],
                    },
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("test-server 未就绪");
    }

    fn ws_client(port: u16) -> WsClient {
        WsClient::connect(format!("ws://127.0.0.1:{port}/?token=test-token"))
    }

    async fn wait_child_idle(client: &WsClient, session_id: &str, timeout_ms: u64) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let res = client
                .request(protocol::method::LIST_SESSIONS, Some(serde_json::json!({})))
                .await
                .unwrap();
            let idle = res["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"].as_str() == Some(session_id))
                .map(|s| s["state"].as_str() == Some("idle"))
                .unwrap_or(false);
            if idle {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "子会话 {session_id} 未在超时内 idle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn engine_start_creates_and_prompts_child_via_real_server() {
        let (port, _guard, m) = spawn_test_server().await;
        let client = ws_client(port);
        let backend = FakeBackend::new(vec![Decision {
            summary: "第一步：在测试机创建子会话实现登录".into(),
            actions: vec![run("测试机", "mock_acp", "实现登录功能")],
            done: false,
            conclusion: None,
        }]);
        let mut engine = WorkflowEngine::new(
            "在测试机用 mock_acp 实现登录功能",
            "",
            "",
            backend,
            vec![client.clone()],
            vec![m],
        );
        engine.start().await.unwrap();

        // 子会话已创建并下发指令（真实 server 上存在该会话，且首条 prompt 后标题非空）
        assert_eq!(engine.session.children.len(), 1, "应创建一个子会话");
        let child = &engine.session.children[0];
        assert_eq!(child.machine_name, "测试机");
        assert_eq!(child.harness, "mock_acp");
        assert_eq!(child.step_desc, "实现登录功能");

        let res = client
            .request(protocol::method::LIST_SESSIONS, Some(serde_json::json!({})))
            .await
            .unwrap();
        let meta = res["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"].as_str() == Some(child.id.as_str()))
            .expect("子会话应在 server 上");
        assert!(
            !meta["title"].as_str().unwrap_or("").is_empty(),
            "子会话标题应非空（首条指令生成）"
        );

        // 打开子会话：返回透传事件（用户消息 + 输出 chunk），GUI 聚合后含下发的指令与
        // mock 的完整输出（真实交付路径，docs/DESIGN.md §5.1/§5.2）
        let open = client
            .request(
                protocol::method::OPEN_SESSION,
                Some(serde_json::json!({ "sessionId": child.id })),
            )
            .await
            .unwrap();
        let events = open["events"].as_array().unwrap();
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| {
                let t = match e["kind"].as_str() {
                    Some("user_message") => e["content"]
                        .as_array()?
                        .iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(""),
                    Some("output_chunk") => e["text"].as_str()?.to_string(),
                    _ => return None,
                };
                Some(t)
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("实现登录功能")),
            "子会话应收到指令: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("完成")),
            "子会话应有 agent 输出: {texts:?}"
        );
    }

    #[tokio::test]
    async fn child_idle_triggers_auto_advance_and_intervene() {
        let (port, _guard, m) = spawn_test_server().await;
        let client = ws_client(port);
        let backend = FakeBackend::new(vec![
            // turn 1：创建子会话
            Decision {
                summary: "创建子会话".into(),
                actions: vec![run("测试机", "mock_acp", "第一步")],
                done: false,
                conclusion: None,
            },
            // turn 2：子会话完成后推进下一步（并发起介入到已存在子会话）
            Decision {
                summary: "第一步完成，推进第二步并介入".into(),
                actions: vec![
                    OrcAction::Run {
                        machine: "测试机".into(),
                        harness: "mock_acp".into(),
                        cwd: "/tmp/work".into(),
                        prompt: "第二步".into(),
                        reuse: None,
                    },
                    OrcAction::Retry {
                        session: "unknown-session".into(), // 引擎应跳过并告警
                        prompt: "重试".into(),
                    },
                ],
                done: true,
                conclusion: Some("工作流完成".into()),
            },
        ]);
        let mut engine =
            WorkflowEngine::new("两步工作流", "", "", backend, vec![client.clone()], vec![m]);
        engine.start().await.unwrap();
        assert_eq!(engine.session.children.len(), 1);
        let child_id = engine.session.children[0].id.clone();

        // 子会话完成（mock 的 turn 已结束）→ 自动推进
        wait_child_idle(&client, &child_id, 5000).await;
        let advanced = engine
            .on_child_state(&child_id, SessionState::Idle, Some("第一步输出".into()))
            .await;
        assert!(advanced, "子会话 idle 应触发自动推进");

        // turn 2 创建第二个子会话；未知会话介入被跳过并记录告警
        assert_eq!(engine.session.children.len(), 2, "应创建第二个子会话");
        assert!(
            engine
                .session
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::System { text } if text.contains("子会话不存在"))),
            "未知会话介入应记录告警"
        );
        assert!(engine.session.done, "conclude 后编排应结束");
        assert_eq!(engine.session.children[0].last_output, "第一步输出");
    }

    #[tokio::test]
    async fn restore_resumes_auto_advance_after_restart() {
        let (port, _guard, m) = spawn_test_server().await;
        let client = ws_client(port);
        let backend = FakeBackend::new(vec![
            Decision {
                summary: "创建子会话".into(),
                actions: vec![run("测试机", "mock_acp", "任务")],
                done: false,
                conclusion: None,
            },
            Decision {
                summary: "恢复后推进".into(),
                actions: vec![],
                done: true,
                conclusion: Some("完成".into()),
            },
        ]);
        let mut engine = WorkflowEngine::new(
            "任务",
            "",
            "",
            backend,
            vec![client.clone()],
            vec![m.clone()],
        );
        engine.start().await.unwrap();
        let child_id = engine.session.children[0].id.clone();
        let saved = engine.session.clone(); // 模拟持久化

        // 模拟 GUI 重开：restore 引擎，喂入子会话当前状态（idle）→ 恢复自动推进
        let backend2 = FakeBackend::new(vec![Decision {
            summary: "恢复后推进".into(),
            actions: vec![],
            done: true,
            conclusion: Some("完成".into()),
        }]);
        let mut restored =
            WorkflowEngine::restore(saved, backend2, vec![client.clone()], vec![m.clone()]);
        wait_child_idle(&client, &child_id, 5000).await;
        let advanced = restored
            .on_child_state(&child_id, SessionState::Idle, None)
            .await;
        assert!(advanced, "重开后子会话 idle 应恢复自动推进");
        assert!(restored.session.done);
    }

    // ---- rig 装配（真实 rig 构建，无网络）----

    #[test]
    fn rig_backend_builds_agent_with_tools() {
        let backend = RigBackend::new(
            OrchestratorConfig {
                api_backend: "chat_completions".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: "sk-test".into(),
                model: "gpt-4o-mini".into(),
            },
            vec!["测试机".into()],
        );
        let client = rig::providers::openai::Client::builder()
            .api_key("sk-test")
            .base_url("http://127.0.0.1:9/v1")
            .build()
            .expect("构建 openai client");
        let agent = backend.build_agent(client.completion_model("gpt-4o-mini"), "preamble");
        assert!(agent.name().is_none()); // 未设置 name
    }

    #[test]
    fn orc_session_to_dialog_items_filters_system() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            vec![],
            vec![MachineSummary {
                name: "测试机".into(),
                harnesses: vec!["mock_acp".into()],
            }],
        );
        let mut engine = engine;
        engine.session.transcript.push(OrcMsg::Orc {
            text: "决策".into(),
        });
        engine.session.transcript.push(OrcMsg::System {
            text: "系统事件".into(),
        });
        let dialog = engine.session.to_dialog_items();
        // 用户计划 + 编排输出；系统事件不进对话流
        assert_eq!(dialog.len(), 2);
        assert!(matches!(&dialog[0], DialogItem::UserMessage { .. }));
        assert!(matches!(&dialog[1], DialogItem::AgentOutput { .. }));
    }

    #[test]
    fn template_as_preamble_not_in_history() {
        // 从模板创建：模板作为系统提示词（preamble），不进入会话历史（PRD §3.7）
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "",
            "",
            "模板：先在测试机实现，再审查",
            backend,
            vec![],
            vec![MachineSummary {
                name: "测试机".into(),
                harnesses: vec!["mock_acp".into()],
            }],
        );
        assert!(engine.session.transcript.is_empty(), "模板不应进入会话历史");
        assert_eq!(engine.session.preamble, "模板：先在测试机实现，再审查");
        assert!(engine.session.title.is_empty(), "无用户目标时不生成标题");

        // 用户目标 + 模板：历史只含用户目标
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "本次只做第一步",
            "",
            "模板：先实现后审查",
            backend,
            vec![],
            vec![MachineSummary {
                name: "测试机".into(),
                harnesses: vec!["mock_acp".into()],
            }],
        );
        assert_eq!(engine.session.transcript.len(), 1);
        assert!(matches!(
            &engine.session.transcript[0],
            OrcMsg::User { text } if text == "本次只做第一步"
        ));
    }

    #[tokio::test]
    async fn unconfigured_rig_backend_records_clear_error() {
        // 编排 agent 未配置 API：start() 失败但错误写入对话历史（System 消息），
        // 会话不处于假忙状态、可继续操作（PRD §4.3 配置校验）
        let backend = Arc::new(RigBackend::new(
            OrchestratorConfig {
                api_backend: "chat_completions".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: String::new(), // 未配置
                model: "gpt-4o-mini".into(),
            },
            vec!["测试机".into()],
        ));
        let client = WsClient::connect("ws://127.0.0.1:1/?token=unused".into());
        let mut engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            vec![client],
            vec![MachineSummary {
                name: "测试机".into(),
                harnesses: vec!["kimi".into()],
            }],
        );
        let res = engine.start().await;
        assert!(res.is_err(), "未配置时应失败");
        assert!(engine.session.transcript.iter().any(
            |m| matches!(m, OrcMsg::System { text } if text.contains("未配置编排 agent API"))
        ));
        assert_eq!(
            engine.session.state,
            SessionState::Idle,
            "失败后不应停留在忙碌状态"
        );
        assert!(!engine.session.done);
    }

    #[tokio::test]
    async fn rig_tools_record_ops_via_tool_context() {
        let state = ToolState::new(vec!["测试机".into()]);
        let mut ctx = rig::tool::ToolContext::new();
        ctx.insert(state.clone());

        // 真实工具实现直驱：create_session 工具
        let res = PlanCreateSession
            .call(
                &mut ctx,
                CreateSessionArgs {
                    machine: "测试机".into(),
                    harness: "mock_acp".into(),
                    cwd: "/tmp".into(),
                    prompt: "实现".into(),
                },
            )
            .await;
        assert!(res.is_ok());
        // prompt_session 工具
        let res = PlanPromptSession
            .call(
                &mut ctx,
                PromptSessionArgs {
                    session: "s1".into(),
                    prompt: "重试".into(),
                },
            )
            .await;
        assert!(res.is_ok());

        let ops = state.take_ops();
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], ToolOp::Create { machine, .. } if machine == "测试机"));
        assert!(matches!(&ops[1], ToolOp::Prompt { session, .. } if session == "s1"));

        // 未知机器报错
        let res = PlanCreateSession
            .call(
                &mut ctx,
                CreateSessionArgs {
                    machine: "不存在".into(),
                    harness: "h".into(),
                    cwd: "/tmp".into(),
                    prompt: "p".into(),
                },
            )
            .await;
        assert!(res.is_err());
    }
}
