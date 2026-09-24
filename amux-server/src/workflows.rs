//! 工作流会话：元数据（workflow.sqlite）、transcript JSONL（对话与活动）、工作流智能体驱动。
//!
//! 语义要点（docs/DESIGN.md「Server」）：
//! - 状态以 Server 元数据为权威；工作流智能体运行中或任一关联普通会话工作中即为工作中
//! - 关联普通会话从「其他状态 → idle 且非取消」时，向工作流会话注入一条用户消息触发调度
//! - 工作中收到的用户消息以 steer 方式注入（下一轮请求前作为用户消息进入对话）

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v2::TextContent;
use amux_common::api::{Session, SessionConfigSetting, Workflow};
use amux_common::domain::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionConfigOption, SessionState,
    StateChangeReason,
};
use parking_lot::Mutex;
use rig_core::completion::message::{
    AssistantContent, Audio, AudioMediaType, DocumentSourceKind, Image, ImageMediaType, MimeType,
    Reasoning, Text as RigText, ToolCall, ToolFunction, ToolResultContent, UserContent,
};
use rig_core::completion::{Message, ToolDefinition};
use uuid::Uuid;

use crate::config_store::ConfigStore;
use crate::machines::MachineHub;
use crate::orchestrator::{self, ToolFuture, Tools};
use crate::sessions::SessionService;
use crate::store::Store;
use crate::timestamps::now_ms;

/// 历史条目分页默认窗口。
const PAGE_LIMIT: usize = 200;

/// 上下文恢复时给工具调用补的工具结果文本，也是内存上下文中过期轮次的占位文本。
const EXPIRED_TOOL_RESULT: &str = "[工具结果已过期]";

/// 内存上下文中保留真实工具结果的轮数（更早轮次替换为过期文本）。
const KEPT_TOOL_RESULT_ROUNDS: usize = 5;

#[derive(Default)]
struct RunState {
    /// 模型对话上下文：进程启动后首次交互从 transcript 恢复，之后在内存中维护
    history: Vec<Message>,
    /// 待注入的 steer 用户消息（完整 ACP ContentBlock）
    steers: Vec<Vec<ContentBlock>>,
    running: bool,
    /// 最近一次活动（进行中活动展示用）
    ongoing: Option<Activity>,
}

pub struct WorkflowService {
    store: Arc<Store>,
    sessions: Arc<SessionService>,
    machines: MachineHub,
    config: Arc<ConfigStore>,
    home: PathBuf,
    runs: Mutex<std::collections::HashMap<String, RunState>>,
}

impl WorkflowService {
    pub fn new(
        store: Arc<Store>,
        sessions: Arc<SessionService>,
        machines: MachineHub,
        config: Arc<ConfigStore>,
        home: PathBuf,
    ) -> Self {
        Self {
            store,
            sessions,
            machines,
            config,
            home,
            runs: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub async fn create(
        &self,
        plan: &str,
        title: Option<String>,
        project: Option<&str>,
    ) -> Result<Workflow, String> {
        let id = Uuid::new_v4().to_string();
        let title = title.unwrap_or_default();
        self.store
            .insert_workflow(&id, &title, SessionState::Idle, plan, project, now_ms());
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<Workflow, String> {
        let row = self.store.workflow(id).ok_or("工作流不存在")?;
        Ok(Workflow {
            id: row.id.clone(),
            title: row.title,
            state: row.state,
            plan: row.plan,
            project: row.project,
            created_at: row.created_at,
            updated_at: row.updated_at,
            linked_sessions: self.linked_sessions(id),
        })
    }

    pub fn list(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> (Vec<Workflow>, bool) {
        let (rows, has_more) = self.store.workflows_page(limit, offset, project);
        (
            rows.into_iter()
                .map(|row| Workflow {
                    linked_sessions: self.linked_sessions(&row.id),
                    id: row.id,
                    title: row.title,
                    state: row.state,
                    plan: row.plan,
                    project: row.project,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
                .collect(),
            has_more,
        )
    }

    fn linked_sessions(&self, workflow_id: &str) -> Vec<Session> {
        self.store
            .linked_sessions(workflow_id)
            .into_iter()
            .filter_map(|session_id| self.store.session(&session_id))
            .collect()
    }

    /// 用户消息：工作中以 steer 注入，否则新起一轮调度。
    pub async fn prompt(
        self: &Arc<Self>,
        id: &str,
        input: Vec<ContentBlock>,
    ) -> Result<(), String> {
        let workflow = self.get(id)?;
        if workflow.title.is_empty() {
            let title = generate_title(&blocks_text(&input));
            if !title.is_empty() {
                self.store.set_workflow_title_if_empty(id, &title);
            }
        }
        self.push_user(id, input);
        Ok(())
    }

    /// 项目删除后，其下所有工作流会话回到未归属（docs/PRD.md「项目管理」）。
    pub fn unassign_project(&self, project: &str) {
        self.store.unassign_project(project);
    }

    pub fn configure(
        &self,
        id: &str,
        title: Option<String>,
        project: Option<Option<String>>,
    ) -> Result<(), String> {
        if let Some(title) = title {
            self.store.set_workflow_title(id, &title);
        }
        if let Some(project) = project {
            self.store.set_workflow_project(id, project.as_deref());
        }
        Ok(())
    }

    /// 删除工作流会话：连同其关联普通会话一并删除。
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        self.store.workflow(id).ok_or("工作流不存在")?;
        let linked = self.store.linked_sessions(id);
        self.store.delete_workflow(id);
        self.runs.lock().remove(id);
        for session_id in linked {
            let _ = self.sessions.delete(&session_id).await;
        }
        let _ = std::fs::remove_file(self.transcript_path(id));
        log::info!("工作流会话已删除: {id}");
        Ok(())
    }

    pub fn history(&self, id: &str, limit: usize, offset: usize) -> (Vec<HistoryItem>, bool) {
        let items: Vec<HistoryItem> = self
            .transcript(id)
            .into_iter()
            .enumerate()
            .filter_map(|(index, line)| {
                // 文件只追加：行号即条目标识，分页刷新时用它识别同一条消息
                let id = format!("line-{index}");
                match line {
                    TranscriptLine::User { content, timestamp } => Some(HistoryItem::UserMessage {
                        id,
                        content,
                        timestamp,
                    }),
                    TranscriptLine::Agent { content, timestamp } => {
                        Some(HistoryItem::AgentMessage {
                            id,
                            content,
                            timestamp,
                        })
                    }
                    _ => None,
                }
            })
            .collect();
        page(items, limit, offset)
    }

    pub fn activities(&self, id: &str, limit: usize, offset: usize) -> (Vec<Activity>, bool) {
        let items: Vec<Activity> = self
            .transcript(id)
            .into_iter()
            .enumerate()
            .filter_map(|(index, line)| line.into_activity(format!("line-{index}")))
            .collect();
        page(items, limit, offset)
    }

    pub fn ongoing_activity(&self, id: &str) -> Option<Activity> {
        let runs = self.runs.lock();
        runs.get(id).and_then(|state| state.ongoing.clone())
    }

    /// 关联普通会话状态落定：非取消地进入空闲时注入驱动消息。
    pub fn on_linked_idle(
        self: &Arc<Self>,
        session_id: &str,
        machine: &str,
        old_state: SessionState,
        new_state: SessionState,
        reason: StateChangeReason,
    ) {
        if new_state != SessionState::Idle || reason == StateChangeReason::Cancelled {
            return;
        }
        let Some(workflow_id) = self.store.workflow_of_session(session_id) else {
            return;
        };
        let message = format!(
            "关联普通会话 `{session_id}@{machine}` 检测到状态变更：{} -> {}，变更原因为 {}",
            state_label(old_state),
            state_label(new_state),
            reason_label(reason)
        );
        self.push_user(
            &workflow_id,
            vec![ContentBlock::Text(TextContent::new(message))],
        );
    }

    /// 推送一条用户消息：运行中进 steer，空闲则起一轮调度。
    fn push_user(self: &Arc<Self>, workflow_id: &str, content: Vec<ContentBlock>) {
        // 用户消息完整进入模型上下文：Image/Audio 映射为对应多模态内容，其余以文本/JSON 呈现
        let mut runs = self.runs.lock();
        let state = runs.entry(workflow_id.to_string()).or_default();
        if state.history.is_empty() {
            // 进程启动后的首次交互：恢复 transcript 里的上下文，再接上本条输入
            state.history = self.restore_history(workflow_id);
        }
        self.append(
            workflow_id,
            &TranscriptLine::User {
                content: content.clone(),
                timestamp: now_ms(),
            },
        );
        self.store.touch_workflow(workflow_id);
        if state.running {
            state.steers.push(content);
            return;
        }
        state.history.push(content_to_user_message(&content));
        drop(runs);
        self.spawn_run(workflow_id.to_string());
    }

    fn spawn_run(self: &Arc<Self>, workflow_id: String) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_once(&workflow_id).await;
        });
    }

    async fn run_once(self: &Arc<Self>, workflow_id: &str) {
        {
            let mut runs = self.runs.lock();
            let state = runs.entry(workflow_id.to_string()).or_default();
            if state.running {
                return;
            }
            state.running = true;
        }
        self.store
            .set_workflow_state(workflow_id, SessionState::Busy);

        let result = self.drive(workflow_id).await;
        match result {
            Ok(text) => {
                if !text.trim().is_empty() {
                    self.append(
                        workflow_id,
                        &TranscriptLine::Agent {
                            content: vec![ContentBlock::Text(TextContent::new(text))],
                            timestamp: now_ms(),
                        },
                    );
                }
            }
            Err(error) => {
                log::warn!("工作流智能体运行失败（{workflow_id}）: {error}");
                self.record_activity(
                    workflow_id,
                    &Activity::Error {
                        id: format!("err-{}", Uuid::new_v4()),
                        timestamp: now_ms(),
                        error: error.clone(),
                    },
                );
            }
        }

        {
            let mut runs = self.runs.lock();
            if let Some(state) = runs.get_mut(workflow_id) {
                state.running = false;
                state.ongoing = None;
            }
        }
        self.recompute_state(workflow_id);
    }

    /// 驱动一轮：每次请求前注入 steer；一轮结束后仍有 steer 则继续。
    async fn drive(self: &Arc<Self>, workflow_id: &str) -> Result<String, String> {
        let config = self
            .config
            .orchestrator()
            .ok_or("工作流智能体未配置".to_string())?;
        let plan = self
            .store
            .workflow(workflow_id)
            .map(|row| row.plan)
            .unwrap_or_default();
        let tools: Arc<dyn Tools> = Arc::new(WorkflowTools {
            service: Arc::clone(self),
            workflow_id: workflow_id.to_string(),
        });
        let mut output = String::new();
        loop {
            let history = {
                let mut runs = self.runs.lock();
                let state = runs.entry(workflow_id.to_string()).or_default();
                fill_missing_tool_results(&mut state.history);
                state.history.clone()
            };
            let text = orchestrator::run(
                &config,
                &orchestrator::preamble(&plan),
                history,
                tools.clone(),
            )
            .await?;
            output.push_str(&text);
            let steers = {
                let mut runs = self.runs.lock();
                let state = runs.entry(workflow_id.to_string()).or_default();
                std::mem::take(&mut state.steers)
            };
            if steers.is_empty() {
                return Ok(output);
            }
            let mut runs = self.runs.lock();
            let state = runs.entry(workflow_id.to_string()).or_default();
            for blocks in steers {
                state.history.push(content_to_user_message(&blocks));
            }
        }
    }

    /// 工作流状态重算：工作流智能体运行中或任一关联会话工作中即为工作中。
    pub fn recompute_state(&self, workflow_id: &str) {
        let running = self
            .runs
            .lock()
            .get(workflow_id)
            .is_some_and(|state| state.running);
        let linked_busy = self
            .store
            .linked_sessions(workflow_id)
            .into_iter()
            .filter_map(|session_id| self.store.session(&session_id))
            .any(|session| session.state == SessionState::Busy);
        let state = if running || linked_busy {
            SessionState::Busy
        } else {
            SessionState::Idle
        };
        self.store.set_workflow_state(workflow_id, state);
    }

    pub fn record_activity(&self, workflow_id: &str, activity: &Activity) {
        self.append(workflow_id, &TranscriptLine::from(activity));
        if let Some(state) = self.runs.lock().get_mut(workflow_id) {
            state.ongoing = Some(activity.clone());
        }
    }

    /// 会话被工作流关联（工作流智能体创建会话时调用）。
    pub fn link_session(&self, workflow_id: &str, session_id: &str) {
        self.store.link_session(workflow_id, session_id);
        self.recompute_state(workflow_id);
    }

    pub fn is_linked(&self, workflow_id: &str, session_id: &str) -> bool {
        self.store
            .workflow_of_session(session_id)
            .is_some_and(|id| id == workflow_id)
    }

    fn transcript_path(&self, workflow_id: &str) -> PathBuf {
        self.home
            .join("workflows")
            .join(format!("{workflow_id}_transcript.jsonl"))
    }

    /// 追加一条 transcript 记录。
    fn append(&self, workflow_id: &str, line: &TranscriptLine) {
        append_line(&self.transcript_path(workflow_id), line);
    }

    /// 读取 transcript 记录（无法解析的行丢弃）。
    fn transcript(&self, workflow_id: &str) -> Vec<TranscriptLine> {
        read_lines(&self.transcript_path(workflow_id))
            .into_iter()
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    /// 从 transcript 恢复模型对话上下文：为每个工具调用补一条过期工具结果。
    fn restore_history(&self, workflow_id: &str) -> Vec<Message> {
        let mut messages: Vec<Message> = Vec::new();
        // transcript 不记回合边界：thinking 归到紧随其后的 assistant 回合，工具结果由 fill 补齐
        let mut thinking: Vec<AssistantContent> = Vec::new();
        let mut calls: Vec<AssistantContent> = Vec::new();
        for line in self.transcript(workflow_id) {
            match line {
                TranscriptLine::Thinking { thinking: text, .. } => {
                    settle_turn(&mut messages, &mut thinking, &mut calls);
                    thinking.push(AssistantContent::Reasoning(Reasoning::new(&text)));
                }
                TranscriptLine::ToolCall {
                    tool_call_id,
                    tool_name,
                    parameters,
                    ..
                } => {
                    calls.push(AssistantContent::ToolCall(ToolCall::from_wire(
                        tool_call_id,
                        ToolFunction::new(tool_name, parse_arguments(&parameters)),
                    )));
                }
                TranscriptLine::User { content, .. } => {
                    settle_turn(&mut messages, &mut thinking, &mut calls);
                    messages.push(content_to_user_message(&content));
                }
                TranscriptLine::Agent { content, .. } => {
                    let text = blocks_text(&content);
                    if calls.is_empty() {
                        // 末回合只有文本输出：与同回合的思考合成一条消息
                        let mut content = std::mem::take(&mut thinking);
                        content.push(AssistantContent::text(text));
                        messages.push(Message::Assistant { id: None, content });
                    } else {
                        settle_turn(&mut messages, &mut thinking, &mut calls);
                        messages.push(Message::assistant(text));
                    }
                }
                // 执行错误只是本地记录，不构成回合边界，也不进模型上下文
                TranscriptLine::Error { .. } => {}
            }
        }
        settle_turn(&mut messages, &mut thinking, &mut calls);
        fill_missing_tool_results(&mut messages);
        messages
    }
}

/// 结算当前累积的 assistant 回合（思考 + 工具调用；工具结果由 `fill_missing_tool_results` 补齐）。
fn settle_turn(
    messages: &mut Vec<Message>,
    thinking: &mut Vec<AssistantContent>,
    calls: &mut Vec<AssistantContent>,
) {
    if thinking.is_empty() && calls.is_empty() {
        return;
    }
    let mut content = std::mem::take(thinking);
    content.append(calls);
    messages.push(Message::Assistant { id: None, content });
}

/// 给没有工具结果的工具调用补一条过期工具结果，保证回放给模型的对话形态完整。
fn fill_missing_tool_results(history: &mut Vec<Message>) {
    let mut index = 0;
    while index < history.len() {
        let calls: Vec<(String, String)> = match &history[index] {
            Message::Assistant { content, .. } => content
                .iter()
                .filter_map(|item| match item {
                    AssistantContent::ToolCall(call) => {
                        Some((call.id.to_string(), call.function.name.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if calls.is_empty() {
            index += 1;
            continue;
        }
        if matches!(history.get(index + 1), Some(message) if has_tool_result(message)) {
            index += 2;
            continue;
        }
        let results: Vec<UserContent> = calls
            .iter()
            .map(|(id, name)| {
                UserContent::tool_result(
                    id.as_str(),
                    name.clone(),
                    vec![ToolResultContent::text(EXPIRED_TOOL_RESULT)],
                )
            })
            .collect();
        history.insert(index + 1, Message::User { content: results });
        index += 2;
    }
}

fn has_tool_result(message: &Message) -> bool {
    matches!(message, Message::User { content } if content.iter().any(|item| matches!(item, UserContent::ToolResult(_))))
}

/// 内存上下文只保留最近若干轮工具结果的真实值，更早轮次替换为过期文本。
fn expire_old_tool_results(history: &mut [Message]) {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if !has_tool_result(message) {
            continue;
        }
        match groups.last_mut() {
            // 相邻的工具结果属于同一轮
            Some(group) if group.last().is_some_and(|last| last + 1 == index) => group.push(index),
            _ => groups.push(vec![index]),
        }
    }
    let expired = groups.len().saturating_sub(KEPT_TOOL_RESULT_ROUNDS);
    for group in &groups[..expired] {
        for &index in group {
            let Message::User { content } = &mut history[index] else {
                continue;
            };
            for item in content.iter_mut() {
                if let UserContent::ToolResult(result) = item {
                    result.content = vec![ToolResultContent::text(EXPIRED_TOOL_RESULT)];
                }
            }
        }
    }
}

/// 反向分页：`items` 按时间正序传入，返回最新在后的窗口与是否还有更早的条目。
fn page<T>(items: Vec<T>, limit: usize, offset: usize) -> (Vec<T>, bool) {
    let mut window: Vec<T> = items
        .into_iter()
        .rev()
        .skip(offset)
        .take(limit + 1)
        .collect();
    let has_more = window.len() > limit;
    window.truncate(limit);
    window.reverse();
    (window, has_more)
}

/// 工作流智能体的工具执行面与活动记录面：全部动作限定在本工作流的关联普通会话上。
struct WorkflowTools {
    service: Arc<WorkflowService>,
    workflow_id: String,
}

impl Tools for WorkflowTools {
    fn take_steers(&self) -> Vec<Message> {
        let mut runs = self.service.runs.lock();
        let Some(state) = runs.get_mut(&self.workflow_id) else {
            return Vec::new();
        };
        let messages: Vec<Message> = std::mem::take(&mut state.steers)
            .into_iter()
            .map(|blocks| content_to_user_message(&blocks))
            .collect();
        // 请求补丁不写入 rig 自己的对话，内存上下文要自己记下 steer
        state.history.extend(messages.iter().cloned());
        messages
    }

    fn record_context(&self, message: Message) {
        let mut runs = self.service.runs.lock();
        if let Some(state) = runs.get_mut(&self.workflow_id) {
            state.history.push(message);
            expire_old_tool_results(&mut state.history);
        }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        tool_definitions()
    }

    fn dispatch<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move { self.call(name, arguments).await })
    }

    fn record_thinking(&self, text: &str) {
        self.service.record_activity(
            &self.workflow_id,
            &Activity::Thinking {
                id: format!("think-{}", Uuid::new_v4()),
                timestamp: now_ms(),
                thinking: text.to_string(),
            },
        );
    }

    fn record_tool_call(&self, call: &ToolCall) {
        self.service.record_activity(
            &self.workflow_id,
            &Activity::ToolCall {
                id: call.id.to_string(),
                timestamp: now_ms(),
                tool_call_id: call.id.to_string(),
                tool_name: call.function.name.clone(),
                title: None,
                parameters: Some(call.function.arguments.to_string()),
            },
        );
    }
}

impl WorkflowTools {
    async fn call(&self, name: &str, arguments: serde_json::Value) -> Result<String, String> {
        let services = &self.service;
        match name {
            "list_agents" => {
                let mut lines = Vec::new();
                for machine in services.machines.machines() {
                    let agents = services
                        .machines
                        .agents(&machine.name)
                        .await
                        .unwrap_or_default();
                    for agent in agents {
                        lines.push(format!(
                            "{}/{}: {}",
                            machine.name,
                            agent.name,
                            if agent.available {
                                "可用"
                            } else {
                                "不可用"
                            }
                        ));
                    }
                }
                Ok(if lines.is_empty() {
                    "没有已连接的机器".to_string()
                } else {
                    lines.join("\n")
                })
            }
            "list_sessions" => {
                let sessions = services.linked_sessions(&self.workflow_id);
                if sessions.is_empty() {
                    return Ok("本工作流暂无关联普通会话".to_string());
                }
                Ok(sessions
                    .iter()
                    .map(|session| {
                        format!(
                            "{} ({}@{}): {} / 标题 {} / 工作目录 {} / worktree 目录 {}",
                            session.id,
                            session.agent,
                            session.machine,
                            state_label(session.state),
                            if session.title.is_empty() {
                                "未命名"
                            } else {
                                session.title.as_str()
                            },
                            session.workspace,
                            if session.worktree_dir.is_empty() {
                                "未启用"
                            } else {
                                session.worktree_dir.as_str()
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "create_session" => {
                let machine = string_arg(&arguments, "machine")?;
                let agent = string_arg(&arguments, "agent")?;
                let cwd = string_arg(&arguments, "cwd")?;
                let worktree = arguments
                    .get("worktree")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let session = services
                    .sessions
                    .create(&machine, &agent, &cwd, worktree, None)
                    .await?;
                services.link_session(&self.workflow_id, &session.id);
                Ok(format!(
                    "已创建关联普通会话 {} （{}@{}, 工作目录 {}）",
                    session.id,
                    agent,
                    machine,
                    services.sessions.work_dir(&session)
                ))
            }
            "prompt_session" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                let prompt = string_arg(&arguments, "prompt")?;
                services
                    .sessions
                    .prompt(
                        &session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    )
                    .await?;
                Ok(format!("已向 {session_id} 下发指令"))
            }
            "cancel_session" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                services.sessions.cancel(&session_id).await?;
                Ok(format!("已取消 {session_id} 进行中的工作"))
            }
            "configure_session" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                let title = arguments
                    .get("title")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let config = arguments
                    .get("config")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<SessionConfigSetting>(value).ok());
                services
                    .sessions
                    .configure(&session_id, title, config, None)
                    .await?;
                Ok(format!("已更新 {session_id} 配置"))
            }
            "get_session_config_options" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                let options: Vec<SessionConfigOption> =
                    services.sessions.config_options(&session_id).await?;
                Ok(serde_json::to_string(&options).unwrap_or_default())
            }
            "read_session_history" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                let (limit, offset) = page_arg(&arguments);
                let (items, _) = services.sessions.history(&session_id, limit, offset);
                Ok(serde_json::to_string(&items).unwrap_or_default())
            }
            "read_session_activities" => {
                let session_id = string_arg(&arguments, "session")?;
                self.require_linked(&session_id)?;
                let (limit, offset) = page_arg(&arguments);
                let (activities, _) = services.sessions.activities(&session_id, limit, offset);
                Ok(serde_json::to_string(&activities).unwrap_or_default())
            }
            other => Err(format!("未知工具: {other}")),
        }
    }

    fn require_linked(&self, session_id: &str) -> Result<(), String> {
        if self.service.is_linked(&self.workflow_id, session_id) {
            Ok(())
        } else {
            Err(format!("{session_id} 不是本工作流的关联普通会话"))
        }
    }
}

/// 工具调用的参数（transcript 中以 JSON 字符串存放）转回 JSON 值。
fn parse_arguments(parameters: &str) -> serde_json::Value {
    serde_json::from_str(parameters).unwrap_or_default()
}

fn string_arg(arguments: &serde_json::Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 分页参数（默认 limit = PAGE_LIMIT，offset = 0）。
fn page_arg(arguments: &serde_json::Value) -> (usize, usize) {
    let limit = arguments
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(PAGE_LIMIT)
        .max(1);
    let offset = arguments
        .get("offset")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(0);
    (limit, offset)
}

/// 工作流工具清单（docs/DESIGN.md「工作流智能体」工具表）。
fn tool_definitions() -> Vec<ToolDefinition> {
    let string =
        |description: &str| serde_json::json!({ "type": "string", "description": description });
    let integer =
        |description: &str| serde_json::json!({ "type": "integer", "description": description });
    vec![
        ToolDefinition {
            name: "list_agents".into(),
            description: "查询各机器的 agent 列表，包括 agent 状态".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "list_sessions".into(),
            description: "查询关联普通会话列表，包含会话 ID、状态、标题、工作目录、worktree 目录、机器、agent 信息".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "create_session".into(),
            description: "创建关联普通会话用于调度智能体执行任务，需指定机器、agent、工作目录、是否开启 worktree，若开启 worktree，系统自动创建 worktree 并让执行智能体在 worktree 目录工作".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "machine": string("机器名"),
                    "agent": string("agent 名"),
                    "cwd": string("工作目录"),
                    "worktree": { "type": "boolean", "description": "是否以 git worktree 方式工作；缺省 false" }
                },
                "required": ["machine", "agent", "cwd"]
            }),
        },
        ToolDefinition {
            name: "prompt_session".into(),
            description: "向指定关联普通会话以用户角色下发指令".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": string("关联普通会话 id"),
                    "prompt": string("指令内容")
                },
                "required": ["session", "prompt"]
            }),
        },
        ToolDefinition {
            name: "cancel_session".into(),
            description: "取消指定关联普通会话进行中的工作".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "session": string("关联普通会话 id") },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "configure_session".into(),
            description: "配置指定关联普通会话的会话标题和会话选项".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": string("关联普通会话 id"),
                    "title": string("会话标题"),
                    "config": {
                        "type": "object",
                        "description": "会话选项设置：type 为 value_id 时 value 是字符串，boolean 时是布尔值",
                        "properties": {
                            "configId": string("会话选项 id"),
                            "type": { "type": "string", "enum": ["value_id", "boolean"] },
                            "value": { "description": "选项值" }
                        },
                        "required": ["configId", "type", "value"]
                    }
                },
                "required": ["session"],
                "anyOf": [{ "required": ["title"] }, { "required": ["config"] }]
            }),
        },
        ToolDefinition {
            name: "get_session_config_options".into(),
            description: "获取指定关联普通会话支持的会话选项，例如模型、推理级别，不同执行智能体可能支持不同的选项".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "session": string("关联普通会话 id") },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "read_session_history".into(),
            description: "分页读取关联普通会话对话内容，包含用户消息和执行智能体输出消息".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": string("关联普通会话 id"),
                    "limit": integer(&format!("每页条数，缺省 {PAGE_LIMIT}")),
                    "offset": integer("跳过的条数，缺省 0")
                },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "read_session_activities".into(),
            description: "分页读取关联普通会话活动内容，包含执行智能体的思考、工具调用和错误".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session": string("关联普通会话 id"),
                    "limit": integer(&format!("每页条数，缺省 {PAGE_LIMIT}")),
                    "offset": integer("跳过的条数，缺省 0")
                },
                "required": ["session"]
            }),
        },
    ]
}

fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "空闲",
        SessionState::Busy => "工作中",
    }
}

fn reason_label(reason: StateChangeReason) -> &'static str {
    match reason {
        StateChangeReason::Completed => "正常结束",
        StateChangeReason::Cancelled => "已取消",
        StateChangeReason::MaxTokens => "达到 token 上限",
        StateChangeReason::MaxTurnRequests => "达到请求次数上限",
        StateChangeReason::Refusal => "agent 拒绝继续",
        StateChangeReason::Aborted => "异常终止",
    }
}

/// `~/.amux/workflows/<workflow_id>_transcript.jsonl` 的一行：`kind` 区分对话与活动，不含工具结果。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TranscriptLine {
    User {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    Agent {
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
    Thinking {
        timestamp: u64,
        thinking: String,
    },
    ToolCall {
        timestamp: u64,
        tool_call_id: String,
        tool_name: String,
        parameters: String,
    },
    Error {
        timestamp: u64,
        error: String,
    },
}

impl From<&Activity> for TranscriptLine {
    fn from(activity: &Activity) -> Self {
        match activity {
            Activity::Thinking {
                timestamp,
                thinking,
                ..
            } => TranscriptLine::Thinking {
                timestamp: *timestamp,
                thinking: thinking.clone(),
            },
            Activity::ToolCall {
                timestamp,
                tool_call_id,
                tool_name,
                parameters,
                ..
            } => TranscriptLine::ToolCall {
                timestamp: *timestamp,
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                parameters: parameters.clone().unwrap_or_default(),
            },
            Activity::Error {
                timestamp, error, ..
            } => TranscriptLine::Error {
                timestamp: *timestamp,
                error: error.clone(),
            },
        }
    }
}

impl TranscriptLine {
    /// 活动类记录转活动条目；`id` 由调用方按 transcript 行号给出（文件只追加，行号稳定）。
    fn into_activity(self, id: String) -> Option<Activity> {
        match self {
            TranscriptLine::Thinking {
                timestamp,
                thinking,
            } => Some(Activity::Thinking {
                id,
                timestamp,
                thinking,
            }),
            TranscriptLine::ToolCall {
                timestamp,
                tool_call_id,
                tool_name,
                parameters,
            } => Some(Activity::ToolCall {
                id,
                timestamp,
                tool_call_id,
                tool_name,
                title: None,
                parameters: Some(parameters),
            }),
            TranscriptLine::Error { timestamp, error } => Some(Activity::Error {
                id,
                timestamp,
                error,
            }),
            TranscriptLine::User { .. } | TranscriptLine::Agent { .. } => None,
        }
    }
}

/// 内容块拼接为纯文本（非文本块丢弃）。
fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// 用户内容块转 rig `Message`：Image/Audio 映射为多模态内容，不能被映射的以 JSON 文本呈现，不丢弃消息。
fn content_to_user_message(blocks: &[ContentBlock]) -> Message {
    Message::User {
        content: blocks.iter().filter_map(block_to_user_content).collect(),
    }
}

/// ACP ContentBlock → rig UserContent；无法映射的块以 JSON 文本呈现，保证不丢弃。
fn block_to_user_content(block: &ContentBlock) -> Option<UserContent> {
    match block {
        ContentBlock::Text(text) => Some(UserContent::Text(RigText::new(&text.text))),
        ContentBlock::Image(image) => match ImageMediaType::from_mime_type(&image.mime_type.0) {
            Some(media_type) => Some(UserContent::Image(Image {
                data: DocumentSourceKind::base64(&image.data),
                media_type: Some(media_type),
                detail: None,
                additional_params: None,
            })),
            None => Some(UserContent::Text(RigText::new(content_to_text(
                std::slice::from_ref(block),
            )))),
        },
        ContentBlock::Audio(audio) => match AudioMediaType::from_mime_type(&audio.mime_type.0) {
            Some(media_type) => Some(UserContent::Audio(Audio {
                data: DocumentSourceKind::base64(&audio.data),
                media_type: Some(media_type),
                additional_params: None,
            })),
            None => Some(UserContent::Text(RigText::new(content_to_text(
                std::slice::from_ref(block),
            )))),
        },
        _ => Some(UserContent::Text(RigText::new(content_to_text(
            std::slice::from_ref(block),
        )))),
    }
}

/// 用户内容块转模型可见文本：文本块原样输出，非文本块以 JSON 呈现，不丢弃任何内容块。
fn content_to_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => serde_json::to_string(other)
                .unwrap_or_else(|error| format!("[无法序列化内容块: {error}]")),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_line(path: &PathBuf, value: &impl serde::Serialize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = match serde_json::to_string(value) {
        Ok(line) => line,
        Err(error) => {
            log::warn!("序列化失败: {error}");
            return;
        }
    };
    use std::io::Write as _;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => {
            let _ = writeln!(file, "{line}");
        }
        Err(error) => log::warn!("写 {} 失败: {error}", path.display()),
    }
}

fn read_lines(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2::ResourceLink;

    #[test]
    fn tool_definitions_cover_documented_tools() {
        let names: Vec<String> = tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        for expected in [
            "list_agents",
            "list_sessions",
            "create_session",
            "prompt_session",
            "cancel_session",
            "configure_session",
            "get_session_config_options",
            "read_session_history",
            "read_session_activities",
        ] {
            assert!(names.contains(&expected.to_string()), "缺少工具 {expected}");
        }
    }

    #[tokio::test]
    async fn list_sessions_includes_documented_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path());
        let workflow = service.create("计划", None, None).await.unwrap();
        let session = service
            .sessions
            .create("pc", "codex", "/repo", false, None)
            .await
            .unwrap();
        service.link_session(&workflow.id, &session.id);
        service
            .sessions
            .configure(&session.id, Some("修复登录".into()), None, None)
            .await
            .unwrap();

        let tools = WorkflowTools {
            service,
            workflow_id: workflow.id,
        };
        let output = tools
            .call("list_sessions", serde_json::json!({}))
            .await
            .unwrap();

        for expected in [
            session.id.as_str(),
            "codex@pc",
            "空闲",
            "标题 修复登录",
            "工作目录 /repo",
            "worktree 目录 未启用",
        ] {
            assert!(output.contains(expected), "缺少 {expected:?}: {output}");
        }
    }

    #[tokio::test]
    async fn workflow_title_comes_from_first_user_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path());
        let workflow = service
            .create("这里是很长的执行计划", None, None)
            .await
            .unwrap();
        assert_eq!(workflow.title, "");

        service
            .prompt(
                &workflow.id,
                vec![ContentBlock::Text(TextContent::new("先实现登录功能"))],
            )
            .await
            .unwrap();
        assert_eq!(service.get(&workflow.id).unwrap().title, "先实现登录功能");

        service
            .prompt(
                &workflow.id,
                vec![ContentBlock::Text(TextContent::new("再补充测试"))],
            )
            .await
            .unwrap();
        assert_eq!(service.get(&workflow.id).unwrap().title, "先实现登录功能");

        let titled = service
            .create("计划", Some("手动标题".into()), None)
            .await
            .unwrap();
        assert_eq!(titled.title, "手动标题");
    }

    #[test]
    fn prompt_session_only_accepts_text_prompt() {
        let definition = tool_definitions()
            .into_iter()
            .find(|definition| definition.name == "prompt_session")
            .expect("缺少 prompt_session");
        let properties = definition.parameters["properties"]
            .as_object()
            .expect("properties 应为对象");
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["session", "prompt"]
        );
        assert_eq!(
            definition.parameters["required"],
            serde_json::json!(["session", "prompt"])
        );
        assert!(
            string_arg(
                &serde_json::json!({ "session": "s1", "content": [{ "type": "text", "text": "x" }] }),
                "prompt"
            )
            .is_err(),
            "content 不能替代 prompt"
        );
    }

    #[test]
    fn page_arg_parses_limit_and_offset_with_defaults() {
        assert_eq!(page_arg(&serde_json::json!({})), (PAGE_LIMIT, 0));
        assert_eq!(
            page_arg(&serde_json::json!({ "limit": 50, "offset": 100 })),
            (50, 100)
        );
        // limit 缺省按默认窗口，offset 缺省为 0
        assert_eq!(
            page_arg(&serde_json::json!({ "offset": 10 })),
            (PAGE_LIMIT, 10)
        );
        // 非法 limit 不 panic，退回默认
        assert_eq!(
            page_arg(&serde_json::json!({ "limit": "x" })),
            (PAGE_LIMIT, 0)
        );
    }

    #[test]
    fn content_to_text_keeps_all_blocks() {
        let text = content_to_text(&[
            ContentBlock::Text(TextContent::new("看看这个")),
            ContentBlock::ResourceLink(ResourceLink::new("a.rs", "file:///dir/a.rs")),
        ]);
        assert!(text.contains("看看这个"), "文本块被丢弃: {text}");
        assert!(
            text.contains("file:///dir/a.rs"),
            "ResourceLink 被丢弃: {text}"
        );
        assert!(text.contains("a.rs"), "ResourceLink name 被丢弃: {text}");
    }

    #[test]
    fn transcript_lines_match_documented_shape() {
        let user = TranscriptLine::User {
            content: vec![ContentBlock::Text(TextContent::new("你好"))],
            timestamp: 7,
        };
        assert_eq!(
            serde_json::to_string(&user).unwrap(),
            r#"{"kind":"user","content":[{"type":"text","text":"你好"}],"timestamp":7}"#
        );
        let call = TranscriptLine::ToolCall {
            timestamp: 9,
            tool_call_id: "call_001".into(),
            tool_name: "prompt_session".into(),
            parameters: r#"{"session":"s1"}"#.into(),
        };
        assert_eq!(
            serde_json::to_string(&call).unwrap(),
            r#"{"kind":"tool_call","timestamp":9,"tool_call_id":"call_001","tool_name":"prompt_session","parameters":"{\"session\":\"s1\"}"}"#
        );
    }

    /// 对话与活动共用一份 transcript：活动读回时按行号取标识、按参数重建展示标题。
    #[tokio::test]
    async fn transcript_serves_history_and_activities() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path());
        let workflow = service.create("计划", None, None).await.unwrap();
        service.runs.lock().entry(workflow.id.clone()).or_default();
        let tools = WorkflowTools {
            service: Arc::clone(&service),
            workflow_id: workflow.id.clone(),
        };

        service.append(
            &workflow.id,
            &TranscriptLine::User {
                content: vec![ContentBlock::Text(TextContent::new("启动"))],
                timestamp: 1,
            },
        );
        tools.record_tool_call(&ToolCall::from_wire(
            "call_001",
            ToolFunction::new(
                "prompt_session".into(),
                serde_json::json!({ "session": "s1", "prompt": "跑测试" }),
            ),
        ));
        tools.record_thinking("先下发指令");

        let (activities, more) = service.activities(&workflow.id, PAGE_LIMIT, 0);
        assert!(!more);
        match &activities[0] {
            Activity::ToolCall {
                id,
                tool_call_id,
                tool_name,
                title,
                parameters,
                ..
            } => {
                assert_eq!(id, "line-1");
                assert_eq!(tool_call_id, "call_001");
                assert_eq!(tool_name, "prompt_session");
                assert_eq!(title, &None);
                assert_eq!(
                    parameters.as_deref(),
                    Some(r#"{"session":"s1","prompt":"跑测试"}"#)
                );
            }
            other => panic!("应为工具调用活动: {other:?}"),
        }
        assert!(matches!(
            activities[1],
            Activity::Thinking { ref thinking, .. } if thinking == "先下发指令"
        ));

        // 进行中活动取自内存，内容与落盘记录一致
        match service.ongoing_activity(&workflow.id) {
            Some(Activity::Thinking { thinking, .. }) => assert_eq!(thinking, "先下发指令"),
            other => panic!("进行中活动应为思考: {other:?}"),
        }

        // 活动行不进对话历史
        service.append(
            &workflow.id,
            &TranscriptLine::Agent {
                content: vec![ContentBlock::Text(TextContent::new("已下发"))],
                timestamp: 4,
            },
        );
        let (history, more) = service.history(&workflow.id, PAGE_LIMIT, 0);
        assert!(!more);
        assert_eq!(
            history
                .iter()
                .map(|item| item.id().to_string())
                .collect::<Vec<_>>(),
            ["line-0", "line-3"]
        );
        assert!(matches!(
            &history[1],
            HistoryItem::AgentMessage { content, .. } if blocks_text(content) == "已下发"
        ));
    }

    /// 模型上下文从 transcript 恢复：工具调用补过期结果，thinking 归到紧随的 assistant 回合。
    #[tokio::test]
    async fn restore_history_fills_expired_results_and_thinking() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path());
        let workflow = service.create("计划", None, None).await.unwrap();
        for line in [
            TranscriptLine::User {
                content: vec![ContentBlock::Text(TextContent::new("启动"))],
                timestamp: 1,
            },
            TranscriptLine::ToolCall {
                timestamp: 2,
                tool_call_id: "call_001".into(),
                tool_name: "list_sessions".into(),
                parameters: "{}".into(),
            },
            TranscriptLine::Thinking {
                timestamp: 3,
                thinking: "看看会话".into(),
            },
            TranscriptLine::Error {
                timestamp: 4,
                error: "模型调用失败: xxx".into(),
            },
            TranscriptLine::Agent {
                content: vec![ContentBlock::Text(TextContent::new("已启动"))],
                timestamp: 5,
            },
            TranscriptLine::User {
                content: vec![ContentBlock::Text(TextContent::new("继续"))],
                timestamp: 6,
            },
        ] {
            service.append(&workflow.id, &line);
        }

        let history = service.restore_history(&workflow.id);
        let json: Vec<String> = history
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect();
        assert_eq!(history.len(), 5);
        assert!(json[0].contains("启动"));
        assert!(json[1].contains("call_001") && json[1].contains("list_sessions"));
        assert!(json[2].contains(EXPIRED_TOOL_RESULT) && json[2].contains("call_001"));
        // thinking 归到紧随其后的 assistant 回合（回合文本输出之前）
        assert!(json[3].contains("看看会话") && json[3].contains("已启动"));
        assert!(json[4].contains("继续"));
        // 执行错误只用于展示，不进模型上下文
        assert!(!json.concat().contains("模型调用失败"));
    }

    /// 进程启动后的首次交互：接上旧 transcript 与本次输入，且不重复本次输入。
    #[tokio::test]
    async fn first_interaction_restores_transcript_without_duplicating_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workflow = {
            let service = test_service(home);
            let workflow = service.create("计划", None, None).await.unwrap();
            for line in [
                TranscriptLine::User {
                    content: vec![ContentBlock::Text(TextContent::new("上一轮"))],
                    timestamp: 1,
                },
                TranscriptLine::ToolCall {
                    timestamp: 2,
                    tool_call_id: "call_001".into(),
                    tool_name: "list_sessions".into(),
                    parameters: "{}".into(),
                },
                TranscriptLine::Agent {
                    content: vec![ContentBlock::Text(TextContent::new("上次输出"))],
                    timestamp: 3,
                },
            ] {
                service.append(&workflow.id, &line);
            }
            workflow
        };
        // 模拟进程重启：同一目录重新开一个服务，内存上下文为空
        let service = test_service(home);
        service.push_user(
            &workflow.id,
            vec![ContentBlock::Text(TextContent::new("新输入"))],
        );

        let history = service
            .runs
            .lock()
            .get(&workflow.id)
            .unwrap()
            .history
            .clone();
        let json: Vec<String> = history
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect();
        assert_eq!(history.len(), 5);
        assert!(json[0].contains("上一轮"));
        assert!(json[1].contains("call_001"));
        assert!(json[2].contains(EXPIRED_TOOL_RESULT));
        assert!(json[3].contains("上次输出"));
        assert!(json[4].contains("新输入"));
    }

    /// 内存上下文只保留最近 5 轮工具结果的真实值。
    #[test]
    fn expire_old_tool_results_keeps_only_recent_rounds() {
        let mut history: Vec<Message> = vec![Message::user("开始")];
        for round in 0..7 {
            let call = ToolCall::from_wire(
                format!("call_{round}"),
                ToolFunction::new("list_sessions".into(), serde_json::json!({})),
            );
            history.push(Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(call)],
            });
            history.push(Message::tool_result(
                format!("call_{round}"),
                "list_sessions",
                format!("结果{round}"),
            ));
        }

        expire_old_tool_results(&mut history);

        let results: Vec<String> = history
            .iter()
            .filter(|message| has_tool_result(message))
            .map(|message| serde_json::to_string(message).unwrap())
            .collect();
        assert_eq!(results.len(), 7);
        for (round, json) in results.iter().enumerate() {
            let expired = json.contains(EXPIRED_TOOL_RESULT);
            assert_eq!(expired, round < 2, "第 {round} 轮: {json}");
        }
        // 未过期的工具调用与其结果仍成对相邻
        assert!(fill_missing_is_noop(&history));
    }

    /// 悬空的工具调用会被补上过期结果，补齐后不再重复补。
    #[test]
    fn missing_tool_results_are_filled_once() {
        let mut history = vec![
            Message::user("开始"),
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall::from_wire(
                    "call_001",
                    ToolFunction::new("list_sessions".into(), serde_json::json!({})),
                ))],
            },
            Message::user("继续"),
        ];

        fill_missing_tool_results(&mut history);

        assert_eq!(history.len(), 4);
        assert!(
            matches!(&history[2], Message::User { content } if matches!(content.first(), Some(UserContent::ToolResult(result)) if matches!(&result.content[..], [ToolResultContent::Text(text)] if text.text == EXPIRED_TOOL_RESULT)))
        );
        assert!(fill_missing_is_noop(&history));
    }

    fn fill_missing_is_noop(history: &[Message]) -> bool {
        let mut clone = history.to_vec();
        fill_missing_tool_results(&mut clone);
        clone.len() == history.len()
    }

    fn test_service(home: &std::path::Path) -> Arc<WorkflowService> {
        use crate::terminals::TerminalCache;

        let store = Arc::new(Store::open(home).unwrap());
        let config = Arc::new(ConfigStore::new(home.to_path_buf()));
        let terminals = Arc::new(TerminalCache::new());
        let (events, _events_rx) = tokio::sync::mpsc::channel(4);
        let machines = MachineHub::new(
            "token".into(),
            events,
            Arc::clone(&terminals),
            Arc::clone(&config),
        );
        let sessions = Arc::new(SessionService::new(
            Arc::clone(&store),
            machines.clone(),
            terminals,
            Arc::clone(&config),
        ));
        Arc::new(WorkflowService::new(
            store,
            sessions,
            machines,
            config,
            home.to_path_buf(),
        ))
    }
}
