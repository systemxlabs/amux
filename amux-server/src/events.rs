//! ACP 事件消费：把 Agent 通知落到会话与工作流（`SessionService::apply` + 工作流驱动）。

use std::sync::Arc;

use amux_common::domain::SessionState;
use tokio::sync::mpsc;

use crate::acp::AcpEvent;
use crate::sessions::SessionService;
use crate::store::Store;
use crate::workflows::WorkflowService;

pub async fn run(
    mut events: mpsc::Receiver<AcpEvent>,
    sessions: Arc<SessionService>,
    workflows: Arc<WorkflowService>,
    store: Arc<Store>,
) {
    while let Some(event) = events.recv().await {
        let applied = sessions.apply(event);
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
        }
        if let Some(workflow_id) = store.workflow_of_session(&applied.session_id) {
            workflows.recompute_state(&workflow_id);
        }
    }
}
