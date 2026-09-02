//! 关联普通会话完成（busy→idle）必须触发系统向工作流会话注入用户消息。
//! `session.state_change` 通知 → `on_state_change` 路由 → `on_child_state` 注入。
//!
//! 注入发生在引擎后台任务（run_engine_on_tokio）中。后端阻塞在 `pending`
//! 上永不返回，保证 tokio 侧任务不会在测试期间完成并跨线程唤醒 GPUI 任务
//!（GPUI 测试调度器禁止非测试线程调度）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use amux_desktop::app::AmuxApp;
use amux_desktop::config::ConfigStore;
use amux_desktop::workflow::{
    ChildSession, Decision, MachineHub, OrcBackend, OrcContext, OrcMsg, WorkflowEngine,
};
use amux_desktop::ws::Notification as WsNotification;
use serde_json::json;

/// 阻塞编排后端：decide 永不返回。注入发生在 decide 之前，阻塞保证
/// tokio 侧任务不会在测试期间完成（避免跨线程唤醒 GPUI 任务）。
struct BlockingBackend;

impl OrcBackend for BlockingBackend {
    fn decide<'a>(
        &'a self,
        _ctx: &'a OrcContext,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'a>> {
        Box::pin(async move {
            std::future::pending::<()>().await;
            Ok(Decision {
                summary: String::new(),
            })
        })
    }
}

#[gpui::test]
fn child_completion_notification_injects_user_message(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let (app, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        AmuxApp::new(store, window, cx)
    });

    // 工作流会话 + 已挂载的关联普通会话（等价于编排智能体
    // create_session 即时挂载后的形态）。不注册机器：on_state_change 里
    // 的 refresh_sessions 会因无机器提前返回，避免额外跨线程任务。
    cx.update(|_window, cx| {
        app.update(cx, |app, _cx| {
            app.workflows.push(WorkflowEngine::new(
                "测试计划",
                "",
                "",
                Arc::new(BlockingBackend),
                Arc::new(MachineHub::default()),
                &data_path,
            ));
            app.workflows[0]
                .session
                .write()
                .children
                .push(ChildSession {
                    id: "child-1".into(),
                    machine_idx: 0,
                    machine_name: "测试机".into(),
                });
        });
    });

    // 模拟 server 推送：关联普通会话完成（busy→idle, completed）
    let n = WsNotification {
        method: protocol::notify::SESSION_STATE_CHANGE.into(),
        params: json!({
            "sessionId": "child-1",
            "oldState": "busy",
            "newState": "idle",
            "reason": "completed",
        }),
    };
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            AmuxApp::on_state_change(app, window, cx, 0, &n);
        });
    });

    // 注入在引擎后台任务（tokio runtime）中执行：轮询 transcript 直到出现
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let found = cx.update(|_, cx| {
            app.read(cx).workflows[0].session.read().transcript.iter().any(|m| {
                matches!(m, OrcMsg::User { text, .. } if text.contains("child-1@测试机 检测到状态变更"))
            })
        });
        if found {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "关联普通会话完成未触发系统向工作流会话注入用户消息"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }
}
