//! 工作目录面板文件内容滚动区布局回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归：文件内容（TextView markdown）直接放进无滚动的容器，长文件
//! 超出视口后既被裁剪也无滚动条，用户无法查看文件后半部分。
//! 本测试锁定不变量：
//! 1) 正文滚动容器内容高远大于视口（内容不被压进视口内）；
//! 2) 滚轮事件能实际移动文件内容（偏移变化）。

use std::sync::Arc;

use gpui::{
    point, px, size, AppContext, IntoElement, Modifiers, ParentElement, Render, ScrollDelta,
    ScrollWheelEvent, Styled, TouchPhase, Window,
};

use protocol::{SessionMeta, SessionState};

use crate::app::{AmuxApp, Panel, Selected};
use crate::config::{ConfigStore, MachineConfig};
use crate::machine::{MachineStatus, MachineView};

struct WorkspacePanelHostView {
    app: gpui::Entity<AmuxApp>,
}

impl Render for WorkspacePanelHostView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let panel = self
            .app
            .update(cx, |app, cx| app.render_panel(window, cx))
            .map(|p| p.into_any_element())
            .unwrap_or_else(|| gpui::div().into_any_element());
        gpui::div().size_full().child(panel)
    }
}

/// 远超视口的假文件内容：约 300 行文本。
fn big_file_content() -> String {
    (0..300).map(|i| format!("line {i}\n")).collect()
}

#[gpui::test]
fn workspace_file_content_scroll_has_viewport_constraint(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        cx.new(|cx| AmuxApp::new(store, window, cx))
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
            machine.sessions.push(SessionMeta {
                id: "session-1".into(),
                agent: "test".into(),
                cwd: "/tmp/ws".into(),
                state: SessionState::Idle,
                title: String::new(),
                created_at: 0,
                last_active_at: 0,
                worktree_dir: String::new(),
                context_size: 0,
                context_window_size: 0,
            });
            machine.workspace_file = Some("big.txt".into());
            machine.workspace_content = big_file_content();
            machine.fs_read_has_more = false;
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Workspace);
            // 面板宽度来自可拖拽调节的 panel_delta_px，测试需给足宽度
            app.panel_delta_px = 420.0 * window.scale_factor();
            cx.notify();
            let _ = window;
        });
    });

    // 渲染真实 AmuxApp（根 → main_row → 面板 → 正文滚动区）。
    // 注意：host 视图实体必须在两次 draw 间复用——每次 draw 新建实体会改变
    // 元素树路径，导致 Div 的滚动状态跨帧丢失。
    let host = cx.new(|_| WorkspacePanelHostView { app: app.clone() });
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, _| {
        host.clone().into_any_element()
    });

    // 回归点 1：正文内容高远大于窗口高（768px）。无视口约束的实现会把
    // 内容压进视口或整体溢出，这里内容高必须真实反映全部文本。
    let content_sel = "workspace-content-scroll";
    let before = cx.debug_bounds(content_sel).expect("正文滚动区未渲染");
    assert!(
        before.size.height > px(1200.),
        "文件内容高度未反映全部文本（height = {:?}）",
        before.size.height
    );

    // 行为验证：在正文可视区内滚轮，内容必须实际移动。
    // 注意 bounds 中心在视口之外（内容远高于视口），滚轮要打在视口上部。
    let wheel_pos = point(
        before.origin.x + before.size.width / 2.0,
        before.origin.y + px(100.),
    );
    cx.simulate_event(ScrollWheelEvent {
        position: wheel_pos,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    // 滚动偏移在下一帧才反映到渲染结果：重绘后再读取 bounds。
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, _| {
        host.clone().into_any_element()
    });
    let after = cx.debug_bounds(content_sel).expect("滚动后正文未渲染");
    assert!(
        after.origin.y < before.origin.y,
        "滚轮事件未移动文件内容（before.y = {:?}, after.y = {:?}）",
        before.origin.y,
        after.origin.y
    );
}
