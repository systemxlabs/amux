//! ACP 事件消费：把 Agent 通知落到会话与工作流（`SessionService::apply` + 工作流驱动）。
//!
//! 事件循环只做状态推进与增量落盘；可能发起 ACP 请求的驱动按聚合分队列串行执行：
//! - 群聊成员会话 → 该群聊的队列
//! - 工作流关联会话 → 该工作流的队列
//! - 其余普通会话 → 各自的会话队列
//!
//! 聚合内部保持顺序，聚合之间并行；队列空闲后自动回收。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use amux_common::domain::SessionState;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::acp::AcpEvent;
use crate::automations::AutomationService;
use crate::db;
use crate::groups::GroupService;
use crate::sessions::{SessionService, StateApplied};
use crate::store::Store;
use crate::workflows::WorkflowService;

/// 单次从事件通道批量取出的上限：把落盘集中在一次阻塞任务里。
const EVENT_BATCH: usize = 256;

/// 队列空闲多久后回收对应 worker；有状态事件时会重新创建。
const FOLLOWUP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 驱动顺序的聚合键。同一键的状态结果严格按事件顺序处理。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FollowUpKey {
    Group(String),
    Workflow(String),
    Session(String),
}

/// 一次驱动任务：状态落定后的驱动，或群聊新消息触发的解析。
enum FollowUp {
    State {
        applied: StateApplied,
        group_id: Option<String>,
        workflow_id: Option<String>,
    },
    GroupParse(String),
}

/// 按聚合拆分的驱动队列。只在事件循环线程投递，worker 独占消费自己的队列。
struct FollowUpQueues {
    workflows: Arc<WorkflowService>,
    groups: Arc<GroupService>,
    automations: Arc<AutomationService>,
    store: Arc<Store>,
    queues: Arc<Mutex<HashMap<FollowUpKey, mpsc::UnboundedSender<FollowUp>>>>,
}

impl FollowUpQueues {
    fn new(
        workflows: Arc<WorkflowService>,
        groups: Arc<GroupService>,
        automations: Arc<AutomationService>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            workflows,
            groups,
            automations,
            store,
            queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 投递到聚合队列。发送与「空闲回收」在同一把锁下，避免消息落入即将退出的队列。
    fn dispatch(&self, job: FollowUp) {
        let key = follow_up_key(&job);
        let mut job = job;
        loop {
            let mut queues = self.queues.lock();
            if let Some(sender) = queues.get(&key) {
                match sender.send(job) {
                    Ok(()) => return,
                    Err(error) => {
                        // worker 已异常退出：丢弃失效发送端，重建队列后重投。
                        job = error.0;
                        queues.remove(&key);
                    }
                }
            }
            let (sender, receiver) = mpsc::unbounded_channel();
            queues.insert(key.clone(), sender.clone());
            self.spawn_worker(key.clone(), receiver);
            if let Err(error) = sender.send(job) {
                job = error.0;
                queues.remove(&key);
            } else {
                return;
            }
        }
    }

    fn spawn_worker(&self, key: FollowUpKey, mut receiver: mpsc::UnboundedReceiver<FollowUp>) {
        let queues = Arc::clone(&self.queues);
        let workflows = Arc::clone(&self.workflows);
        let groups = Arc::clone(&self.groups);
        let automations = Arc::clone(&self.automations);
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(FOLLOWUP_IDLE_TIMEOUT, receiver.recv()).await {
                    Ok(Some(job)) => {
                        handle_follow_up(job, &workflows, &groups, &automations, &store).await;
                    }
                    // 发送端全部断开（通常是 Server 关闭）。
                    Ok(None) => break,
                    Err(_) => {
                        // 与 dispatch 共用锁：确认队列为空后再摘除，之后不会再有人投递到
                        // 这个 channel（新的投递会看到空槽并重建队列）。
                        let mut inner = queues.lock();
                        if receiver.is_empty() {
                            inner.remove(&key);
                            return;
                        }
                    }
                }
            }
            queues.lock().remove(&key);
        });
    }
}

fn follow_up_key(job: &FollowUp) -> FollowUpKey {
    match job {
        FollowUp::GroupParse(group_id) => FollowUpKey::Group(group_id.clone()),
        FollowUp::State {
            applied,
            group_id,
            workflow_id,
        } => match (group_id, workflow_id) {
            (Some(group_id), Some(workflow_id)) => {
                log::warn!(
                    "会话同时属于群聊和工作流，按群聊队列串行: session={} group={group_id} workflow={workflow_id}",
                    applied.session_id
                );
                FollowUpKey::Group(group_id.clone())
            }
            (Some(group_id), None) => FollowUpKey::Group(group_id.clone()),
            (None, Some(workflow_id)) => FollowUpKey::Workflow(workflow_id.clone()),
            (None, None) => FollowUpKey::Session(applied.session_id.clone()),
        },
    }
}

pub async fn run(
    mut events: mpsc::UnboundedReceiver<AcpEvent>,
    sessions: Arc<SessionService>,
    workflows: Arc<WorkflowService>,
    groups: Arc<GroupService>,
    automations: Arc<AutomationService>,
    store: Arc<Store>,
) {
    let followups = FollowUpQueues::new(
        Arc::clone(&workflows),
        Arc::clone(&groups),
        Arc::clone(&automations),
        Arc::clone(&store),
    );

    let mut batch = Vec::with_capacity(EVENT_BATCH);
    loop {
        batch.clear();
        let count = events.recv_many(&mut batch, EVENT_BATCH).await;
        if count == 0 {
            break;
        }
        let events = std::mem::take(&mut batch);
        let sessions = Arc::clone(&sessions);
        let groups = Arc::clone(&groups);
        let store = Arc::clone(&store);
        let jobs = db::blocking(move || {
            let mut jobs = Vec::new();
            let mut parse_groups = HashSet::new();
            for event in events {
                // 群聊成员输出需要引用普通会话消息 ID，apply 前先取出事件内容。
                let agent_message = match &event {
                    AcpEvent::Message {
                        agent_session_id,
                        message_id,
                        ..
                    } => Some((agent_session_id.clone(), message_id.clone())),
                    _ => None,
                };
                let state = sessions.apply(event);
                if let Some((agent_session_id, message_id)) = agent_message {
                    if let Some(session_id) = sessions.session_id_of_agent(&agent_session_id) {
                        if let Some(group_id) = groups.on_member_output(&session_id, &message_id) {
                            parse_groups.insert(group_id);
                        }
                    }
                }
                if let Some(applied) = state {
                    let group_id = store.group_of_session(&applied.session_id);
                    let workflow_id = store.workflow_of_session(&applied.session_id);
                    jobs.push(FollowUp::State {
                        applied,
                        group_id,
                        workflow_id,
                    });
                }
            }
            for group_id in parse_groups {
                jobs.push(FollowUp::GroupParse(group_id));
            }
            jobs
        })
        .await;
        match jobs {
            Ok(jobs) => {
                for job in jobs {
                    followups.dispatch(job);
                }
            }
            Err(error) => log::warn!("ACP 事件落盘失败: {error}"),
        }
        batch = Vec::with_capacity(EVENT_BATCH);
    }

    // 事件通道关闭：让所有队列在空闲超时后自行回收，不阻塞 Server 关闭。
}

/// 驱动任务的处理逻辑。同一聚合的 job 由同一个 worker 串行消费。
async fn handle_follow_up(
    job: FollowUp,
    workflows: &Arc<WorkflowService>,
    groups: &Arc<GroupService>,
    automations: &Arc<AutomationService>,
    store: &Arc<Store>,
) {
    let (applied, group_id, workflow_id) = match job {
        FollowUp::GroupParse(group_id) => {
            groups.parse(&group_id).await;
            return;
        }
        FollowUp::State {
            applied,
            group_id,
            workflow_id,
        } => (applied, group_id, workflow_id),
    };
    let session_id = applied.session_id.clone();
    let session = {
        let store = Arc::clone(store);
        let id = session_id.clone();
        match db::blocking(move || store.session(&id)).await {
            Ok(session) => session,
            Err(error) => {
                log::warn!("读取会话失败（{session_id}）: {error}");
                None
            }
        }
    };
    let Some(session) = session else { return };

    // 工作流驱动：非取消地回到空闲时注入驱动消息
    if applied.old_state == SessionState::Busy && applied.new_state == SessionState::Idle {
        let workflows = Arc::clone(workflows);
        let id = session_id.clone();
        let machine = session.machine.clone();
        let old_state = applied.old_state;
        let new_state = applied.new_state;
        let reason = applied.reason;
        if let Err(error) = db::blocking(move || {
            workflows.on_linked_idle(&id, &machine, old_state, new_state, reason);
        })
        .await
        {
            log::warn!("工作流状态驱动失败（{session_id}）: {error}");
        }
        let automations = Arc::clone(automations);
        let id = session_id.clone();
        let reason = applied.reason;
        if let Err(error) = db::blocking(move || automations.on_session_idle(&id, reason)).await {
            log::warn!("自动化会话状态处理失败（{session_id}）: {error}");
        }
    }

    // 群聊驱动：成员会话状态变更后重算群聊状态并解析未解析消息
    if applied.old_state != applied.new_state {
        if let Some(group_id) = &group_id {
            groups.on_member_state_change(group_id).await;
        }
    }

    if let Some(workflow_id) = workflow_id {
        let service = Arc::clone(workflows);
        let id = workflow_id.clone();
        let _ = db::blocking(move || service.recompute_state(&id)).await;
        let store = Arc::clone(store);
        let id = workflow_id.clone();
        let idle = db::blocking(move || {
            store
                .workflow(&id)
                .is_some_and(|workflow| workflow.state == SessionState::Idle)
        })
        .await
        .unwrap_or(false);
        if idle {
            let automations = Arc::clone(automations);
            let _ = db::blocking(move || automations.on_workflow_idle(&workflow_id)).await;
        }
    }

    if let Some(group_id) = group_id {
        let service = Arc::clone(groups);
        let id = group_id.clone();
        let _ = db::blocking(move || service.recompute_state(&id)).await;
        let store = Arc::clone(store);
        let id = group_id.clone();
        let idle = db::blocking(move || {
            store
                .group(&id)
                .is_some_and(|group| group.state == SessionState::Idle)
        })
        .await
        .unwrap_or(false);
        if idle {
            let automations = Arc::clone(automations);
            let _ = db::blocking(move || automations.on_group_idle(&group_id)).await;
        }
    }
}
