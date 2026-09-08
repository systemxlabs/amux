//! 悬浮按钮不连带改变对话滚动的回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归：悬浮面板切换栏叠在对话框右缘——Scrollbar（absolute 覆盖层）
//! 的「点击轨道即跳转」是全局 mousedown 监听（只按位置判定，不感知被更高
//! 层元素遮挡），点悬浮按钮会连带触发对话框跳到点击位置；顶部按钮对应
//! 「跳到顶部」，即用户所见“点击悬浮按钮对话框滚到最上面”。

use std::sync::Arc;

use gpui::{
    point, px, size, AppContext, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, VisualTestContext,
};

use crate::app::{AmuxApp, Selected};
use crate::config::{ConfigStore, MachineConfig};
use crate::logic::DialogMsg;
use crate::machine::{MachineStatus, MachineView};
use protocol::ContentBlock;

#[gpui::test]
fn rail_button_click_keeps_dialog_scroll(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    std::mem::forget(data_dir); // 测试进程存活期内路径有效即可

    let mut visual = cx.add_empty_window();
    let store = Arc::new(ConfigStore::new(data_path));
    let app = visual.update(|window, cx| {
        gpui_component::init(cx);
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    visual.update(|_window, cx| {
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
            let mut view = crate::aggregate::SessionView::default();
            // 30 组对话：远超视口高度，撑出滚动空间
            for i in 0..30 {
                view.dialog.push(DialogMsg::UserMessage {
                    content: vec![ContentBlock::Text {
                        text: format!("用户消息 {i} 这是一条比较长的内容用来撑出多行气泡"),
                    }],
                    timestamp: 1_700_000_000 + i * 10,
                });
                view.dialog.push(DialogMsg::AgentMessage {
                    content: vec![ContentBlock::Text {
                        text: format!("agent 回复 {i} 这也是一条比较长的内容用来撑出多行气泡"),
                    }],
                    timestamp: 1_700_000_005 + i * 10,
                });
            }
            machine.views.insert("session-1".into(), view);
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
        });
    });

    let draw = |visual: &mut VisualTestContext| {
        visual.draw(
            point(px(0.), px(0.)),
            size(px(1200.), px(800.)),
            |_, _cx| app.clone().into_any_element(),
        );
    };
    draw(&mut visual);
    let scroll = visual.update(|_, cx| app.read(cx).dialog_scroll.clone());

    // 预置滚动到中部（顶部按钮的跳转目标是 0，最能暴露回归）
    scroll.set_offset(point(px(0.), px(-400.)));
    draw(&mut visual);

    for id in [
        "float-workspace",
        "float-diff",
        "float-detail",
        "float-activities",
        "float-plan",
        "float-terminal",
    ] {
        let pos = visual.debug_bounds(id).expect("悬浮按钮未渲染").center();
        visual.simulate_event(MouseMoveEvent {
            position: pos,
            pressed_button: None,
            modifiers: Default::default(),
        });
        visual.simulate_event(MouseDownEvent {
            position: pos,
            button: MouseButton::Left,
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        visual.simulate_event(MouseUpEvent {
            position: pos,
            button: MouseButton::Left,
            modifiers: Default::default(),
            click_count: 1,
        });
        draw(&mut visual);
        let offset = scroll.offset().y;
        assert!(
            offset <= px(-300.),
            "点击 {id} 后对话滚动被连带改变（offset.y = {offset:?}）"
        );
        scroll.set_offset(point(px(0.), px(-400.)));
    }
}
