//! 关联普通会话完成（busy→idle）发生在编排 turn 进行中时的回归测试：
//! 系统必须立即向工作流会话注入用户消息，并在当前 turn 结束后补跑一轮
//! 处理该消息。用真实 AmuxApp 验证完整链路：
//!
//! 真实流程中 `prompt_session` 工具阻塞等待关联普通会话结束，busy→idle 事件必然
//! 在 gate 运行中到达；此前关联普通会话要等整轮 decide 结束才挂载进 `linked_sessions`，
//! 事件到达时工作流查找失败、注入被丢弃。

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use amux_desktop::workflow::{
    AgentSlot, LinkedSession, MachineHub, MachineSummary, OrcBackend, OrcContext, OrcMsg,
    WorkflowEngine,
};
use protocol::{SessionState, StateChangeReason};

/// 可暂停的编排后端：decide 阻塞在信号量上直到测试放行。
/// 用于复现「关联普通会话完成发生在编排 turn 进行中」——prompt_session 工具
/// 阻塞等待关联普通会话结束，busy→idle 事件必然在 gate 运行中到达。
struct PausableBackend {
    started: AtomicUsize,
    started_notify: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    decisions: Mutex<VecDeque<String>>,
}

impl OrcBackend for PausableBackend {
    fn decide<'a>(
        &'a self,
        _ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.started_notify.notify_waiters();
            // 信号量语义：许可会保留给后续 acquire，无 Notify 的丢唤醒竞态
            let permit = self
                .release
                .acquire()
                .await
                .map_err(|_| "信号量关闭".to_string())?;
            permit.forget();
            self.decisions
                .lock()
                .pop_front()
                .ok_or_else(|| "决策用尽".to_string())
        })
    }
}

async fn wait_started(backend: &PausableBackend, expected: usize) {
    loop {
        let notified = backend.started_notify.notified();
        if backend.started.load(Ordering::SeqCst) >= expected {
            return;
        }
        notified.await;
    }
}

fn hub_with_one_machine() -> Arc<MachineHub> {
    let hub = MachineHub::default();
    hub.sync(vec![(
        MachineSummary {
            name: "测试机".into(),
            online: true,
            agents: vec![AgentSlot {
                name: "mock_acp".into(),
                available: true,
            }],
        },
        None,
    )]);
    Arc::new(hub)
}

#[tokio::test]
async fn linked_session_completion_mid_turn_injects_message_and_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(PausableBackend {
        started: AtomicUsize::new(0),
        started_notify: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        decisions: Mutex::new(VecDeque::from(vec![
            "第一轮调度".into(),
            "收到关联普通会话完成，继续下一阶段".into(),
        ])),
    });
    let backend_test = backend.clone();
    let engine = WorkflowEngine::new("计划", "", "", backend, hub_with_one_machine(), dir.path());
    engine.session.write().linked_sessions.push(LinkedSession {
        id: "s_child".into(),
        machine_name: "测试机".into(),
    });

    // 编排 turn 启动并阻塞在第一轮 decide（gate 运行中）
    let engine_task = engine.clone();
    let ta = tokio::spawn(async move { engine_task.advance().await });
    wait_started(&backend_test, 1).await;

    // 关联普通会话完成事件在 turn 进行中到达：必须立即注入用户消息
    let injected = engine
        .on_linked_session_state(
            "测试机",
            "s_child",
            SessionState::Busy,
            SessionState::Idle,
            StateChangeReason::Completed,
        )
        .await
        .unwrap();
    assert!(injected);
    assert!(
        engine
            .session
            .read()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. }
                if text.contains("s_child@测试机 检测到状态变更"))),
        "关联普通会话完成（turn 进行中）应立即注入用户消息"
    );

    // 放行第一轮 decide → 当前 turn 结束 → 因 requested 补跑第二轮
    backend_test.release.add_permits(1);
    wait_started(&backend_test, 2).await;
    backend_test.release.add_permits(1);
    let res = ta.await.unwrap();
    assert!(res.is_ok());

    // 第二轮（带注入消息的 rerun）的编排输出应出现在对话流
    assert!(
        engine.session.read().transcript.iter().any(
            |m| matches!(m, OrcMsg::Orc { text, .. } if text == "收到关联普通会话完成，继续下一阶段")
        ),
        "注入消息应触发编排补跑一轮处理"
    );
}
