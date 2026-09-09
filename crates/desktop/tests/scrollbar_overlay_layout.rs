//! 对话消息框与活动历史面板的滚动条布局回归测试。
//!
//! 历史背景：两个滚动区原本只有原生滚动（无可见滚动条）。滚动条以
//! `Scrollbar`（absolute 覆盖层）形式挂在滚动区的外层容器上——放进滚动
//! 容器内部会随内容滚走。滚动容器右内边距预留 16px 滚动条沟槽（Scrollbar
//! 覆盖滚动区右缘 16px 宽的轨道区）。锁定不变量：
//! 1) 滚动条覆盖层渲染且覆盖滚动区（与外层容器边界一致）；
//! 2) 加覆盖层后滚动区仍参与 flex 布局（不塌缩、不被挤走）且内容可滚
//!    （max_offset 反映超出视口的内容）；
//! 3) 内容（气泡/活动卡片）右缘不进入滚动条轨道区，滚动条不覆盖内容。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement, VisualTestContext};

use amux_desktop::app::{AmuxApp, Panel, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::logic::DialogMsg;
use amux_desktop::machine::{MachineStatus, MachineView};
use protocol::{Activity, ContentBlock};

fn setup(visual: &mut VisualTestContext) -> gpui::Entity<AmuxApp> {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    std::mem::forget(data_dir); // 测试进程存活期内路径有效即可

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
            let mut view = amux_desktop::aggregate::SessionView::default();
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
            // 60 条活动：远超活动历史面板高度
            for i in 0..60 {
                view.activities.push(Activity::Thinking {
                    timestamp: 1_700_000_000 + i,
                    content: format!("思考内容 {i}，用于撑出活动面板滚动空间"),
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
    app
}

#[gpui::test]
fn dialog_scrollbar_overlays_scroll_area(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(visual);
    visual.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );
    let wrap = visual
        .debug_bounds("dialog-wrap")
        .expect("消息框外层容器应参与布局");
    let dialog = visual.debug_bounds("dialog").expect("消息框应参与布局");
    let scrollbar = visual
        .debug_bounds("dialog-scrollbar")
        .expect("对话滚动条覆盖层应渲染");

    // 覆盖层与滚动区边界一致（挂在外层容器上，覆盖整个滚动区）
    assert!(
        (scrollbar.origin.x - wrap.origin.x).abs() <= px(1.)
            && (scrollbar.origin.y - wrap.origin.y).abs() <= px(1.)
            && (scrollbar.size.width - wrap.size.width).abs() <= px(1.)
            && (scrollbar.size.height - wrap.size.height).abs() <= px(1.),
        "对话滚动条应覆盖消息框区域（scrollbar {scrollbar:?} vs wrap {wrap:?}）"
    );
    // 滚动条沟槽：气泡右缘不得进入滚动条右缘 16px 宽的轨道区
    let bubble = visual
        .debug_bounds("dbg-user-bubble")
        .expect("用户气泡应参与布局");
    assert!(
        bubble.origin.x + bubble.size.width <= scrollbar.origin.x + scrollbar.size.width - px(15.),
        "用户气泡不应被滚动条覆盖（气泡右缘 {}，滚动条轨道左缘 {}）",
        bubble.origin.x + bubble.size.width,
        scrollbar.origin.x + scrollbar.size.width - px(16.),
    );
    // 滚动区不塌缩：与外层容器同高
    assert_eq!(dialog.size, wrap.size, "消息框应占满外层容器");
    // 内容超出视口仍可滚
    let scroll = visual.update(|_, cx| app.read(cx).dialog_scroll.clone());
    assert!(
        scroll.max_offset().y > px(1000.),
        "消息内容远超视口，max_offset 应反映全部内容（max_offset.y = {:?}）",
        scroll.max_offset().y
    );
}

#[gpui::test]
fn activities_panel_scrollbar_overlays_scroll_area(cx: &mut gpui::TestAppContext) {
    let visual = cx.add_empty_window();
    let app = setup(visual);
    visual.update(|_window, cx| {
        app.update(cx, |app, cx| {
            // 面板宽度由 panel_delta_px 换算（默认 0 会把面板压成 0 宽）
            app.panel_delta_px = 480.0;
            app.panel = Some(Panel::Activities);
            cx.notify();
        });
    });
    visual.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| app.clone().into_any_element(),
    );
    let panel = visual
        .debug_bounds("activities-panel-wrap")
        .expect("活动历史面板应参与布局");
    let scrollbar = visual
        .debug_bounds("activities-scrollbar")
        .expect("活动历史滚动条覆盖层应渲染");

    assert!(
        (scrollbar.origin.x - panel.origin.x).abs() <= px(1.)
            && (scrollbar.origin.y - panel.origin.y).abs() <= px(1.)
            && (scrollbar.size.width - panel.size.width).abs() <= px(1.)
            && (scrollbar.size.height - panel.size.height).abs() <= px(1.),
        "活动历史滚动条应覆盖面板区域（scrollbar {scrollbar:?} vs panel {panel:?}）"
    );
    // 滚动条沟槽：活动卡片右缘不得进入滚动条右缘 16px 宽的轨道区
    let card = visual
        .debug_bounds("dbg-activity-card")
        .expect("活动卡片应参与布局");
    assert!(
        card.origin.x + card.size.width <= scrollbar.origin.x + scrollbar.size.width - px(15.),
        "活动卡片不应被滚动条覆盖（卡片右缘 {}，滚动条轨道左缘 {}）",
        card.origin.x + card.size.width,
        scrollbar.origin.x + scrollbar.size.width - px(16.),
    );
    let scroll = visual.update(|_, cx| app.read(cx).activities_scroll.clone());
    assert!(
        scroll.max_offset().y > px(1000.),
        "活动内容远超面板高度，max_offset 应反映全部内容（max_offset.y = {:?}）",
        scroll.max_offset().y
    );
}
