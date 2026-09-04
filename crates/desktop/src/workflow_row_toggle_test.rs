//! 工作流会话列表行折叠/展开回归测试：点击展开箭头后，关联普通会话子列表
//! 必须出现（再点击后隐藏）。
//!
//! 用户反馈工作流会话的关联普通会话无法折叠/展开。本测试用真实窗口渲染
//! 整条列表链，定位交互是否生效。

use std::sync::Arc;

use gpui::{point, px};

use crate::app::AmuxApp;
use crate::config::{ApiFormat, ConfigStore, OrchestratorConfig};
use crate::workflow::LinkedSession;

#[gpui::test]
fn workflow_row_toggle_expands_and_collapses_children(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let (app, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        // 编排 agent 需已配置才能创建工作流（推进调用会失败，不影响本测试）
        let _ = store.save_orchestrator(&OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
        });
        AmuxApp::new(store, window, cx)
    });

    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.workflow_input
                .update(cx, |s, cx| s.set_value("测试计划", window, cx));
            app.create_workflow(window, cx);
            // 注入一个已关联的普通会话（模拟编排调度后的形态）
            app.workflows[0]
                .session
                .write()
                .linked_sessions
                .push(LinkedSession {
                    id: "child-1".into(),
                    machine_name: "test".into(),
                });
        });
    });

    // 触发一次窗口绘制：点击标题栏空白（无副作用）
    cx.simulate_click(point(px(600.), px(15.)), gpui::Modifiers::default());

    // 点击 toggle 按钮（行右缘：空闲时行尾为 8px 占位，按钮在其左侧）
    let row = cx.debug_bounds("wf-row").expect("工作流行应参与布局");
    cx.simulate_click(
        point(row.right() - px(30.), row.top() + px(18.)),
        gpui::Modifiers::default(),
    );

    // 强制一帧重绘（点击无副作用的标题栏），使 rendered_frame 反映展开状态
    cx.simulate_click(point(px(600.), px(15.)), gpui::Modifiers::default());

    // 展开后：关联普通会话列表容器渲染（含关联普通会话行）
    cx.debug_bounds("wf-linked-sessions-list")
        .expect("点击展开箭头后关联普通会话列表应渲染");

    // 再次点击：折叠后关联普通会话列表消失
    cx.simulate_click(
        point(row.right() - px(30.), row.top() + px(18.)),
        gpui::Modifiers::default(),
    );
    cx.simulate_click(point(px(600.), px(15.)), gpui::Modifiers::default());
    let list = cx.debug_bounds("wf-linked-sessions-list");
    assert!(
        list.is_none(),
        "再次点击箭头后关联普通会话列表应隐藏: {list:?}"
    );
}
