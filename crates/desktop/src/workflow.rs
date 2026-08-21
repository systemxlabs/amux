//! 工作流引擎（docs/DESIGN.md §10 / §「工作流会话驱动」「工作流会话存储」）：
//! GUI 本地工作流会话 + rig 单 turn 编排。
//!
//! - `OrcSession`：工作流会话状态，可序列化持久化（`.jsonl` 单文件）
//! - `OrcBackend`：单 turn 决策器；真实实现 `RigBackend` 用 rig `Agent::prompt`
//! - `WorkflowEngine`：状态机——首 turn 拆解计划并创建/复用关联普通会话下发指令；
//!   关联普通会话 idle（`session.state_change` 通知驱动）触发自动推进
//! - 会话操作统一经真实 WsClient（SESSION_NEW / SESSION_PROMPT / SESSION_CANCEL）

#[cfg(test)]
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rig::client::CompletionClient;
use rig::completion::Prompt;

use serde::{Deserialize, Serialize};

use protocol::{generate_title, Activity, ContentBlock, SessionState};

use crate::config::OrchestratorConfig;
use crate::logic::DialogMsg;
use crate::ws::WsClient;

// ---- 工作流会话状态（可持久化）----

/// 工作流会话中的一条消息（对话历史）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrcMsg {
    User { text: String },
    Orc { text: String },
}

/// 关联普通会话（工作流驱动的普通会话，由各机器 server 持久化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSession {
    pub id: String,
    pub machine_idx: usize,
    pub machine_name: String,
    pub agent: String,
    pub step_desc: String,
    pub state: SessionState,
    pub last_output: String,
    #[serde(default)]
    pub last_active_at: u64,
}

/// 工作流会话（GUI 本地状态，docs/DESIGN.md「工作流会话存储」）。
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
    #[serde(default)]
    pub cancelled: bool,
    pub done: bool,
    pub transcript: Vec<OrcMsg>,
    pub children: Vec<ChildSession>,
    #[serde(default)]
    pub activities: Vec<Activity>,
    pub created_at: u64,
    pub updated_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前毫秒时间戳（GUI 附件命名等用）。
pub fn now_ts() -> u64 {
    now()
}

// ---- 机器信息（供编排上下文与动作解析）----

/// 某机器上的一个 agent（docs/DESIGN.md `list_agents`：可用性）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSlot {
    pub name: String,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineSummary {
    pub name: String,
    /// 机器是否在线
    #[serde(default)]
    pub online: bool,
    pub agents: Vec<AgentSlot>,
}

impl MachineSummary {
    #[cfg(test)]
    pub fn named(name: &str, agents: &[&str]) -> Self {
        MachineSummary {
            name: name.into(),
            online: true,
            agents: agents
                .iter()
                .map(|a| AgentSlot {
                    name: (*a).into(),
                    available: true,
                })
                .collect(),
        }
    }
}

// ---- 编排决策 ----

/// 编排上下文（每次 decide 的输入）。
#[derive(Debug, Clone)]
pub struct OrcContext {
    pub plan: String,
    pub preamble: String,
    pub transcript: Vec<String>,
    pub child_sessions: Vec<ChildSession>,
    pub clients: Vec<WsClient>,
    pub machines: Vec<MachineSummary>,
}

/// 编排动作（引擎统一执行；会话操作经真实 WsClient，docs/DESIGN.md §10）。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrcAction {
    Run {
        machine: String,
        agent: String,
        cwd: String,
        prompt: String,
        reuse: Option<String>,
    },
    Steer {
        session: String,
        prompt: String,
    },
    Retry {
        session: String,
        prompt: String,
    },
}

/// 单 turn 决策结果。
#[derive(Debug, Clone)]
pub struct Decision {
    pub summary: String,
    pub actions: Vec<OrcAction>,
    pub done: bool,
    pub conclusion: Option<String>,
}

/// 单 turn 决策器（rig 单 turn 模式，docs/DESIGN.md §10）。
pub trait OrcBackend: Send + Sync {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>>;
    fn take_synced_children(&self) -> Option<Vec<ChildSession>> {
        None
    }
    fn take_synced_activities(&self) -> Option<Vec<Activity>> {
        None
    }
}

// ---- 工具规划动作记录 ----

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOp {
    Create {
        machine: String,
        agent: String,
        cwd: String,
        prompt: String,
    },
    Prompt {
        session: String,
        prompt: String,
    },
}

/// 规划动作 → 引擎动作（纯逻辑，可单测）。
#[allow(dead_code)]
pub fn ops_to_actions(ops: Vec<ToolOp>, known_sessions: &[String]) -> Vec<OrcAction> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            ToolOp::Create {
                machine,
                agent,
                cwd,
                prompt,
            } => out.push(OrcAction::Run {
                machine,
                agent,
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

/// 取消工作流时需下发的 CANCEL 请求列表：(机器下标, 子会话 id)（纯函数，可单测）。
pub fn cancel_requests(children: &[ChildSession]) -> Vec<(usize, String)> {
    children
        .iter()
        .map(|c| (c.machine_idx, c.id.clone()))
        .collect()
}

#[derive(Clone)]
pub struct WorkflowEngine {
    pub session: OrcSession,
    backend: Arc<dyn OrcBackend>,
    clients: Vec<WsClient>,
    machines: Vec<MachineSummary>,
    advancing: bool,
    pending_advance: bool,
    /// 工作中收到的用户消息（steer 注入，克隆共享）。
    steer_inbox: Arc<Mutex<Vec<String>>>,
}

impl WorkflowEngine {
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
        let mut transcript = Vec::new();
        if !description.trim().is_empty() {
            transcript.push(OrcMsg::User {
                text: description.to_string(),
            });
        }
        if !context.trim().is_empty() {
            transcript.push(OrcMsg::User {
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
            cancelled: false,
            done: false,
            transcript,
            children: Vec::new(),
            activities: Vec::new(),
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
            steer_inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn restore(
        mut session: OrcSession,
        backend: Arc<dyn OrcBackend>,
        clients: Vec<WsClient>,
        machines: Vec<MachineSummary>,
    ) -> Self {
        // 应用重开后工作流会话回到空闲；重新启动需用户手动触发（PRD §工作流会话）
        if !session.done {
            session.state = SessionState::Idle;
        }
        // 子会话只持久化机器名和旧下标；应用重启或机器列表变化后按机器名重新绑定。
        for child in &mut session.children {
            child.machine_idx = machines
                .iter()
                .position(|machine| machine.name == child.machine_name)
                .unwrap_or(usize::MAX);
        }
        WorkflowEngine {
            session,
            backend,
            clients,
            machines,
            advancing: false,
            pending_advance: false,
            steer_inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[cfg(test)]
    pub async fn start(&mut self) -> Result<(), String> {
        self.advance().await
    }

    pub async fn advance(&mut self) -> Result<(), String> {
        loop {
            if self.advancing {
                self.pending_advance = true;
                return Ok(());
            }
            if self.session.cancelled || self.session.done {
                return Ok(());
            }
            self.advancing = true;
            self.session.state = SessionState::Busy;
            self.session.updated_at = now();
            let result = self.do_advance().await;
            self.advancing = false;
            self.sync_state_from_children();
            self.session.updated_at = now();
            if self.absorb_steer() {
                self.pending_advance = true;
            }
            if !self.pending_advance {
                return result;
            }
            self.pending_advance = false;
            result.as_ref()?;
        }
    }

    async fn do_advance(&mut self) -> Result<(), String> {
        self.session.activities.push(Activity::Thinking {
            timestamp: now(),
            content: "编排智能体正在分析工作流并规划本轮调度".into(),
        });
        let ctx = self.build_context();
        let decision = match self.backend.decide(&ctx).await {
            Ok(d) => d,
            Err(e) => {
                self.session.activities.push(Activity::Error {
                    timestamp: now(),
                    detail: format!("编排 agent 调用失败：{e}"),
                });
                return Err(e);
            }
        };
        self.session.transcript.push(OrcMsg::Orc {
            text: decision.summary.clone(),
        });
        if let Some(c) = &decision.conclusion {
            self.session.transcript.push(OrcMsg::User {
                text: format!("编排结束：{c}"),
            });
            self.session.done = true;
        }
        self.session.done = self.session.done || decision.done;
        if let Err(e) = self.apply_actions(decision.actions).await {
            self.session.activities.push(Activity::Error {
                timestamp: now(),
                detail: format!("执行编排动作失败：{e}"),
            });
            return Err(e);
        }
        if let Some(kids) = self.backend.take_synced_children() {
            self.session.children = kids;
        }
        if let Some(activities) = self.backend.take_synced_activities() {
            self.session.activities.extend(activities);
        }
        Ok(())
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
                })
                .collect(),
            child_sessions: self.session.children.clone(),
            clients: self.clients.clone(),
            machines: self.machines.clone(),
        }
    }

    async fn apply_actions(&mut self, actions: Vec<OrcAction>) -> Result<(), String> {
        for action in actions {
            match action {
                OrcAction::Run {
                    machine,
                    agent,
                    cwd,
                    prompt,
                    reuse,
                } => {
                    let m_idx = self.resolve_machine(&machine)?;
                    let session_id = match reuse {
                        Some(id) if self.session.children.iter().any(|c| c.id == id) => id,
                        Some(id) => return Err(format!("复用的子会话不存在: {id}")),
                        None => {
                            let res = self
                                .clients
                                .get(m_idx)
                                .ok_or_else(|| "机器连接已失效".to_string())?
                                .request(
                                    protocol::method::SESSION_NEW,
                                    Some(serde_json::json!({
                                        "agent": agent,
                                        "cwd": cwd,
                                    })),
                                )
                                .await;
                            match res {
                                Ok(v) => {
                                    let sid = v
                                        .get("session")
                                        .and_then(|s| s.get("id"))
                                        .and_then(|i| i.as_str())
                                        .ok_or_else(|| {
                                            format!(
                                                "创建关联普通会话响应缺少 session.id（{machine}/{agent}）"
                                            )
                                        })?
                                        .to_string();
                                    if sid.is_empty() {
                                        return Err(format!(
                                            "创建关联普通会话响应的 session.id 为空（{machine}/{agent}）"
                                        ));
                                    }
                                    sid
                                }
                                Err(e) => {
                                    return Err(format!(
                                        "创建关联普通会话失败（{machine}/{agent}）：{e}"
                                    ))
                                }
                            }
                        }
                    };
                    if !self.session.children.iter().any(|c| c.id == session_id) {
                        let step_desc = first_line(&prompt);
                        self.session.children.push(ChildSession {
                            id: session_id.clone(),
                            machine_idx: m_idx,
                            machine_name: self.machines[m_idx].name.clone(),
                            agent: agent.clone(),
                            step_desc,
                            state: SessionState::Busy,
                            last_output: String::new(),
                            last_active_at: now(),
                        });
                    }
                    if let Err(error) = self.prompt_child(&session_id, &prompt).await {
                        if let Some(child) = self
                            .session
                            .children
                            .iter_mut()
                            .find(|child| child.id == session_id)
                        {
                            child.state = SessionState::Idle;
                        }
                        self.session.activities.push(Activity::Error {
                            timestamp: now(),
                            detail: error.clone(),
                        });
                        return Err(error);
                    }
                    self.session.activities.push(Activity::ToolCall {
                        timestamp: now(),
                        name: "create_session".into(),
                        title: Some(format!(
                            "在 {} 用 {} 创建关联普通会话 {session_id}",
                            self.machines[m_idx].name, agent
                        )),
                        content: Some(prompt.clone()),
                    });
                }
                OrcAction::Steer { session, prompt } | OrcAction::Retry { session, prompt } => {
                    if let Err(error) = self.prompt_child(&session, &prompt).await {
                        if let Some(child) = self
                            .session
                            .children
                            .iter_mut()
                            .find(|child| child.id == session)
                        {
                            child.state = SessionState::Idle;
                        }
                        self.session.activities.push(Activity::Error {
                            timestamp: now(),
                            detail: error.clone(),
                        });
                        return Err(error);
                    }
                    self.session.activities.push(Activity::ToolCall {
                        timestamp: now(),
                        name: "prompt_session".into(),
                        title: Some(format!("介入关联普通会话 {session}")),
                        content: Some(prompt.clone()),
                    });
                }
            }
        }
        Ok(())
    }

    async fn prompt_child(&mut self, session_id: &str, text: &str) -> Result<(), String> {
        let Some(child) = self.session.children.iter().find(|c| c.id == session_id) else {
            self.session.transcript.push(OrcMsg::User {
                text: format!("关联普通会话不存在：{session_id}"),
            });
            return Err(format!("关联普通会话不存在: {session_id}"));
        };
        let client = self
            .clients
            .get(child.machine_idx)
            .cloned()
            .ok_or_else(|| "机器连接已失效".to_string())?;
        if let Some(c) = self
            .session
            .children
            .iter_mut()
            .find(|c| c.id == session_id)
        {
            c.state = SessionState::Busy;
        }
        let sid = session_id.to_string();
        let input = serde_json::json!({
            "sessionId": session_id,
            "input": [{ "type": "text", "text": text }],
        });
        client
            .request(protocol::method::SESSION_PROMPT, Some(input))
            .await
            .map_err(|e| format!("下发指令失败 {sid}: {e}"))?;
        Ok(())
    }

    fn resolve_machine(&self, name: &str) -> Result<usize, String> {
        if let Some(i) = self.machines.iter().position(|m| m.name == name) {
            return Ok(i);
        }
        Err(format!("机器不可用: {name}"))
    }

    /// 关联普通会话状态变更（GUI 收到 `session.state_change` 通知时调用）。
    /// 关联普通会话变 idle → 按 docs/DESIGN.md 格式注入状态变更并推进。
    pub async fn on_child_state(
        &mut self,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
        output_excerpt: Option<String>,
    ) -> Result<bool, String> {
        let machine = {
            let Some(child) = self
                .session
                .children
                .iter_mut()
                .find(|c| c.id == session_id)
            else {
                return Ok(false);
            };
            if let Some(o) = output_excerpt {
                child.last_output = o;
            }
            child.state = new_state;
            child.last_active_at = now();
            child.machine_name.clone()
        };
        self.sync_state_from_children();
        if new_state == SessionState::Idle {
            // 用户取消工作流导致的子会话状态变更不注入（docs/DESIGN.md §工作流会话驱动）
            if self.session.cancelled || self.session.done {
                return Ok(false);
            }
            self.session.transcript.push(OrcMsg::User {
                text: format!(
                    "关联普通会话 {session_id}@{machine} 检测到状态变更：{old} -> {new}",
                    old = state_label(old_state),
                    new = state_label(new_state)
                ),
            });
            if let Err(e) = self.advance().await {
                self.session.activities.push(Activity::Error {
                    timestamp: now(),
                    detail: format!("关联会话推进失败：{e}"),
                });
                return Err(e);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// 同步记录关联普通会话状态变更（不推进；widget 状态即可视化）。
    pub fn on_child_state_local(&mut self, session_id: &str, state: SessionState) {
        if let Some(child) = self
            .session
            .children
            .iter_mut()
            .find(|c| c.id == session_id)
        {
            child.state = state;
            child.last_active_at = now();
        }
        self.sync_state_from_children();
    }

    fn sync_state_from_children(&mut self) {
        if self.session.cancelled || self.session.done {
            return;
        }
        self.session.state = if self.advancing
            || self
                .session
                .children
                .iter()
                .any(|child| child.state == SessionState::Busy)
        {
            SessionState::Busy
        } else {
            SessionState::Idle
        };
    }

    pub fn begin_busy(&mut self) {
        self.advancing = true;
        self.pending_advance = false;
        self.session.state = SessionState::Busy;
        self.session.updated_at = now();
    }

    pub fn start_advance(&mut self) {
        self.advancing = false;
        self.pending_advance = false;
    }

    pub fn abort_busy(&mut self) {
        self.advancing = false;
        self.pending_advance = false;
        if self.session.state == SessionState::Busy {
            self.session.state = SessionState::Idle;
        }
        self.session.updated_at = now();
    }

    pub fn record_user(&mut self, text: &str) -> bool {
        if self.session.description.trim().is_empty() {
            self.session.description = text.trim().to_string();
        }
        if self.session.title.trim().is_empty() {
            self.session.title = generate_title(text);
        }
        self.session.transcript.push(OrcMsg::User {
            text: text.to_string(),
        });
        self.session.updated_at = now();
        if self.session.done {
            return false;
        }
        if self.session.cancelled {
            self.session.cancelled = false;
            self.session.state = SessionState::Idle;
        }
        if self.advancing {
            // 工作中以 steer 注入，当前 turn 结束后再跑一轮（docs/DESIGN.md 编排智能体 steer）
            self.steer_inbox
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .push(text.to_string());
            return false;
        }
        true
    }

    /// 把 inbox 中尚未出现在 transcript 的 steer 消息合并进来。
    pub fn absorb_steer(&mut self) -> bool {
        let msgs = std::mem::take(
            &mut *self
                .steer_inbox
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）"),
        );
        let mut added = false;
        for text in msgs {
            let exists = self
                .session
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::User { text: t } if t == &text));
            if !exists {
                self.session.transcript.push(OrcMsg::User { text });
                added = true;
            }
        }
        added
    }

    pub fn mark_cancelled(&mut self) {
        self.session.cancelled = true;
        self.session.state = SessionState::Idle;
        self.session.updated_at = now();
        self.session.transcript.push(OrcMsg::User {
            text: "已取消".into(),
        });
    }

    pub async fn send_cancel_to_children(&self) -> Result<(), String> {
        for (machine_idx, session_id) in cancel_requests(&self.session.children) {
            let client = self
                .clients
                .get(machine_idx)
                .cloned()
                .ok_or_else(|| format!("机器连接已失效: {machine_idx}"))?;
            client
                .request(
                    protocol::method::SESSION_CANCEL,
                    Some(serde_json::json!({ "sessionId": session_id })),
                )
                .await
                .map_err(|e| format!("取消关联会话失败 {session_id}: {e}"))?;
        }
        Ok(())
    }

    // ---- 持久化（docs/DESIGN.md「工作流会话存储」：sqlite + 两份 jsonl）----

    pub fn persist(&self, data_dir: &Path) -> std::io::Result<()> {
        crate::wfstore::save(data_dir, &self.session)
    }

    pub fn load_all(data_dir: &Path) -> std::io::Result<Vec<OrcSession>> {
        crate::wfstore::load_all(data_dir)
    }

    pub fn remove(data_dir: &Path, id: &str) -> std::io::Result<()> {
        crate::wfstore::remove(data_dir, id)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

impl OrcSession {
    /// 工作流会话对话流：用户消息（含系统注入的推进消息）与编排输出。
    pub fn to_dialog(&self) -> Vec<DialogMsg> {
        self.transcript
            .iter()
            .map(|m| match m {
                OrcMsg::User { text } => DialogMsg::UserMessage {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: self.updated_at,
                },
                OrcMsg::Orc { text } => DialogMsg::AgentMessage {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: self.updated_at,
                },
            })
            .collect()
    }
}

#[derive(Clone)]
struct LiveRuntime {
    machines: Vec<MachineSummary>,
    clients: Vec<WsClient>,
    children: Arc<Mutex<Vec<ChildSession>>>,
    activities: Arc<Mutex<Vec<Activity>>>,
}

impl LiveRuntime {
    fn machine_index(&self, name: &str) -> Result<usize, String> {
        self.machines
            .iter()
            .position(|m| m.name == name)
            .ok_or_else(|| format!("机器不存在: {name}"))
    }

    fn client(&self, name: &str) -> Result<WsClient, String> {
        let i = self.machine_index(name)?;
        self.clients
            .get(i)
            .cloned()
            .ok_or_else(|| format!("机器未连接: {name}"))
    }

    fn child(&self, session_id: &str) -> Result<ChildSession, String> {
        self.children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .iter()
            .find(|c| c.id == session_id)
            .cloned()
            .ok_or_else(|| format!("关联普通会话不存在: {session_id}"))
    }

    fn record_tool(&self, name: &str, title: impl Into<String>, content: impl Into<String>) {
        self.activities
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .push(Activity::ToolCall {
                timestamp: now(),
                name: name.to_string(),
                title: Some(title.into()),
                content: Some(content.into()),
            });
    }
}

// ---- RigBackend：真实 rig 单 turn 编排（docs/DESIGN.md「编排智能体」）----

pub struct RigBackend {
    cfg: OrchestratorConfig,
    synced_children: Mutex<Option<Vec<ChildSession>>>,
    synced_activities: Mutex<Option<Vec<Activity>>>,
}

impl RigBackend {
    pub fn new(cfg: OrchestratorConfig) -> Self {
        RigBackend {
            cfg,
            synced_children: Mutex::new(None),
            synced_activities: Mutex::new(None),
        }
    }

    pub fn build_agent<M>(&self, model: M, preamble: &str) -> rig::Agent<M>
    where
        M: rig::completion::CompletionModel + 'static,
    {
        rig::AgentBuilder::new(model)
            .preamble(preamble)
            .default_max_turns(8)
            .tool(ListAgents)
            .tool(ListSessions)
            .tool(CreateSession)
            .tool(PromptSession)
            .tool(CancelSession)
            .tool(ReadSessionHistory)
            .tool(ReadSessionActivities)
            .build()
    }

    fn preamble(&self) -> String {
        "你是 amux 的编排智能体。按工作流执行计划和用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在工作流执行计划和用户指令中指定的事项交由用户决定。\n\
         使用工具：list_agents、list_sessions、create_session、prompt_session、cancel_session、\
         read_session_history、read_session_activities。调度动作完成后用中文简述本轮动作并结束 turn。"
            .to_string()
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiFormat {
    ChatCompletions,
    Responses,
    Messages,
}

fn api_format_kind(api_format: &str) -> ApiFormat {
    match api_format {
        "responses" => ApiFormat::Responses,
        "messages" => ApiFormat::Messages,
        _ => ApiFormat::ChatCompletions,
    }
}

impl OrcBackend for RigBackend {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.cfg.is_configured() {
                return Err(
                    "未配置编排 agent API（Base URL / API key / 模型）。请在设置 → 编排 agent 中配置后再创建工作流"
                        .to_string(),
                );
            }
            let mut preamble = self.preamble();
            if !ctx.preamble.trim().is_empty() {
                preamble.push_str("\n\n");
                preamble.push_str("【工作流模板/执行要求】\n");
                preamble.push_str(ctx.preamble.trim());
            }
            let model = self.cfg.model.clone();
            let live = LiveRuntime {
                machines: ctx.machines.clone(),
                clients: ctx.clients.clone(),
                children: Arc::new(Mutex::new(ctx.child_sessions.clone())),
                activities: Arc::new(Mutex::new(Vec::new())),
            };
            let mut tool_ctx = rig::tool::ToolContext::new();
            tool_ctx.insert(live.clone());
            preamble.push_str("\n\n【工作流执行计划】\n");
            preamble.push_str(ctx.plan.trim());
            let transcript_text = if ctx.transcript.is_empty() {
                "（无）".to_string()
            } else {
                ctx.transcript.join("\n")
            };
            let input = format!(
                "用户消息与对话历史：\n{transcript_text}\n\n请用工具完成本轮调度，未指定的事项询问用户。"
            );
            let text = match api_format_kind(&self.cfg.api_format) {
                ApiFormat::ChatCompletions => {
                    let client = rig::providers::openai::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 OpenAI client 失败: {e}"))?;
                    let agent = self
                        .build_agent(client.completions_api().completion_model(model), &preamble);
                    run_orc_turn(agent, input, tool_ctx).await?
                }
                ApiFormat::Responses => {
                    let client = rig::providers::openai::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 OpenAI client 失败: {e}"))?;
                    let agent = self.build_agent(client.completion_model(model), &preamble);
                    run_orc_turn(agent, input, tool_ctx).await?
                }
                ApiFormat::Messages => {
                    let client = rig::providers::anthropic::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 Anthropic client 失败: {e}"))?;
                    let agent = self.build_agent(client.completion_model(model), &preamble);
                    run_orc_turn(agent, input, tool_ctx).await?
                }
            };
            let kids = live
                .children
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .clone();
            *self
                .synced_children
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）") = Some(kids);
            let activities = live
                .activities
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .clone();
            *self
                .synced_activities
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）") = Some(activities);
            Ok(Decision {
                summary: text,
                actions: Vec::new(),
                done: false,
                conclusion: None,
            })
        })
    }

    fn take_synced_children(&self) -> Option<Vec<ChildSession>> {
        self.synced_children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .take()
    }

    fn take_synced_activities(&self) -> Option<Vec<Activity>> {
        self.synced_activities
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .take()
    }
}

fn state_label(s: SessionState) -> &'static str {
    match s {
        SessionState::Idle => "idle",
        SessionState::Busy => "busy",
    }
}

/// 脚本化测试后端（决策序列；用于驱动引擎的纯逻辑测试）。
#[cfg(test)]
#[doc(hidden)]
pub struct FakeBackend {
    decisions: Mutex<VecDeque<Decision>>,
}

#[doc(hidden)]
#[cfg(test)]
#[allow(clippy::new_ret_no_self)]
impl FakeBackend {
    pub fn new(decisions: Vec<Decision>) -> Arc<dyn OrcBackend> {
        Arc::new(FakeBackend {
            decisions: Mutex::new(VecDeque::from(decisions)),
        })
    }

    pub fn new_for_tests() -> Arc<dyn OrcBackend> {
        Self::new(vec![])
    }
}

#[cfg(test)]
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

// ---- rig 调度工具（docs/DESIGN.md「编排智能体」工具表）----

fn live(context: &rig::tool::ToolContext) -> Result<LiveRuntime, rig::tool::ToolExecutionError> {
    context
        .get::<LiveRuntime>()
        .cloned()
        .ok_or_else(|| rig::tool::ToolExecutionError::other("缺少工具运行时"))
}

struct ListAgents;
impl rig::tool::Tool for ListAgents {
    const NAME: &'static str = "list_agents";
    type Args = EmptyArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "已注册机器及各机器的 agent 列表：机器在线状态、agent 可用性".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        _args: EmptyArgs,
    ) -> Result<Self::Output, Self::Error> {
        let live = live(context)?;
        let v: Vec<serde_json::Value> = live
            .machines
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.name,
                    "online": m.online,
                    "agents": m.agents.iter().map(|a| serde_json::json!({
                        "name": a.name,
                        "available": a.available,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let output = serde_json::to_string(&v)
            .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
        live.record_tool(Self::NAME, "查询可用 agent", "");
        Ok(output)
    }
}

struct ListSessions;
impl rig::tool::Tool for ListSessions {
    const NAME: &'static str = "list_sessions";
    type Args = EmptyArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "本工作流的关联普通会话列表（标题、状态、最近活跃、机器在线与否）".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        _args: EmptyArgs,
    ) -> Result<Self::Output, Self::Error> {
        let live = live(context)?;
        let children = live
            .children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .clone();
        let v: Vec<serde_json::Value> = children
            .iter()
            .map(|c| {
                let online = live
                    .machines
                    .iter()
                    .find(|m| m.name == c.machine_name)
                    .map(|m| m.online)
                    .unwrap_or(false);
                serde_json::json!({
                    "id": c.id,
                    "title": c.step_desc,
                    "state": state_label(c.state),
                    "machine": c.machine_name,
                    "agent": c.agent,
                    "machineOnline": online,
                })
            })
            .collect();
        let output = serde_json::to_string(&v)
            .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
        live.record_tool(Self::NAME, "查询关联普通会话", "");
        Ok(output)
    }
}

#[derive(serde::Deserialize, Default)]
struct EmptyArgs {}

struct CreateSession;
impl rig::tool::Tool for CreateSession {
    const NAME: &'static str = "create_session";
    type Args = CreateSessionArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "向指定机器、指定 agent 与工作目录创建关联普通会话，返回会话 ID".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "machine": { "type": "string", "description": "机器名" },
                "agent": { "type": "string", "description": "agent 名" },
                "cwd": { "type": "string", "description": "工作目录" }
            },
            "required": ["machine", "agent", "cwd"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: CreateSessionArgs,
    ) -> Result<Self::Output, Self::Error> {
        let live = live(context)?;
        let idx = live
            .machine_index(&args.machine)
            .map_err(rig::tool::ToolExecutionError::other)?;
        let client = live
            .client(&args.machine)
            .map_err(rig::tool::ToolExecutionError::other)?;
        let res = client
            .request(
                protocol::method::SESSION_NEW,
                Some(serde_json::json!({
                    "agent": args.agent,
                    "cwd": args.cwd,
                })),
            )
            .await
            .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
        let sid = res
            .get("session")
            .and_then(|s| s.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        if sid.is_empty() {
            return Err(rig::tool::ToolExecutionError::other("创建会话未返回 id"));
        }
        let machine_name = args.machine.clone();
        live.children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .push(ChildSession {
                id: sid.clone(),
                machine_idx: idx,
                machine_name,
                agent: args.agent,
                step_desc: args.cwd,
                state: SessionState::Idle,
                last_output: String::new(),
                last_active_at: now(),
            });
        live.record_tool(
            Self::NAME,
            "创建关联普通会话",
            format!("{}@{}", sid, args.machine),
        );
        Ok(sid)
    }
}

#[derive(serde::Deserialize)]
struct CreateSessionArgs {
    machine: String,
    agent: String,
    cwd: String,
}

struct PromptSession;
impl rig::tool::Tool for PromptSession {
    const NAME: &'static str = "prompt_session";
    type Args = PromptSessionArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "向关联普通会话下发指令".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string", "description": "关联普通会话 id" },
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
        let live = live(context)?;
        let child = live
            .child(&args.session)
            .map_err(rig::tool::ToolExecutionError::other)?;
        let client = live
            .clients
            .get(child.machine_idx)
            .cloned()
            .ok_or_else(|| rig::tool::ToolExecutionError::other("机器连接已失效"))?;
        client
            .request(
                protocol::method::SESSION_PROMPT,
                Some(serde_json::json!({
                    "sessionId": args.session,
                    "input": [{ "type": "text", "text": args.prompt }],
                })),
            )
            .await
            .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
        if let Some(c) = live
            .children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .iter_mut()
            .find(|c| c.id == args.session)
        {
            c.state = SessionState::Busy;
        }
        live.record_tool(Self::NAME, "下发指令", format!("session={}", args.session));
        Ok("已下发".into())
    }
}

#[derive(serde::Deserialize)]
struct PromptSessionArgs {
    session: String,
    prompt: String,
}

struct CancelSession;
impl rig::tool::Tool for CancelSession {
    const NAME: &'static str = "cancel_session";
    type Args = SessionRefArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "取消关联普通会话进行中的工作".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string" }
            },
            "required": ["session"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: SessionRefArgs,
    ) -> Result<Self::Output, Self::Error> {
        let live = live(context)?;
        let child = live
            .child(&args.session)
            .map_err(rig::tool::ToolExecutionError::other)?;
        let client = live
            .clients
            .get(child.machine_idx)
            .cloned()
            .ok_or_else(|| rig::tool::ToolExecutionError::other("机器连接已失效"))?;
        client
            .request(
                protocol::method::SESSION_CANCEL,
                Some(serde_json::json!({ "sessionId": args.session })),
            )
            .await
            .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
        live.record_tool(
            Self::NAME,
            "取消关联普通会话",
            format!("session={}", args.session),
        );
        Ok("已取消".into())
    }
}

struct ReadSessionHistory;
impl rig::tool::Tool for ReadSessionHistory {
    const NAME: &'static str = "read_session_history";
    type Args = SessionPageArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "按窗口 / 游标读取关联普通会话对话内容".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string" },
                "limit": { "type": "integer" },
                "before": { "type": "integer" }
            },
            "required": ["session"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: SessionPageArgs,
    ) -> Result<Self::Output, Self::Error> {
        page_session(context, &args, protocol::method::SESSION_HISTORY).await
    }
}

struct ReadSessionActivities;
impl rig::tool::Tool for ReadSessionActivities {
    const NAME: &'static str = "read_session_activities";
    type Args = SessionPageArgs;
    type Output = String;
    type Error = rig::tool::ToolExecutionError;

    fn description(&self) -> String {
        "按窗口 / 游标读取关联普通会话活动内容".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string" },
                "limit": { "type": "integer" },
                "before": { "type": "integer" }
            },
            "required": ["session"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: SessionPageArgs,
    ) -> Result<Self::Output, Self::Error> {
        page_session(context, &args, protocol::method::SESSION_ACTIVITIES).await
    }
}

#[derive(serde::Deserialize)]
struct SessionRefArgs {
    session: String,
}

#[derive(serde::Deserialize)]
struct SessionPageArgs {
    session: String,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    before: Option<u64>,
}

async fn page_session(
    context: &mut rig::tool::ToolContext,
    args: &SessionPageArgs,
    method: &str,
) -> Result<String, rig::tool::ToolExecutionError> {
    let live = live(context)?;
    let child = live
        .child(&args.session)
        .map_err(rig::tool::ToolExecutionError::other)?;
    let client = live
        .clients
        .get(child.machine_idx)
        .cloned()
        .ok_or_else(|| rig::tool::ToolExecutionError::other("机器连接已失效"))?;
    let mut params = serde_json::json!({ "sessionId": args.session });
    if let Some(limit) = args.limit {
        params["limit"] = serde_json::json!(limit);
    }
    if let Some(before) = args.before {
        params["before"] = serde_json::json!(before);
    }
    let res = client
        .request(method, Some(params))
        .await
        .map_err(|e| rig::tool::ToolExecutionError::other(e.to_string()))?;
    let tool_name = if method == protocol::method::SESSION_HISTORY {
        "read_session_history"
    } else {
        "read_session_activities"
    };
    live.record_tool(
        tool_name,
        "读取关联普通会话",
        format!("method={method} session={}", args.session),
    );
    Ok(res.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::tool::Tool as _;

    fn machines() -> Vec<MachineSummary> {
        vec![MachineSummary::named("测试机", &["mock_acp"])]
    }

    fn clients_with_machines() -> (Vec<WsClient>, MachineSummary) {
        let m = machines().remove(0);
        let c = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        (vec![c], m)
    }

    #[test]
    fn ops_to_actions_maps_create_and_prompt() {
        let ops = vec![
            ToolOp::Create {
                machine: "本机".into(),
                agent: "codex".into(),
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
                agent,
                prompt,
                reuse,
                ..
            } => {
                assert_eq!(machine, "本机");
                assert_eq!(agent, "codex");
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
            _ => panic!("未知会话应映射为 Steer"),
        }
    }

    #[test]
    fn cancel_requests_lists_all_children() {
        let children = vec![
            ChildSession {
                id: "a".into(),
                machine_idx: 0,
                machine_name: "m0".into(),
                agent: "h".into(),
                step_desc: "s".into(),
                state: SessionState::Idle,
                last_output: String::new(),
                last_active_at: 0,
            },
            ChildSession {
                id: "b".into(),
                machine_idx: 1,
                machine_name: "m1".into(),
                agent: "h".into(),
                step_desc: "s".into(),
                state: SessionState::Busy,
                last_output: String::new(),
                last_active_at: 0,
            },
        ];
        assert_eq!(
            cancel_requests(&children),
            vec![(0, "a".to_string()), (1, "b".to_string())]
        );
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

    #[tokio::test]
    async fn cancel_marks_cancelled_and_stops_advance() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "无动作".into(),
            actions: vec![],
            done: false,
            conclusion: None,
        }]);
        let mut engine = WorkflowEngine::new("计划", "", "", backend, clients, vec![m]);
        engine.session.children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
            agent: "mock_acp".into(),
            step_desc: "第一步".into(),
            state: SessionState::Busy,
            last_output: String::new(),
            last_active_at: 0,
        });
        engine.mark_cancelled();
        assert!(engine.session.cancelled);
        assert_eq!(engine.session.state, SessionState::Idle);
        engine.advance().await.unwrap();
        assert!(!engine
            .session
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { .. })));
        let should_advance = engine.record_user("先做 A");
        assert!(should_advance, "取消后的新指令应恢复工作流");
    }

    #[tokio::test]
    async fn on_child_state_done_does_not_push_completion() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "无动作".into(),
            actions: vec![],
            done: false,
            conclusion: None,
        }]);
        let mut engine = WorkflowEngine::new("计划", "", "", backend, clients, vec![m]);
        engine.session.done = true;
        engine.session.children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
            agent: "mock_acp".into(),
            step_desc: "第一步".into(),
            state: SessionState::Busy,
            last_output: String::new(),
            last_active_at: 0,
        });
        let advanced = engine
            .on_child_state("s_child", SessionState::Busy, SessionState::Idle, None)
            .await
            .unwrap();
        assert!(!advanced);
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
            .any(|m| matches!(m, OrcMsg::User { text } if text.contains("编排结束"))));
    }

    /// 工作流会话持久化往返（acceptance）。
    #[tokio::test]
    async fn persistence_roundtrip_and_restore() {
        let dir = std::env::temp_dir().join(format!("amux-wf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let mut engine = WorkflowEngine::new("计划A", "", "", backend, clients, vec![m]);
        let id = engine.session.id.clone();
        engine.record_user("立即保存");
        engine.persist(&dir).unwrap();

        let sessions = WorkflowEngine::load_all(&dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].title, "计划A");
        assert!(sessions[0]
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text } if text == "立即保存")));

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

        WorkflowEngine::remove(&dir, &id).unwrap();
        assert!(WorkflowEngine::load_all(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rig_backend_builds_agent_with_tools() {
        let backend = RigBackend::new(OrchestratorConfig {
            api_format: "chat_completions".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "sk-test".into(),
            model: "gpt-4o-mini".into(),
        });
        let client = rig::providers::openai::Client::builder()
            .api_key("sk-test")
            .base_url("http://127.0.0.1:9/v1")
            .build()
            .expect("构建 openai client");
        let agent = backend.build_agent(client.completion_model("gpt-4o-mini"), "preamble");
        assert!(agent.name().is_none());
    }

    #[test]
    fn api_format_kind_dispatches_formats() {
        assert_eq!(
            api_format_kind("chat_completions"),
            ApiFormat::ChatCompletions
        );
        assert_eq!(api_format_kind("responses"), ApiFormat::Responses);
        assert_eq!(api_format_kind("messages"), ApiFormat::Messages);
        assert_eq!(api_format_kind("unknown"), ApiFormat::ChatCompletions);
        assert_eq!(api_format_kind(""), ApiFormat::ChatCompletions);
    }

    #[test]
    fn orc_session_to_dialog_maps_all_transcript_kinds() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            vec![],
            vec![MachineSummary::named("测试机", &["mock_acp"])],
        );
        let mut engine = engine;
        engine.session.transcript.push(OrcMsg::Orc {
            text: "决策".into(),
        });
        let dialog = engine.session.to_dialog();
        assert_eq!(dialog.len(), 2);
        assert!(matches!(&dialog[0], DialogMsg::UserMessage { .. }));
        assert!(matches!(&dialog[1], DialogMsg::AgentMessage { .. }));
    }

    #[test]
    fn template_as_preamble_not_in_history() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "",
            "",
            "模板：先在测试机实现，再审查",
            backend,
            vec![],
            vec![MachineSummary::named("测试机", &["mock_acp"])],
        );
        assert!(engine.session.transcript.is_empty());
        assert_eq!(engine.session.preamble, "模板：先在测试机实现，再审查");
        assert!(engine.session.title.is_empty());
    }

    #[test]
    fn record_user_sets_description_when_empty() {
        let backend = FakeBackend::new_for_tests();
        let mut engine = WorkflowEngine::new(
            "",
            "",
            "模板：先实现后审查",
            backend,
            vec![],
            vec![MachineSummary::named("测试机", &["mock_acp"])],
        );
        let should_advance = engine.record_user("实现登录功能");
        assert_eq!(engine.session.description, "实现登录功能");
        assert_eq!(engine.session.title, "实现登录功能");
        assert!(should_advance);
    }

    #[tokio::test]
    async fn unconfigured_rig_backend_records_clear_error() {
        let backend = Arc::new(RigBackend::new(OrchestratorConfig {
            api_format: "chat_completions".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
        }));
        let client = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        let mut engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            vec![client],
            vec![MachineSummary::named("测试机", &["kimi"])],
        );
        let res = engine.start().await;
        assert!(res.is_err());
        assert!(engine.session.activities.iter().any(
            |a| matches!(a, Activity::Error { detail, .. } if detail.contains("未配置编排 agent API"))
        ));
        assert_eq!(engine.session.state, SessionState::Idle);
    }

    #[tokio::test]
    async fn create_session_tool_requires_live_runtime() {
        let mut ctx = rig::tool::ToolContext::new();
        let res = CreateSession
            .call(
                &mut ctx,
                CreateSessionArgs {
                    machine: "测试机".into(),
                    agent: "mock_acp".into(),
                    cwd: "/tmp".into(),
                },
            )
            .await;
        let err = res.expect_err("缺少 LiveRuntime 应报错");
        assert!(err.to_string().contains("缺少工具运行时"));
    }

    #[tokio::test]
    async fn create_session_tool_rejects_unknown_machine() {
        let client = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        let mut ctx = rig::tool::ToolContext::new();
        ctx.insert(LiveRuntime {
            machines: vec![MachineSummary::named("测试机", &["mock_acp"])],
            clients: vec![client],
            children: Arc::new(Mutex::new(Vec::new())),
            activities: Arc::new(Mutex::new(Vec::new())),
        });
        let res = CreateSession
            .call(
                &mut ctx,
                CreateSessionArgs {
                    machine: "未知机器".into(),
                    agent: "mock_acp".into(),
                    cwd: "/tmp".into(),
                },
            )
            .await;
        let err = res.expect_err("未知机器应报错");
        assert!(err.to_string().contains("机器不存在"));
    }

    /// on_child_state_local 同步更新关联普通会话 busy/idle 状态（GUI 收到 state_change 通知时调用）。
    #[test]
    fn on_child_state_local_updates_busy_and_idle() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            vec![],
            vec![MachineSummary::named("测试机", &["mock_acp"])],
        );
        let mut engine = engine;
        engine.session.children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
            agent: "mock_acp".into(),
            step_desc: "第一步".into(),
            state: SessionState::Idle,
            last_output: String::new(),
            last_active_at: 0,
        });
        engine.on_child_state_local("s_child", SessionState::Busy);
        assert_eq!(engine.session.children[0].state, SessionState::Busy);
        engine.on_child_state_local("s_child", SessionState::Idle);
        assert_eq!(engine.session.children[0].state, SessionState::Idle);
        // 非挂载的会话 id：无副作用，不 panic，不新增条目
        engine.on_child_state_local("missing", SessionState::Busy);
        assert_eq!(engine.session.children.len(), 1);
        assert_eq!(engine.session.children[0].state, SessionState::Idle);
    }
}
