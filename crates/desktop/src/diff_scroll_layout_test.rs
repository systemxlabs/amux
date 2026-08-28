//! 改动审查 diff 滚动区布局回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归（见 commit df93aa2 及本次）：h_flex 默认 items_center、以及 taffy
//! flex 收缩会把滚动容器子项压进视口内（内容永不溢出），导致 overflow_y_scroll
//! 的 max_offset 为 0，滚轮无法滚动。本测试锁定不变量：diff 滚动容器必须有
//! 非零 max_offset，且滚轮事件能改变滚动偏移。

use std::sync::Arc;

use gpui::{
    div, point, px, size, AppContext, IntoElement, Modifiers, ParentElement, Render, ScrollDelta,
    ScrollWheelEvent, Styled, Window,
};

use protocol::{GitChangeStatus, GitDiffFile, GitDiffHunk};

use crate::app::{AmuxApp, Panel, Selected};
use crate::config::{ConfigStore, MachineConfig};
use crate::machine::{MachineStatus, MachineView};

struct DiffPanelHostView {
    app: gpui::Entity<AmuxApp>,
}

impl Render for DiffPanelHostView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let panel = self
            .app
            .update(cx, |app, cx| app.render_panel(window, cx))
            .unwrap_or_else(|| div().into_any_element());
        div().size_full().child(panel)
    }
}

/// 构造一段远大于视口的假 diff：单文件单 hunk，120 行上下文行。
fn big_diff_files() -> Vec<GitDiffFile> {
    // hunk patch 需含 `@@` 头（diff_lines 跳到 `@@` 后才解析行）
    let mut patch = String::from("diff --git a/src/lib.rs b/src/lib.rs\n@@ -1,120 +1,120 @@\n");
    for i in 0..120 {
        patch.push_str(&format!(" line {i}\n"));
    }
    vec![GitDiffFile {
        path: "src/lib.rs".into(),
        status: GitChangeStatus::Modified,
        additions: 0,
        deletions: 0,
        patch: patch.clone(),
        hunks: vec![GitDiffHunk {
            header: "@@ -1,120 +1,120 @@".into(),
            patch,
        }],
    }]
}

#[gpui::test]
fn diff_panel_scroll_has_viewport_constraint(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = std::env::temp_dir().join(format!("amux-diff-test-{}", std::process::id()));

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_dir));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = MachineView::new(MachineConfig {
                name: "test".into(),
                url: "ws://127.0.0.1:9/".into(),
                token: "t".into(),
            });
            machine.status = MachineStatus::Online;
            machine.diff_files = big_diff_files();
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: 0,
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Diff);
            // set_panel 打开面板时会写入的运行时宽度（物理像素）；不设则面板宽为 0
            app.panel_delta_px = 420.0 * window.scale_factor();
            cx.notify();
            let _ = window;
        });
    });

    // 渲染真实 AmuxApp（根 → main_row → 面板 → diff 滚动区）。
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| DiffPanelHostView { app: app.clone() })
            .into_any_element()
    });

    let scroll = cx.update(|_, cx| app.read(cx).diff_scroll.clone());

    // 回归点：文件卡（滚动列的直接子项，带 overflow_hidden）必须保持自然高度，
    // 不被 taffy 压缩进视口（压缩后 max_offset=0、无法滚动）。
    let card_sel: &'static str = Box::leak("dbg-diff-file-src/lib.rs".to_string().into_boxed_str());
    let card_h = cx
        .debug_bounds(card_sel)
        .expect("diff 文件卡未渲染")
        .size
        .height;
    assert!(
        card_h > px(120. * 22.),
        "diff 文件卡被压缩进视口（高度 {card_h}，应为 ~2640px）：滚动区将无内容可滚"
    );
    let max_offset = scroll.max_offset();
    assert!(
        max_offset.y > px(0.),
        "diff 滚动容器丢失视口约束（max_offset.y = {max_offset:?}），滚轮将无法滚动"
    );

    // 行为验证：在 diff 滚动区实测 bounds 中心滚轮，偏移必须变化。
    let scroll_sel: &'static str = Box::leak("dbg-diff-scroll".to_string().into_boxed_str());
    let scroll_bounds = cx.debug_bounds(scroll_sel).expect("滚动区未渲染");
    let center = scroll_bounds.center();
    cx.simulate_event(ScrollWheelEvent {
        position: center,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    let offset = scroll.offset();
    assert!(
        offset.y != px(0.),
        "diff 区域滚轮事件未改变滚动偏移（offset.y = {offset:?}）"
    );
}
