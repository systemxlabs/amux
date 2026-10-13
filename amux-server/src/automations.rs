//! 自动化：定时/Webhook 触发、运行会话与决策模型结果分类。
//!
//! 持久化布局见 docs/DESIGN.md「自动化存储」。

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use agent_client_protocol::schema::v2::TextContent;
use amux_common::api::{
    Automation, AutomationApiFormat, AutomationConfig, AutomationInput, AutomationResultLabel,
    AutomationRun, AutomationRunState, AutomationSessionOptions, AutomationTrigger,
};
use amux_common::domain::{Activity, ContentBlock, HistoryItem, StateChangeReason};
use croner::Cron;
use parking_lot::Mutex;
use rig_typesafeai::types::{Answer, Question};
use rig_typesafeai::{DynamicQuery, Evaluate, JevConfig};
use serde_json::Value;
use uuid::Uuid;

use crate::config_store::ConfigStore;
use crate::sessions::SessionService;
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

    pub fn unassign_project(&self, project: &str) {
        self.store.unassign_automation_project(project);
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
                    worktree_base_branch,
                } => {
                    let session = self
                        .sessions
                        .create(crate::sessions::CreateSessionParams {
                            machine,
                            agent,
                            workspace,
                            use_worktree: *use_worktree,
                            worktree_base_branch: worktree_base_branch.as_deref(),
                            project: automation.project.as_deref(),
                        })
                        .await?;
                    if !automation.title.trim().is_empty() {
                        self.store.set_title(&session.id, automation.title.trim());
                    }
                    self.sessions.prompt(&session.id, prompt()).await?;
                    Ok(RunTarget::Session(session.id))
                }
                AutomationSessionOptions::NewWorkflow { plan } => {
                    let workflow = self
                        .workflows
                        .create(
                            plan,
                            Some(automation.title.clone()),
                            automation.project.as_deref(),
                        )
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
            (Some(session_id), _) => {
                session_context(&self.sessions, session_id, started_at, finished_at)
            }
            (_, Some(workflow_id)) => {
                workflow_context(&self.workflows, workflow_id, started_at, finished_at)
            }
            _ => None,
        };
        match context {
            Some(context) => match self.classify(&automation.result_labels, &context).await {
                Ok(label) => {
                    self.store
                        .set_automation_run_label(&run.id, label.as_deref(), None);
                }
                Err(error) => {
                    log::warn!("自动化结果分类失败: run={}: {error}", run.id);
                    self.store
                        .set_automation_run_label(&run.id, None, Some(&error));
                }
            },
            None => {
                self.store.set_automation_run_label(
                    &run.id,
                    None,
                    Some("会话没有可分类的历史或活动"),
                );
            }
        }
    }

    async fn classify(
        &self,
        labels: &[AutomationResultLabel],
        context: &str,
    ) -> Result<Option<String>, String> {
        let config = self
            .config
            .automation()
            .ok_or("自动化决策模型未配置，无法进行结果分类")?;
        let label = call_decision_model(&config, labels, truncate(context, 16_000)).await?;
        Ok(Some(label))
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
            project: row.project,
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
        AutomationSessionOptions::NewWorkflow { plan } if plan.is_empty() => {
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
            worktree_base_branch,
            ..
        } => {
            *machine = machine.trim().to_string();
            *agent = agent.trim().to_string();
            *workspace = workspace.trim().to_string();
            *worktree_base_branch = worktree_base_branch
                .take()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty());
        }
        AutomationSessionOptions::NewWorkflow { plan } => {
            *plan = plan.trim().to_string();
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
    started_at: u64,
    finished_at: u64,
) -> Option<String> {
    let (history, _) = sessions.history(session_id, 50, 0);
    let (activities, _) = sessions.activities(session_id, 50, 0);
    format_context(history, activities, started_at, finished_at)
}

fn workflow_context(
    workflows: &WorkflowService,
    workflow_id: &str,
    started_at: u64,
    finished_at: u64,
) -> Option<String> {
    let (history, _) = workflows.history(workflow_id, 50, 0);
    let (activities, _) = workflows.activities(workflow_id, 50, 0);
    format_context(history, activities, started_at, finished_at)
}

fn format_context(
    history: Vec<HistoryItem>,
    activities: Vec<Activity>,
    started_at: u64,
    finished_at: u64,
) -> Option<String> {
    let mut lines = Vec::new();
    for item in history
        .into_iter()
        .filter(|item| (started_at..=finished_at).contains(&item.timestamp()))
    {
        match item {
            HistoryItem::UserMessage {
                content, timestamp, ..
            } => lines.push(format!("[{timestamp}] 用户：{}", blocks_text(&content))),
            HistoryItem::AgentMessage {
                content, timestamp, ..
            } => lines.push(format!("[{timestamp}] 智能体：{}", blocks_text(&content))),
        }
    }
    for activity in activities
        .into_iter()
        .filter(|activity| (started_at..=finished_at).contains(&activity.timestamp()))
    {
        lines.push(format!(
            "[{}] {}",
            activity.timestamp(),
            activity_text(&activity)
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
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

fn activity_text(activity: &Activity) -> String {
    match activity {
        Activity::Thinking { thinking, .. } => format!("思考 {thinking}"),
        Activity::ToolCall {
            tool_name,
            title,
            parameters,
            ..
        } => format!(
            "工具调用 {tool_name} {}{}",
            title.as_deref().unwrap_or_default(),
            parameters
                .as_deref()
                .map(|parameters| format!("\n{parameters}"))
                .unwrap_or_default()
        ),
        Activity::Compaction {
            status, summary, ..
        } => format!("上下文压缩 {status} {summary}"),
        Activity::Notice {
            severity,
            title,
            description,
            ..
        } => format!(
            "通知 {severity} {title}{}",
            description
                .as_deref()
                .map(|description| format!("\n{description}"))
                .unwrap_or_default()
        ),
        Activity::Error { error, .. } => format!("错误 {error}"),
    }
}

async fn call_decision_model(
    config: &AutomationConfig,
    labels: &[AutomationResultLabel],
    context: &str,
) -> Result<String, String> {
    match config.api_format {
        AutomationApiFormat::Jev => {}
    }
    let criteria = labels
        .iter()
        .map(|label| {
            (
                label.name.clone(),
                Some(Value::String(label.description.clone())),
            )
        })
        .collect();
    let question = Question::Choice {
        instructions: Value::String("选择最符合任务运行结果的分类".into()),
        criteria,
    };
    let mut definitions = BTreeMap::new();
    definitions.insert("result".to_string(), question);
    let query =
        DynamicQuery::new(definitions).map_err(|error| format!("构造结果分类问题失败: {error}"))?;
    let model = JevConfig::new(config.api_key.clone())
        .model(config.model.clone())
        .with_endpoint(config.base_url.clone())
        .client()
        .evaluation();
    let state = serde_json::json!({ "context": context });
    let result = model
        .evaluate(&state, query)
        .await
        .map_err(|error| format!("决策模型请求失败: {error}"))?;
    let Some(Answer::Choice { choice, .. }) = result.answers.get("result") else {
        return Err("决策模型没有返回选择结果".into());
    };
    if !labels.iter().any(|label| &label.name == choice) {
        return Err(format!("决策模型返回未知分类: {choice}"));
    }
    Ok(choice.clone())
}

fn truncate(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

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
            project: None,
            trigger: AutomationTrigger::Schedule {
                cron: "0 9 * * *".into(),
            },
            session_options: AutomationSessionOptions::NewSession {
                machine: " pc ".into(),
                agent: " codex ".into(),
                workspace: " /repo ".into(),
                use_worktree: false,
                worktree_base_branch: Some(" ".into()),
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
            worktree_base_branch,
            ..
        } = input.session_options
        else {
            panic!("应为新建普通会话");
        };
        assert_eq!(machine, "pc");
        assert_eq!(worktree_base_branch, None);
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
}
