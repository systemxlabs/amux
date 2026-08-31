//! 工作流引擎：
//! GUI 本地工作流会话 + 自研薄工具循环编排。
//!
//! - `OrcSession`：工作流会话状态，可序列化持久化到 SQLite 和两份 JSONL 日志
//! - `OrcBackend`：单 turn 决策器；真实实现 `RigBackend` 保留 rig provider 层，
//!   循环自研（`run_tool_loop`）——请求 → 解析工具调用 → 执行 → 结果回填 →
//!   drain steer 插话 → 再请求，使 steer 能在轮次边界真实注入
//! - `WorkflowEngine`：状态机——首 turn 拆解计划并创建/复用关联普通会话下发指令；
//!   关联普通会话 idle（`session.state_change` 通知驱动）触发自动推进
//! - 会话操作统一经真实 WsClient（SESSION_NEW / SESSION_PROMPT / SESSION_CANCEL）

#[cfg(test)]
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rig_core::client::CompletionClient;
use rig_core::completion::message::{ToolCall, ToolResultContent, UserContent};
use rig_core::completion::{AssistantContent, CompletionModel, Message};

use serde::{Deserialize, Serialize};

use protocol::{
    generate_title, ActivitiesResult, Activity, ContentBlock, HistoryResult, SessionIdParams,
    SessionInfoParams, SessionInfoResult, SessionMeta, SessionNewParams, SessionPageParams,
    SessionPromptParams, SessionResult, SessionState, StateChangeReason,
};

use crate::config::{ApiFormat, OrchestratorConfig};
use crate::logic::DialogMsg;
use crate::ws::WsClient;

/// 工作流会话中的一条消息（对话历史）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrcMsg {
    User { text: String, timestamp: u64 },
    Orc { text: String, timestamp: u64 },
}

/// 关联普通会话的挂载关系（工作流 ↔ 普通会话）。只存路由信息：
/// 标题、忙闲、agent 等一律以机器 server 的会话元数据为权威
/// （编排者经 list_sessions 现查 session.info；GUI 渲染时与本机会话缓存联表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSession {
    pub id: String,
    pub machine_idx: usize,
    pub machine_name: String,
}

/// 工作流会话（GUI 本地状态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrcSession {
    pub id: String,
    pub title: String,
    /// 完整执行计划（用户输入原文，不含 @ 引用展开的上下文；随元数据持久化）
    pub plan: String,
    /// 用户自然语言计划（含 @ 引用展开的上下文）
    pub description: String,
    /// 工作流计划/系统指令（内置进编排 agent 的系统提示词，不进入会话历史）。
    pub preamble: String,
    pub state: SessionState,
    pub transcript: Vec<OrcMsg>,
    pub children: Vec<ChildSession>,
    pub activities: Vec<Activity>,
    pub created_at: u64,
    pub updated_at: u64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

/// 机器运行时注册表：应用侧在机器增删/重连/状态变化时整体同步，
/// 工作流引擎每次推进前快照最新连接与摘要。引擎不再持有冻结的
/// WsClient 列表——否则重连后旧连接的接收端已关闭，工作流从此
/// 无法下发/取消任何子会话，新增机器也对编排 LLM 不可见。
#[derive(Default)]
pub struct MachineHub {
    entries: std::sync::Mutex<Vec<(MachineSummary, WsClient)>>,
}

impl MachineHub {
    /// 应用侧机器视图整体替换（顺序与 app.machines 一致，
    /// 保住 ChildSession.machine_idx 的下标语义）。clients 可短于
    /// machines（缺客户端即该机不可达，zip 截断）。
    pub fn sync(&self, machines: Vec<MachineSummary>, clients: Vec<WsClient>) {
        *self.entries.lock().unwrap() = machines.into_iter().zip(clients).collect();
    }

    /// 推进前快照：拿到最新连接与摘要。
    pub fn snapshot(&self) -> (Vec<MachineSummary>, Vec<WsClient>) {
        let entries = self.entries.lock().unwrap();
        entries.iter().cloned().unzip()
    }
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

/// 编排上下文（每次 decide 的输入）。
#[derive(Debug, Clone)]
pub struct OrcContext {
    pub plan: String,
    pub preamble: String,
    pub transcript: Vec<String>,
    pub child_sessions: Vec<ChildSession>,
    pub clients: Vec<WsClient>,
    pub machines: Vec<MachineSummary>,
    /// 编排进行中用户插话的实时通道：RigBackend 工具循环在每轮请求边界 drain，
    /// 注入为 user 消息。
    /// 与 `WorkflowEngine.steer_inbox` 是同一个 Arc；advance 收尾的 absorb_steer 只兜底剩余项。
    pub steer_inbox: Arc<Mutex<Vec<String>>>,
}

/// 单 turn 决策器（rig 单 turn 模式）。
#[derive(Debug, Clone)]
pub struct Decision {
    pub summary: String,
}

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

/// 取消按钮注入的固定用户消息：
/// 取消走用户消息通道，由编排智能体自行调用 cancel_session 停止调度。
pub const WORKFLOW_CANCEL_PROMPT: &str = "取消当前工作流会话关联的所有普通会话，停止工作流调度";

/// 推进门闩：同一工作流的推进全局串行（修复引擎克隆并发分叉）。
/// running 期间的新触发只置 requested；本轮结束后合并为至多一轮追加推进。
#[derive(Default)]
struct AdvanceGate {
    running: bool,
    requested: bool,
}

/// running 标志的 drop 兜底：turn 中途 panic/早退也不会把引擎永久卡在「推进中」。
struct GateGuard {
    gate: Arc<Mutex<AdvanceGate>>,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.gate
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .running = false;
    }
}

#[derive(Clone)]
pub struct WorkflowEngine {
    /// 共享会话快照：GUI 渲染读、引擎任务写（短临界区），克隆之间天然一致——
    /// 后台推进不再「克隆-跑-整引擎回写」，并发分叉与后写覆盖随之消失。
    pub session: Arc<RwLock<OrcSession>>,
    backend: Arc<dyn OrcBackend>,
    hub: Arc<MachineHub>,
    gate: Arc<Mutex<AdvanceGate>>,
    /// 工作中收到的用户消息（steer 注入；当前轮结束后合并）。
    steer_inbox: Arc<Mutex<Vec<String>>>,
    /// 忙碌子会话计数：子会话状态不落盘（权威在机器 server），仅按状态变更
    /// 事件增减；驱动工作流级忙闲显示与 steer 路由。重启归零。
    busy_children: Arc<Mutex<usize>>,
    /// 活动实时落盘目录：活动产生即追加写 `<data_dir>/sessions/<id>_activities.jsonl`，
    /// 不等 `persist` 整文件快照。
    data_dir: PathBuf,
}

impl WorkflowEngine {
    pub fn new(
        description: &str,
        context: &str,
        preamble: &str,
        backend: Arc<dyn OrcBackend>,
        hub: Arc<MachineHub>,
        data_dir: &Path,
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

                timestamp: now(),
            });
        }
        if !context.trim().is_empty() {
            transcript.push(OrcMsg::User {
                text: "已附加 @ 引用的上下文".into(),

                timestamp: now(),
            });
        }
        let t = now();
        let session = OrcSession {
            id: format!("orc_{}", uuid::Uuid::new_v4()),
            title: generate_title(description),
            plan: description.to_string(),
            description: full,
            preamble: preamble.to_string(),
            state: SessionState::Idle,
            transcript,
            children: Vec::new(),
            activities: Vec::new(),
            created_at: t,
            updated_at: t,
        };
        WorkflowEngine {
            session: Arc::new(RwLock::new(session)),
            backend,
            hub,
            gate: Arc::new(Mutex::new(AdvanceGate::default())),
            steer_inbox: Arc::new(Mutex::new(Vec::new())),
            busy_children: Arc::new(Mutex::new(0)),
            data_dir: data_dir.to_path_buf(),
        }
    }

    pub fn restore(
        mut session: OrcSession,
        backend: Arc<dyn OrcBackend>,
        hub: Arc<MachineHub>,
        data_dir: &Path,
    ) -> Self {
        // 应用重开后工作流会话回到空闲，重新启动需用户手动触发。
        session.state = SessionState::Idle;
        // 子会话只持久化机器名和旧下标；应用重启或机器列表变化后按机器名重新绑定。
        let (machines, _) = hub.snapshot();
        for child in &mut session.children {
            child.machine_idx = machines
                .iter()
                .position(|machine| machine.name == child.machine_name)
                .unwrap_or(usize::MAX);
        }
        WorkflowEngine {
            session: Arc::new(RwLock::new(session)),
            backend,
            hub,
            gate: Arc::new(Mutex::new(AdvanceGate::default())),
            steer_inbox: Arc::new(Mutex::new(Vec::new())),
            busy_children: Arc::new(Mutex::new(0)),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// 短临界区可变访问（长 await 一律发生在锁外）。
    fn with_session<R>(&self, f: impl FnOnce(&mut OrcSession) -> R) -> R {
        let mut s = self.session.write().expect("RwLock 中毒");
        f(&mut s)
    }

    /// 会话 id（短临界区读取）。
    pub fn id(&self) -> String {
        self.session.read().expect("RwLock 中毒").id.clone()
    }

    /// 会话状态（短临界区读取）。
    pub fn state(&self) -> SessionState {
        self.session.read().expect("RwLock 中毒").state
    }

    /// 会话标题（短临界区读取）。
    pub fn title(&self) -> String {
        self.session.read().expect("RwLock 中毒").title.clone()
    }

    /// 会话标题是否为空。
    pub fn title_is_empty(&self) -> bool {
        self.session.read().expect("RwLock 中毒").title.is_empty()
    }

    /// 关联普通会话列表的克隆（短临界区读取）。
    pub fn children(&self) -> Vec<ChildSession> {
        self.session.read().expect("RwLock 中毒").children.clone()
    }

    /// 关联普通会话数量。
    pub fn child_count(&self) -> usize {
        self.session.read().expect("RwLock 中毒").children.len()
    }

    /// 会话快照（仅读字段的克隆；调用方需持有 RwLock 语义）。
    pub fn snapshot(&self) -> OrcSession {
        self.session.read().expect("RwLock 中毒").clone()
    }

    /// 记录一条活动并实时追加落盘（不依赖 `persist` 的整文件快照）。
    pub fn record_activity(&self, act: Activity) {
        self.with_session(|s| s.activities.push(act.clone()));
        self.append_activities(&[act]);
    }

    /// 记录一批活动并实时追加落盘。
    pub fn record_activities(&self, acts: Vec<Activity>) {
        if acts.is_empty() {
            return;
        }
        self.with_session(|s| s.activities.extend(acts.iter().cloned()));
        self.append_activities(&acts);
    }

    /// 追加写活动 JSONL；失败仅记日志，不阻断推进（活动落盘尽力而为）。
    fn append_activities(&self, acts: &[Activity]) {
        let id = self.session.read().expect("RwLock 中毒").id.clone();
        let path = amux_common::session_log::activities_path(&self.data_dir, &id);
        if let Err(e) = amux_common::session_log::append_jsonl(&path, acts) {
            log::error!("工作流活动落盘失败 {id}: {e}");
        }
    }

    #[cfg(test)]
    pub async fn start(&self) -> Result<(), String> {
        self.advance().await
    }

    pub async fn advance(&self) -> Result<(), String> {
        // 单飞 + 合并：running 期间的触发（子会话事件/用户消息/steer）只置 requested，
        // 由持有者在本轮结束后补跑一轮，避免并发双 turn 分叉 transcript。
        {
            let mut g = self.gate.lock().expect("Mutex 中毒（临界区内不应 panic）");
            if g.running {
                g.requested = true;
                return Ok(());
            }
            g.running = true;
        }
        let _guard = GateGuard {
            gate: self.gate.clone(),
        };
        loop {
            self.with_session(|s| {
                s.state = SessionState::Busy;
                s.updated_at = now();
            });
            let result = self.do_advance().await;
            self.sync_state();
            self.with_session(|s| s.updated_at = now());
            let rerun = {
                let mut g = self.gate.lock().expect("Mutex 中毒（临界区内不应 panic）");
                let r = g.requested || self.absorb_steer();
                g.requested = false;
                r
            };
            if !rerun {
                return result;
            }
            result.as_ref()?;
        }
    }

    async fn do_advance(&self) -> Result<(), String> {
        self.record_activity(Activity::Thinking {
            timestamp: now(),
            content: "编排智能体正在分析工作流并规划本轮调度".into(),
        });
        let ctx = self.build_context();
        let decision = match self.backend.decide(&ctx).await {
            Ok(d) => d,
            Err(e) => {
                self.record_activity(Activity::Error {
                    timestamp: now(),
                    detail: format!("编排 agent 调用失败：{e}"),
                });
                return Err(e);
            }
        };
        // 编排输出直接进对话流：静默（纯文本无动作）与推进（带动作）在引擎侧
        // 不作区分，完成与否由编排智能体判断，而非引擎状态位
        self.with_session(|s| {
            s.transcript.push(OrcMsg::Orc {
                text: decision.summary.clone(),

                timestamp: now(),
            });
        });
        if let Some(kids) = self.backend.take_synced_children() {
            self.with_session(|s| s.children = kids);
        }
        if let Some(activities) = self.backend.take_synced_activities() {
            self.record_activities(activities);
        }
        Ok(())
    }

    fn build_context(&self) -> OrcContext {
        // 每次推进前快照：连接与摘要取自 hub 最新状态（重连/加机后即时生效）
        let (machines, clients) = self.hub.snapshot();
        let s = self.session.read().expect("RwLock 中毒");
        OrcContext {
            plan: s.description.clone(),
            preamble: s.preamble.clone(),
            transcript: s
                .transcript
                .iter()
                .map(|m| match m {
                    OrcMsg::User { text, .. } => format!("用户：{text}"),
                    OrcMsg::Orc { text, .. } => format!("编排：{text}"),
                })
                .collect(),
            child_sessions: s.children.clone(),
            clients,
            machines,
            steer_inbox: Arc::clone(&self.steer_inbox),
        }
    }

    /// 关联普通会话状态变更（GUI 收到 `session.state_change` 通知时调用）。
    /// 变 idle 且变更原因非取消 → 注入变更信息并推进。
    /// 取消导致的不注入：编排者不应与用户的取消拉锯。
    pub async fn on_child_state(
        &self,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
        reason: StateChangeReason,
    ) -> Result<bool, String> {
        let machine = {
            let s = self.session.read().expect("RwLock 中毒");
            let Some(child) = s.children.iter().find(|c| c.id == session_id) else {
                return Ok(false);
            };
            child.machine_name.clone()
        };
        self.track_child_state(session_id, old_state, new_state);
        self.sync_state();
        if new_state == SessionState::Idle {
            if reason == StateChangeReason::Cancelled {
                return Ok(false);
            }
            self.with_session(|s| {
                s.transcript.push(OrcMsg::User {
                    text: format!(
                        "关联普通会话 {session_id}@{machine} 检测到状态变更：{old} -> {new}，\
                         变更原因为{why}",
                        old = old_state.as_str(),
                        new = new_state.as_str(),
                        why = reason_label(reason),
                    ),

                    timestamp: now(),
                });
            });
            if let Err(e) = self.advance().await {
                self.record_activity(Activity::Error {
                    timestamp: now(),
                    detail: format!("关联会话推进失败：{e}"),
                });
                return Err(e);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// 仅更新忙碌计数（不推进）。被过滤不推进的变更事件也须经此记账，
    /// 否则取消场景下计数永久偏高。与 [`Self::on_child_state`] 二选一调用，
    /// 不可叠加（重复计数）。
    pub fn note_child_state(
        &self,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
    ) {
        self.track_child_state(session_id, old_state, new_state);
        self.sync_state();
    }

    /// 按状态变更事件增减忙碌子会话计数。
    fn track_child_state(
        &self,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
    ) {
        let mounted = {
            let s = self.session.read().expect("RwLock 中毒");
            s.children.iter().any(|c| c.id == session_id)
        };
        if !mounted || old_state == new_state {
            return;
        }
        let mut busy = self
            .busy_children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）");
        match (old_state, new_state) {
            (SessionState::Idle, SessionState::Busy) => *busy += 1,
            (SessionState::Busy, SessionState::Idle) => *busy = busy.saturating_sub(1),
            _ => {}
        }
    }

    /// 工作流级忙闲：有忙碌子会话即工作中。
    fn sync_state(&self) {
        let busy = *self
            .busy_children
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            > 0;
        self.with_session(|s| {
            s.state = if busy {
                SessionState::Busy
            } else {
                SessionState::Idle
            };
        });
    }

    pub fn begin_busy(&self) {
        self.gate
            .lock()
            .expect("Mutex 中毒（临界区内不应 panic）")
            .requested = false;
        self.with_session(|s| {
            s.state = SessionState::Busy;
            s.updated_at = now();
        });
    }

    /// 返回是否应立即启动推进（false = 已在工作（steer 入队））。
    /// 工作流无终态：任何时刻的用户消息都推进。
    pub fn record_user(&self, text: &str) -> bool {
        let busy = {
            let s = self.session.read().expect("RwLock 中毒");
            s.state == SessionState::Busy
        };
        self.with_session(|s| {
            if s.description.trim().is_empty() {
                s.description = text.trim().to_string();
            }
            if s.title.trim().is_empty() {
                s.title = generate_title(text);
            }
            s.transcript.push(OrcMsg::User {
                text: text.to_string(),

                timestamp: now(),
            });
            s.updated_at = now();
        });
        if busy {
            // 工作中以 steer 注入，当前 turn 结束后再跑一轮。
            self.steer_inbox
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .push(text.to_string());
            return false;
        }
        true
    }

    /// 把 inbox 中尚未出现在 transcript 的 steer 消息合并进来。
    pub fn absorb_steer(&self) -> bool {
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
                .read()
                .expect("RwLock 中毒")
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::User { text: t, .. } if t == &text));
            if !exists {
                self.with_session(|s| {
                    s.transcript.push(OrcMsg::User {
                        text,
                        timestamp: now(),
                    });
                });
                added = true;
            }
        }
        added
    }

    /// 用户点击取消按钮：以用户消息方式注入固定取消指令
    /// （docs/DESIGN.md「工作流会话取消」），由编排智能体自行调用
    /// cancel_session 停止调度。返回是否应立即启动推进（false = 已在工作）。
    pub fn cancel(&self) -> bool {
        self.record_user(WORKFLOW_CANCEL_PROMPT)
    }

    pub fn persist(&self, data_dir: &Path) -> std::io::Result<()> {
        let snapshot = self.session.read().expect("RwLock 中毒").clone();
        crate::wfstore::save(data_dir, &snapshot)
    }

    /// 后台持久化：UI 线程只克隆引擎句柄（session 为 Arc<RwLock>），读盘写在
    /// tokio 后台完成。并发写以「整文件原子替换 + 唯一临时名」保证不损坏。
    pub fn persist_in_background(&self, data_dir: PathBuf) {
        let engine = self.clone();
        crate::ws::runtime().spawn(async move {
            if let Err(e) = engine.persist(&data_dir) {
                log::error!("工作流状态持久化失败 {}: {e}", engine.id());
            }
        });
    }

    pub fn load_all(data_dir: &Path) -> std::io::Result<Vec<OrcSession>> {
        // 惰性元数据加载：只读 sqlite，不读取 transcript/activities 两份 JSONL。
        crate::wfstore::load_all_meta(data_dir)
    }

    /// 按需补齐：把会话的 transcript/activities 从 JSONL 读入（仅打开渲染视图时调用）。
    /// 读盘在锁外完成，短暂持锁合并——避免持写锁做 IO 阻塞渲染与后台推进。
    pub fn backfill(&self, data_dir: &Path) -> std::io::Result<()> {
        let id = {
            let s = self.session.read().expect("RwLock 中毒");
            if !s.transcript.is_empty() || !s.activities.is_empty() {
                // 已加载过（例如推进中正在写内存），避免用磁盘旧快照覆盖活跃状态。
                return Ok(());
            }
            s.id.clone()
        };
        let (transcript, activities) = crate::wfstore::load_payload(data_dir, &id)?;
        self.with_session(|s| {
            // 竞态兜底：读盘期间若已有内容写入内存（推进已开始），不覆盖
            if s.transcript.is_empty() && s.activities.is_empty() {
                s.transcript = transcript;
                s.activities = activities;
            }
        });
        Ok(())
    }

    pub fn remove(data_dir: &Path, id: &str) -> std::io::Result<()> {
        crate::wfstore::remove(data_dir, id)
    }
}

impl OrcSession {
    /// 工作流会话对话流：用户消息（含系统注入的推进消息）与编排输出。
    pub fn to_dialog(&self) -> Vec<DialogMsg> {
        self.transcript
            .iter()
            .map(|m| match m {
                OrcMsg::User { text, timestamp } => DialogMsg::UserMessage {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: *timestamp,
                },
                OrcMsg::Orc { text, timestamp } => DialogMsg::AgentMessage {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: *timestamp,
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
//
// 不再用 rig `Agent::prompt` 的黑盒多 turn：它一旦发起无法中途插话，steer 只能
// 退化为整轮结束后重跑。改为保留 rig provider 层（三种 ApiFormat 仍由 rig 处理），
// 循环自己驱动：请求 → 解析工具调用 → 执行 → 结果回填 → drain steer 插话 → 再请求。

/// 工具循环的模型调用上限（与原 rig default_max_turns(8) 对齐），防失控。
const MAX_TOOL_TURNS: usize = 8;

/// 编排工具清单。
fn tool_definitions() -> Vec<rig_core::completion::ToolDefinition> {
    use rig_core::completion::ToolDefinition;
    vec![
        ToolDefinition {
            name: "list_agents".into(),
            description: "已注册机器及各机器的 agent 列表：机器在线状态、agent 可用性".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "list_sessions".into(),
            description: "本工作流的关联普通会话列表（标题、状态、最近活跃、机器在线与否）".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "create_session".into(),
            description: "向指定机器、指定 agent 与工作目录创建关联普通会话，返回会话 ID".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "machine": { "type": "string", "description": "机器名" },
                    "agent": { "type": "string", "description": "agent 名" },
                    "cwd": { "type": "string", "description": "工作目录" },
                    "worktree": { "type": "boolean", "description": "是否以 git worktree 方式工作（工作计划要求隔离修改主仓库时使用）；缺省 false" }
                },
                "required": ["machine", "agent", "cwd"]
            }),
        },
        ToolDefinition {
            name: "prompt_session".into(),
            description: "向关联普通会话下发指令".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "关联普通会话 id" },
                    "prompt": { "type": "string", "description": "指令内容" }
                },
                "required": ["session", "prompt"]
            }),
        },
        ToolDefinition {
            name: "cancel_session".into(),
            description: "取消关联普通会话进行中的工作".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" }
                },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "read_session_history".into(),
            description: "按窗口 / 游标读取关联普通会话对话内容".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "limit": { "type": "integer" },
                    "before": { "type": "integer" }
                },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "read_session_activities".into(),
            description: "按窗口 / 游标读取关联普通会话活动内容".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "limit": { "type": "integer" },
                    "before": { "type": "integer" }
                },
                "required": ["session"]
            }),
        },
    ]
}

fn assistant_text(choice: &[AssistantContent]) -> String {
    choice
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 薄工具循环：驱动模型直至输出纯文本（turn 结束）。
///
/// - 每轮请求前 drain `steer_inbox`，把用户插话注入为 user 消息（真实 steer）
/// - assistant 响应整体保留（含 reasoning/image 与 message_id），provider 协议
///   要求后续请求原样回传（如 OpenAI Responses API 的 reasoning 配对）
/// - 工具错误作为结果文本回传给模型自行纠正，不中断循环
/// - 纯文本收尾即编排智能体选择静默；引擎不设终态标记
async fn run_tool_loop<M>(
    model: M,
    preamble: &str,
    mut history: Vec<Message>,
    steer_inbox: &Mutex<Vec<String>>,
    live: &LiveRuntime,
) -> Result<String, String>
where
    M: CompletionModel + Clone + 'static,
{
    let tool_defs = tool_definitions();
    for _ in 0..MAX_TOOL_TURNS {
        // 轮次边界：用户插话实时进入下一轮请求
        let steers = std::mem::take(
            &mut *steer_inbox
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）"),
        );
        for text in steers {
            history.push(Message::user(format!("用户：{text}")));
        }
        // builder 把 prompt 追加到 chat_history 末尾，因此最后一条单独传
        let prompt = history.pop().ok_or("编排对话历史为空")?;
        let resp = model
            .completion_request(prompt)
            .preamble(preamble.to_string())
            .messages(history.iter().cloned())
            .tools(tool_defs.clone())
            .send()
            .await
            .map_err(|e| format!("编排 agent 调用失败: {e}"))?;
        let text = assistant_text(&resp.choice);
        let calls: Vec<ToolCall> = resp
            .choice
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        history.push(Message::Assistant {
            id: resp.message_id,
            content: resp.choice,
        });
        if calls.is_empty() {
            // 纯文本收尾 = 编排智能体选择静默（无动作可做）；是否「完成」由它
            // 自行判断，引擎不作状态标记（工作流会话没有 done 状态）
            return Ok(if text.trim().is_empty() {
                "（编排智能体未输出文字）".to_string()
            } else {
                text
            });
        }
        let mut results = Vec::with_capacity(calls.len());
        for tc in calls {
            // 0.42 起 provider 下发的 id 收敛到 tc.provider（call_id 必有，
            // 双标识 wire 另带 item_id）；无则回退 rig 关联句柄 tc.id
            let name = tc.function.name.clone();
            let outcome = match dispatch_tool(live, &name, tc.function.arguments).await {
                Ok(s) => s,
                Err(e) => format!("工具执行失败：{e}"),
            };
            let content = vec![ToolResultContent::text(outcome)];
            match tc.provider.clone() {
                Some(provider) => {
                    let item_id = provider
                        .item_id
                        .clone()
                        .unwrap_or_else(|| tc.id.to_string());
                    results.push(UserContent::tool_result_with_call_id(
                        item_id,
                        provider.call_id,
                        &name,
                        content,
                    ));
                }
                None => results.push(UserContent::tool_result(tc.id, &name, content)),
            }
        }
        history.push(Message::User { content: results });
    }
    Err(format!(
        "编排 agent 连续 {MAX_TOOL_TURNS} 轮未结束 turn（可能陷入循环），已中止本轮推进"
    ))
}

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

    fn preamble(&self) -> String {
        "你是 amux 的编排智能体。按工作流执行计划和用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在工作流执行计划和用户指令中指定的事项交由用户决定。\n\
         使用工具：list_agents、list_sessions、create_session、prompt_session、cancel_session、\
         read_session_history、read_session_activities。调度动作完成后用中文简述本轮动作并结束 turn。\
         当无需任何调度动作时（如全部步骤已完成、计划已无法继续、或需要人类判断），\
         不要调用工具，直接用中文输出说明并结束 turn 等待用户。"
            .to_string()
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
                preamble.push_str("【工作流计划/执行要求】\n");
                preamble.push_str(ctx.preamble.trim());
            }
            let model = self.cfg.model.clone();
            let live = LiveRuntime {
                machines: ctx.machines.clone(),
                clients: ctx.clients.clone(),
                children: Arc::new(Mutex::new(ctx.child_sessions.clone())),
                activities: Arc::new(Mutex::new(Vec::new())),
            };
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
            let history = vec![Message::user(input)];
            let text = match self.cfg.api_format {
                ApiFormat::ChatCompletions => {
                    let client = rig_core::providers::openai::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 OpenAI client 失败: {e}"))?;
                    run_tool_loop(
                        client.completions_api().completion_model(model),
                        &preamble,
                        history,
                        &ctx.steer_inbox,
                        &live,
                    )
                    .await?
                }
                ApiFormat::Responses => {
                    let client = rig_core::providers::openai::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 OpenAI client 失败: {e}"))?;
                    run_tool_loop(
                        client.completion_model(model),
                        &preamble,
                        history,
                        &ctx.steer_inbox,
                        &live,
                    )
                    .await?
                }
                ApiFormat::Messages => {
                    let client = rig_core::providers::anthropic::Client::builder()
                        .api_key(self.cfg.api_key.clone())
                        .base_url(self.cfg.base_url.clone())
                        .build()
                        .map_err(|e| format!("构建 Anthropic client 失败: {e}"))?;
                    run_tool_loop(
                        client.completion_model(model),
                        &preamble,
                        history,
                        &ctx.steer_inbox,
                        &live,
                    )
                    .await?
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
            Ok(Decision { summary: text })
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

/// 状态变更原因的中文标注，用于注入编排对话流。
fn reason_label(r: StateChangeReason) -> &'static str {
    match r {
        StateChangeReason::Completed => "正常完成",
        StateChangeReason::Cancelled => "已取消",
        StateChangeReason::MaxTokens => "达到 token 上限",
        StateChangeReason::MaxTurnRequests => "达到请求次数上限",
        StateChangeReason::Refusal => "agent 拒绝继续",
        StateChangeReason::Aborted => "异常终止",
    }
}

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
//
// 工具调用由循环自己解析 ToolCall 并调用（不再走 agent 运行时）：
// 错误以 String 返回、由循环回传给模型纠正。

/// 工具调用统一入口：按名称分派，参数从模型给出的 JSON 反序列化。
async fn dispatch_tool(
    live: &LiveRuntime,
    name: &str,
    args: serde_json::Value,
) -> Result<String, String> {
    match name {
        "list_agents" => list_agents(live).await,
        "list_sessions" => list_sessions(live).await,
        "create_session" => create_session(live, parse_args(name, args)?).await,
        "prompt_session" => prompt_session(live, parse_args(name, args)?).await,
        "cancel_session" => cancel_session(live, parse_args(name, args)?).await,
        "read_session_history" => {
            read_session_page(
                live,
                parse_args(name, args)?,
                protocol::method::SESSION_HISTORY,
            )
            .await
        }
        "read_session_activities" => {
            read_session_page(
                live,
                parse_args(name, args)?,
                protocol::method::SESSION_ACTIVITIES,
            )
            .await
        }
        other => Err(format!("未知工具: {other}")),
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    args: serde_json::Value,
) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("{tool} 参数解析失败: {e}"))
}

async fn list_agents(live: &LiveRuntime) -> Result<String, String> {
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
    let output = serde_json::to_string(&v).map_err(|e| format!("序列化失败: {e}"))?;
    live.record_tool("list_agents", "查询可用 agent", "");
    Ok(output)
}

async fn list_sessions(live: &LiveRuntime) -> Result<String, String> {
    use std::collections::{BTreeMap, HashMap};
    // 标题/忙闲/agent 以机器 server 为权威：按机器分组批量现查 session.info，
    // 本地不缓存这些易漂移的字段
    let children = live
        .children
        .lock()
        .expect("Mutex 中毒（临界区内不应 panic）")
        .clone();
    let mut ids_by_machine: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for c in &children {
        ids_by_machine
            .entry(c.machine_idx)
            .or_default()
            .push(c.id.clone());
    }
    let mut metas: HashMap<String, SessionMeta> = HashMap::new();
    for (idx, ids) in ids_by_machine {
        let Some(client) = live.clients.get(idx) else {
            continue;
        };
        if let Ok(r) = client
            .request::<_, SessionInfoResult>(
                protocol::method::SESSION_INFO,
                Some(SessionInfoParams { session_ids: ids }),
            )
            .await
        {
            for m in r.sessions {
                metas.insert(m.id.clone(), m);
            }
        }
    }
    let online_of = |c: &ChildSession| {
        live.machines
            .iter()
            .find(|m| m.name == c.machine_name)
            .map(|m| m.online)
            .unwrap_or(false)
    };
    let v: Vec<serde_json::Value> = children
        .iter()
        .map(|c| match metas.get(&c.id) {
            Some(m) => serde_json::json!({
                "id": c.id,
                "title": m.title,
                "state": m.state.as_str(),
                "lastActiveAt": m.last_active_at,
                "machine": c.machine_name,
                "agent": m.agent,
                "machineOnline": online_of(c),
            }),
            // server 侧已不存在（被删除等）：显式告知编排者
            None => serde_json::json!({
                "id": c.id,
                "machine": c.machine_name,
                "machineOnline": online_of(c),
                "missing": true,
            }),
        })
        .collect();
    let output = serde_json::to_string(&v).map_err(|e| format!("序列化失败: {e}"))?;
    live.record_tool("list_sessions", "查询关联普通会话", "");
    Ok(output)
}

#[derive(serde::Deserialize)]
struct CreateSessionArgs {
    machine: String,
    agent: String,
    cwd: String,
    /// 工作计划要求以 worktree 方式工作时由编排智能体传入；缺省 false
    #[serde(default)]
    worktree: bool,
}

async fn create_session(live: &LiveRuntime, args: CreateSessionArgs) -> Result<String, String> {
    let idx = live.machine_index(&args.machine)?;
    let client = live.client(&args.machine)?;
    let res = client
        .request::<_, SessionResult>(
            protocol::method::SESSION_NEW,
            Some(SessionNewParams {
                agent: args.agent.clone(),
                cwd: args.cwd.clone(),
                use_worktree: args.worktree,
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    let sid = res.session.id;
    if sid.is_empty() {
        return Err("创建会话未返回 id".to_string());
    }
    let machine_name = args.machine.clone();
    live.children
        .lock()
        .expect("Mutex 中毒（临界区内不应 panic）")
        .push(ChildSession {
            id: sid.clone(),
            machine_idx: idx,
            machine_name,
        });
    live.record_tool(
        "create_session",
        "创建关联普通会话",
        format!("{}@{}", sid, args.machine),
    );
    Ok(sid)
}

#[derive(serde::Deserialize)]
struct PromptSessionArgs {
    session: String,
    prompt: String,
}

async fn prompt_session(live: &LiveRuntime, args: PromptSessionArgs) -> Result<String, String> {
    let child = live.child(&args.session)?;
    let client = live
        .clients
        .get(child.machine_idx)
        .cloned()
        .ok_or_else(|| "机器连接已失效".to_string())?;
    let input = SessionPromptParams {
        session_id: args.session.clone(),
        input: vec![ContentBlock::Text { text: args.prompt }],
    };
    client
        .request_ok(protocol::method::SESSION_PROMPT, Some(input))
        .await
        .map_err(|e| e.to_string())?;
    live.record_tool(
        "prompt_session",
        "下发指令",
        format!("session={}", args.session),
    );
    Ok("已下发".into())
}

#[derive(serde::Deserialize)]
struct SessionRefArgs {
    session: String,
}

async fn cancel_session(live: &LiveRuntime, args: SessionRefArgs) -> Result<String, String> {
    let child = live.child(&args.session)?;
    let client = live
        .clients
        .get(child.machine_idx)
        .cloned()
        .ok_or_else(|| "机器连接已失效".to_string())?;
    client
        .request_ok(
            protocol::method::SESSION_CANCEL,
            Some(SessionIdParams {
                session_id: args.session.clone(),
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    live.record_tool(
        "cancel_session",
        "取消关联普通会话",
        format!("session={}", args.session),
    );
    Ok("已取消".into())
}

#[derive(serde::Deserialize)]
struct SessionPageArgs {
    session: String,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    before: Option<u64>,
}

async fn read_session_page(
    live: &LiveRuntime,
    args: SessionPageArgs,
    method: &'static str,
) -> Result<String, String> {
    let child = live.child(&args.session)?;
    let client = live
        .clients
        .get(child.machine_idx)
        .cloned()
        .ok_or_else(|| "机器连接已失效".to_string())?;
    let params = SessionPageParams {
        session_id: args.session.clone(),
        limit: args.limit.map(|l| l as usize),
        before: args.before,
    };
    let res: serde_json::Value = if method == protocol::method::SESSION_HISTORY {
        let r = client
            .request::<_, HistoryResult>(protocol::method::SESSION_HISTORY, Some(params))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(r).unwrap()
    } else {
        let r = client
            .request::<_, ActivitiesResult>(protocol::method::SESSION_ACTIVITIES, Some(params))
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(r).unwrap()
    };
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
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn machines() -> Vec<MachineSummary> {
        vec![MachineSummary::named("测试机", &["mock_acp"])]
    }

    fn clients_with_machines() -> (Vec<WsClient>, MachineSummary) {
        let m = machines().remove(0);
        let c = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        (vec![c], m)
    }

    fn test_hub(machines: Vec<MachineSummary>, clients: Vec<WsClient>) -> Arc<MachineHub> {
        let hub = MachineHub::default();
        hub.sync(machines, clients);
        Arc::new(hub)
    }

    /// 每个测试独立的临时数据目录（活动实时落盘与持久化测试共用）。
    fn temp_data_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "amux-wf-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn test_live() -> LiveRuntime {
        LiveRuntime {
            machines: vec![MachineSummary::named("测试机", &["mock_acp"])],
            clients: vec![WsClient::connect_with_token(
                "ws://127.0.0.1:1".into(),
                "unused".into(),
            )],
            children: Arc::new(Mutex::new(Vec::new())),
            activities: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn request_texts(req: &rig_core::completion::CompletionRequest) -> Vec<String> {
        let mut out = Vec::new();
        for m in req.chat_history.iter() {
            match m {
                Message::User { content } => {
                    for c in content.iter() {
                        if let UserContent::Text(t) = c {
                            out.push(t.text.clone());
                        }
                    }
                }
                Message::Assistant { content, .. } => {
                    for c in content.iter() {
                        if let AssistantContent::Text(t) = c {
                            out.push(t.text.clone());
                        }
                    }
                }
                Message::System { content } => out.push(content.clone()),
            }
        }
        out
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
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        assert_eq!(engine.session.read().unwrap().title, "实现登录功能");
        assert!(engine
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "实现登录功能\n然后写测试")));
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
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        assert!(engine
            .session
            .read()
            .unwrap()
            .description
            .contains("[上下文]"));
        assert!(engine
            .session
            .read()
            .unwrap()
            .description
            .contains("src/main.rs"));
    }

    /// 取消按钮注入固定取消指令并推进（docs/DESIGN.md「工作流会话取消」）。
    #[tokio::test]
    async fn cancel_injects_cancel_prompt_and_advances() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "已按指令取消".into(),
        }]);
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        assert!(engine.cancel(), "空闲工作流取消应立即推进");
        assert!(engine
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|msg| matches!(msg, OrcMsg::User { text, .. } if text == WORKFLOW_CANCEL_PROMPT)));
        engine.advance().await.unwrap();
        assert!(engine
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|msg| matches!(msg, OrcMsg::Orc { text, .. } if text == "已按指令取消")));
    }

    #[tokio::test]
    async fn on_child_state_idle_always_triggers_advance() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "本轮静默".into(),
        }]);
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        engine.session.write().unwrap().children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
        });
        let advanced = engine
            .on_child_state(
                "s_child",
                SessionState::Busy,
                SessionState::Idle,
                StateChangeReason::Completed,
            )
            .await
            .unwrap();
        assert!(advanced);
        assert!(
            engine
                .session
                .read()
                .unwrap()
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::Orc { text, .. } if text == "本轮静默")),
            "静默决策也应作为编排输出进入对话流"
        );
        assert!(
            engine
                .session
                .read()
                .unwrap()
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::User { text, .. }
                    if text.contains("busy -> idle，变更原因为正常完成"))),
            "注入文本应包含变更原因"
        );
    }

    #[tokio::test]
    async fn cancelled_reason_idle_event_does_not_advance() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![Decision {
            summary: "不应发生".into(),
        }]);
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        engine.session.write().unwrap().children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
        });
        let advanced = engine
            .on_child_state(
                "s_child",
                SessionState::Busy,
                SessionState::Idle,
                StateChangeReason::Cancelled,
            )
            .await
            .unwrap();
        assert!(!advanced);
        assert!(!engine
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text.contains("状态变更"))));
    }

    #[tokio::test]
    async fn persistence_roundtrip_and_restore() {
        let dir = std::env::temp_dir().join(format!("amux-wf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine =
            WorkflowEngine::new("计划A", "", "", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().unwrap().id.clone();
        engine.record_user("立即保存");
        engine.persist(&dir).unwrap();

        let sessions = WorkflowEngine::load_all(&dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].title, "计划A");
        // 惰性元数据加载：启动路径不读取 transcript。
        assert!(sessions[0].transcript.is_empty());
        assert!(sessions[0].activities.is_empty());

        // 打开会话时按需补齐，再 restore 推进。
        let mut opened = sessions[0].clone();
        let (transcript, activities) = crate::wfstore::load_payload(&dir, &opened.id).unwrap();
        opened.transcript = transcript;
        opened.activities = activities;
        assert!(opened
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "立即保存")));

        let backend2 = FakeBackend::new(vec![Decision {
            summary: "恢复后推进".into(),
        }]);
        let (clients2, m2) = clients_with_machines();
        let engine2 = WorkflowEngine::restore(opened, backend2, test_hub(vec![m2], clients2), &dir);
        engine2.start().await.unwrap();
        assert!(engine2
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { text, .. } if text == "恢复后推进")));

        // 推进后仍能持久化；再次惰性加载 + 补齐应恢复出完整 transcript（含推进消息）。
        engine2.persist(&dir).unwrap();
        let reloaded = WorkflowEngine::load_all(&dir).unwrap();
        assert!(reloaded[0].transcript.is_empty());
        let mut opened2 = reloaded[0].clone();
        let (transcript2, activities2) = crate::wfstore::load_payload(&dir, &opened2.id).unwrap();
        opened2.transcript = transcript2;
        opened2.activities = activities2;
        assert!(opened2
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "立即保存")));
        assert!(opened2
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::Orc { text, .. } if text == "恢复后推进")));

        WorkflowEngine::remove(&dir, &id).unwrap();
        assert!(WorkflowEngine::load_all(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_loads_payload_and_never_clobbers_live_session() {
        let dir = std::env::temp_dir().join(format!("amux-wf-backfill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine =
            WorkflowEngine::new("计划B", "", "", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().unwrap().id.clone();
        engine.record_user("准备保存");
        engine.persist(&dir).unwrap();

        // 从元数据惰性加载恢复出的 engine，backfill 补齐 transcript。
        let sessions = WorkflowEngine::load_all(&dir).unwrap();
        assert!(sessions[0].transcript.is_empty());
        let (clients2, m2) = clients_with_machines();
        let engine2 = WorkflowEngine::restore(
            sessions[0].clone(),
            FakeBackend::new(vec![]),
            test_hub(vec![m2], clients2),
            &dir,
        );
        engine2.backfill(&dir).unwrap();
        assert!(engine2
            .session
            .read()
            .unwrap()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "准备保存")));

        // 已在内存（推进中）的会话：backfill 不应被磁盘旧快照覆盖。
        let live = engine2.session.read().unwrap().transcript.len();
        engine2.record_user("推进中新增");
        engine2.backfill(&dir).unwrap();
        let after = engine2.session.read().unwrap();
        assert_eq!(after.transcript.len(), live + 1);
        assert!(after
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "推进中新增")));

        WorkflowEngine::remove(&dir, &id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn activities_recorded_are_immediately_on_disk() {
        let dir = temp_data_dir();
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new("计划", "", "", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().unwrap().id.clone();

        engine.record_activity(Activity::Thinking {
            timestamp: 1,
            content: "想".into(),
        });

        // 活动实时追加写盘：无需 persist，磁盘即可读到。
        let path = amux_common::session_log::activities_path(&dir, &id);
        let acts = amux_common::session_log::read_jsonl::<Activity>(&path).unwrap();
        assert_eq!(acts.len(), 1, "活动应实时落盘");
        assert!(matches!(&acts[0], Activity::Thinking { content, .. } if content == "想"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_definitions_cover_all_tools() {
        let names: Vec<String> = tool_definitions().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "list_agents",
                "list_sessions",
                "create_session",
                "prompt_session",
                "cancel_session",
                "read_session_history",
                "read_session_activities",
            ]
        );
    }

    #[test]
    fn api_format_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ApiFormat::ChatCompletions).unwrap(),
            "\"chat_completions\""
        );
        assert_eq!(
            serde_json::to_string(&ApiFormat::Responses).unwrap(),
            "\"responses\""
        );
        assert_eq!(
            serde_json::from_str::<ApiFormat>("\"messages\"").unwrap(),
            ApiFormat::Messages
        );
        assert!(serde_json::from_str::<ApiFormat>("\"graphql\"").is_err());
    }

    #[test]
    fn orc_session_to_dialog_maps_all_transcript_kinds() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(vec![MachineSummary::named("测试机", &["mock_acp"])], vec![]),
            &temp_data_dir(),
        );
        let engine = engine;
        engine
            .session
            .write()
            .unwrap()
            .transcript
            .push(OrcMsg::Orc {
                text: "决策".into(),

                timestamp: now(),
            });
        let dialog = engine.session.read().unwrap().to_dialog();
        assert_eq!(dialog.len(), 2);
        assert!(matches!(&dialog[0], DialogMsg::UserMessage { .. }));
        assert!(matches!(&dialog[1], DialogMsg::AgentMessage { .. }));
    }

    #[test]
    fn plan_as_preamble_not_in_history() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "",
            "",
            "计划：先在测试机实现，再审查",
            backend,
            test_hub(vec![MachineSummary::named("测试机", &["mock_acp"])], vec![]),
            &temp_data_dir(),
        );
        assert!(engine.session.read().unwrap().transcript.is_empty());
        assert_eq!(
            engine.session.read().unwrap().preamble,
            "计划：先在测试机实现，再审查"
        );
        assert!(engine.session.read().unwrap().title.is_empty());
    }

    #[test]
    fn record_user_sets_description_when_empty() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "",
            "",
            "计划：先实现后审查",
            backend,
            test_hub(vec![MachineSummary::named("测试机", &["mock_acp"])], vec![]),
            &temp_data_dir(),
        );
        let should_advance = engine.record_user("实现登录功能");
        assert_eq!(engine.session.read().unwrap().description, "实现登录功能");
        assert_eq!(engine.session.read().unwrap().title, "实现登录功能");
        assert!(should_advance);
    }

    #[tokio::test]
    async fn unconfigured_rig_backend_records_clear_error() {
        let backend = Arc::new(RigBackend::new(OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
        }));
        let client = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(
                vec![MachineSummary::named("测试机", &["kimi"])],
                vec![client],
            ),
            &temp_data_dir(),
        );
        let res = engine.start().await;
        assert!(res.is_err());
        assert!(engine.session.read().unwrap().activities.iter().any(
            |a| matches!(a, Activity::Error { detail, .. } if detail.contains("未配置编排 agent API"))
        ));
        assert_eq!(engine.session.read().unwrap().state, SessionState::Idle);
    }

    #[tokio::test]
    async fn dispatch_tool_rejects_unknown_name_and_bad_args() {
        let live = test_live();
        let err = dispatch_tool(&live, "no_such_tool", serde_json::json!({}))
            .await
            .expect_err("未知工具应报错");
        assert!(err.contains("未知工具"));

        let err = dispatch_tool(&live, "create_session", serde_json::json!({ "machine": 1 }))
            .await
            .expect_err("参数缺失应报错");
        assert!(err.contains("create_session 参数解析失败"));
    }

    #[tokio::test]
    async fn create_session_tool_rejects_unknown_machine() {
        let live = test_live();
        let err = dispatch_tool(
            &live,
            "create_session",
            serde_json::json!({ "machine": "未知机器", "agent": "mock_acp", "cwd": "/tmp" }),
        )
        .await
        .expect_err("未知机器应报错");
        assert!(err.contains("机器不存在"));
    }

    #[test]
    fn note_child_state_tracks_busy_count() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            "",
            "",
            backend,
            test_hub(vec![MachineSummary::named("测试机", &["mock_acp"])], vec![]),
            &temp_data_dir(),
        );
        let engine = engine;
        engine.session.write().unwrap().children.push(ChildSession {
            id: "s_child".into(),
            machine_idx: 0,
            machine_name: "测试机".into(),
        });
        engine.note_child_state("s_child", SessionState::Idle, SessionState::Busy);
        assert_eq!(engine.session.read().unwrap().state, SessionState::Busy);
        engine.note_child_state("s_child", SessionState::Busy, SessionState::Idle);
        assert_eq!(engine.session.read().unwrap().state, SessionState::Idle);
        engine.note_child_state("missing", SessionState::Idle, SessionState::Busy);
        assert_eq!(engine.session.read().unwrap().state, SessionState::Idle);
    }

    #[tokio::test]
    async fn tool_loop_runs_tools_then_returns_text() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "list_agents", serde_json::json!({})),
            MockTurn::text("已查询可用 agent，本轮无调度动作"),
        ]);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let out = run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("循环应正常结束");
        assert_eq!(out, "已查询可用 agent，本轮无调度动作");
        assert_eq!(model.request_count(), 2);
        let second = &model.requests()[1];
        let has_tool_result = second.chat_history.iter().any(|m| {
            matches!(
                m,
                Message::User { content } if content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_)))
            )
        });
        assert!(has_tool_result, "第二轮请求应携带工具结果消息");
        assert!(matches!(
            second.chat_history.first(),
            Some(Message::System { .. })
        ));
    }

    #[tokio::test]
    async fn tool_loop_drains_steers_into_next_request() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("call_1", "list_agents", serde_json::json!({})),
            MockTurn::text("收到插话，调整方向"),
        ]);
        let live = test_live();
        let inbox = Mutex::new(vec!["中途插话：换一个 agent".to_string()]);
        let out = run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("循环应正常结束");
        assert_eq!(out, "收到插话，调整方向");
        let first_texts = request_texts(&model.requests()[0]);
        assert!(
            first_texts
                .iter()
                .any(|t| t.contains("用户：中途插话：换一个 agent")),
            "插话应出现在第一轮请求中"
        );
        assert!(inbox.lock().unwrap().is_empty(), "插话只注入一次");
    }

    #[tokio::test]
    async fn tool_loop_caps_at_max_turns() {
        let turns: Vec<MockTurn> = (0..MAX_TOOL_TURNS)
            .map(|i| MockTurn::tool_call(format!("c{i}"), "list_agents", serde_json::json!({})))
            .collect();
        let model = MockCompletionModel::from_turns(turns);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let res = run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("未结束 turn"));
        assert_eq!(model.request_count(), MAX_TOOL_TURNS);
    }
}
