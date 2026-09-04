//! 侧栏与右侧面板拖拽手柄的隔离回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归：gpui 的 `on_drag_move` 是窗口级全局监听（按拖拽载荷 TypeId
//! 过滤），两个手柄曾共用 `()` 载荷且 origin 在 mouse up 后不清理——先拖过
//! 一侧后，再拖另一侧会同时改变两边宽度。锁定不变量：拖拽任一手柄只改
//! 变对应视图的宽度，另一侧保持不变。

use std::sync::Arc;

use gpui::{
    point, px, size, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    VisualTestContext,
};

use amux_desktop::app::{AmuxApp, Panel, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

fn read_widths(cx: &mut VisualTestContext, app: &gpui::Entity<AmuxApp>) -> (f32, f32) {
    cx.update(|_, cx| {
        let app = app.read(cx);
        (app.sidebar_width_px, app.panel_delta_px)
    })
}

/// 水平拖拽：按下 → 移动（首次移动建立 active_drag，随后才触发 drag_move）
/// → 再次移动到终点 → 抬起。终点重复一次确保处理器用的是最终位置。
fn drag(
    cx: &mut VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    cx.simulate_event(MouseDownEvent {
        position: from,
        button: MouseButton::Left,
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseMoveEvent {
        position: to,
        pressed_button: Some(MouseButton::Left),
        modifiers: Default::default(),
    });
    cx.simulate_event(MouseMoveEvent {
        position: to,
        pressed_button: Some(MouseButton::Left),
        modifiers: Default::default(),
    });
    cx.simulate_event(MouseUpEvent {
        position: to,
        button: MouseButton::Left,
        modifiers: Default::default(),
        click_count: 1,
    });
}

#[gpui::test]
fn resize_handles_are_isolated(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    // AmuxApp 必须是窗口根视图：手柄 mouse_down 内部的 window.refresh()
    // 会触发测试平台按根视图重绘，根视图若为空则整个事件监听帧被清空。
    let (app, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        AmuxApp::new(store, window, cx)
    });

    cx.update(|window, cx| {
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
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            // 直接展开右侧面板（同 tests/diff_scroll_layout.rs）
            app.panel = Some(Panel::Diff);
            app.panel_delta_px = 405.0 * window.scale_factor();
            cx.notify();
        });
    });

    // 回归点 1：拖侧栏手柄只加宽侧栏，面板宽度不变
    cx.draw(point(px(0.), px(0.)), size(px(1400.), px(900.)), |_, _| {
        app.clone().into_any_element()
    });
    let (s0, p0) = read_widths(cx, &app);
    let handle = cx
        .debug_bounds("sidebar-resize-handle")
        .expect("侧栏手柄未渲染");
    drag(
        cx,
        handle.center(),
        point(handle.center().x + px(60.), handle.center().y),
    );
    let (s1, p1) = read_widths(cx, &app);
    assert!(s1 > s0, "拖侧栏手柄应加宽侧栏（{s0} → {s1}）");
    assert_eq!(p1, p0, "拖侧栏手柄不应影响右侧面板宽度");

    // 回归点 2：拖面板手柄只收窄面板——侧栏 origin 已残留，
    // 旧实现会在此同步漂移侧栏。窗口 960×540 逻辑像素（scale 2）下面板已到
    // 可用宽度上限，故向右拖（收窄）验证面板自身变化。
    let handle = cx
        .debug_bounds("panel-resize-handle")
        .expect("面板手柄未渲染");
    drag(
        cx,
        handle.center(),
        point(handle.center().x + px(60.), handle.center().y),
    );
    let (s2, p2) = read_widths(cx, &app);
    assert!(p2 < p1, "拖面板手柄应收窄面板（{p1} → {p2}）");
    assert_eq!(s2, s1, "拖面板手柄不应影响侧栏宽度");

    // 回归点 3（对称）：面板 origin 现已残留，再拖侧栏时面板不得漂移
    let handle = cx
        .debug_bounds("sidebar-resize-handle")
        .expect("侧栏手柄未渲染");
    drag(
        cx,
        handle.center(),
        point(handle.center().x + px(40.), handle.center().y),
    );
    let (s3, p3) = read_widths(cx, &app);
    assert!(s3 > s2, "拖侧栏手柄应继续加宽侧栏（{s2} → {s3}）");
    assert_eq!(p3, p2, "再拖侧栏手柄不应影响右侧面板宽度");
}
