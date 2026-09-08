//! 改动审查 diff 滚动区布局回归测试：用真实 AmuxApp 渲染整条布局链。
//!
//! 历史回归（见 commit df93aa2）：h_flex 默认 items_center、taffy flex 收缩
//! 会把滚动容器子项压进视口内，导致滚动区 max_offset 为 0。
//! 虚拟化后（v_virtual_list）滚动区改为按行高虚拟渲染：本测试锁定不变量为
//! 1) 滚动容器 max_offset 反映全部行的总高（内容远大于视口仍可滚）；
//! 2) 文件头行保持固定行高（40px，虚拟列表的尺寸契约）；
//! 3) 滚轮事件能改变滚动偏移。

use std::sync::Arc;

use gpui::{
    div, point, px, size, AppContext, IntoElement, Modifiers, ParentElement, Render, ScrollDelta,
    ScrollWheelEvent, Styled, Window,
};

use protocol::{GitChangeStatus, GitDiffFile, GitDiffHunk};

use amux_desktop::app::{AmuxApp, Panel, Selected};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

struct DiffPanelHostView {
    app: gpui::Entity<AmuxApp>,
}

impl Render for DiffPanelHostView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let panel = self
            .app
            .update(cx, |app, cx| app.render_panel(window, cx))
            .map(|p| p.into_any_element())
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
            machine.diff.update(cx, |st, _| {
                st.files = big_diff_files();
                st.rebuild_rows(window.rem_size());
            });
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Diff);
            app.panel_delta_px = 420.0 * window.scale_factor();
            cx.notify();
            let _ = window;
        });
    });

    // 渲染真实 AmuxApp（根 → main_row → 面板 → diff 虚拟列表）。
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| DiffPanelHostView { app: app.clone() })
            .into_any_element()
    });

    let scroll = cx.update(|_, cx| app.read(cx).diff_scroll.clone());

    // 回归点 1：总内容高（40 文件头 + 28 hunk 头 + 120×22 行 = 2708px）远大于
    // 视口，虚拟列表的 max_offset 必须反映全部行高而非可见行高。
    let max_offset = scroll.max_offset();
    assert!(
        max_offset.y > px(1500.),
        "diff 虚拟列表 max_offset 未反映全部行高（max_offset.y = {max_offset:?}）"
    );

    // 回归点 2：文件头行固定行高 40px（虚拟列表按此定位与渲染）。
    let header_sel: &'static str =
        Box::leak("dbg-diff-file-src/lib.rs".to_string().into_boxed_str());
    let header_h = cx
        .debug_bounds(header_sel)
        .expect("diff 文件头行未渲染")
        .size
        .height;
    assert_eq!(
        header_h,
        px(40.0),
        "文件头行高应锁定为 40px（虚拟列表按此渲染）：{header_h}"
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

/// 回归：文件树中文件行必须相对目录分组头缩进，不能与分组头左对齐。
#[gpui::test]
fn diff_tree_file_rows_indent_under_group(cx: &mut gpui::TestAppContext) {
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
            machine.diff.update(cx, |st, _| {
                st.files = vec![GitDiffFile {
                    path: "src/lib.rs".into(),
                    status: GitChangeStatus::Modified,
                    additions: 1,
                    deletions: 0,
                    patch: String::new(),
                    hunks: vec![],
                }];
                st.rebuild_rows(window.rem_size());
            });
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Diff);
            cx.notify();
        });
    });

    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| DiffPanelHostView { app: app.clone() })
            .into_any_element()
    });

    let group_sel: &'static str = Box::leak("dbg-diff-tree-group-src".to_string().into_boxed_str());
    let file_sel: &'static str =
        Box::leak("dbg-diff-tree-file-src/lib.rs".to_string().into_boxed_str());
    let group_x = cx
        .debug_bounds(group_sel)
        .expect("目录分组头未渲染")
        .origin
        .x;
    let file_x = cx.debug_bounds(file_sel).expect("文件行未渲染").origin.x;
    assert!(
        file_x > group_x + px(8.),
        "文件行未相对目录分组头缩进（group_x = {group_x:?}, file_x = {file_x:?}）"
    );
}

/// 回归：目录内文件不得以根级行重复出现在文件树顶部。曾经把全部改动
/// 文件下标都铺在树顶，导致文件树上方多出一份重复的改动文件列表。
#[gpui::test]
fn diff_tree_no_duplicate_root_rows_for_grouped_files(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    let make_file = |path: &str| GitDiffFile {
        path: path.into(),
        status: GitChangeStatus::Modified,
        additions: 1,
        deletions: 0,
        patch: String::new(),
        hunks: vec![],
    };

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
            machine.diff.update(cx, |st, _| {
                st.files = vec![make_file("src/lib.rs"), make_file("README.md")];
                st.rebuild_rows(window.rem_size());
            });
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: "test".into(),
                id: "session-1".into(),
            });
            app.panel = Some(Panel::Diff);
            cx.notify();
        });
    });

    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| DiffPanelHostView { app: app.clone() })
            .into_any_element()
    });

    // 真正的根级文件仍在树顶渲染
    let readme_sel: &'static str =
        Box::leak("dbg-diff-tree-root-file-README.md".to_string().into_boxed_str());
    cx.debug_bounds(readme_sel)
        .expect("根级文件应渲染在文件树顶部");

    // 目录内文件只出现在目录节点下，树顶不得有重复行
    let grouped_root_sel: &'static str =
        Box::leak("dbg-diff-tree-root-file-src/lib.rs".to_string().into_boxed_str());
    assert!(
        cx.debug_bounds(grouped_root_sel).is_none(),
        "目录内文件不应以根级行重复出现在文件树顶部"
    );
    let grouped_sel: &'static str =
        Box::leak("dbg-diff-tree-file-src/lib.rs".to_string().into_boxed_str());
    cx.debug_bounds(grouped_sel)
        .expect("目录内文件应仍可通过目录节点渲染");
}
