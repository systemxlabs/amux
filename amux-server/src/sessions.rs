//! 普通会话服务：生命周期、惰性 agent 会话创建/恢复、状态与缓存、终端、worktree。
//!
//! 语义要点（docs/DESIGN.md「Server」各节）：
//! - 会话状态以 Server 元数据为权威，变更来自 Agent 的 `state_update` 通知
//! - 惰性创建：发指令或查询会话选项时才向 Agent 发 `session/new`
//! - 惰性恢复：已有 agent 会话在首次交互时 `session/resume`；失败只记错误活动，不改元数据
//! - 删除立即生效，资源清理（agent 会话、worktree、终端）异步尽力而为

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use amux_common::api::{Session, SessionConfigSetting, Terminal as ApiTerminal, TerminalOutput};
use amux_common::domain::{
    generate_title, Activity, ContentBlock, FsListParams, HistoryItem, SessionConfigOption,
    SessionPlanEntry, SessionState, SlashCommand, StateChangeReason, TerminalOpenParams,
    TerminalResizeParams,
};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::acp::AcpEvent;
use crate::config_store::ConfigStore;
use crate::machines::MachineHub;
use crate::store::Store;
use crate::terminals::TerminalCache;
use crate::timestamps::now_ms;

/// 会话长时间无活动后关闭 agent 侧会话的阈值。
const IDLE_CLOSE_AFTER_MS: u64 = 60 * 60 * 1000;
/// worktree 过期清理阈值。
const WORKTREE_EXPIRE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// 一次状态落定的结果（工作流驱动据此注入提示词）。
#[derive(Debug, Clone)]
pub struct StateApplied {
    pub session_id: String,
    pub old_state: SessionState,
    pub new_state: SessionState,
    pub reason: StateChangeReason,
}

#[derive(Default)]
pub struct SessionCaches {
    options: Mutex<HashMap<String, Vec<SessionConfigOption>>>,
    commands: Mutex<HashMap<String, Vec<SlashCommand>>>,
    plans: Mutex<HashMap<String, Vec<SessionPlanEntry>>>,
    contexts: Mutex<HashMap<String, (u64, u64)>>,
    /// 工具调用活动的当前字段（`tool_call_update` 未携带的字段保持不变）
    tool_calls: Mutex<HashMap<(String, String), Activity>>,
    /// agent 会话 id → amux 会话 id
    agent_sessions: Mutex<HashMap<String, String>>,
    /// 已完成（创建或恢复）agent 会话的 amux 会话
    resumed: Mutex<HashSet<String>>,
}

pub struct SessionService {
    store: Arc<Store>,
    machines: MachineHub,
    terminals: Arc<TerminalCache>,
    config: Arc<ConfigStore>,
    pub caches: Arc<SessionCaches>,
}

impl SessionService {
    pub fn new(
        store: Arc<Store>,
        machines: MachineHub,
        terminals: Arc<TerminalCache>,
        config: Arc<ConfigStore>,
    ) -> Self {
        Self {
            store,
            machines,
            terminals,
            config,
            caches: Arc::new(SessionCaches::default()),
        }
    }

    pub fn get(&self, id: &str) -> Result<Session, String> {
        self.store
            .session(id)
            .ok_or_else(|| "会话不存在".to_string())
    }

    /// 会话列表分页：最近活跃的非关联普通会话（`GET /sessions`）。
    pub fn list(&self, limit: usize, offset: usize) -> (Vec<Session>, bool) {
        self.store.sessions_page(limit, offset)
    }

    pub fn agent_session_id(&self, id: &str) -> Option<String> {
        self.store.agent_session_id(id)
    }

    /// 新建会话：仅在 Server 侧写入；worktree 方式立即在机器上创建。
    pub async fn create(
        &self,
        machine: &str,
        agent: &str,
        workspace: &str,
        use_worktree: bool,
    ) -> Result<Session, String> {
        let available = self
            .machines
            .agents(machine)
            .await
            .map_err(|_| "机器未连接".to_string())?
            .into_iter()
            .any(|item| item.name == agent && item.available);
        if !available {
            return Err(format!("agent 不可用: {agent}@{machine}"));
        }
        let worktree_dir = if use_worktree {
            self.machines.worktree_new(machine, workspace).await?
        } else {
            String::new()
        };
        let now = now_ms();
        let session = Session {
            id: Uuid::new_v4().to_string(),
            machine: machine.to_string(),
            agent: agent.to_string(),
            title: String::new(),
            state: SessionState::Idle,
            workspace: workspace.to_string(),
            worktree_dir,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_session(&session)?;
        self.config.record_workspace(machine, workspace);
        log::info!("会话已创建: {} ({}@{})", session.id, agent, machine);
        Ok(session)
    }

    /// 发送指令：惰性创建/恢复 agent 会话，prompt 受理后立即落盘用户消息。
    pub async fn prompt(&self, id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        let session = self.get(id)?;
        self.ensure_agent_session(&session).await?;
        let agent_session_id = self
            .agent_session_id(id)
            .ok_or_else(|| "agent 会话未就绪".to_string())?;
        let conn = self.machines.acp(&session.machine, &session.agent).await?;
        conn.prompt(&agent_session_id, input.clone()).await?;

        // prompt 已受理：用户消息立即落盘（忽略 Agent 回放的 user_message*）
        let text: String = input
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let content = serde_json::to_string(&input).unwrap_or_default();
        self.store
            .upsert_message(id, &Uuid::new_v4().to_string(), "user", &content, now_ms());
        if session.title.is_empty() && !text.is_empty() {
            self.store.set_title(id, &generate_title(&text));
        }
        self.store.touch(id);
        Ok(())
    }

    /// 取消：仅当会话正在工作中（空闲会话不触达 Agent）。
    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        let session = self.get(id)?;
        if session.state == SessionState::Idle {
            return Ok(());
        }
        let Some(agent_session_id) = self.agent_session_id(id) else {
            return Ok(());
        };
        let conn = self.machines.acp(&session.machine, &session.agent).await?;
        conn.cancel(&agent_session_id)
    }

    pub async fn configure(
        &self,
        id: &str,
        title: Option<String>,
        config: Option<SessionConfigSetting>,
    ) -> Result<(), String> {
        if let Some(title) = title {
            self.store.set_title(id, &title);
        }
        if let Some(config) = config {
            let session = self.get(id)?;
            self.ensure_agent_session(&session).await?;
            let agent_session_id = self
                .agent_session_id(id)
                .ok_or_else(|| "agent 会话未就绪".to_string())?;
            let conn = self.machines.acp(&session.machine, &session.agent).await?;
            let options = conn
                .set_config_option(&agent_session_id, &config.config_id, config.value)
                .await?;
            self.caches.options.lock().insert(id.to_string(), options);
        }
        Ok(())
    }

    /// 删除：元数据立即删除，资源清理异步尽力而为。
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let session = self.get(id)?;
        let agent_session_id = self.agent_session_id(id);
        self.store.delete_session(id);
        self.forget(id);

        let terminals = self.terminals.remove_session(id);
        let machines = self.machines.clone();
        let worktree_dir = session.worktree_dir.clone();
        let workspace = session.workspace.clone();
        let machine = session.machine.clone();
        let agent = session.agent.clone();
        tokio::spawn(async move {
            for terminal_id in terminals {
                machines.terminal_close(&machine, &terminal_id).await;
            }
            if let Some(agent_session_id) = agent_session_id {
                if let Ok(conn) = machines.acp(&machine, &agent).await {
                    conn.close(&agent_session_id).await;
                    let _ = conn.delete(&agent_session_id).await;
                }
            }
            if !worktree_dir.is_empty() {
                machines
                    .worktree_remove(&machine, &workspace, &worktree_dir)
                    .await;
            }
        });
        log::info!("会话已删除: {id}");
        Ok(())
    }

    pub async fn config_options(&self, id: &str) -> Result<Vec<SessionConfigOption>, String> {
        let session = self.get(id)?;
        self.ensure_agent_session(&session).await?;
        Ok(self
            .caches
            .options
            .lock()
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    pub fn slash_commands(&self, id: &str) -> Vec<SlashCommand> {
        self.caches
            .commands
            .lock()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn plan(&self, id: &str) -> Vec<SessionPlanEntry> {
        self.caches
            .plans
            .lock()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn context(&self, id: &str) -> (u64, u64) {
        self.caches
            .contexts
            .lock()
            .get(id)
            .copied()
            .unwrap_or((0, 0))
    }

    /// 进行中的活动：会话工作中时取最近一条活动。
    pub fn ongoing_activity(&self, id: &str) -> Option<Activity> {
        let session = self.store.session(id)?;
        (session.state == SessionState::Busy)
            .then(|| self.store.latest_activity(id))
            .flatten()
    }

    pub fn history(&self, id: &str, limit: usize, offset: usize) -> (Vec<HistoryItem>, bool) {
        self.store.messages_page(id, limit, offset)
    }

    pub fn activities(&self, id: &str, limit: usize, offset: usize) -> (Vec<Activity>, bool) {
        self.store.activities_page(id, limit, offset)
    }

    /// 工作目录 diff（worktree 会话以 worktree 目录为准）。
    pub async fn diff(&self, id: &str) -> Result<amux_common::domain::GitDiffResult, String> {
        let session = self.get(id)?;
        self.ensure_worktree(&session).await;
        self.machines
            .git_diff(&session.machine, &self.work_dir(&session))
            .await
    }

    // ---------- 终端 ----------

    pub async fn terminal_open(
        &self,
        id: &str,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let session = self.get(id)?;
        let cwd = cwd.unwrap_or_else(|| self.work_dir(&session));
        let terminal_id = self
            .machines
            .terminal_open(
                &session.machine,
                TerminalOpenParams {
                    cwd: cwd.clone(),
                    cols,
                    rows,
                },
            )
            .await?;
        self.terminals.open(id, &terminal_id, &cwd, cols, rows);
        Ok(terminal_id)
    }

    pub fn terminals(&self, id: &str) -> Vec<ApiTerminal> {
        self.terminals.list(id)
    }

    pub async fn terminal_input(
        &self,
        id: &str,
        terminal_id: &str,
        data: String,
    ) -> Result<(), String> {
        let machine = self.terminal_machine(id, terminal_id)?;
        self.machines
            .terminal_input(
                &machine,
                amux_common::domain::TerminalInputParams {
                    terminal_id: terminal_id.to_string(),
                    data,
                },
            )
            .await
    }

    pub async fn terminal_resize(
        &self,
        id: &str,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let machine = self.terminal_machine(id, terminal_id)?;
        self.terminals.resize(terminal_id, cols, rows);
        self.machines
            .terminal_resize(
                &machine,
                TerminalResizeParams {
                    terminal_id: terminal_id.to_string(),
                    cols,
                    rows,
                },
            )
            .await
    }

    pub fn terminal_output(
        &self,
        terminal_id: &str,
        cursor: Option<u64>,
    ) -> Option<TerminalOutput> {
        self.terminals.read(terminal_id, cursor)
    }

    pub async fn terminal_close(&self, id: &str, terminal_id: &str) -> Result<(), String> {
        let machine = self.terminal_machine(id, terminal_id)?;
        self.terminals.remove(terminal_id);
        self.machines.terminal_close(&machine, terminal_id).await;
        Ok(())
    }

    fn terminal_machine(&self, id: &str, terminal_id: &str) -> Result<String, String> {
        if !self.terminals.contains(terminal_id) {
            return Err("终端不存在".to_string());
        }
        Ok(self.get(id)?.machine)
    }

    // ---------- 内部 ----------

    /// 会话的工作目录：启用 worktree 时用 worktree 目录。
    pub fn work_dir(&self, session: &Session) -> String {
        if session.worktree_dir.is_empty() {
            session.workspace.clone()
        } else {
            session.worktree_dir.clone()
        }
    }

    /// 惰性创建/恢复 agent 侧会话；resume 失败只记错误活动，不改元数据。
    pub async fn ensure_agent_session(&self, session: &Session) -> Result<(), String> {
        self.ensure_worktree(session).await;
        let cwd = self.work_dir(session);
        let conn = self.machines.acp(&session.machine, &session.agent).await?;
        let resumed = self.caches.resumed.lock().contains(&session.id);
        match self.store.agent_session_id(&session.id) {
            None => {
                let (agent_session_id, options) = conn.new_session(&cwd).await?;
                self.store
                    .set_agent_session_id(&session.id, &agent_session_id);
                self.caches
                    .agent_sessions
                    .lock()
                    .insert(agent_session_id, session.id.clone());
                self.caches
                    .options
                    .lock()
                    .insert(session.id.clone(), options);
                self.caches.resumed.lock().insert(session.id.clone());
            }
            Some(agent_session_id) => {
                if resumed {
                    return Ok(());
                }
                match conn.resume_session(&agent_session_id, &cwd).await {
                    Ok(options) => {
                        self.caches
                            .agent_sessions
                            .lock()
                            .insert(agent_session_id, session.id.clone());
                        self.caches
                            .options
                            .lock()
                            .insert(session.id.clone(), options);
                        self.caches.resumed.lock().insert(session.id.clone());
                    }
                    Err(error) => {
                        log::warn!("session/resume 失败（{}）: {error}", session.id);
                        self.record_error(&session.id, &format!("会话恢复失败: {error}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// worktree 按需重建：目录不可访问时在同一路径重建。
    async fn ensure_worktree(&self, session: &Session) {
        if session.worktree_dir.is_empty() {
            return;
        }
        let accessible = self
            .machines
            .fs_list(
                &session.machine,
                FsListParams {
                    path: Some(session.worktree_dir.clone()),
                    limit: 1,
                    offset: 0,
                },
            )
            .await
            .is_ok();
        if accessible {
            return;
        }
        log::info!("worktree 目录缺失，按原路径重建: {}", session.worktree_dir);
        if let Err(error) = self
            .machines
            .worktree_resume(&session.machine, &session.workspace, &session.worktree_dir)
            .await
        {
            log::warn!("worktree 重建失败: {error}");
            self.record_error(&session.id, &format!("worktree 重建失败: {error}"));
        }
    }

    pub fn record_error(&self, session_id: &str, message: &str) {
        let id = format!("err-{}", Uuid::new_v4());
        let activity = Activity::Error {
            id: id.clone(),
            timestamp: now_ms(),
            error: message.to_string(),
        };
        let content = serde_json::to_string(&activity).unwrap_or_default();
        self.store
            .upsert_activity(session_id, &id, "error", &content, now_ms());
    }

    /// 应用 ACP 事件：状态、消息、活动、选项/命令/计划/上下文缓存。
    /// 返回状态落定结果（工作流驱动用），其他事件返回 `None`。
    pub fn apply(&self, event: AcpEvent) -> Option<StateApplied> {
        match event {
            AcpEvent::State {
                agent_session_id,
                state,
                reason,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let old_state = self.store.session(&session.id)?.state;
                self.store.set_state(&session.id, state);
                Some(StateApplied {
                    session_id: session.id,
                    old_state,
                    new_state: state,
                    reason,
                })
            }
            AcpEvent::Message {
                agent_session_id,
                message_id,
                text,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let blocks = vec![ContentBlock::Text { text }];
                let content = serde_json::to_string(&blocks).unwrap_or_default();
                self.store
                    .upsert_message(&session.id, &message_id, "agent", &content, now_ms());
                self.store.touch(&session.id);
                None
            }
            AcpEvent::Thinking {
                agent_session_id,
                message_id,
                text,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let id = format!("think-{message_id}");
                let activity = Activity::Thinking {
                    id: id.clone(),
                    timestamp: now_ms(),
                    thinking: text,
                };
                let content = serde_json::to_string(&activity).unwrap_or_default();
                self.store
                    .upsert_activity(&session.id, &id, "thinking", &content, now_ms());
                None
            }
            AcpEvent::ToolCall {
                agent_session_id,
                tool_call_id,
                name,
                title,
                parameters,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let content = {
                    let mut tool_calls = self.caches.tool_calls.lock();
                    let entry = tool_calls
                        .entry((session.id.clone(), tool_call_id.clone()))
                        .or_insert_with(|| Activity::ToolCall {
                            id: tool_call_id.clone(),
                            timestamp: now_ms(),
                            tool_call_id: tool_call_id.clone(),
                            tool_name: String::new(),
                            title: None,
                            parameters: None,
                        });
                    if let Activity::ToolCall {
                        tool_name,
                        title: current_title,
                        parameters: current_parameters,
                        ..
                    } = entry
                    {
                        if let Some(name) = name {
                            *tool_name = name;
                        }
                        if let Some(title) = title {
                            *current_title = Some(title);
                        }
                        if let Some(parameters) = parameters {
                            *current_parameters = Some(parameters);
                        }
                    }
                    serde_json::to_string(&*entry).unwrap_or_default()
                };
                self.store.upsert_activity(
                    &session.id,
                    &tool_call_id,
                    "tool_call",
                    &content,
                    now_ms(),
                );
                None
            }
            AcpEvent::Error {
                agent_session_id,
                message,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    self.record_error(&session.id, &message);
                }
                None
            }
            AcpEvent::Options {
                agent_session_id,
                options,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    self.caches.options.lock().insert(session.id, options);
                }
                None
            }
            AcpEvent::Commands {
                agent_session_id,
                commands,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    self.caches.commands.lock().insert(session.id, commands);
                }
                None
            }
            AcpEvent::Plan {
                agent_session_id,
                entries,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    self.caches.plans.lock().insert(session.id, entries);
                }
                None
            }
            AcpEvent::Context {
                agent_session_id,
                used,
                size,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    self.caches.contexts.lock().insert(session.id, (used, size));
                }
                None
            }
            AcpEvent::AgentRestarted { machine, agent } => {
                for session in self.store.sessions_of_agent(&machine, &agent) {
                    self.store.set_state(&session.id, SessionState::Idle);
                    self.forget(&session.id);
                }
                None
            }
        }
    }

    fn session_of_agent(&self, agent_session_id: &str) -> Option<Session> {
        let session_id = self
            .caches
            .agent_sessions
            .lock()
            .get(agent_session_id)
            .cloned()?;
        self.store.session(&session_id)
    }

    fn forget(&self, session_id: &str) {
        self.caches.resumed.lock().remove(session_id);
        self.caches
            .agent_sessions
            .lock()
            .retain(|_, value| value != session_id);
        self.caches.options.lock().remove(session_id);
        self.caches.commands.lock().remove(session_id);
        self.caches.plans.lock().remove(session_id);
        self.caches.contexts.lock().remove(session_id);
    }

    /// 后台维护：关闭长时间无活动的 agent 会话、清理过期 worktree。
    pub async fn maintain(&self) {
        let now = now_ms();
        for session in self.store.sessions_all() {
            // 从未有活动的会话以创建时间为基准，避免会话刚建好就被关闭
            let last_activity = self
                .store
                .last_activity_at(&session.id)
                .unwrap_or(session.created_at);
            if idle_expired(&session, last_activity, now) {
                if let Some(agent_session_id) = self.store.agent_session_id(&session.id) {
                    if let Ok(conn) = self.machines.acp(&session.machine, &session.agent).await {
                        conn.close(&agent_session_id).await;
                        self.caches.resumed.lock().remove(&session.id);
                        log::info!("会话长时间无活动，已关闭 agent 侧会话: {}", session.id);
                    }
                }
            }
            if !session.worktree_dir.is_empty()
                && now.saturating_sub(session.updated_at) > WORKTREE_EXPIRE_AFTER_MS
            {
                self.machines
                    .worktree_remove(&session.machine, &session.workspace, &session.worktree_dir)
                    .await;
                log::info!("会话 worktree 过期清理: {}", session.id);
            }
        }
    }
}

/// 是否关闭 agent 侧会话：会话空闲且长时间无新活动（docs/DESIGN.md「ACP 通信」）。
///
/// 活动时间取自 `activities` 表：会话元数据的 `updated_at` 只随用户指令、agent 消息与状态
/// 变更更新，thinking/tool_call 不写它，长 turn 会因此被误判为无活动。
fn idle_expired(session: &Session, last_activity: u64, now: u64) -> bool {
    session.state == SessionState::Idle && now.saturating_sub(last_activity) > IDLE_CLOSE_AFTER_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(state: SessionState) -> Session {
        Session {
            id: "s1".into(),
            machine: "pc".into(),
            agent: "codex".into(),
            title: String::new(),
            state,
            workspace: "/tmp".into(),
            worktree_dir: String::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn busy_session_is_never_closed() {
        let now = 10 * IDLE_CLOSE_AFTER_MS;
        assert!(!idle_expired(&session(SessionState::Busy), 0, now));
    }

    #[test]
    fn idle_session_closed_after_one_hour_without_activity() {
        let now = 10 * IDLE_CLOSE_AFTER_MS;
        assert!(!idle_expired(
            &session(SessionState::Idle),
            now - IDLE_CLOSE_AFTER_MS,
            now
        ));
        assert!(idle_expired(
            &session(SessionState::Idle),
            now - IDLE_CLOSE_AFTER_MS - 1,
            now
        ));
    }
}
