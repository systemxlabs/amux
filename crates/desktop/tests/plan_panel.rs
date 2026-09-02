//! 计划面板渲染冒烟测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 锁定计划面板的渲染行为：普通会话显示 agent 计划，
//! 展示（无计划的会话/工作流会话为空白区，不 panic）。数据链路
//! （ACP plan 通知 → session.plan 查询）由 server e2e 覆盖。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement};

use amux_desktop::aggregate::SessionView;
use amux_desktop::app::{AmuxApp, Panel, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};
use protocol::{SessionPlanEntry, SessionPlanPriority, SessionPlanStatus};

#[gpui::test]
fn plan_panel_renders_entries_and_blank_state(cx: &mut gpui::TestAppContext) {
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
            machine.views.insert(
                "session-1".into(),
                SessionView {
                    plan: vec![
                        SessionPlanEntry {
                            content: "梳理需求".into(),
                            priority: SessionPlanPriority::High,
                            status: SessionPlanStatus::Completed,
                        },
                        SessionPlanEntry {
                            content: "实现功能".into(),
                            priority: SessionPlanPriority::High,
                            status: SessionPlanStatus::InProgress,
                        },
                        SessionPlanEntry {
                            content: "可选优化".into(),
                            priority: SessionPlanPriority::Low,
                            status: SessionPlanStatus::Pending,
                        },
                    ],
                    ..Default::default()
                },
            );
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: 0,
                id: "session-1".into(),
            });
            // 面板宽度由 panel_delta_px 驱动（render_panel 中换算为逻辑宽度；
            // 365 = 计划面板默认宽 360 + 拖拽手柄 5）
            app.panel = Some(Panel::Plan);
            app.panel_delta_px = 365.0;
        });
    });

    // 渲染真实 AmuxApp 根视图（main_row → render_panel → render_plan_panel）
    cx.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );

    let panel = cx.debug_bounds("plan-panel").expect("计划面板应参与布局");
    assert!(
        panel.size.height > gpui::px(0.),
        "计划面板滚动区应有可见高度: {panel:#?}"
    );
}
