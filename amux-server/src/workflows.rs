//! 工作流会话：元数据（workflow.sqlite）、JSONL 对话与活动、编排智能体驱动。
//!
//! 语义要点（docs/DESIGN.md「Server」）：
//! - 状态以 Server 元数据为权威；编排智能体运行中或任一关联普通会话工作中即为工作中
//! - 关联普通会话从「其他状态 → idle 且非取消」时，向工作流会话注入一条用户消息触发调度
//! - 工作中收到的用户消息以 steer 方式注入（下一轮请求前作为用户消息进入对话）

use std::path::PathBuf;
use std::sync::Arc;

use amux_common::api::{Session, SessionConfigSetting, Workflow};
use amux_common::domain::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionConfigOption, SessionState,
    StateChangeReason,
};
use parking_lot::Mutex;
use rig_core::completion::message::ToolCall;
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

#[derive(Default)]
struct RunState {
    /// rig 对话历史（含工具消息）
    history: Vec<Message>,
    /// 待注入的 steer 用户消息
    steers: Vec<String>,
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

    pub async fn create(&self, plan: &str, title: Option<String>) -> Result<Workflow, String> {
        let id = Uuid::new_v4().to_string();
        let title = title.unwrap_or_else(|| generate_title(plan));
        self.store
            .insert_workflow(&id, &title, SessionState::Idle, plan, now_ms());
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<Workflow, String> {
        let row = self.store.workflow(id).ok_or("工作流不存在")?;
        Ok(Workflow {
            id: row.id.clone(),
            title: row.title,
            state: row.state,
            plan: row.plan,
            created_at: row.created_at,
            updated_at: row.updated_at,
            linked_sessions: self.linked_sessions(id),
        })
    }

    pub fn list(&self, limit: usize, offset: usize) -> (Vec<Workflow>, bool) {
        let (rows, has_more) = self.store.workflows_page(limit, offset);
        (
            rows.into_iter()
                .map(|row| Workflow {
                    linked_sessions: self.linked_sessions(&row.id),
                    id: row.id,
                    title: row.title,
                    state: row.state,
                    plan: row.plan,
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
        self.get(id)?;
        self.push_user(id, input);
        Ok(())
    }

    pub fn configure(&self, id: &str, title: Option<String>) -> Result<(), String> {
        if let Some(title) = title {
            self.store.set_workflow_title(id, &title);
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
        for path in [self.history_path(id), self.activities_path(id)] {
            let _ = std::fs::remove_file(path);
        }
        log::info!("工作流会话已删除: {id}");
        Ok(())
    }

    pub fn history(&self, id: &str, limit: usize, offset: usize) -> (Vec<HistoryItem>, bool) {
        let lines = read_lines(&self.history_path(id));
        let mut items: Vec<HistoryItem> = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<HistoryLine>(line).ok())
            .map(|line| HistoryItem::from(&line))
            .collect();
        items.reverse();
        let window: Vec<HistoryItem> = items.into_iter().skip(offset).take(limit + 1).collect();
        let has_more = window.len() > limit;
        let mut window = window;
        window.truncate(limit);
        window.reverse();
        (window, has_more)
    }

    pub fn activities(&self, id: &str, limit: usize, offset: usize) -> (Vec<Activity>, bool) {
        let lines = read_lines(&self.activities_path(id));
        let mut activities: Vec<Activity> = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Activity>(line).ok())
            .collect();
        activities.reverse();
        let window: Vec<Activity> = activities
            .into_iter()
            .skip(offset)
            .take(limit + 1)
            .collect();
        let has_more = window.len() > limit;
        let mut window = window;
        window.truncate(limit);
        window.reverse();
        (window, has_more)
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
        self.push_user(&workflow_id, vec![ContentBlock::Text { text: message }]);
    }

    /// 推送一条用户消息：运行中进 steer，空闲则起一轮调度。
    fn push_user(self: &Arc<Self>, workflow_id: &str, content: Vec<ContentBlock>) {
        // 编排对话只承载文本，非文本块不进 rig 历史
        let text = blocks_text(&content);
        append_line(
            &self.history_path(workflow_id),
            &HistoryLine {
                role: "user".to_string(),
                content,
                timestamp: now_ms(),
            },
        );
        self.store.touch_workflow(workflow_id);
        let running = {
            let mut runs = self.runs.lock();
            let state = runs.entry(workflow_id.to_string()).or_default();
            if state.running {
                state.steers.push(text);
                true
            } else {
                state.history.push(Message::user(text));
                false
            }
        };
        if !running {
            self.spawn_run(workflow_id.to_string());
        }
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
                    append_line(
                        &self.history_path(workflow_id),
                        &HistoryLine {
                            role: "agent".to_string(),
                            content: vec![ContentBlock::Text { text: text.clone() }],
                            timestamp: now_ms(),
                        },
                    );
                    self.runs
                        .lock()
                        .entry(workflow_id.to_string())
                        .or_default()
                        .history
                        .push(Message::assistant(text));
                }
            }
            Err(error) => {
                log::warn!("编排智能体运行失败（{workflow_id}）: {error}");
                self.record_activity(
                    workflow_id,
                    &Activity::Error {
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
            .ok_or("编排智能体未配置".to_string())?;
        let plan = self
            .store
            .workflow(workflow_id)
            .map(|row| row.plan)
            .unwrap_or_default();
        let tools = WorkflowTools {
            service: Arc::clone(self),
            workflow_id: workflow_id.to_string(),
        };
        let mut output = String::new();
        loop {
            let history = self
                .runs
                .lock()
                .get(workflow_id)
                .map(|s| s.history.clone())
                .unwrap_or_default();
            let text =
                orchestrator::run(&config, &orchestrator::preamble(&plan), history, &tools).await?;
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
            for steer in steers {
                state.history.push(Message::user(format!("用户：{steer}")));
            }
        }
    }

    /// 工作流状态重算：编排智能体运行中或任一关联会话工作中即为工作中。
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
        append_line(&self.activities_path(workflow_id), activity);
        if let Some(state) = self.runs.lock().get_mut(workflow_id) {
            state.ongoing = Some(activity.clone());
        }
    }

    /// 会话被工作流关联（编排智能体创建会话时调用）。
    pub fn link_session(&self, workflow_id: &str, session_id: &str) {
        self.store.link_session(workflow_id, session_id);
        self.recompute_state(workflow_id);
    }

    pub fn is_linked(&self, workflow_id: &str, session_id: &str) -> bool {
        self.store
            .workflow_of_session(session_id)
            .is_some_and(|id| id == workflow_id)
    }

    fn history_path(&self, workflow_id: &str) -> PathBuf {
        self.home
            .join("workflows")
            .join(format!("{workflow_id}_history.jsonl"))
    }

    fn activities_path(&self, workflow_id: &str) -> PathBuf {
        self.home
            .join("workflows")
            .join(format!("{workflow_id}_activities.jsonl"))
    }
}

/// 编排智能体的工具执行面与活动记录面：全部动作限定在本工作流的关联普通会话上。
struct WorkflowTools {
    service: Arc<WorkflowService>,
    workflow_id: String,
}

impl Tools for WorkflowTools {
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
                timestamp: now_ms(),
                thinking: text.to_string(),
            },
        );
    }

    fn record_tool_call(&self, call: &ToolCall) {
        self.service.record_activity(
            &self.workflow_id,
            &Activity::ToolCall {
                timestamp: now_ms(),
                tool_call_id: call.id.to_string(),
                tool_name: call.function.name.clone(),
                title: tool_title(&call.function.name, &call.function.arguments),
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
                            "{} ({}@{}): {} / 工作目录 {}",
                            session.id,
                            session.agent,
                            session.machine,
                            state_label(session.state),
                            services.sessions.work_dir(session)
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
                    .create(&machine, &agent, &cwd, worktree)
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
                    .prompt(&session_id, vec![ContentBlock::Text { text: prompt }])
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
                    .configure(&session_id, title, config)
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

/// 工具调用的展示标题（活动历史中与工具名并列展示）。
fn tool_title(name: &str, arguments: &serde_json::Value) -> Option<String> {
    let session = string_arg(arguments, "session").unwrap_or_else(|_| "会话".to_string());
    Some(match name {
        "list_agents" => "列出机器与 agent".to_string(),
        "list_sessions" => "列出关联普通会话".to_string(),
        "create_session" => format!(
            "创建普通会话 {}@{}",
            string_arg(arguments, "machine").unwrap_or_default(),
            string_arg(arguments, "agent").unwrap_or_default()
        ),
        "prompt_session" => format!("向 {session} 下发指令"),
        "cancel_session" => format!("取消 {session} 进行中的工作"),
        "configure_session" => format!("配置 {session}"),
        "get_session_config_options" => format!("读取 {session} 会话选项"),
        "read_session_history" => format!("读取 {session} 对话内容"),
        "read_session_activities" => format!("读取 {session} 活动内容"),
        _ => return None,
    })
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

/// 编排工具清单（docs/DESIGN.md「编排智能体」工具表）。
fn tool_definitions() -> Vec<ToolDefinition> {
    let string =
        |description: &str| serde_json::json!({ "type": "string", "description": description });
    let integer =
        |description: &str| serde_json::json!({ "type": "integer", "description": description });
    vec![
        ToolDefinition {
            name: "list_agents".into(),
            description: "已连接机器及各机器的 agent 列表与可用性".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "list_sessions".into(),
            description: "本工作流的关联普通会话列表（标题、状态等）".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "create_session".into(),
            description: "创建关联普通会话，返回会话 id".into(),
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
            description: "向指定关联普通会话下发指令".into(),
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
            description: "配置指定关联普通会话：会话标题或会话选项".into(),
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
            description: "获取指定关联普通会话的会话选项".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "session": string("关联普通会话 id") },
                "required": ["session"]
            }),
        },
        ToolDefinition {
            name: "read_session_history".into(),
            description: "分页读取指定关联普通会话的对话内容".into(),
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
            description: "分页读取指定关联普通会话的活动内容".into(),
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

/// 对话历史行（JSONL）：`content` 为内容块数组，与设计约定一致。
#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryLine {
    role: String,
    content: Vec<ContentBlock>,
    timestamp: u64,
}

impl From<&HistoryLine> for HistoryItem {
    fn from(line: &HistoryLine) -> Self {
        let content = line.content.clone();
        if line.role == "user" {
            HistoryItem::UserMessage {
                content,
                timestamp: line.timestamp,
            }
        } else {
            HistoryItem::AgentMessage {
                content,
                timestamp: line.timestamp,
            }
        }
    }
}

/// 内容块拼接为纯文本（非文本块丢弃）。
fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
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
    fn history_line_round_trips_content_blocks() {
        let line = HistoryLine {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "你好".into(),
            }],
            timestamp: 7,
        };
        let json = serde_json::to_string(&line).unwrap();
        assert_eq!(
            json,
            r#"{"role":"user","content":[{"type":"text","text":"你好"}],"timestamp":7}"#
        );
        let parsed: HistoryLine = serde_json::from_str(&json).unwrap();
        assert_eq!(
            HistoryItem::from(&parsed),
            HistoryItem::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "你好".into()
                }],
                timestamp: 7,
            }
        );
    }

    /// 编排过程中的工具调用与思考立即进活动历史，并作为进行中活动对外可见。
    #[tokio::test]
    async fn recorded_activities_are_persisted_and_reported_as_ongoing() {
        use std::sync::Arc;

        use rig_core::completion::message::ToolFunction;

        use crate::terminals::TerminalCache;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let store = Arc::new(Store::open(&home).unwrap());
        let config = Arc::new(ConfigStore::new(home.clone()));
        let terminals = Arc::new(TerminalCache::new());
        let (events, _events_rx) = tokio::sync::mpsc::channel(4);
        let machines = MachineHub::new("token".into(), events, Arc::clone(&terminals));
        let sessions = Arc::new(SessionService::new(
            Arc::clone(&store),
            machines.clone(),
            terminals,
            Arc::clone(&config),
        ));
        let service = Arc::new(WorkflowService::new(
            Arc::clone(&store),
            sessions,
            machines,
            config,
            home,
        ));
        let workflow = service.create("计划", None).await.unwrap();
        service.runs.lock().entry(workflow.id.clone()).or_default();

        let tools = WorkflowTools {
            service: Arc::clone(&service),
            workflow_id: workflow.id.clone(),
        };
        tools.record_tool_call(&ToolCall::from_wire(
            "call_001",
            ToolFunction::new("list_agents".into(), serde_json::json!({})),
        ));
        tools.record_thinking("先看机器列表");

        let (activities, _) = service.activities(&workflow.id, PAGE_LIMIT, 0);
        match &activities[0] {
            Activity::ToolCall {
                tool_call_id,
                tool_name,
                title,
                parameters,
                ..
            } => {
                assert_eq!(tool_call_id, "call_001");
                assert_eq!(tool_name, "list_agents");
                assert_eq!(title.as_deref(), Some("列出机器与 agent"));
                assert_eq!(parameters.as_deref(), Some("{}"));
            }
            other => panic!("应为工具调用活动: {other:?}"),
        }
        assert!(matches!(
            activities[1],
            Activity::Thinking { ref thinking, .. } if thinking == "先看机器列表"
        ));
        assert_eq!(
            service.ongoing_activity(&workflow.id),
            Some(activities[1].clone())
        );
    }
}
