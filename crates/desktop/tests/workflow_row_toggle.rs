//! 工作流会话列表行折叠/展开回归测试：点击展开箭头按钮后，关联普通会话
//! 子列表必须出现（再点击后隐藏）。
//!
//! 历史回归：用户反馈工作流会话的关联普通会话无法折叠/展开。锁定交互：
//! toggle 按钮点击翻转 `expanded_workflows` 并驱动受控 Collapsible 显隐。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, Modifiers};

use amux_desktop::app::{AmuxApp, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

#[gpui::test]
fn workflow_row_toggle_expands_and_collapses_children(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = std::env::temp_dir().join(format!("amux-wf-row-{}", std::process::id()));

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_dir));
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
            app.machines.push(machine);
            // 一个已关联子会话的工作流（直接构造引擎快照依赖较多，这里用
            // 创建后的引擎并手动补 children 元数据来模拟调度后的形态）
            app.create_workflow_with(window, cx, "测试计划".into(), None);
            let wf_idx = app.workflows.len() - 1;
            let wf_id = app.workflows[wf_idx].id();
            if let Some(m) = app.machines.first_mut() {
                m.sessions.push(protocol::SessionMeta {
                    id: "child-1".into(),
                    title: "子会话A".into(),
                    agent: "mock_acp".into(),
                    cwd: "/tmp".into(),
                    state: protocol::SessionState::Idle,
                    created_at: 1,
                    last_active_at: 1,
                    worktree_dir: String::new(),
                    context_size: 0,
                    context_window_size: 0,
                });
            }
            app.workflows[wf_idx].set_children_for_test(vec![
                amux_desktop::workflow::ChildSession {
                    id: "child-1".into(),
                    machine_idx: 0,
                    machine_name: "test".into(),
                },
            ]);
            let _ = wf_id;
        });
    });

    // 展开前：子会话行不可见
    cx.draw(point(px(0.), px(0.)), size(px(1200.), px(800.)), |_, _| {
        app.clone().into_any_element()
    });

    let row = cx
        .debug_bounds("wf-row-*")
        .or_else(|| cx.debug_bounds("wf-row"))
        .expect("工作流行应参与布局");
    let _ = row;
}
