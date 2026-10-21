//! 自动化：定时/Webhook 触发、运行会话与决策模型结果分类。
//!
//! 持久化布局见 docs/DESIGN.md「自动化存储」。

use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::v2::TextContent;
use amux_common::api::{
    Automation, AutomationInput, AutomationResultLabel, AutomationRun, AutomationRunState,
    AutomationSessionOptions, AutomationTrigger,
};
use amux_common::domain::{ContentBlock, HistoryItem, StateChangeReason, WorktreeBase};
use croner::Cron;
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::config_store::ConfigStore;
use crate::sessions::{SessionService, SessionWorktree};
use crate::store::{AutomationRow, Store};
use crate::timestamps::now_ms;
use crate::workflows::WorkflowService;

/// 定时检查周期；cron 最小粒度为分钟，检查周期只需小于一分钟。
const SCHEDULER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

enum RunTarget {
    Session(String),
    Workflow(String),
}

pub struct AutomationService {
    store: Arc<Store>,
    sessions: Arc<SessionService>,
    workflows: Arc<WorkflowService>,
    config: Arc<ConfigStore>,
    last_schedule_check: Mutex<u64>,
}

impl AutomationService {
    pub fn new(
        store: Arc<Store>,
        sessions: Arc<SessionService>,
        workflows: Arc<WorkflowService>,
        config: Arc<ConfigStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            sessions,
            workflows,
            config,
            last_schedule_check: Mutex::new(now_ms()),
        })
    }

    /// 启动定时调度器。服务与 Server 同生命周期。
    pub fn start(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SCHEDULER_INTERVAL).await;
                service.trigger_due_schedules().await;
                service.reconcile_runs().await;
            }
        });
    }

    pub fn list(&self) -> Vec<Automation> {
        self.store
            .automations()
            .into_iter()
            .map(|row| self.view(row))
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Automation, String> {
        let row = self
            .store
            .automation(id)
            .ok_or("自动化不存在".to_string())?;
        Ok(self.view(row))
    }

    pub fn create(&self, input: AutomationInput) -> Result<Automation, String> {
        let input = normalize_input(input)?;
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.store.insert_automation(&id, &input, now, now);
        log::info!("自动化已创建: automation={id}");
        self.get(&id)
    }

    pub fn update(&self, id: &str, input: AutomationInput) -> Result<Automation, String> {
        let input = normalize_input(input)?;
        if !self.store.update_automation(id, &input, now_ms()) {
            return Err("自动化不存在".to_string());
        }
        log::info!("自动化已更新: automation={id}");
        self.get(id)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        if !self.store.delete_automation(id) {
            return Err("自动化不存在".to_string());
        }
        log::info!("自动化已删除: automation={id}");
        Ok(())
    }

    pub fn runs_page(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<AutomationRun>, bool), String> {
        self.store
            .automation(id)
            .ok_or("自动化不存在".to_string())?;
        Ok(self.store.automation_runs_page(id, limit, offset))
    }

    /// 手动或触发器发起一次运行。
    pub async fn trigger(self: &Arc<Self>, id: &str) -> Result<AutomationRun, String> {
        let automation = self.get(id)?;
        let run_id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.store.insert_automation_run(&AutomationRun {
            id: run_id.clone(),
            automation_id: automation.id.clone(),
            session_id: None,
            workflow_id: None,
            state: AutomationRunState::Queued,
            result_label: None,
            error: None,
            created_at: now,
            started_at: None,
            finished_at: None,
        });

        let prompt = || {
            vec![ContentBlock::Text(TextContent::new(
                automation.task.clone(),
            ))]
        };
        let target_result: Result<RunTarget, String> = async {
            match &automation.session_options {
                AutomationSessionOptions::ExistingSession { session_id } => {
                    self.sessions.prompt(session_id, prompt()).await?;
                    Ok(RunTarget::Session(session_id.clone()))
                }
                AutomationSessionOptions::ExistingWorkflow { workflow_id } => {
                    self.workflows.prompt(workflow_id, prompt()).await?;
                    Ok(RunTarget::Workflow(workflow_id.clone()))
                }
                AutomationSessionOptions::NewSession {
                    machine,
                    agent,
                    workspace,
                    use_worktree,
                    worktree_base,
                    project,
                } => {
                    let worktree = SessionWorktree::new(*use_worktree, worktree_base.as_ref())?;
                    let session = self
                        .sessions
                        .create(crate::sessions::CreateSessionParams {
                            machine,
                            agent,
                            workspace,
                            worktree,
                            project: project.as_deref(),
                        })
                        .await?;
                    if !automation.title.trim().is_empty() {
                        self.store.set_title(&session.id, automation.title.trim());
                    }
                    self.sessions.prompt(&session.id, prompt()).await?;
                    Ok(RunTarget::Session(session.id))
                }
                AutomationSessionOptions::NewWorkflow { plan, project } => {
                    let workflow = self
                        .workflows
                        .create(plan, Some(automation.title.clone()), project.as_deref())
                        .await?;
                    self.workflows.prompt(&workflow.id, prompt()).await?;
                    Ok(RunTarget::Workflow(workflow.id))
                }
            }
        }
        .await;
        let target = match target_result {
            Ok(target) => target,
            Err(error) => {
                self.store.fail_automation_run(&run_id, &error, now_ms());
                return Err(error);
            }
        };
        let started_at = now_ms();
        match &target {
            RunTarget::Session(session_id) => {
                self.store
                    .start_automation_run(&run_id, Some(session_id), None, started_at);
            }
            RunTarget::Workflow(workflow_id) => {
                self.store
                    .start_automation_run(&run_id, None, Some(workflow_id), started_at);
            }
        }
        log::info!("自动化已触发: automation={id} run={run_id}");
        self.store
            .automation_run(&run_id)
            .ok_or_else(|| "运行记录写入失败".to_string())
    }

    /// 周期性回收已完成但未收到状态通知的运行（主要覆盖自主结束的工作流会话）。
    async fn reconcile_runs(self: &Arc<Self>) {
        let now = now_ms();
        for run in self.store.running_automation_runs() {
            if now.saturating_sub(run.started_at.unwrap_or(run.created_at)) < 30_000 {
                continue;
            }
            let idle_session = run.session_id.as_deref().is_some_and(|session_id| {
                self.store
                    .session(session_id)
                    .is_some_and(|session| session.state == amux_common::domain::SessionState::Idle)
            });
            let idle_workflow = run.workflow_id.as_deref().is_some_and(|workflow_id| {
                self.store.workflow(workflow_id).is_some_and(|workflow| {
                    workflow.state == amux_common::domain::SessionState::Idle
                })
            });
            if idle_session || idle_workflow {
                self.finish_run(&run).await;
            }
        }
    }

    /// 普通会话结束时的主动回收。
    pub fn on_session_idle(self: &Arc<Self>, session_id: &str, reason: StateChangeReason) {
        let Some(run) = self.store.running_automation_run_for_session(session_id) else {
            return;
        };
        let service = Arc::clone(self);
        tokio::spawn(async move {
            if reason == StateChangeReason::Aborted {
                service
                    .store
                    .fail_automation_run(&run.id, "会话异常终止", now_ms());
                return;
            }
            service.finish_run(&run).await;
        });
    }

    /// 工作流会话结束时的主动回收。
    pub fn on_workflow_idle(self: &Arc<Self>, workflow_id: &str) {
        let Some(run) = self.store.running_automation_run_for_workflow(workflow_id) else {
            return;
        };
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.finish_run(&run).await;
        });
    }

    async fn finish_run(&self, run: &AutomationRun) {
        let Some(automation) = self.store.automation(&run.automation_id) else {
            self.store
                .fail_automation_run(&run.id, "自动化不存在", now_ms());
            return;
        };
        let finished_at = now_ms();
        self.store.finish_automation_run(&run.id, finished_at);
        let started_at = run.started_at.unwrap_or(run.created_at);
        let context = match (&run.session_id, &run.workflow_id) {
            (Some(session_id), _) => session_context(
                &self.sessions,
                session_id,
                &automation.task,
                started_at,
                finished_at,
            ),
            (_, Some(workflow_id)) => workflow_context(
                &self.workflows,
                workflow_id,
                &automation.task,
                started_at,
                finished_at,
            ),
            _ => Err("自动化运行缺少会话".into()),
        };
        match context {
            Ok(context) => match self.classify(&automation.result_labels, &context).await {
                Ok(label) => {
                    self.store
                        .set_automation_run_label(&run.id, Some(&label), None);
                }
                Err(error) => {
                    log::warn!("自动化结果分类失败: run={}: {error}", run.id);
                    self.store
                        .set_automation_run_label(&run.id, None, Some(&error));
                }
            },
            Err(error) => {
                self.store
                    .set_automation_run_label(&run.id, None, Some(&error));
            }
        }
    }

    async fn classify(
        &self,
        labels: &[AutomationResultLabel],
        context: &str,
    ) -> Result<String, String> {
        let config = self
            .config
            .automation()
            .ok_or("自动化决策模型未配置，无法进行结果分类")?;
        crate::system_one::evaluate_choice(&config, labels, context).await
    }

    fn view(&self, row: AutomationRow) -> Automation {
        let next_run_at = match (&row.trigger, row.enabled) {
            (AutomationTrigger::Schedule { cron }, true) => next_occurrence_ms(cron, now_ms()).ok(),
            _ => None,
        };
        Automation {
            id: row.id.clone(),
            title: row.title,
            enabled: row.enabled,
            task: row.task,
            trigger: row.trigger,
            session_options: row.session_options,
            result_labels: row.result_labels,
            created_at: row.created_at,
            updated_at: row.updated_at,
            next_run_at,
            last_run: self.store.latest_automation_run(&row.id),
        }
    }

    async fn trigger_due_schedules(self: &Arc<Self>) {
        let now = now_ms();
        let from = {
            let mut last = self.last_schedule_check.lock();
            let from = *last;
            *last = now;
            from
        };
        let due: Vec<String> = self
            .store
            .automations()
            .into_iter()
            .filter(|automation| automation.enabled)
            .filter(|automation| {
                let AutomationTrigger::Schedule { cron } = &automation.trigger else {
                    return false;
                };
                let Ok(cron) = Cron::from_str(cron) else {
                    return false;
                };
                cron.find_next_occurrence(&zoned(from), false)
                    .is_ok_and(|next| next.timestamp().as_millisecond() as u64 <= now)
            })
            .map(|automation| automation.id)
            .collect();
        for id in &due {
            if let Err(error) = self.trigger(id).await {
                log::warn!("定时触发自动化失败: automation={id}: {error}");
            }
        }
    }

    /// GitHub Webhook 统一入口：匹配仓库与事件后触发所有命中的自动化。
    pub async fn github_webhook(
        self: &Arc<Self>,
        event: &str,
        body: &Value,
    ) -> Result<usize, String> {
        let repository = body
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            .ok_or("Webhook 缺少 repository.full_name".to_string())?;
        let event = event.trim();
        if event.is_empty() {
            return Err("缺少 X-GitHub-Event 请求头".into());
        }
        let matches: Vec<String> = self
            .store
            .automations()
            .into_iter()
            .filter(|automation| automation.enabled)
            .filter(|automation| {
                matches!(
                    &automation.trigger,
                    AutomationTrigger::GithubWebhook {
                        repository: configured,
                        events,
                    } if configured == repository
                        && (events.is_empty() || events.iter().any(|item| item == event))
                )
            })
            .map(|automation| automation.id)
            .collect();
        for id in &matches {
            if let Err(error) = self.trigger(id).await {
                log::warn!("GitHub Webhook 触发自动化失败: automation={id}: {error}");
            }
        }
        Ok(matches.len())
    }
}

fn normalize_input(mut input: AutomationInput) -> Result<AutomationInput, String> {
    input.title = input.title.trim().to_string();
    input.task = input.task.trim().to_string();
    input.result_labels = normalize_labels(input.result_labels)?;
    normalize_trigger(&mut input.trigger);
    normalize_session_options(&mut input.session_options);

    if input.title.is_empty() {
        return Err("标题不能为空".into());
    }
    if input.task.is_empty() {
        return Err("任务不能为空".into());
    }
    match &input.session_options {
        AutomationSessionOptions::ExistingSession { session_id } if session_id.is_empty() => {
            return Err("已有普通会话不能为空".into());
        }
        AutomationSessionOptions::ExistingWorkflow { workflow_id } if workflow_id.is_empty() => {
            return Err("已有工作流会话不能为空".into());
        }
        AutomationSessionOptions::NewSession {
            machine,
            agent,
            workspace,
            ..
        } if machine.is_empty() || agent.is_empty() || workspace.is_empty() => {
            return Err("机器、智能体和工作目录不能为空".into());
        }
        AutomationSessionOptions::NewSession {
            use_worktree: true,
            worktree_base: None,
            ..
        } => return Err("开启 worktree 时必须指定基分支".into()),
        AutomationSessionOptions::NewWorkflow { plan, .. } if plan.is_empty() => {
            return Err("工作流计划不能为空".into());
        }
        _ => {}
    }
    match &input.trigger {
        AutomationTrigger::Schedule { cron } => {
            Cron::from_str(cron).map_err(|error| format!("cron 表达式无效: {error}"))?;
        }
        AutomationTrigger::GithubWebhook { repository, events } => {
            if repository.is_empty() {
                return Err("GitHub 仓库不能为空".into());
            }
            if events.iter().any(|event| event.is_empty()) {
                return Err("GitHub 事件类型不能为空".into());
            }
        }
    }
    Ok(input)
}

fn normalize_trigger(trigger: &mut AutomationTrigger) {
    match trigger {
        AutomationTrigger::Schedule { cron } => *cron = cron.trim().to_string(),
        AutomationTrigger::GithubWebhook { repository, events } => {
            *repository = repository.trim().to_string();
            events.retain(|event| !event.trim().is_empty());
            for event in events.iter_mut() {
                *event = event.trim().to_ascii_lowercase();
            }
            events.sort();
            events.dedup();
        }
    }
}

fn normalize_session_options(options: &mut AutomationSessionOptions) {
    match options {
        AutomationSessionOptions::ExistingSession { session_id } => {
            *session_id = session_id.trim().to_string();
        }
        AutomationSessionOptions::ExistingWorkflow { workflow_id } => {
            *workflow_id = workflow_id.trim().to_string();
        }
        AutomationSessionOptions::NewSession {
            machine,
            agent,
            workspace,
            worktree_base,
            project,
            ..
        } => {
            *machine = machine.trim().to_string();
            *agent = agent.trim().to_string();
            *workspace = workspace.trim().to_string();
            if let Some(base) = worktree_base {
                match base {
                    WorktreeBase::Local { name } => {
                        *name = name.trim().to_string();
                        if name.is_empty() {
                            *worktree_base = None;
                        }
                    }
                    WorktreeBase::Remote { remote, branch } => {
                        *remote = remote.trim().to_string();
                        *branch = branch.trim().to_string();
                        if remote.is_empty() || branch.is_empty() {
                            *worktree_base = None;
                        }
                    }
                }
            }
            *project = project
                .take()
                .map(|project| project.trim().to_string())
                .filter(|project| !project.is_empty());
        }
        AutomationSessionOptions::NewWorkflow { plan, project } => {
            *plan = plan.trim().to_string();
            *project = project
                .take()
                .map(|project| project.trim().to_string())
                .filter(|project| !project.is_empty());
        }
    }
}

fn normalize_labels(
    labels: Vec<AutomationResultLabel>,
) -> Result<Vec<AutomationResultLabel>, String> {
    let mut normalized = Vec::new();
    for label in labels {
        let name = label.name.trim().to_string();
        let description = label.description.trim().to_string();
        if name.is_empty() || description.is_empty() {
            return Err("结果分类的标签名和描述不能为空".into());
        }
        if normalized
            .iter()
            .any(|item: &AutomationResultLabel| item.name == name)
        {
            return Err(format!("结果分类重复: {name}"));
        }
        normalized.push(AutomationResultLabel { name, description });
    }
    if normalized.len() < 2 {
        return Err("结果分类至少需要两个".into());
    }
    Ok(normalized)
}

fn next_occurrence_ms(cron: &str, from_ms: u64) -> Result<u64, String> {
    let cron = Cron::from_str(cron).map_err(|error| format!("cron 表达式无效: {error}"))?;
    let next = cron
        .find_next_occurrence(&zoned(from_ms), false)
        .map_err(|error| format!("计算下一次运行时间失败: {error}"))?;
    Ok(next.timestamp().as_millisecond() as u64)
}

fn zoned(timestamp_ms: u64) -> jiff::Zoned {
    jiff::Timestamp::from_millisecond(timestamp_ms as i64)
        .expect("毫秒时间戳必须在 jiff 支持范围内")
        .to_zoned(jiff::tz::TimeZone::system())
}

fn session_context(
    sessions: &SessionService,
    session_id: &str,
    task: &str,
    started_at: u64,
    finished_at: u64,
) -> Result<String, String> {
    format_context(
        task,
        collect_history(|limit, offset| sessions.history(session_id, limit, offset)),
        started_at,
        finished_at,
    )
}

fn workflow_context(
    workflows: &WorkflowService,
    workflow_id: &str,
    task: &str,
    started_at: u64,
    finished_at: u64,
) -> Result<String, String> {
    format_context(
        task,
        collect_history(|limit, offset| workflows.history(workflow_id, limit, offset)),
        started_at,
        finished_at,
    )
}

/// 历史接口按最新优先分页，每页内部再按时间正序返回；按页前插可还原完整时序。
fn collect_history(
    mut page: impl FnMut(usize, usize) -> (Vec<HistoryItem>, bool),
) -> Vec<HistoryItem> {
    const PAGE_SIZE: usize = 100;
    let mut offset = 0;
    let mut history = Vec::new();
    loop {
        let (mut items, has_more) = page(PAGE_SIZE, offset);
        offset += items.len();
        if items.is_empty() {
            break;
        }
        items.append(&mut history);
        history = items;
        if !has_more {
            break;
        }
    }
    history
}

fn format_context(
    task: &str,
    history: Vec<HistoryItem>,
    started_at: u64,
    finished_at: u64,
) -> Result<String, String> {
    let mut lines = Vec::new();
    let mut has_agent_message = false;
    for item in history
        .into_iter()
        .filter(|item| (started_at..=finished_at).contains(&item.timestamp()))
    {
        match item {
            HistoryItem::UserMessage { content, .. } => {
                lines.push(format!("user: {}", blocks_text(&content)));
            }
            HistoryItem::AgentMessage { content, .. } => {
                has_agent_message = true;
                lines.push(format!("agent: {}", blocks_text(&content)));
            }
        }
    }
    if !has_agent_message {
        return Err("Agent 无任何输出".into());
    }
    Ok(format!(
        "智能体正在执行一个自动化任务，任务内容是：{task}。\n\n以下是智能体会话消息：\n{}",
        lines.join("\n")
    ))
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, text: &str, timestamp: u64) -> HistoryItem {
        let content = vec![ContentBlock::Text(TextContent::new(text))];
        match role {
            "user" => HistoryItem::UserMessage {
                id: format!("user-{timestamp}"),
                content,
                timestamp,
            },
            "agent" => HistoryItem::AgentMessage {
                id: format!("agent-{timestamp}"),
                content,
                timestamp,
            },
            _ => panic!("未知角色"),
        }
    }

    fn labels() -> Vec<AutomationResultLabel> {
        vec![
            AutomationResultLabel {
                name: "通过".into(),
                description: "任务成功完成".into(),
            },
            AutomationResultLabel {
                name: "失败".into(),
                description: "任务未成功完成".into(),
            },
        ]
    }

    fn input() -> AutomationInput {
        AutomationInput {
            title: " 每日任务 ".into(),
            enabled: true,
            task: " 执行任务 ".into(),
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
            },
            session_options: AutomationSessionOptions::NewSession {
                machine: " pc ".into(),
                agent: " codex ".into(),
                workspace: " /repo ".into(),
                use_worktree: false,
                worktree_base: Some(WorktreeBase::Local { name: " ".into() }),
                project: Some(" p ".into()),
            },
            result_labels: labels(),
        }
    }

    #[test]
    fn validates_and_normalizes_input() {
        let input = normalize_input(input()).unwrap();
        assert_eq!(input.title, "每日任务");
        let AutomationSessionOptions::NewSession {
            machine,
            worktree_base,
            project,
            ..
        } = input.session_options
        else {
            panic!("应为新建普通会话");
        };
        assert_eq!(machine, "pc");
        assert_eq!(worktree_base, None);
        assert_eq!(project.as_deref(), Some("p"));
    }

    #[test]
    fn worktree_automation_requires_base_branch() {
        let mut input = input();
        let AutomationSessionOptions::NewSession {
            use_worktree,
            worktree_base,
            ..
        } = &mut input.session_options
        else {
            panic!("应为新建普通会话");
        };
        *use_worktree = true;
        *worktree_base = None;

        assert_eq!(
            normalize_input(input).unwrap_err(),
            "开启 worktree 时必须指定基分支"
        );
    }

    #[test]
    fn requires_two_result_labels_with_descriptions() {
        let mut one = input();
        one.result_labels.truncate(1);
        assert!(normalize_input(one).is_err());

        let mut missing_description = input();
        missing_description.result_labels[0].description = " ".into();
        assert!(normalize_input(missing_description).is_err());
    }

    #[test]
    fn rejects_invalid_cron() {
        let mut input = input();
        input.trigger = AutomationTrigger::Schedule {
            cron: "not a cron".into(),
        };
        assert!(normalize_input(input).is_err());
    }

    #[test]
    fn computes_next_occurrence_once() {
        let from = 1_725_800_000_000u64;
        let next = next_occurrence_ms("0 0 * * *", from).unwrap();
        assert!(next > from);
        assert!(next - from <= 24 * 60 * 60 * 1000);
    }

    #[test]
    fn classification_context_uses_only_run_conversation() {
        let context = format_context(
            "检查构建",
            vec![
                message("user", "旧任务", 1),
                message("user", "开始构建", 10),
                message("agent", "构建通过", 11),
                message("agent", "下一次任务", 20),
            ],
            10,
            11,
        )
        .unwrap();

        assert_eq!(
            context,
            "智能体正在执行一个自动化任务，任务内容是：检查构建。\n\n以下是智能体会话消息：\nuser: 开始构建\nagent: 构建通过"
        );
    }

    #[test]
    fn classification_context_rejects_run_without_agent_output() {
        let error =
            format_context("检查构建", vec![message("user", "开始构建", 10)], 10, 10).unwrap_err();
        assert_eq!(error, "Agent 无任何输出");
    }

    #[test]
    fn collection_reassembles_latest_first_pages_in_time_order() {
        let pages = [
            (
                vec![message("agent", "第三条", 3), message("agent", "第四条", 4)],
                true,
            ),
            (
                vec![message("user", "第一条", 1), message("agent", "第二条", 2)],
                false,
            ),
        ];
        let mut index = 0;
        let history = collect_history(|_, _| {
            let page = pages[index].clone();
            index += 1;
            page
        });

        assert_eq!(
            history
                .iter()
                .map(HistoryItem::timestamp)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
    }
}
