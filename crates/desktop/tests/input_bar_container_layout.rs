//! 消息框与底部操作区（快捷按钮+实时活动+输入框）的容器分离回归测试。
//!
//! 历史问题：三者在主卡片上是消息框的直接兄弟节点，视觉上与消息区融为
//! 一体，被反馈为「遮挡对话消息框」。现在三者收敛进独立底部容器
//! （input-bar），锁定不变量：
//! 1) 底部容器整体位于消息框之下，两者边界不相交；
//! 2) 快捷按钮行与输入框都落在底部容器内部（容器分离结构不被回退）；
//! 3) 消息框保持 flex_1 收缩：输入框撑高（auto_grow 上限 8 行）、快捷按钮
//!    换行、活动条出现时，消息框仍完整可见、不被盖住。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement, VisualTestContext};

use amux_desktop::aggregate::SessionView;
use amux_desktop::app::{AmuxApp, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::logic::DialogMsg;
use amux_desktop::machine::{MachineStatus, MachineView};
use protocol::{Activity, ContentBlock};

fn setup(visual: &mut VisualTestContext, live: Option<Activity>) -> gpui::Entity<AmuxApp> {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    std::mem::forget(data_dir); // 测试进程存活期内路径有效即可

    let store = Arc::new(ConfigStore::new(data_path));
    store.add_quick_command("编译检查", "请运行编译检查");
    store.add_quick_command("测试", "请运行测试");

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
            let mut view = SessionView::default();
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
            // 实时活动展示中：活动条参与底部容器布局
            view.live = live;
            machine.views.insert("session-1".into(), view);
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
        });
    });
    app
}

#[gpui::test]
fn bottom_bar_stays_below_dialog(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(
        visual,
        Some(Activity::Thinking {
            timestamp: 1_700_009_999,
            thinking: "正在思考一个很长的问题".into(),
        }),
    );
    visual.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );
    let dialog = visual.debug_bounds("dialog").expect("消息框应参与布局");
    let bar = visual
        .debug_bounds("input-bar")
        .expect("底部操作区容器应参与布局");
    let quick = visual
        .debug_bounds("quick-buttons")
        .expect("快捷按钮应参与布局");
    let input = visual
        .debug_bounds("input-align-input")
        .expect("输入框应参与布局");

    // 不变量 1：底部容器整体在消息框之下，边界不交叠
    assert!(
        dialog.bottom() <= bar.top() + px(1.),
        "底部操作区（顶沿 {}）不得遮挡消息框（底沿 {}）",
        bar.top(),
        dialog.bottom()
    );
    // 不变量 2：快捷按钮与输入框都收敛进底部容器内部
    for (name, bounds) in [("快捷按钮", quick), ("输入框", input)] {
        assert!(
            bounds.top() >= bar.top() - px(1.) && bounds.bottom() <= bar.bottom() + px(1.),
            "{name} 应位于底部操作区容器内部",
            name = name,
        );
    }
    // 不变量 3：消息框仍占据消息区（未被挤到 0 高）
    assert!(
        dialog.size.height >= px(120.),
        "消息框高度不应塌缩（实际 {}）",
        dialog.size.height
    );
}

#[gpui::test]
fn bottom_bar_stays_below_dialog_on_short_window(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(
        visual,
        Some(Activity::Thinking {
            timestamp: 1_700_009_999,
            thinking: "正在思考一个很长的问题".into(),
        }),
    );
    visual.draw(point(px(0.), px(0.)), size(px(900.), px(500.)), |_, _cx| {
        app.clone().into_any_element()
    });
    let dialog = visual.debug_bounds("dialog").expect("消息框应参与布局");
    let bar = visual
        .debug_bounds("input-bar")
        .expect("底部操作区容器应参与布局");
    assert!(
        dialog.bottom() <= bar.top() + px(1.),
        "矮窗口下底部操作区（顶沿 {}）不得遮挡消息框（底沿 {}）",
        bar.top(),
        dialog.bottom()
    );
    assert!(
        dialog.size.height >= px(120.),
        "矮窗口下消息框高度不应塌缩（实际 {}）",
        dialog.size.height
    );
}

#[gpui::test]
fn grown_input_keeps_dialog_visible(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(
        visual,
        Some(Activity::Thinking {
            timestamp: 1_700_009_999,
            thinking: "正在思考一个很长的问题".into(),
        }),
    );
    visual.update(|window, cx| {
        app.update(cx, |app, cx| {
            // auto_grow 上限 8 行，模拟用户粘贴长文本
            let text = (1..=8)
                .map(|i| format!("第 {i} 行粘贴内容"))
                .collect::<Vec<_>>()
                .join("\n");
            app.input_state
                .update(cx, |s, cx| s.set_value(&text, window, cx));
        });
    });
    visual.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );
    let dialog = visual.debug_bounds("dialog").expect("消息框应参与布局");
    let bar = visual
        .debug_bounds("input-bar")
        .expect("底部操作区容器应参与布局");
    assert!(
        dialog.bottom() <= bar.top() + px(1.),
        "输入框撑高后底部操作区（顶沿 {}）不得遮挡消息框（底沿 {}）",
        bar.top(),
        dialog.bottom()
    );
    assert!(
        dialog.size.height >= px(120.),
        "输入框撑高后消息框高度不应塌缩（实际 {}）",
        dialog.size.height
    );
}

#[gpui::test]
fn activity_appearance_keeps_dialog_bottom_pinned(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(visual, None);
    let draw = |visual: &mut VisualTestContext, app: &gpui::Entity<AmuxApp>| {
        visual.draw(
            point(px(0.), px(0.)),
            size(px(1200.), px(800.)),
            |_, _cx| app.clone().into_any_element(),
        );
    };
    // 对应真实入口 open_session：进入会话即显式贴底
    visual.update(|_window, cx| {
        app.update(cx, |app, _cx| app.dialog_scroll_to_bottom());
    });
    draw(visual, &app);
    assert!(
        visual.read_entity(&app, |app, _| app.dialog_at_bottom()),
        "初始应贴底显示最新消息"
    );

    // 突然出现实时活动：底部操作区撑高、对话框视口变矮。
    // 回归点：视口变矮后旧 offset 不再贴底，最新消息会被顶出可视区，
    // 视觉上表现为被输入框「遮挡」——贴底跟随必须在渲染期重申
    visual.update(|_window, cx| {
        app.update(cx, |app, cx| {
            let machine = app.machines.get_mut(0).unwrap();
            let view = machine.views.get_mut("session-1").unwrap();
            view.live = Some(Activity::Thinking {
                timestamp: 1_700_009_999,
                thinking: "正在思考一个很长的问题".into(),
            });
            cx.notify();
        });
    });
    draw(visual, &app);
    assert!(
        visual.read_entity(&app, |app, _| app.dialog_at_bottom()),
        "活动条出现、对话框视口变矮后仍应贴底显示最新消息"
    );

    // 布局不变量不受影响：底部操作区不得与消息框交叠
    let dialog = visual.debug_bounds("dialog").expect("消息框应参与布局");
    let bar = visual
        .debug_bounds("input-bar")
        .expect("底部操作区容器应参与布局");
    assert!(
        dialog.bottom() <= bar.top() + px(1.),
        "底部操作区（顶沿 {}）不得遮挡消息框（底沿 {}）",
        bar.top(),
        dialog.bottom()
    );
}
