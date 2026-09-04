//! 会话选项设置链路回归测试：模拟「点开下拉 → 选择菜单项」的完整交互。
//!
//! 历史回归：下拉菜单项的 on_click 回调运行在窗口事件分发栈内（`&mut App`
//! 上下文），此前用 `weak.update_in` 更新实体——窗口已在 update stack 上时
//! 该调用静默失败，设置请求从未发出（GUI 无反应、两端无日志）。修复为
//! `entity.update`（同会话右键菜单的可用模式）。
//!
//! 锁定不变量：选择菜单项后必须发起 `session.configure`。测试环境 WS 不可达，
//! 请求失败会弹出错误通知——以「通知出现」作为请求已发起的可观察证据。

use std::rc::Rc;
use std::sync::Arc;

use gpui::{point, px, AppContext, Modifiers};

use amux_desktop::app::{AmuxApp, Selected, SelectedConfigOptions};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};
use gpui_component::{Root, WindowExt};
use protocol::{SessionConfigKind, SessionConfigOption, SessionConfigSelectEntry};

#[gpui::test]
fn config_option_select_triggers_configure(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    cx.update(gpui_component::init);

    // Root 必须是窗口根视图（push_notification 等组件层依赖 Root::update）
    let app = Rc::new(std::cell::RefCell::new(None));
    let app_for_window = app.clone();
    let (_root, cx) = cx.add_window_view(|window, cx| {
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        let app = cx.new(|cx| AmuxApp::new(store, window, cx));
        *app_for_window.borrow_mut() = Some(app.clone());
        Root::new(app, window, cx)
    });
    let app = app.borrow().as_ref().expect("app 已构建").clone();

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
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            // select 类选项：当前值 B，候选 A/B（菜单点击目标为第一项 A）
            app.config_options = Some(SelectedConfigOptions {
                machine: "test".into(),
                session_id: "session-1".into(),
                loading: false,
                options: vec![SessionConfigOption {
                    id: "model".into(),
                    name: "Model".into(),
                    description: None,
                    category: None,
                    kind: SessionConfigKind::Select {
                        current_value: "b".into(),
                        options: vec![
                            SessionConfigSelectEntry {
                                value: "a".into(),
                                name: "A".into(),
                            },
                            SessionConfigSelectEntry {
                                value: "b".into(),
                                name: "B".into(),
                            },
                        ],
                    },
                }],
            });
        });
    });

    // 触发一次窗口绘制（点击无副作用的标题栏），使 rendered_frame 含选项行
    cx.simulate_click(point(px(600.), px(15.)), Modifiers::default());

    // 点击下拉按钮：弹出菜单并夺取焦点（菜单打开的证据）
    let row = cx
        .debug_bounds("cfg-row-test-model")
        .expect("会话选项行应参与布局");
    cx.simulate_click(
        point(row.right() - px(20.), row.center().y),
        Modifiers::default(),
    );
    let menu_focused = cx.update(|window, cx| window.focused(cx).is_some());
    assert!(menu_focused, "点击下拉按钮后应弹出菜单（菜单获得焦点）");

    // 键盘选择第一项 A（焦点在菜单上：down 选中首项，enter 触发）
    cx.simulate_keystrokes("down enter");

    // WS 不可达 → configure 失败 → 错误通知弹出 = 请求已发起的可观察证据
    let notified = cx.update(|window, cx| !window.notifications(cx).is_empty());
    assert!(
        notified,
        "选择菜单项后应发起 session.configure 请求（以失败通知为证）"
    );
}
