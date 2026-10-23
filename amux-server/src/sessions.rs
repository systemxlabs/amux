//! 普通会话服务：生命周期、惰性 agent 会话创建/恢复、状态与缓存、终端、worktree。
//!
//! 语义要点（docs/DESIGN.md「Server」各节）：
//! - 会话状态以 Server 元数据为权威，变更来自 Agent 的 `state_update` 通知
//! - 惰性创建：发指令或查询会话选项时才向 Agent 发 `session/new`
//! - 惰性恢复：已有 agent 会话在首次交互时 `session/resume`
//! - 删除立即生效，资源清理（agent 会话、worktree、终端）异步尽力而为

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use amux_common::api::{Session, SessionConfigSetting, Terminal as ApiTerminal, Usage};
use amux_common::domain::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionConfigOption, SessionPlanEntry,
    SessionState, SlashCommand, StateChangeReason, TerminalOpenParams, TerminalResizeParams,
    WorktreeBase,
};
use parking_lot::Mutex;
use uuid::Uuid;

use agent_client_protocol::schema::v2::{
    CompactionStatus, NoticeSeverity, ToolCallStatus, ToolKind,
};

use crate::acp::AcpEvent;
use crate::config_store::ConfigStore;
use crate::machines::MachineHub;
use crate::store::Store;
use crate::terminals::{TerminalCache, TerminalSubscription, IDLE_EXPIRE_MS};
use crate::timestamps::now_ms;

/// 会话长时间无活动后关闭 agent 侧会话的阈值。
const IDLE_CLOSE_AFTER_MS: u64 = 60 * 60 * 1000;

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
    usages: Mutex<HashMap<String, Usage>>,
    /// 工具调用活动的当前字段（`tool_call_update` 未携带的字段保持不变）
    tool_calls: Mutex<HashMap<(String, String), Activity>>,
    /// 普通会话实时活动 ID（docs/DESIGN.md「普通会话实时活动」）；内容仍在磁盘
    live_activity: Mutex<HashMap<String, String>>,
    /// agent 会话 id → amux 会话 id
    agent_sessions: Mutex<HashMap<String, String>>,
    /// 已完成（创建或恢复）agent 会话的 amux 会话
    resumed: Mutex<HashSet<String>>,
}

impl SessionCaches {
    /// 清除会话在 Server 进程内的全部运行时状态；持久化元数据与 agent_session_id 不受影响。
    fn forget(&self, session_id: &str) {
        self.resumed.lock().remove(session_id);
        self.agent_sessions
            .lock()
            .retain(|_, value| value != session_id);
        self.options.lock().remove(session_id);
        self.commands.lock().remove(session_id);
        self.plans.lock().remove(session_id);
        self.usages.lock().remove(session_id);
        self.live_activity.lock().remove(session_id);
        self.tool_calls
            .lock()
            .retain(|(current_session, _), _| current_session != session_id);
    }
}

pub struct SessionService {
    store: Arc<Store>,
    machines: MachineHub,
    terminals: Arc<TerminalCache>,
    config: Arc<ConfigStore>,
    pub caches: Arc<SessionCaches>,
}

/// 会话的 worktree 方式。
pub enum SessionWorktree<'a> {
    /// 直接在工作目录内工作。
    Disabled,
    /// 以指定基分支创建 worktree，agent 在 worktree 目录内工作。
    Base(&'a WorktreeBase),
}

impl<'a> SessionWorktree<'a> {
    /// 归一化外部输入：开启 worktree 时必须给出基分支。
    pub fn new(use_worktree: bool, base: Option<&'a WorktreeBase>) -> Result<Self, String> {
        match (use_worktree, base) {
            (false, _) => Ok(Self::Disabled),
            (true, Some(base)) => Ok(Self::Base(base)),
            (true, None) => Err("开启 worktree 时必须指定基分支".to_string()),
        }
    }
}

/// 新建普通会话所需的输入。
pub struct CreateSessionParams<'a> {
    pub machine: &'a str,
    pub agent: &'a str,
    pub workspace: &'a str,
    pub worktree: SessionWorktree<'a>,
    pub title: Option<&'a str>,
    pub project: Option<&'a str>,
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

    /// 会话列表分页：非关联普通会话（`GET /sessions`）；`project` 指定时只返回该项目会话。
    pub fn list(&self, limit: usize, offset: usize, project: Option<&str>) -> (Vec<Session>, bool) {
        self.store.sessions_page(limit, offset, project)
    }

    /// 项目删除后，其下所有会话回到未归属（docs/PRD.md「项目管理」）。
    pub fn unassign_project(&self, project: &str) {
        self.store.unassign_project(project);
    }

    pub fn agent_session_id(&self, id: &str) -> Option<String> {
        self.store.agent_session_id(id)
    }

    /// 反查 ACP 会话对应的 amux 会话 ID。
    pub fn session_id_of_agent(&self, agent_session_id: &str) -> Option<String> {
        self.caches
            .agent_sessions
            .lock()
            .get(agent_session_id)
            .cloned()
    }

    /// 新建会话：仅在 Server 侧写入；worktree 方式立即在机器上创建。
    ///
    /// 不再校验 agent 是否可用：新建会话视图只允许选择已发现的可用 agent，真正不可用
    /// 时会在发送指令时暴露。落库与最近工作目录记录都是阻塞 I/O，放到阻塞线程池执行，
    /// 不占用 async 执行器；仍在响应前完成，保证返回的会话立即可读。
    pub async fn create(&self, request: CreateSessionParams<'_>) -> Result<Session, String> {
        let CreateSessionParams {
            machine,
            agent,
            workspace,
            worktree,
            title,
            project,
        } = request;
        let id = Uuid::new_v4().to_string();
        let worktree_dir = match worktree {
            SessionWorktree::Disabled => String::new(),
            SessionWorktree::Base(base) => {
                log::info!(
                    "会话 worktree 创建请求: session={id} machine={machine} repo={workspace} base={}",
                    base.display_name()
                );
                let dir = self
                    .machines
                    .create_worktree(machine, workspace, base, &id)
                    .await?;
                log::info!("会话 worktree 已创建: session={id} machine={machine} path={dir}");
                dir
            }
        };
        let now = now_ms();
        let session = Session {
            id,
            workflow_id: None,
            group_id: None,
            machine: machine.to_string(),
            agent: agent.to_string(),
            title: title.unwrap_or_default().to_string(),
            state: SessionState::Idle,
            project: project.map(str::to_string),
            pinned: false,
            unread: false,
            workspace: workspace.to_string(),
            worktree_dir,
            created_at: now,
            updated_at: now,
        };
        let store = Arc::clone(&self.store);
        let config = Arc::clone(&self.config);
        let machine = machine.to_string();
        let workspace = workspace.to_string();
        let project = session.project.clone();
        let to_store = session.clone();
        tokio::task::spawn_blocking(move || {
            let stored = store.insert_session(&to_store);
            config.record_workspace(&machine, &workspace, project.as_deref());
            stored
        })
        .await
        .map_err(|error| format!("写入会话任务失败: {error}"))??;
        log::info!(
            "会话已创建: session={} machine={} agent={}",
            session.id,
            session.machine,
            session.agent
        );
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
        let message_id = conn.prompt(&agent_session_id, input.clone()).await?;

        // prompt 已受理：用户消息立即落盘（忽略 Agent 回放的 user_message*）
        let content = serde_json::to_string(&input).unwrap_or_default();
        self.store
            .upsert_message(id, &message_id, "user", &content, now_ms());
        let text: String = input
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect();
        if !text.is_empty() {
            self.store.set_title_if_empty(id, &generate_title(&text));
        }
        self.store.touch(id);
        log::info!(
            "会话指令已发送: session={id} machine={} agent={} agent_session={agent_session_id}",
            session.machine,
            session.agent
        );
        Ok(())
    }

    /// 取消：仅当会话正在工作中（空闲会话不触达 Agent）。
    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        let session = self.get(id)?;
        if session.state == SessionState::Idle {
            log::info!("忽略取消请求，会话已空闲: session={id}");
            return Ok(());
        }
        let Some(agent_session_id) = self.agent_session_id(id) else {
            log::info!("忽略取消请求，agent 会话未就绪: session={id}");
            return Ok(());
        };
        let conn = self.machines.acp(&session.machine, &session.agent).await?;
        conn.cancel(&agent_session_id)?;
        log::info!(
            "会话取消请求已发送: session={id} machine={} agent={} agent_session={agent_session_id}",
            session.machine,
            session.agent
        );
        Ok(())
    }

    pub async fn configure(
        &self,
        id: &str,
        title: String,
        config: Option<SessionConfigSetting>,
        project: Option<String>,
        pinned: bool,
    ) -> Result<(), String> {
        let session = self.get(id)?;
        if !title.is_empty() {
            self.store.set_title(id, &title);
        }
        self.store.set_session_project(id, project.as_deref());
        self.store.set_session_pinned(id, pinned);
        let has_config = config.is_some();
        if let Some(config) = config {
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
        log::info!(
            "会话配置已更新: session={id} machine={} agent={} config={}",
            session.machine,
            session.agent,
            has_config
        );
        Ok(())
    }

    /// 标记会话已读（docs/DESIGN.md「会话交互视图」）。
    pub fn read(&self, id: &str) -> Result<(), String> {
        self.get(id)?;
        self.store.set_session_unread(id, false);
        Ok(())
    }

    /// 删除：元数据立即删除，资源清理异步尽力而为。
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let session = self.get(id)?;
        let agent_session_id = self.agent_session_id(id);
        self.store.delete_session(id);
        self.caches.forget(id);

        let terminals = self.terminals.remove_session(id);
        let machines = self.machines.clone();
        let worktree_dir = session.worktree_dir.clone();
        let workspace = session.workspace.clone();
        let machine = session.machine.clone();
        let agent = session.agent.clone();
        let session_id = id.to_string();
        tokio::spawn(async move {
            for terminal_id in terminals {
                log::info!(
                    "会话删除清理终端: session={session_id} terminal={terminal_id} machine={machine}"
                );
                machines.terminal_close(&machine, &terminal_id).await;
            }
            if let Some(agent_session_id) = agent_session_id {
                if let Ok(conn) = machines.acp(&machine, &agent).await {
                    if let Err(error) = conn.close(&agent_session_id).await {
                        log::warn!(
                            "session/close 失败: session={session_id} agent_session={agent_session_id}: {error}"
                        );
                    }
                    let _ = conn.delete(&agent_session_id).await;
                }
            }
            if !worktree_dir.is_empty() {
                log::info!(
                    "会话删除清理 worktree: session={session_id} machine={machine} path={worktree_dir}"
                );
                machines
                    .worktree_remove(&machine, &workspace, &worktree_dir, &session_id)
                    .await;
            }
        });
        log::info!("会话已删除: session={id}");
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

    pub fn usage(&self, id: &str) -> Option<Usage> {
        self.caches.usages.lock().get(id).copied()
    }

    /// 记录实时活动 ID（docs/DESIGN.md「普通会话实时活动」）。
    fn set_live_activity(&self, session_id: &str, live_id: String) {
        self.caches
            .live_activity
            .lock()
            .insert(session_id.to_string(), live_id);
    }
    /// 进行中的活动：按内存中的实时活动 ID 从磁盘读取。
    pub fn ongoing_activity(&self, id: &str) -> Option<Activity> {
        let live_id = self.caches.live_activity.lock().get(id)?.clone();
        self.store.activity_by_id(id, &live_id)
    }

    pub fn history(&self, id: &str, limit: usize, offset: usize) -> (Vec<HistoryItem>, bool) {
        self.store.messages_page(id, limit, offset)
    }

    pub fn activities(&self, id: &str, limit: usize, offset: usize) -> (Vec<Activity>, bool) {
        self.store.activities_page(id, limit, offset)
    }

    /// 工作目录 diff（worktree 会话以 worktree 目录为准）。
    pub async fn diff(
        &self,
        id: &str,
        base: &str,
    ) -> Result<amux_common::domain::GitDiffResult, String> {
        let session = self.get(id)?;
        self.machines
            .git_diff(&session.machine, &self.exec_dir(&session), base)
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
        let cwd = cwd.unwrap_or_else(|| self.exec_dir(&session));
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
        log::info!(
            "终端已打开: session={id} terminal={terminal_id} cwd={cwd} cols={cols} rows={rows}"
        );
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
        self.terminals.touch(terminal_id);
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

    pub fn subscribe_terminal(&self, terminal_id: &str) -> Option<TerminalSubscription> {
        self.terminals.subscribe(terminal_id)
    }

    pub async fn terminal_close(&self, id: &str, terminal_id: &str) -> Result<(), String> {
        let machine = self.terminal_machine(id, terminal_id)?;
        self.terminals.remove(terminal_id);
        self.machines.terminal_close(&machine, terminal_id).await;
        log::info!("终端已关闭: session={id} terminal={terminal_id}");
        Ok(())
    }

    fn terminal_machine(&self, id: &str, terminal_id: &str) -> Result<String, String> {
        if !self.terminals.contains(terminal_id) {
            return Err("终端不存在".to_string());
        }
        Ok(self.get(id)?.machine)
    }

    // ---------- 内部 ----------

    /// 会话的执行目录：启用 worktree 时用 worktree 目录。
    pub fn exec_dir(&self, session: &Session) -> String {
        if session.worktree_dir.is_empty() {
            session.workspace.clone()
        } else {
            session.worktree_dir.clone()
        }
    }

    /// 惰性创建/恢复 agent 侧会话；resume 失败时记录错误并返回。
    pub async fn ensure_agent_session(&self, session: &Session) -> Result<(), String> {
        let cwd = self.exec_dir(session);
        let conn = self.machines.acp(&session.machine, &session.agent).await?;
        let resumed = self.caches.resumed.lock().contains(&session.id);
        match self.store.agent_session_id(&session.id) {
            None => {
                let (agent_session_id, options) = conn.new_session(&cwd).await?;
                log::info!(
                    "agent 会话已创建: session={} machine={} agent={} agent_session={agent_session_id} cwd={cwd}",
                    session.id,
                    session.machine,
                    session.agent
                );
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
                        log::info!(
                            "agent 会话已恢复: session={} machine={} agent={} agent_session={agent_session_id} cwd={cwd}",
                            session.id,
                            session.machine,
                            session.agent
                        );
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
                        log::warn!(
                            "session/resume 失败: session={} agent_session={agent_session_id}: {error}",
                            session.id
                        );
                        self.record_error(&session.id, &format!("会话恢复失败: {error}"));
                        return Err(format!("会话恢复失败: {error}"));
                    }
                }
            }
        }
        Ok(())
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
                if state == SessionState::Idle {
                    self.caches.live_activity.lock().remove(&session.id);
                }
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
                content,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let content = serde_json::to_string(&content).unwrap_or_default();
                self.store
                    .upsert_message(&session.id, &message_id, "agent", &content, now_ms());
                self.store.set_session_unread(&session.id, true);
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
                self.set_live_activity(&session.id, id);
                None
            }
            AcpEvent::Compaction {
                agent_session_id,
                compaction_id,
                status,
                summary,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let activity = Activity::Compaction {
                    id: compaction_id.clone(),
                    timestamp: now_ms(),
                    status: compaction_status_str(&status),
                    summary,
                };
                let content = serde_json::to_string(&activity).unwrap_or_default();
                self.store.upsert_activity(
                    &session.id,
                    &compaction_id,
                    "compaction",
                    &content,
                    now_ms(),
                );
                if status == CompactionStatus::InProgress {
                    self.set_live_activity(&session.id, compaction_id);
                }
                None
            }
            AcpEvent::ToolCall {
                agent_session_id,
                tool_call_id,
                status,
                name,
                kind,
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
                    apply_tool_call_patch(entry, name, kind, title, parameters);
                    serde_json::to_string(&*entry).unwrap_or_default()
                };
                self.store.upsert_activity(
                    &session.id,
                    &tool_call_id,
                    "tool_call",
                    &content,
                    now_ms(),
                );
                if let Some(ToolCallStatus::Pending | ToolCallStatus::InProgress) = status {
                    // 工具调用终态不立即结束实时活动；由 state_update=idle 统一清空，
                    // 避免前台查询 ongoing_activity 时已提前为空（docs/DESIGN.md「普通会话实时活动」）。
                    self.set_live_activity(&session.id, tool_call_id);
                }
                None
            }
            AcpEvent::Notice {
                agent_session_id,
                severity,
                title,
                description,
            } => {
                let session = self.session_of_agent(&agent_session_id)?;
                let id = format!("notice-{}", Uuid::new_v4());
                let activity = Activity::Notice {
                    id: id.clone(),
                    timestamp: now_ms(),
                    severity: notice_severity_str(&severity),
                    title,
                    description,
                };
                let content = serde_json::to_string(&activity).unwrap_or_default();
                self.store
                    .upsert_activity(&session.id, &id, "notice", &content, now_ms());
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
            AcpEvent::Usage {
                agent_session_id,
                used,
                size,
            } => {
                if let Some(session) = self.session_of_agent(&agent_session_id) {
                    let usage = Usage { used, size };
                    let mut usages = self.caches.usages.lock();
                    if usage.is_empty() {
                        usages.remove(&session.id);
                    } else {
                        usages.insert(session.id, usage);
                    }
                }
                None
            }
            AcpEvent::AgentRestarted { machine, agent } => {
                for session in self.store.sessions_of_agent(&machine, &agent) {
                    self.store.set_state(&session.id, SessionState::Idle);
                    self.caches.forget(&session.id);
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

    /// 后台维护：关闭长时间无活动的 agent 会话、删除长期无输入输出的终端。
    pub async fn maintain(&self) {
        let now = now_ms();
        for session in self.store.sessions_all() {
            // 从未有活动的会话以创建时间为基准，避免会话刚建好就被关闭
            let last_activity = self
                .store
                .last_activity_at(&session.id)
                .unwrap_or(session.created_at);
            let resumed = self.caches.resumed.lock().contains(&session.id);
            if should_close_idle_agent_session(idle_expired(&session, last_activity, now), resumed)
            {
                if let Some(agent_session_id) = self.store.agent_session_id(&session.id) {
                    if let Ok(conn) = self.machines.acp(&session.machine, &session.agent).await {
                        if let Err(error) = conn.close(&agent_session_id).await {
                            log::warn!(
                                "session/close 失败: session={} agent_session={agent_session_id}: {error}",
                                session.id
                            );
                        }
                        self.caches.forget(&session.id);
                        log::info!(
                            "会话长时间无活动，已关闭 agent 侧会话: session={} agent_session={agent_session_id}",
                            session.id
                        );
                    }
                }
            }
        }
        // 终端超过 1 天没有输入输出即删除，并通知机器关闭对应 PTY（docs/DESIGN.md「终端存储」）
        for (session_id, terminal_id) in self.terminals.sweep_idle(now, IDLE_EXPIRE_MS) {
            if let Ok(session) = self.get(&session_id) {
                self.machines
                    .terminal_close(&session.machine, &terminal_id)
                    .await;
            }
            log::info!("终端长时间无输入输出，已删除: session={session_id} terminal={terminal_id}");
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

/// ACP 强类型 → 活动历史中持久化/线上使用的字符串表示。
fn compaction_status_str(status: &CompactionStatus) -> String {
    match status {
        CompactionStatus::InProgress => "in_progress".into(),
        CompactionStatus::Completed => "completed".into(),
        CompactionStatus::Failed => "failed".into(),
        CompactionStatus::Cancelled => "cancelled".into(),
        CompactionStatus::Other(value) => value.clone(),
        _ => "unknown".into(),
    }
}

fn notice_severity_str(severity: &NoticeSeverity) -> String {
    match severity {
        NoticeSeverity::Info => "info".into(),
        NoticeSeverity::Warning => "warning".into(),
        NoticeSeverity::Error => "error".into(),
        NoticeSeverity::Other(value) => value.clone(),
        _ => "unknown".into(),
    }
}

fn tool_kind_str(kind: &ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

/// 工具调用字段补丁：`tool_call_update` 未携带的字段保持不变。
fn apply_tool_call_patch(
    activity: &mut Activity,
    name: Option<String>,
    kind: Option<ToolKind>,
    title: Option<String>,
    parameters: Option<String>,
) {
    let Activity::ToolCall {
        tool_name,
        title: current_title,
        parameters: current_parameters,
        ..
    } = activity
    else {
        return;
    };

    let kind = kind.as_ref().map(tool_kind_str);
    if let Some(name) = name {
        if name.trim().is_empty() {
            *tool_name = kind.unwrap_or_default();
        } else {
            *tool_name = name;
        }
    } else if tool_name.trim().is_empty() {
        if let Some(kind) = kind {
            *tool_name = kind;
        }
    }
    if let Some(title) = title {
        *current_title = Some(title);
    }
    if let Some(parameters) = parameters {
        *current_parameters = Some(parameters);
    }
}

/// 只有内存中仍标记为 resumed 的会话才需要关闭；关闭后该标记会被移除，
/// 避免后续维护仅凭持久化的 agent_session_id 反复关闭同一会话。
fn should_close_idle_agent_session(idle_expired: bool, resumed: bool) -> bool {
    idle_expired && resumed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_requires_base_branch() {
        assert!(SessionWorktree::new(true, None).is_err());
        assert!(matches!(
            SessionWorktree::new(false, None).unwrap(),
            SessionWorktree::Disabled
        ));
        assert!(matches!(
            SessionWorktree::new(
                true,
                Some(&WorktreeBase::parse("refs/heads/main").unwrap())
            )
            .unwrap(),
            SessionWorktree::Base(base) if base.as_str() == "refs/heads/main"
        ));
    }

    fn session(state: SessionState) -> Session {
        Session {
            id: "s1".into(),
            workflow_id: None,
            group_id: None,
            machine: "pc".into(),
            agent: "codex".into(),
            title: String::new(),
            state,
            project: None,
            pinned: false,
            unread: false,
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

    #[test]
    fn idle_agent_session_closes_only_while_resumed() {
        assert!(!should_close_idle_agent_session(true, false));
        assert!(should_close_idle_agent_session(true, true));
    }

    #[test]
    fn tool_call_name_falls_back_to_kind() {
        let mut activity = Activity::ToolCall {
            id: "call-1".into(),
            timestamp: 1,
            tool_call_id: "call-1".into(),
            tool_name: String::new(),
            title: None,
            parameters: None,
        };

        apply_tool_call_patch(
            &mut activity,
            None,
            Some(ToolKind::Read),
            Some("读取配置".into()),
            None,
        );
        apply_tool_call_patch(&mut activity, Some("read_file".into()), None, None, None);
        apply_tool_call_patch(&mut activity, None, Some(ToolKind::Edit), None, None);
        apply_tool_call_patch(
            &mut activity,
            Some(String::new()),
            Some(ToolKind::Execute),
            None,
            None,
        );

        let Activity::ToolCall {
            tool_name, title, ..
        } = activity
        else {
            panic!("应保持工具调用活动");
        };
        assert_eq!(tool_name, "execute");
        assert_eq!(title.as_deref(), Some("读取配置"));
    }

    #[test]
    fn forgetting_session_clears_all_runtime_caches() {
        let caches = SessionCaches::default();
        caches.resumed.lock().insert("s1".into());
        caches
            .agent_sessions
            .lock()
            .insert("agent-s1".into(), "s1".into());
        caches.options.lock().insert("s1".into(), Vec::new());
        caches.commands.lock().insert("s1".into(), Vec::new());
        caches.plans.lock().insert("s1".into(), Vec::new());
        caches
            .usages
            .lock()
            .insert("s1".into(), Usage { used: 1, size: 2 });
        caches.tool_calls.lock().insert(
            ("s1".into(), "call-1".into()),
            Activity::ToolCall {
                id: "call-1".into(),
                timestamp: 1,
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                title: None,
                parameters: None,
            },
        );
        caches
            .live_activity
            .lock()
            .insert("s1".into(), "call-1".into());

        caches.forget("s1");

        assert!(!caches.resumed.lock().contains("s1"));
        assert!(!caches.agent_sessions.lock().contains_key("agent-s1"));
        assert!(!caches.options.lock().contains_key("s1"));
        assert!(!caches.commands.lock().contains_key("s1"));
        assert!(!caches.plans.lock().contains_key("s1"));
        assert!(!caches.usages.lock().contains_key("s1"));
        assert!(!caches
            .tool_calls
            .lock()
            .contains_key(&("s1".into(), "call-1".into())));
        assert!(!caches.live_activity.lock().contains_key("s1"));
    }
}
