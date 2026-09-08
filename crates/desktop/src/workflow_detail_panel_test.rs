//! 工作流会话详情面板回归测试：详情数据仅来自应用侧会话元数据——
//! 关联普通会话列表取自 OrcSession.linked_sessions（不依赖机器/server 状态），
//! 且工作流会话不展示普通会话专属的工作目录项。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement};
use protocol::{SessionMeta, SessionState};

use crate::app::{AmuxApp, Panel, Selected};
use crate::config::{ApiFormat, ConfigStore, MachineConfig, OrchestratorConfig};
use crate::machine::{MachineStatus, MachineView};
use crate::workflow::LinkedSession;

#[gpui::test]
fn workflow_detail_shows_linked_sessions_without_cwd_row(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        let _ = store.save_orchestrator(&OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            effort: "high".into(),
        });
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.workflow_input
                .update(cx, |s, cx| s.set_value("测试计划", window, cx));
            app.create_workflow(window, cx);
            // 关联普通会话仅存在于应用侧元数据（OrcSession.linked_sessions）；
            // 未注册任何机器，详情渲染不应依赖机器/server 状态
            app.workflows[0]
                .session
                .write()
                .linked_sessions
                .push(LinkedSession {
                    id: "child-1".into(),
                    machine_name: "remote".into(),
                });
            app.panel = Some(Panel::Detail);
            app.panel_delta_px = 365.0;
        });
    });

    cx.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );

    let section = cx
        .debug_bounds("wf-detail-linked-sessions")
        .expect("工作流详情应渲染关联普通会话区块");
    assert!(section.size.height > px(0.));
    let row = cx
        .debug_bounds("wf-detail-linked-session")
        .expect("关联普通会话行应渲染");
    assert!(row.size.height > px(0.));
    assert!(
        cx.debug_bounds("detail-cwd-row").is_none(),
        "工作流会话详情不应展示工作目录"
    );
}

#[gpui::test]
fn session_detail_keeps_cwd_row(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = MachineView::new(
                MachineConfig {
                    name: "test".into(),
                    url: "ws://127.0.0.1:9/".into(),
                    token: "t".into(),
                },
                cx,
            );
            machine.status = MachineStatus::Online;
            machine.sessions.push(SessionMeta {
                id: "session-1".into(),
                agent: "codex".into(),
                cwd: "/workspace".into(),
                state: SessionState::Idle,
                title: "标题".into(),
                created_at: 1,
                last_active_at: 1,
                worktree_dir: String::new(),
                context_size: 0,
                context_window_size: 0,
            });
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Detail);
            app.panel_delta_px = 365.0;
        });
    });

    cx.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );

    let row = cx
        .debug_bounds("detail-cwd-row")
        .expect("普通会话详情应展示工作目录");
    assert!(row.size.height > px(0.));
    assert!(
        cx.debug_bounds("wf-detail-linked-sessions").is_none(),
        "普通会话详情不应渲染关联普通会话区块"
    );
}
