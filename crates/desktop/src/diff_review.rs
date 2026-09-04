//! 改动审查面板：每机器的状态实体 + 面板渲染（虚拟化 diff 列表）。

use std::collections::HashSet;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, checkbox::Checkbox, label::Label, notification::Notification as UiNotification,
    scroll::ScrollableElement, spinner::Spinner, tag::Tag, tooltip::Tooltip, *,
};

use protocol::{
    ContentBlock, GitChangeStatus, GitDiffFile, OpResult, SessionPromptParams, WorkspaceDiffParams,
    WorkspaceDiffResult, WorkspaceRestoreParams,
};

use crate::app::AmuxApp;
use crate::diff::{diff_lines, DiffLine, DiffLineKind};
use crate::logic::group_changed_files_by_parent;

impl AmuxApp {
    pub(crate) fn load_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
    ) {
        let Some((selected_machine, session_id)) = self.open_session_target() else {
            return;
        };
        if selected_machine != machine_name {
            return;
        }
        let Some(m) = self.machine_by_name(machine_name) else {
            return;
        };
        if !m.sessions.iter().any(|s| s.id == session_id) {
            return;
        }
        let client = m.client.clone();
        let machine_name = machine_name.to_string();
        let generation = m.connection_generation;
        // 请求 id / loading / error 存于本机器的 DiffReviewState 实体
        let request_id = m.diff.update(cx, |st, _| {
            st.request_id = st.request_id.saturating_add(1);
            st.loading = true;
            st.error = None;
            st.request_id
        });
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
                let is_current = this.is_current_machine_connection(&machine_name, generation)
                    && this.is_selected_session(&machine_name, &session_id)
                    && this
                        .machine_by_name(&machine_name)
                        .is_some_and(|m| m.diff.read(cx).request_id == request_id);
                if !is_current {
                    return;
                }
                let mut error_message = None;
                if let Some(m) = this.machine_mut_by_name(&machine_name) {
                    m.diff.update(cx, |st, _| match &res {
                        Ok(r) => {
                            st.files = r.files.clone();
                            st.not_repo = r.not_repo;
                            st.rebuild_rows(w.rem_size());
                        }
                        Err(error) => {
                            st.files.clear();
                            st.not_repo = false;
                            st.rebuild_rows(w.rem_size());
                            error_message = Some(format!("加载改动失败：{error}"));
                        }
                    });
                    m.diff.update(cx, |st, _| {
                        st.loading = false;
                        st.error = error_message.clone();
                    });
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
        machine_name: &str,
        path: Option<String>,
        patch: Option<String>,
    ) {
        let Some(m) = self.machine_by_name(machine_name) else {
            return;
        };
        let Some(session_id) = self.open_session_target().map(|(_, id)| id) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = machine_name.to_string();
        let generation = m.connection_generation;
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
                if !this.is_current_machine_connection(&machine_name, generation)
                    || !this.is_selected_session(&machine_name, &session_id)
                {
                    return;
                }
                match res {
                    Ok(result) if result.ok => this.load_diff(w, cx, &machine_name),
                    Ok(result) => {
                        if let Some(m) = this.machine_mut_by_name(&machine_name) {
                            m.workspace_error =
                                Some(result.message.unwrap_or_else(|| "恢复改动失败".into()));
                        }
                    }
                    Err(error) => {
                        if let Some(m) = this.machine_mut_by_name(&machine_name) {
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
        machine_name: &str,
        path: String,
        hunk: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machine_mut_by_name(machine_name) else {
            return;
        };
        let key = (path, hunk);
        m.diff.update(cx, |st, _| {
            if !st.selection.remove(&key) {
                st.selection.insert(key);
            }
        });
    }

    pub(crate) fn is_diff_selected(
        &self,
        machine_name: &str,
        path: &str,
        hunk: Option<usize>,
        cx: &Context<Self>,
    ) -> bool {
        self.machine_by_name(machine_name).is_some_and(|m| {
            m.diff
                .read(cx)
                .selection
                .contains(&(path.to_string(), hunk))
        })
    }

    pub(crate) fn clear_diff_selection(&mut self, machine_name: &str, cx: &mut Context<Self>) {
        let Some(machine) = self.machine_idx_by_name(machine_name) else {
            return;
        };
        if let Some(m) = self.machines.get_mut(machine) {
            m.diff.update(cx, |st, _| st.selection.clear());
        }
    }

    pub(crate) fn send_selected_diff(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
    ) {
        let Some(machine) = self.machine_idx_by_name(machine_name) else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        if !m.status.online() {
            if let Some(m) = self.machine_mut(machine) {
                m.notice = Some("机器离线，无法发送选中的改动".into());
            }
            cx.notify();
            return;
        }
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let (selection, files) = {
            let st = m.diff.read(cx);
            (st.selection.clone(), st.files.clone())
        };
        let Some(session_id) = self.open_session_target().map(|(_, id)| id) else {
            return;
        };

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
            let result = client
                .request_ok(protocol::method::SESSION_PROMPT, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if !this.is_current_machine_connection(&machine_name, generation)
                    || !this.is_selected_session(&machine_name, &session_id)
                {
                    return;
                }
                match result {
                    Ok(()) => this.refresh_dialog(w, cx, &machine_name, session_id),
                    Err(error) => {
                        if let Some(m) = this.machine_mut_by_name(&machine_name) {
                            m.notice = Some(format!("发送选中改动失败：{error}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn render_diff_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let machine_name = self.active_machine();
        let machine = machine_name
            .as_deref()
            .and_then(|name| self.machine_by_name(name));
        let files = machine
            .map(|m| m.diff.read(cx).files.clone())
            .unwrap_or_default();
        let not_repo = machine.map(|m| m.diff.read(cx).not_repo).unwrap_or(false);
        let diff_loading = machine.map(|m| m.diff.read(cx).loading).unwrap_or(false);
        let diff_error = machine.and_then(|m| m.diff.read(cx).error.clone());
        let has_selection = machine.is_some_and(|m| !m.diff.read(cx).selection.is_empty());
        let can_send = self.open_session_target().is_some();
        let diff_tree_collapsed = machine
            .map(|m| m.diff.read(cx).tree_collapsed)
            .unwrap_or(false);
        let diff_changes_collapsed = machine
            .map(|m| m.diff.read(cx).changes_collapsed)
            .unwrap_or(false);
        let rem_size = window.rem_size();
        if let Some(m) = machine {
            let needs_rebuild = m.diff.read(cx).item_sizes_rem_size != Some(rem_size);
            if needs_rebuild {
                m.diff.update(cx, |st, _| st.rebuild_rows(rem_size));
            }
        }
        let (_rows, item_sizes, file_header_rows) = machine
            .map(|m| {
                let st = m.diff.read(cx);
                (
                    st.rows.clone(),
                    st.item_sizes.clone(),
                    st.file_header_rows.clone(),
                )
            })
            .unwrap_or_default();
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
                                this.send_selected_diff(window, cx, &machine);
                            }
                        })),
                )
                .child(
                    Button::new("diff-clear-selection")
                        .small()
                        .label("清空选择")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.clear_diff_selection(&machine, cx);
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
                            if let Some(view) = this.machine_mut_by_name(&machine) {
                                view.diff.update(cx, |st, _| {
                                    st.tree_collapsed = !st.tree_collapsed;
                                });
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
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if let Some(machine) = this.active_machine() {
                            if let Some(view) = this.machine_mut_by_name(&machine) {
                                view.diff.update(cx, |st, _| {
                                    st.changes_collapsed = !st.changes_collapsed;
                                    st.rebuild_rows(rem_size);
                                });
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
        let Some(machine_name) = machine_name else {
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
        // 左侧文件树区域：按父目录路径分组展示，分组默认展开、可单独折叠，
        // 文件列表只包含实际发生改动的文件。
        let groups = group_changed_files_by_parent(
            &files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        );
        let collapsed_groups = machine
            .map(|m| m.diff.read(cx).collapsed_groups.clone())
            .unwrap_or_default();
        let mut tree_items: Vec<gpui::AnyElement> = Vec::new();
        for (dir, indices) in &groups {
            let collapsed = collapsed_groups.contains(dir);
            let group_label = if dir.is_empty() {
                "（根目录）".to_string()
            } else {
                dir.clone()
            };
            let group_key = dir.clone();
            let tree_machine = machine_name.clone();
            // 分组头使用 Button，展开状态也能通过键盘和辅助技术访问。
            tree_items.push(
                Button::new(format!("diff-group-{dir}"))
                    .xsmall()
                    .ghost()
                    .w_full()
                    .toggled(!collapsed)
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if let Some(m) = this.machine_mut_by_name(&tree_machine) {
                            m.diff.update(cx, |st, _| {
                                if !st.collapsed_groups.remove(&group_key) {
                                    st.collapsed_groups.insert(group_key.clone());
                                }
                            });
                        }
                        cx.notify();
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .justify_start()
                            .gap_1()
                            .pl_1()
                            .child(
                                Label::new(if collapsed { "▸" } else { "▾" })
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                Label::new(group_label)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .into_any_element(),
            );
            if collapsed {
                continue;
            }
            for fi in indices {
                let Some(f) = files.get(*fi) else { continue };
                let path = f.path.clone();
                let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
                let diff_scroll = self.diff_scroll.clone();
                let item_sizes = item_sizes.clone();
                let app = cx.entity();
                // 行号在本帧内确定（虚拟列表按位置定位），闭包只需捕获数值
                let Some(&row_ix) = file_header_rows.get(*fi) else {
                    continue;
                };
                tree_items.push(
                    // Button 提供整行焦点与键盘激活；内部布局保留文件名和统计列的左对齐。
                    Button::new(format!("diff-tree-{path}"))
                        .xsmall()
                        .ghost()
                        .w_full()
                        .on_click(move |_ev, _window, cx| {
                            // scroll_to_item 是非严格模式，目标行已可见时不滚动；
                            // 点击文件必须定位到对应 diff 区域，直接按行高
                            // 累计设置滚动偏移（行高为文档化固定几何）。
                            // set_offset 不触发重绘，必须显式 notify
                            let y: f32 = item_sizes
                                .iter()
                                .take(row_ix)
                                .map(|s| s.height.as_f32())
                                .sum();
                            diff_scroll.base_handle().set_offset(point(px(0.), px(-y)));
                            app.update(cx, |_, cx| cx.notify());
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .justify_start()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    Label::new(file_name)
                                        .text_xs()
                                        .truncate()
                                        .min_w_0()
                                        .flex_1()
                                        .text_color(cx.theme().foreground),
                                )
                                .child(
                                    Label::new(format!("+{}", f.additions))
                                        .text_xs()
                                        .text_color(cx.theme().success),
                                )
                                .child(
                                    Label::new(format!("-{}", f.deletions))
                                        .text_xs()
                                        .ml_1()
                                        .text_color(cx.theme().danger),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }
        // 虚拟化：只渲染可视范围内的行（行高为文档化几何——diff 行等宽
        // 字符不换行，高度固定）
        let diff_list = v_virtual_list(
            cx.entity(),
            "diff-panel",
            item_sizes,
            move |this, range, window, cx| {
                let Some(machine_name) = this.active_machine() else {
                    return Vec::new();
                };
                this.render_diff_rows(&machine_name, range, window, cx)
            },
        )
        .track_scroll(&self.diff_scroll);
        // 折叠时左侧区域整个不渲染，展开/折叠由工具栏按钮控制
        let tree = (!diff_tree_collapsed).then(|| {
            v_flex()
                .w_48() // diff 文件树宽度：随 rem 缩放并给 diff 内容区让位
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
        });
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
                    .children(tree)
                    .child(
                        // 注意：h_flex() 默认 items_center，子项高度会退化为内容高度，
                        // 必须显式 h_full 约束为行高，否则虚拟列表无视口可滚
                        div()
                            .id("diff-panel")
                            .debug_selector(|| "dbg-diff-scroll".into())
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .child(diff_list),
                    ),
            )
            .into_any()
    }
    /// 渲染 [range) 内的行（虚拟列表回调）。
    pub(crate) fn render_diff_rows(
        &self,
        machine_name: &str,
        range: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(m) = self.machine_by_name(machine_name) else {
            return Vec::new();
        };
        let files = m.diff.read(cx).files.clone();
        let row_kinds = m.diff.read(cx).rows.clone();
        let mut out = Vec::with_capacity(range.len());
        // 同 hunk 的连续行只解析一次（可视行有序，命中率高）；
        // 此前每行都全量解析所在 hunk
        let mut memo_key: (usize, usize) = (usize::MAX, usize::MAX);
        let mut memo_lines: Vec<DiffLine> = Vec::new();
        for ix in range {
            let Some(kind) = row_kinds.get(ix) else {
                continue;
            };
            match *kind {
                DiffRowKind::FileHeader(fi) => {
                    if let Some(f) = files.get(fi) {
                        out.push(self.render_diff_file_header_row(machine_name, fi, f, cx));
                    }
                }
                DiffRowKind::HunkHeader(fi, hi) => {
                    if let Some(f) = files.get(fi) {
                        if let Some(h) = f.hunks.get(hi) {
                            out.push(self.render_diff_hunk_header_row(machine_name, fi, hi, h, cx));
                        }
                    }
                }
                DiffRowKind::Line(fi, hi, li) => {
                    if let Some(f) = files.get(fi) {
                        if let Some(h) = f.hunks.get(hi) {
                            if memo_key != (fi, hi) {
                                memo_lines = diff_lines(h);
                                memo_key = (fi, hi);
                            }
                            if let Some(line) = memo_lines.get(li) {
                                out.push(Self::render_diff_line_row(line, cx));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn render_diff_file_header_row(
        &self,
        machine_name: &str,
        _fi: usize,
        f: &GitDiffFile,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = f.path.clone();
        let path_for_restore = path.clone();
        let patch_for_restore = f.patch.clone();
        let dbg_path = path.clone();
        let selected = self.is_diff_selected(machine_name, &path, None, cx);
        let path_for_tooltip = path.clone();
        let select_machine = machine_name.to_string();
        h_flex()
            .id(ElementId::Name(format!("dbg-diff-file-{path}").into()))
            .debug_selector(move || format!("dbg-diff-file-{dbg_path}"))
            .tooltip(move |window, cx| Tooltip::new(path_for_tooltip.clone()).build(window, cx))
            .w_full()
            .h(rems(2.5))
            .px_2()
            .gap_2()
            .items_center()
            .bg(cx.theme().muted.opacity(0.35))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Checkbox::new(format!("diff-sel-file-{}", path))
                    .checked(selected)
                    .on_click({
                        let app = cx.entity();
                        let path = path.clone();
                        let machine_name = select_machine.clone();
                        move |_, _window, cx| {
                            app.update(cx, |this, cx| {
                                this.toggle_diff_selection(&machine_name, path.clone(), None, cx);
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_hidden()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(
                        Label::new(path.clone())
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .flex_shrink_0(),
                    ),
            )
            .child(
                Label::new(format!("+{}", f.additions))
                    .text_xs()
                    .text_color(cx.theme().success),
            )
            .child(
                Label::new(format!("-{}", f.deletions))
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
                    .tooltip("撤销该文件全部改动")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some((machine, _)) = this.selected_workspace() {
                            this.restore_workspace(
                                window,
                                cx,
                                &machine,
                                Some(path_for_restore.clone()),
                                Some(patch_for_restore.clone()),
                            );
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_diff_hunk_header_row(
        &self,
        machine_name: &str,
        fi: usize,
        hi: usize,
        h: &protocol::GitDiffHunk,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(path) = self
            .machine_by_name(machine_name)
            .and_then(|m| m.diff.read(cx).files.get(fi).map(|f| f.path.clone()))
        else {
            return div().into_any_element();
        };
        let hunk_selected = self.is_diff_selected(machine_name, &path, Some(hi), cx);
        let hunk_path = path;
        let hunk_patch = h.patch.clone();
        let select_machine = machine_name.to_string();
        h_flex()
            .w_full()
            .h(rems(1.75))
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
                        let machine_name = select_machine.clone();
                        move |_, _window, cx| {
                            app.update(cx, |this, cx| {
                                this.toggle_diff_selection(
                                    &machine_name,
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
                Button::new(format!("restore-hunk-{hunk_path}-{hi}"))
                    .small()
                    .ghost()
                    .icon(IconName::Undo)
                    .tooltip("撤销此块改动")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some((machine, _)) = this.selected_workspace() {
                            this.restore_workspace(
                                window,
                                cx,
                                &machine,
                                Some(hunk_path.clone()),
                                Some(hunk_patch.clone()),
                            );
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_diff_line_row(line: &DiffLine, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (background, marker, marker_color) = match line.kind {
            DiffLineKind::Addition => (cx.theme().success.opacity(0.16), "+", cx.theme().success),
            DiffLineKind::Deletion => (cx.theme().danger.opacity(0.16), "-", cx.theme().danger),
            DiffLineKind::Context => (cx.theme().popover, " ", cx.theme().muted_foreground),
        };
        h_flex()
            .w_full()
            .h(rems(1.375))
            .items_center()
            .bg(background)
            .child(
                div()
                    .w(rems(3.))
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
                    .w(rems(3.))
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
                div().w(rems(1.5)).h_full().justify_center().child(
                    Label::new(marker)
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(marker_color),
                ),
            )
            .child(
                Label::new(line.content.clone())
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .whitespace_nowrap()
                    .flex_shrink_0(),
            )
            .into_any_element()
    }
}

/// 扁平行描述（顺序 = 渲染顺序；仅在行模型重建时调用一次）。
fn build_diff_rows(files: &[GitDiffFile], changes_collapsed: bool) -> Vec<DiffRowKind> {
    let mut rows = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        rows.push(DiffRowKind::FileHeader(fi));
        if changes_collapsed {
            continue;
        }
        for (hi, h) in f.hunks.iter().enumerate() {
            rows.push(DiffRowKind::HunkHeader(fi, hi));
            for (li, _) in diff_lines(h).iter().enumerate() {
                rows.push(DiffRowKind::Line(fi, hi, li));
            }
        }
    }
    rows
}

/// 虚拟化 diff 列表的行描述（扁平化：文件头 / hunk 头 / diff 行）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum DiffRowKind {
    FileHeader(usize),
    HunkHeader(usize, usize),
    Line(usize, usize, usize),
}

impl DiffRowKind {
    /// 文档化几何：diff 行等宽字符 whitespace_nowrap 不换行，行高固定，
    /// 虚拟列表据此定位与渲染（超出行数不渲染）。
    pub(crate) fn height(&self, rem_size: Pixels) -> Pixels {
        match self {
            DiffRowKind::FileHeader(_) => rems(2.5).to_pixels(rem_size),
            DiffRowKind::HunkHeader(..) => rems(1.75).to_pixels(rem_size),
            DiffRowKind::Line(..) => rems(1.375).to_pixels(rem_size),
        }
    }
}

/// 每机器的改动审查状态。
/// 独立实体：diff 数据量与交互（选择/折叠/撤销）自成生命周期，
/// 与机器连接状态、会话状态解耦。
#[derive(Default)]
pub struct DiffReviewState {
    pub files: Vec<GitDiffFile>,
    pub(crate) not_repo: bool,
    pub(crate) selection: HashSet<(String, Option<usize>)>,
    /// 陈旧响应丢弃：响应只在其 request_id 仍为最新时写入
    pub(crate) request_id: u64,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) tree_collapsed: bool,
    pub(crate) changes_collapsed: bool,
    /// 单独折叠的分组（父目录路径）；缺失即展开。
    pub(crate) collapsed_groups: HashSet<String>,
    /// 扁平行模型缓存（含每行尺寸与文件头行号索引）：仅在 files /
    /// changes_collapsed 变更的写点重建一次。此前渲染路径每帧重建行模型并对
    /// 每个 hunk 全量解析 diff_lines，大 diff 时是 O(n²)/帧。
    pub(crate) rows: std::rc::Rc<Vec<DiffRowKind>>,
    pub(crate) item_sizes: std::rc::Rc<Vec<gpui::Size<Pixels>>>,
    /// `item_sizes` 使用的窗口 rem 基准；窗口缩放后需重建虚拟行尺寸。
    pub(crate) item_sizes_rem_size: Option<Pixels>,
    /// 文件头所在虚拟行号（按文件下标索引），供文件树点击滚动定位
    pub(crate) file_header_rows: Vec<usize>,
}

impl DiffReviewState {
    /// 行模型重建（files / changes_collapsed 变更后调用）。
    pub fn rebuild_rows(&mut self, rem_size: Pixels) {
        let rows = build_diff_rows(&self.files, self.changes_collapsed);
        // 文件头行号：顺序扫描一次（rows 与 files 同序）
        let mut header_rows = Vec::with_capacity(self.files.len());
        for (ix, row) in rows.iter().enumerate() {
            if let DiffRowKind::FileHeader(fi) = row {
                if *fi == header_rows.len() {
                    header_rows.push(ix);
                }
            }
        }
        self.rows = std::rc::Rc::new(rows);
        self.item_sizes = std::rc::Rc::new(
            self.rows
                .iter()
                .map(|k| gpui::size(px(100.), k.height(rem_size)))
                .collect(),
        );
        self.item_sizes_rem_size = Some(rem_size);
        self.file_header_rows = header_rows;
    }
}
