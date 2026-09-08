//! 工作流引擎：
//! GUI 本地工作流会话 + 自研薄工具循环编排。
//!
//! - `OrcSession`：工作流会话状态，可序列化持久化到 SQLite 和两份 JSONL 日志
//! - `OrcBackend`：单 turn 决策器；真实实现 `RigBackend` 保留 rig provider 层，
//!   循环自研（`run_tool_loop`，参考 rig-agent 的流式运行时）——流式请求 →
//!   文本增量实时进对话流、reasoning 增量合并为 thinking 活动 → 解析工具调用 →
//!   执行 → 结果回填 → drain steer 插话 → 再流式请求，使 steer 能在轮次边界
//!   真实注入，编排输出对用户实时可见
//! - `WorkflowEngine`：状态机——首 turn 拆解计划并创建/复用关联普通会话下发指令；
//!   关联普通会话 idle（`session.state_change` 通知驱动）触发自动推进
//! - 会话操作统一经真实 WsClient（SESSION_NEW / SESSION_PROMPT / SESSION_CANCEL）

#[cfg(test)]
use amux_common::session_log::read_jsonl;
use amux_common::session_log::{activities_path, append_jsonl, history_path};
use parking_lot::{Mutex, RwLock};
#[cfg(test)]
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    Reasoning, ReasoningContent, ToolCall, ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, CompletionModel, Message};
use rig_core::streaming::StreamedAssistantContent;

use serde::{Deserialize, Serialize};

use protocol::{
    generate_title, ActivitiesResult, Activity, ContentBlock, HistoryItem, HistoryResult,
    SessionConfigOptionsResult, SessionConfigSetting, SessionConfigureParams, SessionIdParams,
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
/// 机器一律按 `machine_name` 解析：它是唯一稳定身份，下标会随机器列表
/// 增删重排而失效。
#[derive(Debug, Clone)]
pub struct LinkedSession {
    pub id: String,
    pub machine_name: String,
}

/// 工作流会话（GUI 本地状态）。
#[derive(Debug, Clone)]
pub struct OrcSession {
    pub id: String,
    pub title: String,
    /// 用户输入的完整执行计划；随元数据持久化
    pub plan: String,
    pub state: SessionState,
    pub transcript: Vec<OrcMsg>,
    pub linked_sessions: Vec<LinkedSession>,
    pub activities: Vec<Activity>,
    pub created_at: u64,
    pub last_active_at: u64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 仅内存使用（机器摘要快照），无序列化需求。
#[derive(Debug, Clone)]
pub struct AgentSlot {
    pub name: String,
    pub available: bool,
}

/// 仅内存使用（机器摘要快照），无序列化需求。
#[derive(Debug, Clone)]
pub struct MachineSummary {
    pub name: String,
    /// 机器是否在线
    pub online: bool,
    pub agents: Vec<AgentSlot>,
}

/// 引擎 → 应用事件（应用订阅后即时刷新 UI）。
#[derive(Debug, Clone)]
pub enum HubEvent {
    /// 编排智能体已挂载新的关联普通会话（create_session 工具执行成功）；
    /// 应用按 machine_name 刷新对应机器的会话列表。
    LinkedSessionMounted { machine_name: String },
}

/// 机器运行时注册表：应用侧在机器增删/重连/状态变化时整体同步，
/// 工作流引擎每次推进前快照最新连接与摘要。引擎不再持有冻结的
/// WsClient 列表——否则重连后旧连接的接收端已关闭，工作流从此
/// 无法下发/取消任何关联普通会话，新增机器也对编排 LLM 不可见。
/// 同时充当引擎 → 应用的轻量事件通道（关联普通会话挂载后通知应用即时刷新）。
/// 单台机器的运行时条目：摘要与连接成对存放。
/// 替代平行 Vec——下标对齐只靠约定维护，重排/截断时会静默错位；
/// `client` 为 None 即该机不可达（原「clients 可短于 machines，zip 截断」语义的显式化）。
#[derive(Debug, Clone)]
pub struct MachineEntry {
    pub summary: MachineSummary,
    pub client: Option<WsClient>,
}

#[derive(Debug)]
pub struct MachineHub {
    entries: Mutex<Vec<MachineEntry>>,
    events: tokio::sync::broadcast::Sender<HubEvent>,
}

impl Default for MachineHub {
    fn default() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(64);
        MachineHub {
            entries: Mutex::new(Vec::new()),
            events,
        }
    }
}

impl MachineHub {
    /// 应用侧机器视图整体替换（顺序与 app.machines 一致）。
    /// `client` 为 None 的机器不可达（编排侧报「机器未连接」）。
    pub fn sync(&self, machines: Vec<(MachineSummary, Option<WsClient>)>) {
        *self.entries.lock() = machines
            .into_iter()
            .map(|(summary, client)| MachineEntry { summary, client })
            .collect();
    }

    /// 推进前快照：拿到最新连接与摘要。
    pub fn snapshot(&self) -> Vec<MachineEntry> {
        self.entries.lock().clone()
    }

    /// 通知应用：编排智能体已挂载新的关联普通会话（create_session 工具）。
    /// 事件丢失只影响刷新时机（应用侧 10s 轮询兜底），不阻塞编排。
    pub fn linked_session_mounted(&self, machine_name: &str) {
        let _ = self.events.send(HubEvent::LinkedSessionMounted {
            machine_name: machine_name.to_string(),
        });
    }

    /// 订阅引擎事件（应用侧）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<HubEvent> {
        self.events.subscribe()
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
#[derive(Clone)]
pub struct OrcContext {
    pub plan: String,
    pub transcript: Vec<String>,
    pub linked_sessions: Vec<LinkedSession>,
    /// 引擎共享会话：create_session 工具即时挂载关联普通会话，
    /// 不等整轮 decide 结束就让 GUI 看到关联关系
    pub session: Arc<RwLock<OrcSession>>,
    pub machines: Vec<MachineEntry>,
    /// 引擎 → 应用事件通道：挂载新关联普通会话后通知应用即时刷新会话列表
    pub hub: Arc<MachineHub>,
    /// 关联普通会话挂载后的即时落库钩子（create_session 工具挂载后立即调用；
    /// 整轮结束后的 persist 只兜底全量快照）
    pub persist_on_linked_session_mounted: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 编排实时活动钩子：工具调用、模型 reasoning 等真实进展实时记录并落盘
    pub record_tool_activity: Option<Arc<dyn Fn(Activity) + Send + Sync>>,
    /// 编排进行中用户插话的实时通道：RigBackend 工具循环在每轮请求边界 drain，
    /// 注入为 user 消息。
    /// 与 `WorkflowEngine.steer_inbox` 是同一个 Arc；advance 收尾的 absorb_steer 只兜底剩余项。
    pub steer_inbox: Arc<Mutex<Vec<String>>>,
    /// 编排输出草稿：流式文本实时进入对话流；
    /// 引擎在整轮结束后据此判断 backend 是否已自行提交输出，避免重复推送。
    pub draft: Arc<OrcDraft>,
    /// 进行中实时活动槽（见 `WorkflowEngine.current_activity`）：工具循环写入，
    /// 动作结束即清除。
    pub current: Arc<Mutex<Option<Activity>>>,
}

/// 编排输出草稿：turn 期间流式文本先以一条 evolving 的编排消息写入
/// transcript（GUI 定时刷新即可看到生成中的输出），整轮成功后原地保留
/// （即最终输出），turn 失败则移除，不留半截文本。
///
/// transcript 在 turn 期间只增不删，草稿按下标定位自身消息；turn 期间
/// 可能并发插入的只有用户消息（record_user/absorb_steer），不影响下标。
pub struct OrcDraft {
    session: Arc<RwLock<OrcSession>>,
    slot: Mutex<Option<usize>>,
}

impl OrcDraft {
    fn new(session: Arc<RwLock<OrcSession>>) -> Self {
        OrcDraft {
            session,
            slot: Mutex::new(None),
        }
    }

    /// 追加流式文本增量：首个增量创建草稿消息，后续追加到最后一条。
    fn append(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let mut slot = self.slot.lock();
        let mut s = self.session.write();
        match *slot {
            Some(i) => {
                if let Some(OrcMsg::Orc { text, .. }) = s.transcript.get_mut(i) {
                    text.push_str(delta);
                }
            }
            None => {
                s.transcript.push(OrcMsg::Orc {
                    text: delta.to_string(),
                    timestamp: now(),
                });
                *slot = Some(s.transcript.len() - 1);
            }
        }
    }

    /// 提交一条完整的编排输出消息（无流式增量的收尾/兜底文案）。
    fn push_message(&self, text: &str) {
        let mut slot = self.slot.lock();
        let mut s = self.session.write();
        s.transcript.push(OrcMsg::Orc {
            text: text.to_string(),
            timestamp: now(),
        });
        if slot.is_none() {
            *slot = Some(s.transcript.len() - 1);
        }
    }

    /// turn 失败时移除草稿消息（幂等）。
    fn discard(&self) {
        let mut slot = self.slot.lock();
        if let Some(i) = slot.take() {
            let mut s = self.session.write();
            if matches!(s.transcript.get(i), Some(OrcMsg::Orc { .. })) {
                s.transcript.remove(i);
            }
        }
    }

    /// 本 turn 是否已有编排输出进入 transcript。
    fn started(&self) -> bool {
        self.slot.lock().is_some()
    }

    /// 草稿消息在 transcript 中的下标（尚未起草时 None）。
    fn slot(&self) -> Option<usize> {
        *self.slot.lock()
    }
}

pub trait OrcBackend: Send + Sync {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
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
/// 释放门闩后同步工作流状态，避免最后一个关联普通会话在编排 turn 结束前完成时
/// 把状态错误地改成空闲，或 turn 异常退出后永远保持工作中。
struct GateGuard {
    gate: Arc<Mutex<AdvanceGate>>,
    busy_linked_sessions: Arc<Mutex<usize>>,
    session: Arc<RwLock<OrcSession>>,
    active: bool,
}

impl GateGuard {
    /// 在已持有 gate 时正常结束推进。
    fn finish_locked(
        active: &mut bool,
        busy_linked_sessions: &Arc<Mutex<usize>>,
        session: &Arc<RwLock<OrcSession>>,
        gate: &mut AdvanceGate,
    ) {
        if !*active {
            return;
        }
        gate.running = false;
        let busy = *busy_linked_sessions.lock() > 0;
        let mut session = session.write();
        session.state = if busy {
            SessionState::Busy
        } else {
            SessionState::Idle
        };
        session.last_active_at = now();
        *active = false;
    }

    /// 正常结束时在同一 gate 临界区内释放 running 并同步最终状态。
    /// 释放后新的用户消息才能可靠地判断为「启动新 turn」，旧 guard 也不能
    /// 在新 turn 已开始后再次把 running 改回 false。
    fn finish(&mut self) {
        if !self.active {
            return;
        }
        let gate_arc = Arc::clone(&self.gate);
        let busy_linked_sessions = Arc::clone(&self.busy_linked_sessions);
        let session = Arc::clone(&self.session);
        let mut gate = gate_arc.lock();
        Self::finish_locked(&mut self.active, &busy_linked_sessions, &session, &mut gate);
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        // panic/错误早退兜底；正常路径已由 finish 释放并使 guard 失效。
        self.finish();
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
    /// 关联普通会话忙碌计数：其状态不落盘（权威在机器 server），仅按状态变更
    /// 事件增减；驱动工作流级忙闲显示与 steer 路由。重启归零。
    busy_linked_sessions: Arc<Mutex<usize>>,
    /// 活动实时落盘目录：活动产生即追加写 `<data_dir>/sessions/<id>_activities.jsonl`，
    /// 不等 `persist` 整文件快照。
    data_dir: PathBuf,
    /// 编排智能体正在进行的实时活动（进行中才有）：流式思考增量、执行中的
    /// 工具调用。动作结束即清除——历史活动不充当实时展示（修复工具执行
    /// 完毕后实时活动条一直展示）。与 `OrcContext.current` 是同一个 Arc。
    current_activity: Arc<Mutex<Option<Activity>>>,
    /// 串行化同一工作流的后台持久化，避免旧快照在新快照之后落盘。
    persist_lock: Arc<Mutex<()>>,
    /// 标记为删除中的工作流；墓碑保留在所有引擎克隆之间，阻止排队的旧持久化复活记录。
    deleted: Arc<AtomicBool>,
    /// 串行化同一工作流的历史/活动 JSONL 追加，避免并发写者交错行内容。
    log_lock: Arc<Mutex<()>>,
}

impl WorkflowEngine {
    pub fn new(
        plan: &str,
        backend: Arc<dyn OrcBackend>,
        hub: Arc<MachineHub>,
        data_dir: &Path,
    ) -> Self {
        let transcript = Vec::new();
        // 执行计划不进对话消息历史：计划存元数据，用户在对话界面输入消息，
        // 经 record_user 写入 transcript 并触发推进。
        let t = now();
        let session = OrcSession {
            id: format!("orc_{}", uuid::Uuid::new_v4()),
            // 标题由用户首个指令生成（record_user），不取自工作流计划
            title: String::new(),
            plan: plan.to_string(),
            state: SessionState::Idle,
            transcript,
            linked_sessions: Vec::new(),
            activities: Vec::new(),
            created_at: t,
            last_active_at: t,
        };
        WorkflowEngine::with_parts(session, backend, hub, data_dir)
    }

    pub fn restore(
        mut session: OrcSession,
        backend: Arc<dyn OrcBackend>,
        hub: Arc<MachineHub>,
        data_dir: &Path,
    ) -> Self {
        // 应用重开后工作流会话回到空闲，重新启动需用户手动触发。
        session.state = SessionState::Idle;
        WorkflowEngine::with_parts(session, backend, hub, data_dir)
    }

    /// 两个构造函数的共享主体：并发状态（gate/inbox/锁）一律全新空态。
    fn with_parts(
        session: OrcSession,
        backend: Arc<dyn OrcBackend>,
        hub: Arc<MachineHub>,
        data_dir: &Path,
    ) -> Self {
        WorkflowEngine {
            session: Arc::new(RwLock::new(session)),
            backend,
            hub,
            data_dir: data_dir.to_path_buf(),
            gate: Arc::new(Mutex::new(AdvanceGate::default())),
            steer_inbox: Arc::new(Mutex::new(Vec::new())),
            busy_linked_sessions: Arc::new(Mutex::new(0)),
            current_activity: Arc::new(Mutex::new(None)),
            persist_lock: Arc::new(Mutex::new(())),
            deleted: Arc::new(AtomicBool::new(false)),
            log_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 短临界区可变访问（长 await 一律发生在锁外）。
    fn with_session<R>(&self, f: impl FnOnce(&mut OrcSession) -> R) -> R {
        let mut s = self.session.write();
        f(&mut s)
    }

    /// 会话 id（短临界区读取）。
    pub fn id(&self) -> String {
        self.session.read().id.clone()
    }

    /// 会话状态（短临界区读取）。
    pub fn state(&self) -> SessionState {
        self.session.read().state
    }

    /// 会话标题（短临界区读取）。
    pub fn title(&self) -> String {
        self.session.read().title.clone()
    }

    /// 会话标题是否为空。
    pub fn title_is_empty(&self) -> bool {
        self.session.read().title.is_empty()
    }

    /// 关联普通会话列表的克隆（短临界区读取）。
    pub fn linked_sessions(&self) -> Vec<LinkedSession> {
        self.session.read().linked_sessions.clone()
    }

    /// 关联普通会话数量。
    pub fn linked_session_count(&self) -> usize {
        self.session.read().linked_sessions.len()
    }

    /// 会话快照（仅读字段的克隆；调用方需持有 RwLock 语义）。
    pub fn snapshot(&self) -> OrcSession {
        self.session.read().clone()
    }

    /// 编排智能体正在进行的实时活动（无进行中动作即 None）。
    pub fn current_activity(&self) -> Option<Activity> {
        self.current_activity.lock().clone()
    }

    fn clear_current_activity(&self) {
        *self.current_activity.lock() = None;
    }

    /// 记录一条活动并实时追加落盘（不依赖 `persist` 的整文件快照）。
    pub fn record_activity(&self, act: Activity) {
        self.with_session(|s| s.activities.push(act.clone()));
        self.append_activities(&[act]);
    }

    /// 追加写活动 JSONL；失败仅记日志，不阻断推进（活动落盘尽力而为）。
    fn append_activities(&self, acts: &[Activity]) {
        let _log = self.log_lock.lock();
        let id = self.session.read().id.clone();
        let path = activities_path(&self.data_dir, &id);
        if let Err(e) = append_jsonl(&path, acts) {
            log::error!("工作流活动落盘失败 {id}: {e}");
        }
    }

    /// 追加写对话历史 JSONL：条目在内存中合并完整后立即落盘（见 DESIGN
    /// 「流式输出合并后写入」）。失败仅记日志，不阻断推进。
    fn append_history(&self, items: &[HistoryItem]) {
        let _log = self.log_lock.lock();
        let id = self.session.read().id.clone();
        let path = history_path(&self.data_dir, &id);
        if let Err(e) = append_jsonl(&path, items) {
            log::error!("工作流对话历史落盘失败 {id}: {e}");
        }
    }

    pub async fn advance(&self) -> Result<(), String> {
        // 单飞 + 合并：running 期间的触发（关联普通会话事件/用户消息/steer）只置 requested，
        // 由持有者在本轮结束后补跑一轮，避免并发双 turn 分叉 transcript。
        {
            let mut g = self.gate.lock();
            if g.running {
                g.requested = true;
                return Ok(());
            }
            g.running = true;
        }
        let mut guard = GateGuard {
            gate: self.gate.clone(),
            busy_linked_sessions: self.busy_linked_sessions.clone(),
            session: self.session.clone(),
            active: true,
        };
        loop {
            self.with_session(|s| {
                s.state = SessionState::Busy;
                s.last_active_at = now();
            });
            let result = self.do_advance().await;
            self.sync_state();
            self.with_session(|s| s.last_active_at = now());
            let rerun = {
                let mut g = self.gate.lock();
                let steer_added = self.absorb_steer_locked();
                let rerun = g.requested || steer_added;
                g.requested = false;
                if !rerun {
                    let busy_linked_sessions = Arc::clone(&guard.busy_linked_sessions);
                    let session = Arc::clone(&guard.session);
                    GateGuard::finish_locked(
                        &mut guard.active,
                        &busy_linked_sessions,
                        &session,
                        &mut g,
                    );
                }
                rerun
            };
            if !rerun {
                return result;
            }
            result.as_ref()?;
        }
    }

    async fn do_advance(&self) -> Result<(), String> {
        // 清残留：上一 turn 的进行中活动不应延续到新 turn
        self.clear_current_activity();
        // 不构造活动占位：实时活动只记录真实进展（模型 reasoning、
        // 调度工具调用），由工具循环经 record_tool_activity 上报
        let ctx = self.build_context();
        let decision = match self.backend.decide(&ctx).await {
            Ok(text) => text,
            Err(e) => {
                self.clear_current_activity();
                self.record_activity(Activity::Error {
                    timestamp: now(),
                    detail: format!("编排 agent 调用失败：{e}"),
                });
                return Err(e);
            }
        };
        // turn 结束即无进行中动作（关联会话仍工作时实时活动条为空）
        self.clear_current_activity();
        // 编排输出直接进对话流：流式输出已由工具循环经草稿消息实时写入
        // （含静默/收尾兜底文案），这里只补推未经流式路径的后端输出
        // （如测试用 FakeBackend）；完成与否由编排智能体判断，而非引擎状态位
        if let Some(i) = ctx.draft.slot() {
            // 流式路径：最终输出已合并进草稿消息，整轮成功时把这条完整消息追加落盘。
            let item = {
                let s = self.session.read();
                match s.transcript.get(i) {
                    Some(OrcMsg::Orc { text, timestamp }) => Some(HistoryItem::AgentMessage {
                        content: vec![ContentBlock::Text { text: text.clone() }],
                        timestamp: *timestamp,
                    }),
                    _ => None,
                }
            };
            if let Some(item) = item {
                self.append_history(&[item]);
            }
        } else {
            // 非流式后端（如测试 FakeBackend）：推入完整输出后立即追加落盘。
            let output = decision;
            let ts = now();
            self.with_session(|s| {
                s.transcript.push(OrcMsg::Orc {
                    text: output.clone(),
                    timestamp: ts,
                });
            });
            self.append_history(&[HistoryItem::AgentMessage {
                content: vec![ContentBlock::Text { text: output }],
                timestamp: ts,
            }]);
        }
        Ok(())
    }

    fn build_context(&self) -> OrcContext {
        // 每次推进前快照：连接与摘要取自 hub 最新状态（重连/加机后即时生效）
        let machines = self.hub.snapshot();
        // 关联普通会话挂载后立即落库（后台任务写盘），不依赖整轮结束后的 persist 快照
        let engine = self.clone();
        let data_dir = self.data_dir.clone();
        let persist_on_linked_session_mounted: Option<Arc<dyn Fn() + Send + Sync>> =
            Some(Arc::new(move || {
                engine.persist_in_background(data_dir.clone());
            }));
        let s = self.session.read();
        let record_engine = self.clone();
        OrcContext {
            plan: s.plan.clone(),
            record_tool_activity: Some(Arc::new(move |act| record_engine.record_activity(act))),
            transcript: s
                .transcript
                .iter()
                .map(|m| match m {
                    OrcMsg::User { text, .. } => format!("用户：{text}"),
                    OrcMsg::Orc { text, .. } => format!("编排：{text}"),
                })
                .collect(),
            linked_sessions: s.linked_sessions.clone(),
            session: Arc::clone(&self.session),
            machines,
            hub: Arc::clone(&self.hub),
            persist_on_linked_session_mounted,
            steer_inbox: Arc::clone(&self.steer_inbox),
            draft: Arc::new(OrcDraft::new(Arc::clone(&self.session))),
            current: Arc::clone(&self.current_activity),
        }
    }

    /// 关联普通会话状态变更（GUI 收到 `session.state_change` 通知时调用）。
    /// 变 idle 且变更原因非取消 → 注入变更信息并推进。
    /// 取消导致的不注入：编排者不应与用户的取消拉锯。
    pub async fn on_linked_session_state(
        &self,
        machine_name: &str,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
        reason: StateChangeReason,
    ) -> Result<bool, String> {
        let mounted = {
            let s = self.session.read();
            s.linked_sessions
                .iter()
                .any(|c| c.machine_name == machine_name && c.id == session_id)
        };
        if !mounted {
            return Ok(false);
        }
        self.track_linked_session_state(machine_name, session_id, old_state, new_state);
        self.sync_state();
        if new_state == SessionState::Idle {
            if reason == StateChangeReason::Cancelled {
                return Ok(false);
            }
            let msg = format!(
                "关联普通会话 {session_id}@{machine_name} 检测到状态变更：{old} -> {new}，\
                 变更原因为{why}",
                old = old_state.as_str(),
                new = new_state.as_str(),
                why = reason_label(reason),
            );
            let ts = now();
            self.with_session(|s| {
                s.transcript.push(OrcMsg::User {
                    text: msg.clone(),
                    timestamp: ts,
                });
            });
            // 系统注入的用户消息同样即时追加落盘。
            self.append_history(&[HistoryItem::UserMessage {
                content: vec![ContentBlock::Text { text: msg }],
                timestamp: ts,
            }]);
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
    /// 否则取消场景下计数永久偏高。与 [`Self::on_linked_session_state`] 二选一调用，
    /// 不可叠加（重复计数）。
    pub fn note_linked_session_state(
        &self,
        machine_name: &str,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
    ) {
        self.track_linked_session_state(machine_name, session_id, old_state, new_state);
        self.sync_state();
    }

    /// 按状态变更事件增减忙碌关联普通会话计数。
    fn track_linked_session_state(
        &self,
        machine_name: &str,
        session_id: &str,
        old_state: SessionState,
        new_state: SessionState,
    ) {
        let mounted = {
            let s = self.session.read();
            s.linked_sessions
                .iter()
                .any(|c| c.machine_name == machine_name && c.id == session_id)
        };
        if !mounted || old_state == new_state {
            return;
        }
        let mut busy = self.busy_linked_sessions.lock();
        match (old_state, new_state) {
            (SessionState::Idle, SessionState::Busy) => *busy += 1,
            (SessionState::Busy, SessionState::Idle) => *busy = busy.saturating_sub(1),
            _ => {}
        }
    }

    fn sync_state(&self) {
        let orchestrating = self.gate.lock().running;
        let linked_session_busy = *self.busy_linked_sessions.lock() > 0;
        self.with_session(|s| {
            s.state = if orchestrating || linked_session_busy {
                SessionState::Busy
            } else {
                SessionState::Idle
            };
        });
    }

    pub fn begin_busy(&self) {
        self.gate.lock().requested = false;
        self.with_session(|s| {
            s.state = SessionState::Busy;
            s.last_active_at = now();
        });
    }

    /// 标记后台推进已排队但尚未进入 `advance`；不修改 gate，避免吞掉并发 turn 的重跑请求。
    pub fn mark_busy_pending(&self) {
        self.with_session(|s| {
            s.state = SessionState::Busy;
            s.last_active_at = now();
        });
    }

    /// 返回是否应立即启动推进（false = 编排 turn 已在工作，消息走 steer）。
    /// 关联普通会话忙但编排空闲时仍应启动新的编排 turn，不能把用户消息留在 steer 队列。
    pub fn record_user(&self, text: &str) -> bool {
        // gate 同时保护「turn 是否运行」与 steer 入队：不能先读 running、释放锁，
        // 再入队，否则恰好撞上 turn 收尾时可能既没被本轮吸收，也没触发新 turn。
        // 锁顺序固定为 gate → steer_inbox → session，与推进收尾一致。
        let mut gate = self.gate.lock();
        let orchestrating = gate.running;
        let mut steer = orchestrating.then(|| self.steer_inbox.lock());
        if let Some(steer) = &mut steer {
            steer.push(text.to_string());
        }
        if orchestrating {
            // 当前 turn 收尾后必须再跑一轮；消息已先写入 transcript，不能依赖
            // absorb_steer 通过「新增 transcript」来判断是否需要重跑。
            // gate 保护下设置 requested，与收尾检查原子配对。
            gate.requested = true;
        }
        let ts = now();
        self.with_session(|s| {
            if s.title.trim().is_empty() {
                s.title = generate_title(text);
            }
            s.transcript.push(OrcMsg::User {
                text: text.to_string(),
                timestamp: ts,
            });
            s.last_active_at = ts;
        });
        // 用户消息一产生即为完整条目，立即追加落盘。
        self.append_history(&[HistoryItem::UserMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp: ts,
        }]);
        drop(steer);
        drop(gate);
        !orchestrating
    }

    /// 把 inbox 中尚未出现在 transcript 的 steer 消息合并进来。
    /// 调用方已持有 gate（advance 收尾），避免重复获取 gate。
    fn absorb_steer_locked(&self) -> bool {
        let msgs = std::mem::take(&mut *self.steer_inbox.lock());
        let mut added = false;
        for text in msgs {
            let exists = self
                .session
                .read()
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

    /// 用户点击取消按钮：以用户消息方式注入固定取消指令，由编排智能体调用
    /// `cancel_session` 停止调度。返回是否应立即启动推进（false = 已在工作）。
    pub fn cancel(&self) -> bool {
        self.record_user(WORKFLOW_CANCEL_PROMPT)
    }

    pub fn persist(&self, data_dir: &Path) -> std::io::Result<()> {
        let _persist = self.persist_lock.lock();
        if self.deleted.load(Ordering::Acquire) {
            return Ok(());
        }
        let snapshot = self.session.read().clone();
        crate::wfstore::save(data_dir, &snapshot)
    }

    /// 标记工作流进入删除流程；所有引擎克隆共享该墓碑。
    pub fn mark_deleted(&self) {
        self.deleted.store(true, Ordering::Release);
    }

    /// 远端删除失败或连接发生变化时撤销删除墓碑。
    pub fn unmark_deleted(&self) {
        self.deleted.store(false, Ordering::Release);
    }

    /// 在持久化锁内删除工作流，确保已排队的旧快照不能在删除之后回写。
    pub fn remove_deleted(&self, data_dir: &Path) -> std::io::Result<()> {
        let _persist = self.persist_lock.lock();
        if !self.deleted.load(Ordering::Acquire) {
            return Ok(());
        }
        crate::wfstore::remove(data_dir, &self.id())
    }

    /// 后台持久化：UI 线程只克隆引擎句柄（session 为 Arc<RwLock>），元数据落在
    /// tokio 后台写 sqlite；对话历史/活动已由引擎在条目完整时实时追加落盘。
    pub fn persist_in_background(&self, data_dir: PathBuf) {
        let engine = self.clone();
        crate::ws::runtime().spawn(async move {
            if let Err(e) = engine.persist(&data_dir) {
                log::error!("工作流状态持久化失败 {}: {e}", engine.id());
            }
        });
    }

    pub fn load_window(data_dir: &Path, limit: usize) -> std::io::Result<(Vec<OrcSession>, bool)> {
        // 惰性元数据加载：只读 sqlite，不读取 transcript/activities 两份 JSONL。
        crate::wfstore::load_meta_window(data_dir, limit)
    }

    /// 全库工作流的关联普通会话身份（含未加载进内存的窗口外工作流）。
    pub fn load_all_linked_session_ids(
        data_dir: &Path,
    ) -> std::io::Result<std::collections::HashSet<(String, String)>> {
        crate::wfstore::load_all_linked_session_ids(data_dir)
    }

    pub fn has_linked_sessions_on_machine(
        data_dir: &Path,
        machine_name: &str,
    ) -> std::io::Result<bool> {
        crate::wfstore::has_linked_sessions_on_machine(data_dir, machine_name)
    }

    /// 全量读取入口：仅存储测试使用。
    #[cfg(test)]
    pub fn load_all(data_dir: &Path) -> std::io::Result<Vec<OrcSession>> {
        crate::wfstore::load_all_meta(data_dir)
    }

    /// 按需补齐：把会话的 transcript/activities 从 JSONL 读入（仅打开渲染视图时调用）。
    /// 读盘在锁外完成，短暂持锁合并——避免持写锁做 IO 阻塞渲染与后台推进。
    pub fn backfill(&self, data_dir: &Path) -> std::io::Result<()> {
        let id = {
            let s = self.session.read();
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
    machines: Vec<MachineEntry>,
    linked_sessions: Arc<Mutex<Vec<LinkedSession>>>,
    /// 引擎共享会话：create_session 工具即时挂载关联普通会话（不等整轮 decide 结束）
    session: Arc<RwLock<OrcSession>>,
    /// 引擎 → 应用事件通道：挂载新关联普通会话后通知应用即时刷新会话列表
    hub: Arc<MachineHub>,
    /// 关联普通会话挂载后的即时落库钩子（见 `OrcContext.persist_on_linked_session_mounted`）
    persist_on_linked_session_mounted: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 编排工具调用活动钩子（见 `OrcContext.record_tool_activity`）
    record_tool_activity: Option<Arc<dyn Fn(Activity) + Send + Sync>>,
    /// 编排输出草稿（见 `OrcContext.draft`）：流式文本实时进 transcript
    draft: Arc<OrcDraft>,
    /// 进行中实时活动槽（见 `OrcContext.current`）
    current: Arc<Mutex<Option<Activity>>>,
}

impl OrcContext {
    /// 工具循环运行时：OrcContext 的可变子集（linked_sessions 需要在
    /// turn 内即时挂载，包成共享槽）。
    fn live(&self) -> LiveRuntime {
        LiveRuntime {
            machines: self.machines.clone(),
            linked_sessions: Arc::new(Mutex::new(self.linked_sessions.clone())),
            session: Arc::clone(&self.session),
            hub: Arc::clone(&self.hub),
            persist_on_linked_session_mounted: self.persist_on_linked_session_mounted.clone(),
            record_tool_activity: self.record_tool_activity.clone(),
            draft: Arc::clone(&self.draft),
            current: Arc::clone(&self.current),
        }
    }
}

impl LiveRuntime {
    fn machine_index(&self, name: &str) -> Result<usize, String> {
        self.machines
            .iter()
            .position(|m| m.summary.name == name)
            .ok_or_else(|| format!("机器不存在: {name}"))
    }

    fn client(&self, name: &str) -> Result<WsClient, String> {
        let i = self.machine_index(name)?;
        self.machines[i]
            .client
            .clone()
            .ok_or_else(|| format!("机器未连接: {name}"))
    }

    fn linked_session(&self, session_id: &str) -> Result<LinkedSession, String> {
        self.linked_sessions
            .lock()
            .iter()
            .find(|c| c.id == session_id)
            .cloned()
            .ok_or_else(|| format!("关联普通会话不存在: {session_id}"))
    }

    /// 挂载关联普通会话：同时写入工具循环列表、引擎共享会话并立即落库。
    /// 引擎会话即时更新让 GUI 立即把新会话视为工作流关联会话；
    /// 工具循环列表供本 turn 内 list_sessions 立即可见；立即落库保证
    /// 应用崩溃/退出时关联关系不丢（不等整轮结束后的 persist 快照）。
    fn mount_linked_session(&self, linked: LinkedSession) {
        self.linked_sessions.lock().push(linked.clone());
        self.session.write().linked_sessions.push(linked);
        if let Some(persist) = &self.persist_on_linked_session_mounted {
            persist();
        }
    }

    /// 进行中实时活动：思考中（流式增量即更新；非思考态则新建）。
    fn set_thinking(&self, delta: &str) {
        let mut cur = self.current.lock();
        match &mut *cur {
            Some(Activity::Thinking { content, .. }) => content.push_str(delta),
            _ => {
                *cur = Some(Activity::Thinking {
                    timestamp: now(),
                    content: delta.to_string(),
                });
            }
        }
    }

    /// 进行中动作结束（成功或失败）。
    fn clear_current(&self) {
        *self.current.lock() = None;
    }
}
//
// 不再用 rig `Agent::prompt` 的黑盒多 turn：它一旦发起无法中途插话，steer 只能
// 退化为整轮结束后重跑。改为保留 rig provider 层（三种 ApiFormat 仍由 rig 处理），
// 循环自己驱动（参考 rig-agent 的流式运行时）：
// 流式请求 → 文本增量实时进对话流 / reasoning 增量合并为 thinking 活动 →
// 解析工具调用 → 执行 → 结果回填 → drain steer 插话 → 再流式请求。

/// 工具循环的模型调用上限，防失控。编排一轮可能合理地做几十次调度
/// （创建多个关联普通会话、逐个下发指令、回读状态），rig 通用默认的 8 轮会被
/// 合法长 turn 误杀，放宽到 32；真死循环由重复调用检测兜底。
const MAX_TOOL_TURNS: usize = 32;

/// 连续发出完全相同调用（工具 + 参数）超过该次数判定为死循环。
const MAX_IDENTICAL_CALLS: usize = 3;

/// 编排工具名：工具 schema（tool_definitions）与 dispatch 分发共用同一常量，
/// 任一侧拼写漂移都会让工具静默失效。
mod tool_names {
    pub const LIST_AGENTS: &str = "list_agents";
    pub const LIST_SESSIONS: &str = "list_sessions";
    pub const CREATE_SESSION: &str = "create_session";
    pub const PROMPT_SESSION: &str = "prompt_session";
    pub const CANCEL_SESSION: &str = "cancel_session";
    pub const CONFIGURE_SESSION: &str = "configure_session";
    pub const GET_SESSION_CONFIG_OPTIONS: &str = "get_session_config_options";
    pub const READ_SESSION_HISTORY: &str = "read_session_history";
    pub const READ_SESSION_ACTIVITIES: &str = "read_session_activities";
}

/// 编排工具清单。
fn tool_definitions() -> Vec<rig_core::completion::ToolDefinition> {
    use rig_core::completion::ToolDefinition;
    use tool_names::*;
    vec![
        ToolDefinition {
            name: LIST_AGENTS.into(),
            description: "已注册机器及各机器的 agent 列表：机器在线状态、agent 可用性".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: LIST_SESSIONS.into(),
            description: "本工作流的关联普通会话列表（标题、状态、创建/活跃时间、机器在线与否、工作目录、worktree 目录、上下文用量）".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: CREATE_SESSION.into(),
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
            name: PROMPT_SESSION.into(),
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
            name: CANCEL_SESSION.into(),
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
            name: CONFIGURE_SESSION.into(),
            description: "配置关联普通会话的标题或会话选项；至少提供 title 或 config".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "关联普通会话 id" },
                    "title": { "type": "string", "description": "会话标题" },
                    "config": {
                        "type": "object",
                        "description": "要设置的会话选项；type 为 value_id 时提供 value 字符串，type 为 boolean 时提供 value 布尔值",
                        "properties": {
                            "configId": { "type": "string", "description": "会话选项 id" },
                            "type": { "type": "string", "enum": ["value_id", "boolean"] },
                            "value": { "description": "选项值：字符串或布尔值" }
                        },
                        "required": ["configId", "type", "value"]
                    }
                },
                "required": ["session"],
                "anyOf": [
                    { "required": ["title"] },
                    { "required": ["config"] }
                ]
            }),
        },
        ToolDefinition {
            name: GET_SESSION_CONFIG_OPTIONS.into(),
            description: "获取关联普通会话当前由 agent 提供的完整会话选项集合".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "关联普通会话 id" }
                },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: READ_SESSION_HISTORY.into(),
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
            name: READ_SESSION_ACTIVITIES.into(),
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

/// 完整 Reasoning part 上报为 thinking 活动（该 part 增量合并后的最终文本）。
fn record_reasoning(live: &LiveRuntime, reasoning: &Reasoning) {
    let Some(record) = &live.record_tool_activity else {
        return;
    };
    let text = reasoning
        .content
        .iter()
        .filter_map(|b| match b {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            ReasoningContent::Summary(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    if !text.trim().is_empty() {
        record(Activity::Thinking {
            timestamp: now(),
            content: text,
        });
    }
}

/// 薄工具循环：驱动模型直至输出纯文本（turn 结束）。
///
/// - 每轮请求前 drain `steer_inbox`，把用户插话注入为 user 消息（真实 steer）
/// - 流式消费模型输出：文本增量实时经 [`OrcDraft`] 写入对话流，reasoning 增量
///   按 part 合并、part 结束即上报 thinking 活动）；turn 失败移除草稿，不留半截输出
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
    // 死循环检测：连续完全相同（工具 + 参数）的调用计数
    let mut last_call: Option<String> = None;
    let mut identical_runs = 0usize;
    for _ in 0..MAX_TOOL_TURNS {
        // 轮次边界：用户插话实时进入下一轮请求
        let steers = std::mem::take(&mut *steer_inbox.lock());
        for text in steers {
            history.push(Message::user(format!("用户：{text}")));
        }
        // builder 把 prompt 追加到 chat_history 末尾，因此最后一条单独传；
        // 请求后必须放回 history——否则作为 prompt 传出的工具结果消息会从
        // 后续轮次序列中消失，deepseek 等严格校验的 API 会报 400
        // "insufficient tool messages following tool_calls message"
        let prompt = history.pop().ok_or("编排对话历史为空")?;
        let mut resp = match model
            .completion_request(prompt.clone())
            .preamble(preamble.to_string())
            .messages(history.iter().cloned())
            .tools(tool_defs.clone())
            .stream()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                live.draft.discard();
                return Err(format!("编排 agent 调用失败: {e}"));
            }
        };
        history.push(prompt);
        // 流式消费：文本增量实时进对话流草稿；reasoning 增量按 part 合并，
        // part 结束（完整 Reasoning 事件或流结束）即上报 thinking 活动
        let mut reasoning_parts: Vec<(String, String)> = Vec::new();
        loop {
            match resp.next().await {
                Some(Ok(StreamedAssistantContent::Text(t))) => live.draft.append(&t.text),
                Some(Ok(StreamedAssistantContent::ReasoningDelta { id, reasoning, .. })) => {
                    // 实时活动：思考中，增量即更新
                    live.set_thinking(&reasoning);
                    match reasoning_parts.iter_mut().find(|(i, _)| *i == id) {
                        Some((_, acc)) => acc.push_str(&reasoning),
                        None => reasoning_parts.push((id, reasoning)),
                    }
                }
                Some(Ok(StreamedAssistantContent::Reasoning { reasoning, id })) => {
                    // 完整事件取代同 part 已累积的增量（rig 聚合语义）
                    reasoning_parts.retain(|(i, _)| *i != id);
                    record_reasoning(live, &reasoning);
                    live.clear_current();
                }
                // 工具调用等聚合进 choice，流结束后统一处理
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    live.draft.discard();
                    return Err(format!("编排 agent 调用失败: {e}"));
                }
                None => break,
            }
        }
        // provider 未发完整 Reasoning 事件的 part：流结束时以累积增量兜底上报
        if let Some(record) = &live.record_tool_activity {
            for (_, text) in reasoning_parts {
                if !text.trim().is_empty() {
                    record(Activity::Thinking {
                        timestamp: now(),
                        content: text,
                    });
                }
            }
        }
        live.clear_current();
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
            id: resp.message_id.clone(),
            content: resp.choice,
        });
        if calls.is_empty() {
            // 纯文本收尾 = 编排智能体选择静默（无动作可做）；是否「完成」由它
            // 自行判断，引擎不作状态标记（工作流会话没有 done 状态）
            let out = if text.trim().is_empty() {
                // 无有效输出：清掉纯空白草稿，以兜底文案收尾
                live.draft.discard();
                "（编排智能体未输出文字）".to_string()
            } else {
                text
            };
            // 增量从未到达时补提交最终输出，保证「Ok 即已进对话流」的契约
            if !live.draft.started() {
                live.draft.push_message(&out);
            }
            return Ok(out);
        }
        let mut results = Vec::with_capacity(calls.len());
        for tc in &calls {
            // 0.42 起 provider 下发的 id 收敛到 tc.provider（call_id 必有，
            // 双标识 wire 另带 item_id）；无则回退 rig 关联句柄 tc.id
            let name = tc.function.name.clone();
            let outcome = match dispatch_tool(live, &name, tc.function.arguments.clone()).await {
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
                None => results.push(UserContent::tool_result(tc.id.clone(), &name, content)),
            }
        }
        history.push(Message::User { content: results });

        // 死循环检测：连续 {MAX_IDENTICAL_CALLS} 次完全相同的调用即中止本轮
        let sig = calls
            .iter()
            .map(|tc| format!("{} {}", tc.function.name, tc.function.arguments))
            .collect::<Vec<_>>()
            .join(";");
        if last_call.as_deref() == Some(sig.as_str()) {
            identical_runs += 1;
        } else {
            identical_runs = 1;
        }
        last_call = Some(sig);
        if identical_runs >= MAX_IDENTICAL_CALLS {
            log::warn!("编排 agent 连续重复相同调用，判定死循环，中止本轮推进");
            let out =
                "（本轮检测到编排智能体反复执行相同调度，已中止；请检查工作流计划或输入消息继续）"
                    .to_string();
            live.draft.push_message(&out);
            return Ok(out);
        }
    }
    // 轮次上限：优雅收尾而非报错——已完成的调度保留在对话历史，下一轮
    // （用户消息/关联普通会话事件驱动）从断点继续；以错误呈现会让用户无从继续
    log::warn!("编排 agent 连续 {MAX_TOOL_TURNS} 轮未结束 turn，本轮收尾");
    let out = format!("（本轮工具调度已达 {MAX_TOOL_TURNS} 次上限，已暂停；可输入消息继续推进）");
    live.draft.push_message(&out);
    Ok(out)
}

pub struct RigBackend {
    cfg: OrchestratorConfig,
}

impl RigBackend {
    pub fn new(cfg: OrchestratorConfig) -> Self {
        RigBackend { cfg }
    }

    fn preamble(&self) -> String {
        "你是 amux 的编排智能体。按工作流执行计划和用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在工作流执行计划和用户指令中指定的事项交由用户决定。\n\
         使用工具：list_agents、list_sessions、create_session、prompt_session、cancel_session、\
         configure_session、get_session_config_options、read_session_history、read_session_activities。\
         调度动作完成后用中文简述本轮动作并结束 turn。\
         当无需任何调度动作时（如全部步骤已完成、计划已无法继续、或需要人类判断），\
         不要调用工具，直接用中文输出说明并结束 turn 等待用户。"
            .to_string()
    }
}

impl OrcBackend for RigBackend {
    fn decide<'a>(
        &'a self,
        ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.cfg.is_configured() {
                return Err(
                    "未配置编排 agent API（Base URL / API key / 模型）。请在设置 → 编排 agent 中配置后再创建工作流"
                        .to_string(),
                );
            }
            let mut preamble = self.preamble();
            let model = self.cfg.model.clone();
            let live = ctx.live();
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
            // 三种 ApiFormat 只在模型构建处不同，工具循环完全一致
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
            Ok(text)
        })
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
    decisions: Mutex<VecDeque<String>>,
}

#[doc(hidden)]
#[cfg(test)]
#[allow(clippy::new_ret_no_self)]
impl FakeBackend {
    pub fn new(decisions: Vec<String>) -> Arc<dyn OrcBackend> {
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
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.decisions
                .lock()
                .pop_front()
                .ok_or_else(|| "决策用尽".to_string())
        })
    }
}
//
// 工具调用由循环自己解析 ToolCall 并调用（不再走 agent 运行时）：
// 错误以 String 返回、由循环回传给模型纠正。

/// 工具调用统一入口：按名称分派，参数从模型给出的 JSON 反序列化。
/// 工具参数摘要（单行、120 字截断），供活动条目展示。
fn one_line_summary(args: &serde_json::Value) -> String {
    let text = args.to_string();
    let line = text.lines().next().unwrap_or("").trim();
    amux_common::text::truncate(line, 120)
}

async fn dispatch_tool(
    live: &LiveRuntime,
    name: &str,
    args: serde_json::Value,
) -> Result<String, String> {
    // 编排调度动作实时记录为活动并落盘（同普通会话的 tool_call 事件），
    // 同一条活动也是进行中实时槽的内容；执行完毕（成功或失败）即清除，
    // 不让已完成的历史活动继续转圈
    let activity = Activity::ToolCall {
        timestamp: now(),
        name: name.to_string(),
        title: Some(one_line_summary(&args)),
        content: None,
    };
    if let Some(record) = &live.record_tool_activity {
        record(activity.clone());
    }
    *live.current.lock() = Some(activity);
    // match 分支必须用路径形式引用常量：裸大写标识符会被编译器
    // 当作新绑定（catch-all），整个分发静默落到第一个分支。
    let result = match name {
        tool_names::LIST_AGENTS => list_agents(live).await,
        tool_names::LIST_SESSIONS => list_sessions(live).await,
        tool_names::CREATE_SESSION => run_parsed(name, args, |a| create_session(live, a)).await,
        tool_names::PROMPT_SESSION => run_parsed(name, args, |a| prompt_session(live, a)).await,
        tool_names::CANCEL_SESSION => run_parsed(name, args, |a| cancel_session(live, a)).await,
        tool_names::CONFIGURE_SESSION => {
            run_parsed(name, args, |a| configure_session(live, a)).await
        }
        tool_names::GET_SESSION_CONFIG_OPTIONS => {
            run_parsed(name, args, |a| get_session_config_options(live, a)).await
        }
        tool_names::READ_SESSION_HISTORY => {
            run_parsed(name, args, |a| {
                read_session_page::<HistoryResult>(live, a, PageKind::History)
            })
            .await
        }
        tool_names::READ_SESSION_ACTIVITIES => {
            run_parsed(name, args, |a| {
                read_session_page::<ActivitiesResult>(live, a, PageKind::Activities)
            })
            .await
        }
        other => Err(format!("未知工具: {other}")),
    };
    live.clear_current();
    result
}

/// 工具参数反序列化 + 调用（dispatch_tool 各分支共用的样板）。
async fn run_parsed<T, F, Fut>(tool: &str, args: serde_json::Value, f: F) -> Result<String, String>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    match parse_args(tool, args) {
        Ok(args) => f(args).await,
        Err(error) => Err(error),
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
                "name": m.summary.name,
                "online": m.summary.online,
                "agents": m.summary.agents.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "available": a.available,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let output = serde_json::to_string(&v).map_err(|e| format!("序列化失败: {e}"))?;
    Ok(output)
}

async fn list_sessions(live: &LiveRuntime) -> Result<String, String> {
    use std::collections::{BTreeMap, HashMap};
    // 标题/忙闲/agent 以机器 server 为权威：按机器分组批量现查 session.info，
    // 本地不缓存这些易漂移的字段
    let linked_sessions = live.linked_sessions.lock().clone();
    let mut ids_by_machine: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in &linked_sessions {
        ids_by_machine
            .entry(c.machine_name.clone())
            .or_default()
            .push(c.id.clone());
    }
    let mut metas: HashMap<String, SessionMeta> = HashMap::new();
    for (machine_name, ids) in ids_by_machine {
        let Ok(client) = live.client(&machine_name) else {
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
    let online_of = |c: &LinkedSession| {
        live.machines
            .iter()
            .find(|m| m.summary.name == c.machine_name)
            .map(|m| m.summary.online)
            .unwrap_or(false)
    };
    let v: Vec<serde_json::Value> = linked_sessions
        .iter()
        .map(|c| match metas.get(&c.id) {
            Some(m) => serde_json::json!({
                "id": c.id,
                "title": m.title,
                "state": m.state.as_str(),
                "createdAt": m.created_at,
                "lastActiveAt": m.last_active_at,
                "machine": c.machine_name,
                "agent": m.agent,
                "machineOnline": online_of(c),
                "cwd": m.cwd,
                "worktreeDir": m.worktree_dir,
                "contextSize": m.context_size,
                "contextWindowSize": m.context_window_size,
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
    // 即时挂载：关联普通会话立即进入工作流会话关联列表（不等整轮 decide 结束），
    // 并通知应用刷新会话列表——否则在下次轮询/状态变更前，新会话会以
    // 独立普通会话身份出现在侧栏，而非挂在工作流会话之下
    live.mount_linked_session(LinkedSession {
        id: sid.clone(),
        machine_name: machine_name.clone(),
    });
    live.hub.linked_session_mounted(&machine_name);
    Ok(sid)
}

#[derive(serde::Deserialize)]
struct PromptSessionArgs {
    session: String,
    prompt: String,
}

async fn prompt_session(live: &LiveRuntime, args: PromptSessionArgs) -> Result<String, String> {
    let linked_session = live.linked_session(&args.session)?;
    let client = live.client(&linked_session.machine_name)?;
    let input = SessionPromptParams {
        session_id: args.session.clone(),
        input: vec![ContentBlock::Text { text: args.prompt }],
    };
    client
        .request_ok(protocol::method::SESSION_PROMPT, Some(input))
        .await
        .map_err(|e| e.to_string())?;
    Ok("已下发".into())
}

#[derive(serde::Deserialize)]
struct SessionRefArgs {
    session: String,
}

async fn cancel_session(live: &LiveRuntime, args: SessionRefArgs) -> Result<String, String> {
    let linked_session = live.linked_session(&args.session)?;
    let client = live.client(&linked_session.machine_name)?;
    client
        .request_ok(
            protocol::method::SESSION_CANCEL,
            Some(SessionIdParams {
                session_id: args.session.clone(),
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok("已取消".into())
}

#[derive(serde::Deserialize)]
struct ConfigureSessionArgs {
    session: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    config: Option<SessionConfigSetting>,
}

async fn configure_session(
    live: &LiveRuntime,
    args: ConfigureSessionArgs,
) -> Result<String, String> {
    let linked_session = live.linked_session(&args.session)?;
    if args.title.is_none() && args.config.is_none() {
        return Err("configure_session 至少设置 title 或 config 之一".into());
    }
    let client = live.client(&linked_session.machine_name)?;
    client
        .request_ok(
            protocol::method::SESSION_CONFIGURE,
            Some(SessionConfigureParams {
                session_id: args.session.clone(),
                title: args.title,
                config: args.config,
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok("已配置".into())
}

async fn get_session_config_options(
    live: &LiveRuntime,
    args: SessionRefArgs,
) -> Result<String, String> {
    let linked_session = live.linked_session(&args.session)?;
    let client = live.client(&linked_session.machine_name)?;
    let result = client
        .request::<_, SessionConfigOptionsResult>(
            protocol::method::SESSION_CONFIG_OPTIONS,
            Some(SessionIdParams {
                session_id: args.session.clone(),
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&result).map_err(|e| format!("序列化失败: {e}"))
}

#[derive(serde::Deserialize)]
struct SessionPageArgs {
    session: String,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    before: Option<u64>,
}

/// 分页读取的两个目标（对话 / 活动）：内部判别用枚举，仅在请求边界
/// 映射为协议方法名，避免把方法名字符串当分支条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageKind {
    History,
    Activities,
}

impl PageKind {
    fn method(self) -> &'static str {
        match self {
            PageKind::History => protocol::method::SESSION_HISTORY,
            PageKind::Activities => protocol::method::SESSION_ACTIVITIES,
        }
    }
}

async fn read_session_page<R: serde::de::DeserializeOwned + serde::Serialize>(
    live: &LiveRuntime,
    args: SessionPageArgs,
    kind: PageKind,
) -> Result<String, String> {
    let linked_session = live.linked_session(&args.session)?;
    let client = live.client(&linked_session.machine_name)?;
    let params = SessionPageParams {
        session_id: args.session.clone(),
        limit: args.limit.map(|l| l as usize),
        before: args.before,
    };
    let r: R = client
        .request::<_, R>(kind.method(), Some(params))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&r).map_err(|e| format!("序列化失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::providers::openai as rig_openai;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    fn machines() -> Vec<MachineSummary> {
        vec![MachineSummary::named("测试机", &["mock_acp"])]
    }

    fn clients_with_machines() -> (Vec<WsClient>, MachineSummary) {
        let m = machines().remove(0);
        let c = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        (vec![c], m)
    }

    fn test_hub(machines: Vec<MachineSummary>, clients: Vec<WsClient>) -> Arc<MachineHub> {
        test_hub_with(machines, clients.into_iter().map(Some).collect())
    }

    fn test_hub_with(
        machines: Vec<MachineSummary>,
        clients: Vec<Option<WsClient>>,
    ) -> Arc<MachineHub> {
        let hub = MachineHub::default();
        hub.sync(machines.into_iter().zip(clients).collect());
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
        let session = Arc::new(RwLock::new(OrcSession {
            id: "orc_test".into(),
            title: String::new(),
            plan: String::new(),
            state: SessionState::Idle,
            transcript: Vec::new(),
            linked_sessions: Vec::new(),
            activities: Vec::new(),
            created_at: 0,
            last_active_at: 0,
        }));
        LiveRuntime {
            machines: vec![MachineEntry {
                summary: MachineSummary::named("测试机", &["mock_acp"]),
                client: Some(WsClient::connect_with_token(
                    "ws://127.0.0.1:1".into(),
                    "unused".into(),
                )),
            }],
            linked_sessions: Arc::new(Mutex::new(Vec::new())),
            draft: Arc::new(OrcDraft::new(session.clone())),
            session,
            hub: Arc::new(MachineHub::default()),
            persist_on_linked_session_mounted: None,
            record_tool_activity: None,
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// 回归：create_session 挂载关联普通会话必须同时写入引擎共享会话（GUI 即时可见），
    /// 否则整轮 decide 结束前，新会话会以独立普通会话身份出现在会话列表。
    #[test]
    fn mount_linked_session_registers_into_tool_list_and_engine_session() {
        let live = test_live();
        live.mount_linked_session(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        assert_eq!(
            live.linked_sessions.lock().len(),
            1,
            "工具循环内 list_sessions 应立即看到新关联普通会话"
        );
        assert_eq!(
            live.session.read().linked_sessions.len(),
            1,
            "引擎共享会话应立即挂载关联普通会话（关联关系即时生效）"
        );
    }

    /// 回归：create_session 挂载关联普通会话后应立即写入元数据库（sqlite），
    /// 不等整轮 decide 结束——否则应用中途退出会丢失关联关系。
    #[test]
    fn mounted_child_is_persisted_immediately() {
        let dir = temp_data_dir();
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new_for_tests(),
            test_hub(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![WsClient::connect_with_token(
                    "ws://127.0.0.1:1".into(),
                    "unused".into(),
                )],
            ),
            &dir,
        );
        let ctx = engine.build_context();
        // 同步落库钩子（真实钩子为后台 persist_in_background，这里同步执行以便断言）
        let persist_engine = engine.clone();
        let persist_dir = dir.clone();
        let live = LiveRuntime {
            machines: ctx.machines.clone(),
            linked_sessions: Arc::new(Mutex::new(ctx.linked_sessions.clone())),
            session: ctx.session.clone(),
            hub: ctx.hub.clone(),
            draft: Arc::new(OrcDraft::new(ctx.session.clone())),
            persist_on_linked_session_mounted: Some(Arc::new(move || {
                let _ = persist_engine.persist(&persist_dir);
            })),
            record_tool_activity: None,
            current: Arc::new(Mutex::new(None)),
        };
        live.mount_linked_session(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        // 未等整轮 decide 结束：元数据库应立即包含刚挂载的关联普通会话
        let sessions = WorkflowEngine::load_all(&dir).unwrap();
        assert_eq!(sessions[0].linked_sessions.len(), 1);
        assert_eq!(sessions[0].linked_sessions[0].id, "s_child");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关联普通会话挂载事件可被应用侧订阅到（驱动即时刷新）。
    #[test]
    fn linked_session_mounted_event_reaches_subscriber() {
        let hub = MachineHub::default();
        let mut rx = hub.subscribe();
        hub.linked_session_mounted("测试机");
        match rx.try_recv() {
            Ok(HubEvent::LinkedSessionMounted { machine_name }) => {
                assert_eq!(machine_name, "测试机");
            }
            other => panic!("应收到 LinkedSessionMounted 事件，实际 {other:?}"),
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
    fn title_empty_until_first_user_message() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new(
            "实现登录功能\n然后写测试",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        {
            let session = engine.session.read();
            // 新建时标题为空（列表显示占位），由用户首个指令生成
            assert!(session.title.is_empty());
            // 计划存元数据，不进对话消息历史
            assert_eq!(session.plan, "实现登录功能\n然后写测试");
            assert!(session.transcript.is_empty());
        }
        // 用户首条指令生成标题
        engine.record_user("实现登录功能");
        assert_eq!(
            engine.session.read().title,
            "实现登录功能",
            "标题应取自用户首个指令"
        );
    }

    /// 取消按钮注入固定取消指令并推进。
    #[tokio::test]
    async fn cancel_injects_cancel_prompt_and_advances() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec!["已按指令取消".into()]);
        let engine = WorkflowEngine::new(
            "计划",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        assert!(engine.cancel(), "空闲工作流取消应立即推进");
        assert!(engine
            .session
            .read()
            .transcript
            .iter()
            .any(|msg| matches!(msg, OrcMsg::User { text, .. } if text == WORKFLOW_CANCEL_PROMPT)));
        engine.advance().await.unwrap();
        assert!(engine
            .session
            .read()
            .transcript
            .iter()
            .any(|msg| matches!(msg, OrcMsg::Orc { text, .. } if text == "已按指令取消")));
    }

    #[tokio::test]
    async fn on_linked_session_state_idle_always_triggers_advance() {
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec!["本轮静默".into()]);
        let engine = WorkflowEngine::new(
            "计划",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        engine.session.write().linked_sessions.push(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        let advanced = engine
            .on_linked_session_state(
                "测试机",
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
                .transcript
                .iter()
                .any(|m| matches!(m, OrcMsg::Orc { text, .. } if text == "本轮静默")),
            "静默决策也应作为编排输出进入对话流"
        );
        assert!(
            engine
                .session
                .read()
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
        let backend = FakeBackend::new(vec!["不应发生".into()]);
        let engine = WorkflowEngine::new(
            "计划",
            backend,
            test_hub(vec![m], clients),
            &temp_data_dir(),
        );
        engine.session.write().linked_sessions.push(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        let advanced = engine
            .on_linked_session_state(
                "测试机",
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
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text.contains("状态变更"))));
    }

    #[test]
    fn deleted_workflow_cannot_be_resurrected_by_stale_persist() {
        let dir = temp_data_dir();
        let (clients, machine) = clients_with_machines();
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new(vec![]),
            test_hub(vec![machine], clients),
            &dir,
        );
        engine.persist(&dir).unwrap();
        let stale_clone = engine.clone();

        engine.mark_deleted();
        engine.remove_deleted(&dir).unwrap();
        assert!(WorkflowEngine::load_all(&dir).unwrap().is_empty());

        // 模拟删除完成前已排队的后台持久化任务：共享删除墓碑后必须跳过写回。
        stale_clone.persist(&dir).unwrap();
        assert!(WorkflowEngine::load_all(&dir).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persistence_roundtrip_and_restore() {
        let dir = std::env::temp_dir().join(format!("amux-wf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (clients, m) = clients_with_machines();
        let backend = FakeBackend::new(vec![]);
        let engine = WorkflowEngine::new("计划A", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().id.clone();
        engine.record_user("立即保存");
        engine.persist(&dir).unwrap();

        let sessions = WorkflowEngine::load_all(&dir).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        // 标题由用户首个指令（而非工作流计划）生成
        assert_eq!(sessions[0].title, "立即保存");
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

        let backend2 = FakeBackend::new(vec!["恢复后推进".into()]);
        let (clients2, m2) = clients_with_machines();
        let engine2 = WorkflowEngine::restore(opened, backend2, test_hub(vec![m2], clients2), &dir);
        engine2.advance().await.unwrap();
        assert!(engine2
            .session
            .read()
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
        let engine = WorkflowEngine::new("计划B", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().id.clone();
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
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. } if text == "准备保存")));

        // 已在内存（推进中）的会话：backfill 不应被磁盘旧快照覆盖。
        let live = engine2.session.read().transcript.len();
        engine2.record_user("推进中新增");
        engine2.backfill(&dir).unwrap();
        let after = engine2.session.read();
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
        let engine = WorkflowEngine::new("计划", backend, test_hub(vec![m], clients), &dir);
        let id = engine.session.read().id.clone();

        engine.record_activity(Activity::Thinking {
            timestamp: 1,
            content: "想".into(),
        });

        // 活动实时追加写盘：无需 persist，磁盘即可读到。
        let path = activities_path(&dir, &id);
        let acts = read_jsonl::<Activity>(&path).unwrap();
        assert_eq!(acts.len(), 1, "活动应实时落盘");
        assert!(matches!(&acts[0], Activity::Thinking { content, .. } if content == "想"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_activity_appends_remain_valid_jsonl() {
        let dir = temp_data_dir();
        let (clients, m) = clients_with_machines();
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new(vec![]),
            test_hub(vec![m], clients),
            &dir,
        );
        let workers = 4;
        let per_worker = 25;
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let engine = engine.clone();
                scope.spawn(move || {
                    for item in 0..per_worker {
                        engine.record_activity(Activity::Thinking {
                            timestamp: (worker * per_worker + item) as u64,
                            content: format!("worker-{worker}-item-{item}"),
                        });
                    }
                });
            }
        });

        let id = engine.id();
        let activities = read_jsonl::<Activity>(&activities_path(&dir, &id)).unwrap();
        assert_eq!(activities.len(), workers * per_worker);
        assert!(activities.iter().all(|activity| matches!(
            activity,
            Activity::Thinking { content, .. } if content.starts_with("worker-")
        )));
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
                "configure_session",
                "get_session_config_options",
                "read_session_history",
                "read_session_activities",
            ]
        );
    }

    #[test]
    fn configure_session_args_map_to_session_configure_wire_shape() {
        let args: ConfigureSessionArgs = parse_args(
            "configure_session",
            serde_json::json!({
                "session": "child-1",
                "title": "实现配置",
                "config": {
                    "configId": "model",
                    "type": "value_id",
                    "value": "fast"
                }
            }),
        )
        .expect("configure_session 参数应能解析");
        let wire = serde_json::to_value(SessionConfigureParams {
            session_id: args.session,
            title: args.title,
            config: args.config,
        })
        .unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "sessionId": "child-1",
                "title": "实现配置",
                "config": {
                    "configId": "model",
                    "type": "value_id",
                    "value": "fast"
                }
            })
        );
    }

    #[tokio::test]
    async fn configure_session_rejects_empty_configuration_before_rpc() {
        let live = test_live();
        live.linked_sessions.lock().push(LinkedSession {
            id: "child-1".into(),
            machine_name: "测试机".into(),
        });
        let err = configure_session(
            &live,
            ConfigureSessionArgs {
                session: "child-1".into(),
                title: None,
                config: None,
            },
        )
        .await
        .expect_err("没有 title 或 config 时应拒绝调用");
        assert!(err.contains("至少设置 title 或 config"));
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
            backend,
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.session.write().transcript.push(OrcMsg::User {
            text: "开始".into(),

            timestamp: now(),
        });
        engine.session.write().transcript.push(OrcMsg::Orc {
            text: "决策".into(),

            timestamp: now(),
        });
        let dialog = engine.session.read().to_dialog();
        assert_eq!(dialog.len(), 2);
        assert!(matches!(&dialog[0], DialogMsg::UserMessage { .. }));
        assert!(matches!(&dialog[1], DialogMsg::AgentMessage { .. }));
    }

    #[test]
    fn plan_is_metadata_not_history() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划：先在测试机实现，再审查",
            backend,
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        assert!(engine.session.read().transcript.is_empty());
        assert_eq!(engine.session.read().plan, "计划：先在测试机实现，再审查");
        assert!(engine.session.read().title.is_empty());
    }

    #[tokio::test]
    async fn unconfigured_rig_backend_records_clear_error() {
        let backend = Arc::new(RigBackend::new(OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            effort: "high".into(),
        }));
        let client = WsClient::connect_with_token("ws://127.0.0.1:1".into(), "unused".into());
        let engine = WorkflowEngine::new(
            "计划",
            backend,
            test_hub(
                vec![MachineSummary::named("测试机", &["kimi"])],
                vec![client],
            ),
            &temp_data_dir(),
        );
        let res = engine.advance().await;
        assert!(res.is_err());
        assert!(engine.session.read().activities.iter().any(
            |a| matches!(a, Activity::Error { detail, .. } if detail.contains("未配置编排 agent API"))
        ));
        assert_eq!(engine.session.read().state, SessionState::Idle);
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
        assert!(
            live.current.lock().is_none(),
            "工具参数解析失败也必须清理实时活动"
        );
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
    fn note_linked_session_state_tracks_busy_count() {
        let backend = FakeBackend::new_for_tests();
        let engine = WorkflowEngine::new(
            "计划",
            backend,
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.session.write().linked_sessions.push(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        engine.note_linked_session_state(
            "测试机",
            "s_child",
            SessionState::Idle,
            SessionState::Busy,
        );
        assert_eq!(engine.session.read().state, SessionState::Busy);
        engine.note_linked_session_state(
            "测试机",
            "s_child",
            SessionState::Busy,
            SessionState::Idle,
        );
        assert_eq!(engine.session.read().state, SessionState::Idle);
        engine.note_linked_session_state(
            "测试机",
            "missing",
            SessionState::Idle,
            SessionState::Busy,
        );
        assert_eq!(engine.session.read().state, SessionState::Idle);
    }

    #[test]
    fn user_message_starts_turn_when_only_child_is_busy() {
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new_for_tests(),
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.session.write().linked_sessions.push(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        engine.note_linked_session_state(
            "测试机",
            "s_child",
            SessionState::Idle,
            SessionState::Busy,
        );

        assert!(engine.record_user("继续处理"));
        assert!(engine.steer_inbox.lock().is_empty());
    }

    #[test]
    fn user_message_requests_rerun_when_orchestrator_is_busy() {
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new_for_tests(),
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.gate.lock().running = true;

        assert!(!engine.record_user("中途补充"));
        let gate = engine.gate.lock();
        assert!(gate.requested, "中途消息必须请求下一轮推进");
        drop(gate);
        assert_eq!(engine.steer_inbox.lock().as_slice(), ["中途补充"]);
    }

    #[test]
    fn pending_busy_marker_preserves_requested_rerun() {
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new_for_tests(),
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.gate.lock().requested = true;

        engine.mark_busy_pending();

        assert_eq!(engine.state(), SessionState::Busy);
        assert!(engine.gate.lock().requested);
    }

    #[test]
    fn workflow_stays_busy_while_orchestrator_turn_runs() {
        let engine = WorkflowEngine::new(
            "计划",
            FakeBackend::new_for_tests(),
            test_hub_with(
                vec![MachineSummary::named("测试机", &["mock_acp"])],
                vec![None],
            ),
            &temp_data_dir(),
        );
        engine.gate.lock().running = true;

        engine.sync_state();
        assert_eq!(engine.state(), SessionState::Busy);

        engine.gate.lock().running = false;
        engine.sync_state();
        assert_eq!(engine.state(), SessionState::Idle);
    }
    #[tokio::test]
    async fn tool_loop_runs_tools_then_returns_text() {
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::tool_call(
                "call_1",
                "list_agents",
                serde_json::json!({}),
            )],
            vec![MockStreamEvent::text("已查询可用 agent，本轮无调度动作")],
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
        assert!(
            matches!(
                live.session.read().transcript.last(),
                Some(OrcMsg::Orc { text, .. }) if text == "已查询可用 agent，本轮无调度动作"
            ),
            "流式文本应实时进入对话流（草稿即最终输出）"
        );
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
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::tool_call(
                "call_1",
                "list_agents",
                serde_json::json!({}),
            )],
            vec![MockStreamEvent::text("收到插话，调整方向")],
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
        assert!(inbox.lock().is_empty(), "插话只注入一次");
    }

    #[tokio::test]
    async fn tool_loop_caps_at_max_turns() {
        // 参数各不相同，避开死循环检测，专测轮次上限
        let turns: Vec<Vec<MockStreamEvent>> = (0..MAX_TOOL_TURNS)
            .map(|i| {
                vec![MockStreamEvent::tool_call(
                    format!("c{i}"),
                    "list_sessions",
                    serde_json::json!({"machine": i}),
                )]
            })
            .collect();
        let model = MockCompletionModel::from_stream_turns(turns);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let res = run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("达上限应优雅收尾而非报错");
        assert!(res.contains("已达"), "收尾文案应提示已达上限: {res}");
        assert_eq!(model.request_count(), MAX_TOOL_TURNS);
        // 收尾文案也应提交进对话流
        assert!(matches!(
            live.session.read().transcript.last(),
            Some(OrcMsg::Orc { text, .. }) if *text == res
        ));
    }

    /// 连续完全相同（工具 + 参数）的调用超过阈值判定死循环，立即收尾。
    #[tokio::test]
    async fn tool_loop_detects_identical_call_loop() {
        let turns: Vec<Vec<MockStreamEvent>> = (0..MAX_IDENTICAL_CALLS)
            .map(|_| {
                vec![MockStreamEvent::tool_call(
                    "c",
                    "list_agents",
                    serde_json::json!({}),
                )]
            })
            .collect();
        let model = MockCompletionModel::from_stream_turns(turns);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let res = run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("死循环应优雅收尾");
        assert!(
            res.contains("反复执行相同调度"),
            "收尾文案应说明死循环中止: {res}"
        );
        // 连续 3 次相同调用即中止，不再消耗后续轮次
        assert_eq!(model.request_count(), MAX_IDENTICAL_CALLS);
    }

    /// wire 级回归：工具循环各轮请求转成 OpenAI wire 消息后，assistant
    /// (tool_calls) 必须紧跟匹配其 id 的 tool 消息（deepseek 曾报
    /// "insufficient tool messages following tool_calls message"）。含 steer
    /// 插话与多轮工具调用场景。
    #[tokio::test]
    async fn tool_loop_wire_sequence_keeps_tool_results_adjacent() {
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::tool_call(
                "call_1",
                "list_agents",
                serde_json::json!({}),
            )],
            vec![MockStreamEvent::tool_call(
                "call_2",
                "list_sessions",
                serde_json::json!({}),
            )],
            vec![MockStreamEvent::text("已完成本轮调度")],
        ]);
        let inbox = Mutex::new(vec!["换一个 agent 重试".to_string()]);
        let live = test_live();
        run_tool_loop(
            model.clone(),
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("循环应正常结束");

        for (round, req) in model.requests().iter().enumerate() {
            let mut wire: Vec<rig_openai::Message> = Vec::new();
            for m in req.chat_history.iter() {
                wire.extend(Vec::<rig_openai::Message>::try_from(m.clone()).unwrap());
            }
            // 逐条扫描 wire 消息：assistant(tool_calls) 之后必须由 id 匹配的
            // tool 消息全部应答后才能出现其他消息
            let mut awaiting: Vec<String> = Vec::new();
            for m in &wire {
                match m {
                    rig_openai::Message::Assistant { tool_calls, .. } => {
                        assert!(
                            awaiting.is_empty(),
                            "第 {round} 轮请求中前一组 tool_calls 尚未全部应答: {awaiting:?}\n{wire:#?}"
                        );
                        awaiting = tool_calls.iter().map(|tc| tc.id.clone()).collect();
                    }
                    rig_openai::Message::ToolResult { tool_call_id, .. } => {
                        awaiting.retain(|id| *id != *tool_call_id);
                    }
                    _ => {}
                }
            }
            assert!(
                awaiting.is_empty(),
                "第 {round} 轮请求中 tool 消息未覆盖全部 tool_call id: {awaiting:?}\n{wire:#?}"
            );
        }
    }

    /// 流式文本增量应实时合并为同一条草稿编排消息（GUI 定时刷新即可见
    /// 生成中的输出；落库为合并后的完整输出）。
    #[tokio::test]
    async fn tool_loop_streams_text_deltas_into_transcript() {
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("你"),
            MockStreamEvent::text("好，"),
            MockStreamEvent::text("世界"),
        ]]);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let out = run_tool_loop(
            model,
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("循环应正常结束");
        assert_eq!(out, "你好，世界");
        let s = live.session.read();
        assert_eq!(s.transcript.len(), 1, "多段增量应合并为同一条编排消息");
        assert!(matches!(&s.transcript[0], OrcMsg::Orc { text, .. } if text == "你好，世界"));
    }

    /// reasoning 增量按 part 合并，流结束（provider 未发完整事件时）上报为
    /// thinking 活动。
    #[tokio::test]
    async fn tool_loop_records_thinking_from_reasoning_stream() {
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::reasoning_delta("思考"),
            MockStreamEvent::reasoning_delta("第一步"),
            MockStreamEvent::text("结论"),
        ]]);
        let mut live = test_live();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        live.record_tool_activity = Some(Arc::new(move |act| {
            if let Activity::Thinking { content, .. } = act {
                sink.lock().push(content);
            }
        }));
        let inbox = Mutex::new(Vec::new());
        let out = run_tool_loop(
            model,
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await
        .expect("循环应正常结束");
        assert_eq!(out, "结论");
        assert_eq!(
            *seen.lock(),
            vec!["思考第一步".to_string()],
            "reasoning 增量应合并为一条 thinking 活动"
        );
        assert!(
            live.current.lock().is_none(),
            "思考结束（流结束）后进行中活动应清除"
        );
    }

    /// 思考增量实时更新进行中活动槽（part 结束由流处理清除）。
    #[test]
    fn thinking_deltas_update_current_activity() {
        let live = test_live();
        live.set_thinking("想");
        live.set_thinking("法");
        assert!(matches!(
            &*live.current.lock(),
            Some(Activity::Thinking { content, .. }) if content == "想法"
        ));
        live.clear_current();
        assert!(live.current.lock().is_none());
    }

    /// 工具执行期间实时活动槽应展示执行中的工具，执行完毕（即使失败）即清除——
    /// 历史活动不充当实时展示（回归：工具执行完毕后实时活动条一直转圈）。
    /// 用假 WS server（auth 后不回包）使 prompt_session 悬在执行中，确定性观察。
    #[tokio::test]
    async fn tool_in_progress_shows_current_activity_then_clears() {
        use futures_util::SinkExt as _;
        // 与 rig Message 重名，改用别名
        use tokio_tungstenite::tungstenite::Message as WsMessage;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // 第一帧 auth：回认证成功；之后的业务请求一律不回包，使请求悬在进行中
            if let Some(Ok(WsMessage::Text(t))) = ws.next().await {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": v["id"].as_u64().unwrap(),
                    "result": { "ok": true }
                });
                let _ = ws.send(WsMessage::text(resp.to_string())).await;
            }
            // 持续读取保持连接，不回包
            while ws.next().await.is_some() {}
        });

        let mut live = test_live();
        let client = WsClient::connect_with_token(format!("ws://{addr}"), "unused".into());
        // 客户端认证完成前发出的业务请求会被立即拒绝（AUTH_FAILED），先等
        // auth_ok 再触发工具调用，使请求真正悬在执行中
        let mut auth_rx = client.subscribe();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(200), auth_rx.recv()).await
            {
                Ok(Ok(n)) if n.method == crate::ws::lifecycle::AUTH_OK => break,
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(_)) => panic!("客户端连接已关闭"),
                Err(_) => assert!(tokio::time::Instant::now() < deadline, "等待客户端认证超时"),
            }
        }
        live.machines[0].client = Some(client);
        live.mount_linked_session(LinkedSession {
            id: "s_child".into(),
            machine_name: "测试机".into(),
        });
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::tool_call(
                "call_1",
                "prompt_session",
                serde_json::json!({ "session": "s_child", "prompt": "干" }),
            )],
            vec![MockStreamEvent::text("已下发")],
        ]);
        let inbox = Mutex::new(Vec::new());
        let live_task = live.clone();
        let task = tokio::spawn(async move {
            run_tool_loop(
                model,
                "preamble",
                vec![Message::user("计划")],
                &inbox,
                &live_task,
            )
            .await
        });

        // 工具悬在执行中：实时活动槽应展示执行中的 prompt_session
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let cur = live.current.lock().clone();
            if matches!(&cur, Some(Activity::ToolCall { name, .. }) if name == "prompt_session") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待进行中实时活动超时"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 断开连接：工具执行失败返回、循环继续；实时活动槽应清除
        server.abort();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(out, "已下发");
        assert!(live.current.lock().is_none(), "工具执行完毕实时活动应清除");
    }

    /// 流式失败（turn 中途报错）不应残留半截编排输出。
    #[tokio::test]
    async fn tool_loop_discards_partial_output_on_stream_error() {
        let model = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("半截输出"),
            MockStreamEvent::error("boom"),
        ]]);
        let live = test_live();
        let inbox = Mutex::new(Vec::new());
        let res = run_tool_loop(
            model,
            "preamble",
            vec![Message::user("计划")],
            &inbox,
            &live,
        )
        .await;
        assert!(res.is_err(), "流式错误应使本轮推进失败");
        assert!(
            live.session.read().transcript.is_empty(),
            "turn 失败应移除草稿消息"
        );
    }
}
