//! 工作目录联想下拉「外点取消」回归测试。
//!
//! 历史回归：联想下拉是手搓浮层（无 Popover 的遮罩/焦点管理），弹出后
//! 点击对话框其他位置无法收起，一直遮挡排在其后的控件。锁定不变量：
//! 1) 鼠标按下落在联想列表之外时联想取消；
//! 2) 点击列表项正常回填（不被外点取消误伤）。
//!
//! 注意：不得使用测试平台的 `draw()` 辅助函数——它不处理 deferred 绘制，
//! 会把悬空的 deferred 元素留在窗口帧里，下一次正式重绘即崩溃
//! （`prepaint_deferred_draws` 解引用已清空的 element arena）。此处只走
//! 正式窗口重绘（flush_effects 驱动），事件派发用 `simulate_event`。

use std::cell::RefCell;
use std::sync::Arc;

use gpui::{
    div, point, px, AppContext, Entity, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent,
    ParentElement, Render, Styled, VisualTestContext, Window,
};

use amux_desktop::app::{AmuxApp, CwdSuggestion};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

struct PickerHostView {
    app: Entity<AmuxApp>,
}

impl Render for PickerHostView {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let picker = self.app.update(cx, |app, cx| {
            app.render_workspace_picker(cx).into_any_element()
        });
        div().size_full().child(picker)
    }
}

fn entry(path: &str, is_dir: bool) -> protocol::FsEntry {
    protocol::FsEntry {
        name: path.rsplit('/').next().unwrap().into(),
        path: path.into(),
        is_dir,
        size: 0,
    }
}

fn mouse_down_at(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>) {
    cx.simulate_event(MouseDownEvent {
        position,
        button: MouseButton::Left,
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
}

fn click_at(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>) {
    mouse_down_at(cx, position);
    cx.simulate_event(MouseUpEvent {
        position,
        button: MouseButton::Left,
        modifiers: Default::default(),
        click_count: 1,
    });
}

/// 窗口根视图即宿主：事件处理链里的 window.refresh() 会按根视图重绘，
/// 根视图若为空则事件监听帧被清空（同 tests/resize_handle_drag.rs）。
fn setup(
    cx: &mut gpui::TestAppContext,
) -> (
    Entity<AmuxApp>,
    Entity<PickerHostView>,
    &mut VisualTestContext,
) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    std::mem::forget(data_dir); // 测试进程存活期内路径有效即可

    let holder = RefCell::new(None);
    let (host, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path));
        let app = cx.new(|cx| AmuxApp::new(store, window, cx));
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
        });
        *holder.borrow_mut() = Some(app.clone());
        PickerHostView { app }
    });
    (holder.into_inner().unwrap(), host, visual)
}

fn set_suggestion(
    app: &Entity<AmuxApp>,
    host: &Entity<PickerHostView>,
    cx: &mut VisualTestContext,
) {
    cx.update(|_window, cx| {
        app.update(cx, |app, _cx| {
            app.cwd_suggestion = Some(CwdSuggestion {
                matches: vec![entry("/home", true), entry("/opt", true)],
            });
        });
        // AmuxApp 是普通实体，其 notify 不会标脏窗口；宿主是窗口根视图，
        // 必须显式通知才会重绘出联想下拉
        host.update(cx, |_, cx| cx.notify());
    });
}

fn suggestion_is_set(app: &Entity<AmuxApp>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, cx| app.read(cx).cwd_suggestion.is_some())
}

#[gpui::test]
fn cwd_suggestion_dismisses_on_mouse_down_outside(cx: &mut gpui::TestAppContext) {
    let (app, host, cx) = setup(cx);

    set_suggestion(&app, &host, cx);
    // debug_bounds 内部 flush 一次正式重绘，联想列表随之渲染
    let list = cx
        .debug_bounds("cwd-suggest-list")
        .expect("联想列表应参与布局");
    assert!(suggestion_is_set(&app, cx));

    // 回归点：按下落在列表外（下方空白处）→ 联想取消
    let outside = point(
        list.origin.x + px(2.),
        list.origin.y + list.size.height + px(30.),
    );
    mouse_down_at(cx, outside);
    assert!(
        !suggestion_is_set(&app, cx),
        "点击联想列表以外应取消联想（列表边界 {list:?}）"
    );
}

#[gpui::test]
fn cwd_suggestion_item_click_still_fills_input(cx: &mut gpui::TestAppContext) {
    let (app, host, cx) = setup(cx);

    set_suggestion(&app, &host, cx);
    let item = cx
        .debug_bounds("cwd-suggest-option-/home")
        .expect("联想项应参与布局");

    // 点击列表项：外点取消不得误伤——回填照常发生（目录补分隔符）
    click_at(cx, item.center());
    let value =
        cx.update(|_window, cx| app.read(cx).session_cwd_input.read(cx).value().to_string());
    assert_eq!(value, "/home/", "点击联想项应回填目录路径");
}
