//! ACP 事件消费：把 Agent 通知落到会话与工作流（`SessionService::apply` + 工作流驱动）。

use std::sync::Arc;

use amux_common::domain::SessionState;
use tokio::sync::mpsc;

use crate::acp::AcpEvent;
use crate::automations::AutomationService;
use crate::groups::GroupService;
use crate::sessions::SessionService;
use crate::store::Store;
use crate::workflows::WorkflowService;

pub async fn run(
    mut events: mpsc::Receiver<AcpEvent>,
    sessions: Arc<SessionService>,
    workflows: Arc<WorkflowService>,
    groups: Arc<GroupService>,
    automations: Arc<AutomationService>,
    store: Arc<Store>,
) {
    while let Some(event) = events.recv().await {
        // 群聊成员输出需要引用普通会话消息 ID，apply 前先取出事件内容。
        let agent_message = match &event {
            AcpEvent::Message {
                agent_session_id,
                message_id,
                content,
            } => Some((
                agent_session_id.clone(),
                message_id.clone(),
                content.clone(),
            )),
            _ => None,
        };
        let applied = sessions.apply(event);
        if let Some((agent_session_id, message_id, content)) = agent_message {
            if let Some(session_id) = sessions.session_id_of_agent(&agent_session_id) {
                groups.on_member_output(&session_id, &message_id, &content);
            }
        }
        let Some(applied) = applied else { continue };
        let Some(session) = store.session(&applied.session_id) else {
            continue;
        };
        // 工作流驱动：非取消地回到空闲时注入驱动消息
        if applied.old_state == SessionState::Busy && applied.new_state == SessionState::Idle {
            workflows.on_linked_idle(
                &applied.session_id,
                &session.machine,
                applied.old_state,
                applied.new_state,
                applied.reason,
            );
            groups
                .on_member_idle(&applied.session_id, applied.reason)
                .await;
            automations.on_session_idle(&applied.session_id, applied.reason);
        }
        if let Some(workflow_id) = store.workflow_of_session(&applied.session_id) {
            workflows.recompute_state(&workflow_id);
            if store
                .workflow(&workflow_id)
                .is_some_and(|workflow| workflow.state == SessionState::Idle)
            {
                automations.on_workflow_idle(&workflow_id);
            }
        }
        if let Some(group_id) = store.group_of_session(&applied.session_id) {
            groups.recompute_state(&group_id);
            if store
                .group(&group_id)
                .is_some_and(|group| group.state == SessionState::Idle)
            {
                automations.on_group_idle(&group_id);
            }
        }
    }
}
