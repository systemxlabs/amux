//! 关联普通会话完成（busy→idle）发生在编排 turn 进行中时的回归测试：
//! 系统必须立即向工作流会话注入用户消息，并在当前 turn 结束后补跑一轮
//! 处理该消息（docs/DESIGN.md「工作流会话驱动」）。
//!
//! 真实流程中 `prompt_session` 工具阻塞等待子会话结束，busy→idle 事件必然
//! 在 gate 运行中到达；此前子会话要等整轮 decide 结束才挂载进 `children`，
//! 事件到达时工作流查找失败、注入被丢弃。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use amux_desktop::workflow::{
    AgentSlot, ChildSession, Decision, MachineHub, MachineSummary, OrcBackend, OrcContext, OrcMsg,
    WorkflowEngine,
};
use amux_desktop::ws::WsClient;
use protocol::{SessionState, StateChangeReason};

/// 可暂停的编排后端：decide 阻塞在信号量上直到测试放行。
/// 用于复现「子会话完成发生在编排 turn 进行中」——prompt_session 工具
/// 阻塞等待子会话结束，busy→idle 事件必然在 gate 运行中到达。
struct PausableBackend {
    started: AtomicUsize,
    release: tokio::sync::Semaphore,
    decisions: Mutex<VecDeque<Decision>>,
}

impl OrcBackend for PausableBackend {
    fn decide<'a>(
        &'a self,
        _ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            // 信号量语义：许可会保留给后续 acquire，无 Notify 的丢唤醒竞态
            let permit = self
                .release
                .acquire()
                .await
                .map_err(|_| "信号量关闭".to_string())?;
            permit.forget();
            self.decisions
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .pop_front()
                .ok_or_else(|| "决策用尽".to_string())
        })
    }
}

async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("等待超时");
}

fn hub_with_one_machine() -> Arc<MachineHub> {
    let hub = MachineHub::default();
    hub.sync(
        vec![MachineSummary {
            name: "测试机".into(),
            online: true,
            agents: vec![AgentSlot {
                name: "mock_acp".into(),
                available: true,
            }],
        }],
        vec![WsClient::connect_with_token(
            "ws://127.0.0.1:1".into(),
            "unused".into(),
        )],
    );
    Arc::new(hub)
}

#[tokio::test]
async fn child_completion_mid_turn_injects_message_and_reruns() {
    let dir = std::env::temp_dir().join(format!("amux-wf-midturn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let backend = Arc::new(PausableBackend {
        started: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
        decisions: Mutex::new(VecDeque::from(vec![
            Decision {
                summary: "第一轮调度".into(),
            },
            Decision {
                summary: "收到子会话完成，继续下一阶段".into(),
            },
        ])),
    });
    let backend_test = backend.clone();
    let engine = WorkflowEngine::new("计划", "", "", backend, hub_with_one_machine(), &dir);
    engine.session.write().unwrap().children.push(ChildSession {
        id: "s_child".into(),
        machine_idx: 0,
        machine_name: "测试机".into(),
    });

    // 编排 turn 启动并阻塞在第一轮 decide（gate 运行中）
    let engine_task = engine.clone();
    let ta = tokio::spawn(async move { engine_task.advance().await });
    wait_until(|| backend_test.started.load(Ordering::SeqCst) >= 1).await;

    // 子会话完成事件在 turn 进行中到达：必须立即注入用户消息
    let injected = engine
        .on_child_state(
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
            .unwrap()
            .transcript
            .iter()
            .any(|m| matches!(m, OrcMsg::User { text, .. }
                if text.contains("s_child@测试机 检测到状态变更"))),
        "子会话完成（turn 进行中）应立即注入用户消息"
    );

    // 放行第一轮 decide → 当前 turn 结束 → 因 requested 补跑第二轮
    backend_test.release.add_permits(1);
    wait_until(|| backend_test.started.load(Ordering::SeqCst) >= 2).await;
    backend_test.release.add_permits(1);
    let res = ta.await.unwrap();
    assert!(res.is_ok());

    // 第二轮（带注入消息的 rerun）的编排输出应出现在对话流
    assert!(
        engine.session.read().unwrap().transcript.iter().any(
            |m| matches!(m, OrcMsg::Orc { text, .. } if text == "收到子会话完成，继续下一阶段")
        ),
        "注入消息应触发编排补跑一轮处理"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
