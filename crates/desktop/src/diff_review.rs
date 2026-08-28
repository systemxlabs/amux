use std::collections::HashSet;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, checkbox::Checkbox, label::Label, notification::Notification as UiNotification,
    scroll::ScrollableElement, spinner::Spinner, tag::Tag, *,
};

use protocol::{
    ContentBlock, GitChangeStatus, OpResult, SessionPromptParams, WorkspaceDiffParams,
    WorkspaceDiffResult, WorkspaceRestoreParams,
};

use crate::diff::{diff_lines, DiffLineKind};
use crate::logic::{build_changed_file_tree, DiffFileTreeNode};

use crate::app::{AmuxApp, Selected};

impl AmuxApp {
    pub(crate) fn load_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine && m.sessions.iter().any(|s| s.id == *id) => {
                id.clone()
            }
            _ => return,
        };
        let client = m.client.clone();
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.diff_request_id = m.diff_request_id.saturating_add(1);
                m.diff_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff_loading = true;
            m.diff_error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceDiffParams {
                session_id: session_id.clone(),
                path: None,
            };
            let res = client
                .request::<_, WorkspaceDiffResult>(protocol::method::WORKSPACE_DIFF, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                let is_current = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                ) && this
                    .machines
                    .get(machine)
                    .is_some_and(|m| m.diff_request_id == request_id);
                if !is_current {
                    return;
                }
                let mut error_message = None;
                if let Some(m) = this.machines.get_mut(machine) {
                    match &res {
                        Ok(r) => {
                            m.diff_files = r.files.clone();
                            m.diff_not_repo = r.not_repo;
                        }
                        Err(error) => {
                            m.diff_files.clear();
                            m.diff_not_repo = false;
                            error_message = Some(format!("加载改动失败：{error}"));
                        }
                    }
                    m.diff_loading = false;
                    m.diff_error = error_message.clone();
                }
                if let Some(error) = error_message {
                    w.push_notification(
                        UiNotification::error(error.clone()).title("无法加载改动"),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn restore_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: Option<String>,
        patch: Option<String>,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let Some(session_id) = self.selected.as_ref().and_then(|selected| match selected {
            Selected::Session { id, .. } => Some(id.clone()),
            Selected::Workflow { .. } => None,
        }) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceRestoreParams {
                session_id: session_id.clone(),
                path,
                patch,
            };
            let res = client
                .request::<_, OpResult>(protocol::method::WORKSPACE_RESTORE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match res {
                    Ok(result) if result.ok => this.load_diff(w, cx, machine),
                    Ok(result) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.workspace_error =
                                Some(result.message.unwrap_or_else(|| "恢复改动失败".into()));
                        }
                    }
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.workspace_error = Some(format!("恢复改动失败：{error}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn toggle_diff_selection(
        &mut self,
        machine: usize,
        path: String,
        hunk: Option<usize>,
        _cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let key = (path, hunk);
        if !m.diff_selection.remove(&key) {
            m.diff_selection.insert(key);
        }
    }

    pub(crate) fn is_diff_selected(&self, machine: usize, path: &str, hunk: Option<usize>) -> bool {
        self.machine(machine)
            .is_some_and(|m| m.diff_selection.contains(&(path.to_string(), hunk)))
    }

    pub(crate) fn clear_diff_selection(&mut self, machine: usize, _cx: &mut Context<Self>) {
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff_selection.clear();
        }
    }

    pub(crate) fn send_selected_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let selection: HashSet<(String, Option<usize>)> = m.diff_selection.clone();
        let files = m.diff_files.clone();
        let Some(Selected::Session { id, .. }) = self.selected.clone() else {
            return;
        };
        let session_id = id.clone();

        let mut patches: Vec<String> = Vec::new();
        for f in &files {
            let file_selected = selection.contains(&(f.path.clone(), None));
            for (i, h) in f.hunks.iter().enumerate() {
                if file_selected || selection.contains(&(f.path.clone(), Some(i))) {
                    patches.push(format!("// {}\n{}", f.path, h.patch));
                }
            }
            // 没有拆分 hunk 时（如新增/删除整文件），整文件选中用完整 patch。
            if file_selected && f.hunks.is_empty() {
                patches.push(format!("// {}\n{}", f.path, f.patch));
            }
        }
        if patches.is_empty() {
            return;
        }
        let prompt = format!(
            "请审查以下选中的代码改动并给出意见或执行所需修改：\n```diff\n{}\n```",
            patches.join("\n")
        );
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPromptParams {
                session_id: session_id.clone(),
                input: vec![ContentBlock::Text { text: prompt }],
            };
            let _ = client
                .request_ok(
                    protocol::method::SESSION_PROMPT,
                    Some(serde_json::to_value(&params).unwrap()),
                )
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                this.refresh_dialog(w, cx, machine, session_id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 递归渲染改动文件树的节点（目录 + 文件叶子）。
    /// 目录行全展开；文件行点击后滚动到右侧对应 diff 块。层级经缩进左对齐。
    pub(crate) fn push_diff_file_tree_nodes(
        &self,
        nodes: &std::collections::BTreeMap<String, DiffFileTreeNode>,
        out: &mut Vec<gpui::AnyElement>,
        depth: usize,
        machine_idx: usize,
        cx: &mut Context<Self>,
    ) {
        for (name, node) in nodes {
            match node {
                DiffFileTreeNode::Dir(children) => {
                    out.push(
                        h_flex()
                            .items_center()
                            .pl(px((depth * 12 + 4) as f32))
                            .py_0p5()
                            .child(
                                Label::new(name.clone())
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .into_any_element(),
                    );
                    self.push_diff_file_tree_nodes(children, out, depth + 1, machine_idx, cx);
                }
                DiffFileTreeNode::File { path_index } => {
                    let fi = *path_index;
                    let (additions, deletions) = {
                        let files = &self.machines[machine_idx].diff_files;
                        let f = &files[fi];
                        (f.additions, f.deletions)
                    };
                    let tree_path = self.machines[machine_idx].diff_files[fi].path.clone();
                    let diff_scroll = self.diff_scroll.clone();
                    out.push(
                        h_flex()
                            .items_center()
                            .pl(px((depth * 12 + 4) as f32))
                            .min_w_0()
                            .child(
                                Button::new(format!("diff-tree-{tree_path}"))
                                    .xsmall()
                                    .ghost()
                                    .label(name.clone())
                                    .on_click(cx.listener(move |_this, _ev, _window, _cx| {
                                        diff_scroll.scroll_to_top_of_item(fi);
                                    })),
                            )
                            .child(
                                Label::new(format!("+{additions}"))
                                    .text_xs()
                                    .ml_1()
                                    .text_color(cx.theme().success),
                            )
                            .child(
                                Label::new(format!("-{deletions}"))
                                    .text_xs()
                                    .text_color(cx.theme().danger),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
    }

    pub(crate) fn render_diff_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let machine = self.active_machine();
        let files = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_files.clone())
            .unwrap_or_default();
        let not_repo = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_not_repo)
            .unwrap_or(false);
        let diff_loading = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_loading)
            .unwrap_or(false);
        let diff_error = machine
            .and_then(|i| self.machine(i))
            .and_then(|m| m.diff_error.clone());
        let has_selection = machine
            .and_then(|i| self.machine(i))
            .is_some_and(|m| !m.diff_selection.is_empty());
        let can_send = matches!(&self.selected, Some(Selected::Session { .. }));
        let diff_tree_collapsed = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_tree_collapsed)
            .unwrap_or(false);
        let diff_changes_collapsed = machine
            .and_then(|i| self.machine(i))
            .map(|m| m.diff_changes_collapsed)
            .unwrap_or(false);
        let mut content_children: Vec<gpui::AnyElement> = Vec::new();
        let toolbar = h_flex()
            .items_center()
            .gap_2()
            .child(
                Label::new("改动审查")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(div().flex_1())
            .when(has_selection && can_send, |h| {
                h.child(
                    Button::new("diff-send-selected")
                        .small()
                        .primary()
                        .label("发送选中到会话")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.send_selected_diff(window, cx, machine);
                            }
                        })),
                )
                .child(
                    Button::new("diff-clear-selection")
                        .small()
                        .label("清空选择")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.clear_diff_selection(machine, cx);
                                cx.notify();
                            }
                        })),
                )
            })
            .child(
                Button::new("diff-toggle-tree")
                    .small()
                    .ghost()
                    .label(if diff_tree_collapsed {
                        "展开文件树"
                    } else {
                        "折叠文件树"
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        if let Some(machine) = this.active_machine() {
                            if let Some(view) = this.machines.get_mut(machine) {
                                view.diff_tree_collapsed = !view.diff_tree_collapsed;
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("diff-toggle-changes")
                    .small()
                    .ghost()
                    .label(if diff_changes_collapsed {
                        "展开改动"
                    } else {
                        "折叠改动"
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        if let Some(machine) = this.active_machine() {
                            if let Some(view) = this.machines.get_mut(machine) {
                                view.diff_changes_collapsed = !view.diff_changes_collapsed;
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("close-panel-diff")
                    .small()
                    .ghost()
                    .icon(IconName::Close)
                    .tooltip("关闭面板")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.set_panel(window, cx, None);
                    })),
            )
            .into_any_element();
        if let Some(error) = diff_error {
            content_children.push(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Label::new(error).text_sm().text_color(cx.theme().danger))
                    .into_any_element(),
            );
        } else if diff_loading {
            content_children.push(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Spinner::new())
                    .child(
                        Label::new("正在加载改动…")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element(),
            );
        } else if not_repo {
            content_children.push(
                Label::new("当前工作目录不是 git 仓库")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            );
        } else if files.is_empty() {
            content_children.push(
                Label::new("暂无改动")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element(),
            );
        }
        let Some(machine_idx) = machine else {
            return v_flex()
                .w_full()
                .h_full()
                .gap_2()
                .p_3()
                .bg(cx.theme().popover)
                .border_l_1()
                .border_color(cx.theme().border)
                .child(toolbar)
                .into_any();
        };
        // 左侧文件树：按路径聚合的目录树，默认全展开、左对齐，仅含改动文件
        let changed_tree =
            build_changed_file_tree(&files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>());
        let mut tree_items: Vec<gpui::AnyElement> = Vec::new();
        for f in files.iter() {
            let path = f.path.clone();
            let patch = f.patch.clone();
            let additions = f.additions;
            let deletions = f.deletions;
            let file_selected = self.is_diff_selected(machine_idx, &path, None);
            let path_for_restore = path.clone();
            let patch_for_restore = patch.clone();
            let mut file_children: Vec<gpui::AnyElement> = Vec::new();
            file_children.push(
                h_flex()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .items_center()
                    .bg(cx.theme().muted.opacity(0.35))
                    .child(
                        Checkbox::new(format!("diff-sel-file-{}", path))
                            .checked(file_selected)
                            .on_click({
                                let app = cx.entity();
                                let path = path.clone();
                                move |_, _window, cx| {
                                    app.update(cx, |this, cx| {
                                        this.toggle_diff_selection(
                                            machine_idx,
                                            path.clone(),
                                            None,
                                            cx,
                                        );
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        // 路径用等宽字体：与 diff 正文一致的技术文本质感
                        Label::new(path.clone())
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .font_weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!("+{additions}"))
                            .text_xs()
                            .text_color(cx.theme().success),
                    )
                    .child(
                        Label::new(format!("-{deletions}"))
                            .text_xs()
                            .text_color(cx.theme().danger),
                    )
                    .child(
                        match &f.status {
                            GitChangeStatus::Added => Tag::success(),
                            GitChangeStatus::Deleted => Tag::danger(),
                            GitChangeStatus::Modified => Tag::warning(),
                        }
                        .small()
                        .rounded_full()
                        .child(
                            Label::new(match &f.status {
                                GitChangeStatus::Added => "A",
                                GitChangeStatus::Deleted => "D",
                                GitChangeStatus::Modified => "M",
                            })
                            .text_xs(),
                        ),
                    )
                    .child(
                        Button::new(format!("restore-{path}"))
                            .small()
                            .ghost()
                            .icon(IconName::Undo)
                            .label("撤销该文件")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                if let Some((machine, _)) = this.selected_workspace() {
                                    this.restore_workspace(
                                        window,
                                        cx,
                                        machine,
                                        Some(path_for_restore.clone()),
                                        Some(patch_for_restore.clone()),
                                    );
                                }
                            })),
                    )
                    .into_any_element(),
            );
            let hunks_iter: Box<dyn Iterator<Item = (usize, &protocol::GitDiffHunk)>> =
                if diff_changes_collapsed {
                    Box::new(std::iter::empty())
                } else {
                    Box::new(f.hunks.iter().enumerate())
                };
            for (hi, h) in hunks_iter {
                let hunk_selected = self.is_diff_selected(machine_idx, &path, Some(hi));
                let hunk_path = path.clone();
                let hunk_patch = h.patch.clone();
                let mut hunk_children: Vec<gpui::AnyElement> = Vec::new();
                hunk_children.push(
                    h_flex()
                        .w_full()
                        .h_7()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .bg(cx.theme().primary.opacity(0.12))
                        .child(
                            Checkbox::new(format!("diff-sel-hunk-{}-{hi}", hunk_path))
                                .checked(hunk_selected)
                                .on_click({
                                    let app = cx.entity();
                                    let hunk_path = hunk_path.clone();
                                    move |_, _window, cx| {
                                        app.update(cx, |this, cx| {
                                            this.toggle_diff_selection(
                                                machine_idx,
                                                hunk_path.clone(),
                                                Some(hi),
                                                cx,
                                            );
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                        .child(
                            Label::new(h.header.clone())
                                .text_xs()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().primary),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(format!("restore-hunk-{path}-{hi}"))
                                .small()
                                .ghost()
                                .icon(IconName::Undo)
                                .label("撤销此块")
                                .on_click(cx.listener({
                                    let hunk_path = hunk_path.clone();
                                    move |this, _ev, window, cx| {
                                        if let Some((machine, _)) = this.selected_workspace() {
                                            this.restore_workspace(
                                                window,
                                                cx,
                                                machine,
                                                Some(hunk_path.clone()),
                                                Some(hunk_patch.clone()),
                                            );
                                        }
                                    }
                                })),
                        )
                        .into_any_element(),
                );
                for line in diff_lines(h) {
                    let (background, marker, marker_color) = match line.kind {
                        DiffLineKind::Addition => {
                            (cx.theme().success.opacity(0.16), "+", cx.theme().success)
                        }
                        DiffLineKind::Deletion => {
                            (cx.theme().danger.opacity(0.16), "-", cx.theme().danger)
                        }
                        DiffLineKind::Context => {
                            (cx.theme().popover, " ", cx.theme().muted_foreground)
                        }
                    };
                    hunk_children.push(
                        // 代码视图：行号/标记为对齐的等宽数据列，固定像素宽度以保持跨行对齐
                        h_flex()
                            .w_full()
                            .min_h(px(22.))
                            .items_center()
                            .bg(background)
                            .child(
                                div()
                                    .w(px(48.))
                                    .h_full()
                                    .px_2()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(cx.theme().border.opacity(0.45))
                                    .child(
                                        Label::new(
                                            line.old_number
                                                .map(|number| number.to_string())
                                                .unwrap_or_default(),
                                        )
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(48.))
                                    .h_full()
                                    .px_2()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(cx.theme().border.opacity(0.45))
                                    .child(
                                        Label::new(
                                            line.new_number
                                                .map(|number| number.to_string())
                                                .unwrap_or_default(),
                                        )
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .child(
                                div().w(px(24.)).h_full().justify_center().child(
                                    Label::new(marker)
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(marker_color),
                                ),
                            )
                            .child(
                                Label::new(line.content)
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .whitespace_nowrap()
                                    .flex_shrink_0(),
                            )
                            .into_any_element(),
                    );
                }
                file_children.push(
                    v_flex()
                        .w_full()
                        .gap_0()
                        .children(hunk_children)
                        .into_any_element(),
                );
            }
            content_children.push(
                v_flex()
                    .id(format!("diff-file-{path}"))
                    .w_full()
                    .gap_0()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .overflow_hidden()
                    .children(file_children)
                    .into_any_element(),
            );
        }
        self.push_diff_file_tree_nodes(&changed_tree, &mut tree_items, 0, machine_idx, cx);
        let tree = if diff_tree_collapsed {
            v_flex()
                .w_8()
                .h_full()
                .child(Label::new("树"))
                .into_any_element()
        } else {
            v_flex()
                .w(px(220.0)) // diff 文件树面板固定宽度
                .h_full()
                .min_h_0()
                .gap_1()
                .p_1()
                .bg(cx.theme().muted)
                .rounded_md()
                .overflow_y_scrollbar()
                .child(
                    Label::new("改动文件")
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .children(tree_items)
                .into_any_element()
        };
        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .gap_2()
                    .child(tree)
                    .child(
                        // 注意：h_flex() 默认 items_center，子项高度会退化为内容高度，
                        // 必须显式 h_full 约束为行高，否则 overflow_y_scroll 不生效
                        div()
                            .id("diff-panel")
                            .v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .gap_2()
                            .overflow_y_scroll()
                            .track_scroll(&self.diff_scroll)
                            .children(content_children),
                    ),
            )
            .into_any()
    }
}
