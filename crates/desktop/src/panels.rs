use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, label::Label, scroll::ScrollableElement, spinner::Spinner, text::TextView,
    tooltip::Tooltip, *,
};

use serde_json::json;

use protocol::{
    SessionPlanEntry, SessionPlanStatus, SessionState, WorkspaceListResult, WorkspaceReadParams,
    WorkspaceReadResult,
};

use crate::display::info_row;
use crate::logic::context_usage_text;
use crate::machine::WorkspaceDirectory;
use crate::text::{format_local_time, TimePrecision};

use crate::app::{AmuxApp, Panel, Selected};

/// 改动面板图标：文件 diff（文件轮廓内含 +/−）。gpui-component 默认图标集
/// 无对应图标，SVG 由应用自有资产提供（main.rs `AmuxAssets`）。
struct FileDiffIcon;

impl IconNamed for FileDiffIcon {
    fn path(self) -> SharedString {
        "icons/file-diff.svg".into()
    }
}

impl AmuxApp {
    /// 计划面板：展示会话计划（docs/PRD.md「会话计划」），若无则空白。
    /// 仅普通会话持有计划（计划来自该会话 agent 的 ACP `plan` 通知）；
    /// 工作流为多子会话聚合，不展示。
    pub(crate) fn render_plan_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let plan = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .map(|v| v.plan.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("会话计划")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-plan")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭面板")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(
                div()
                    .id("plan-panel")
                    .debug_selector(|| "plan-panel".into())
                    .flex_1()
                    .v_flex()
                    .gap_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.plan_scroll)
                    .children(plan.iter().map(|e| Self::plan_entry_row(e, cx))),
            )
            .into_any()
    }

    /// 单条计划条目：状态符号 + 描述。完成置灰，进行中高亮（docs/PRD.md「会话计划」）。
    fn plan_entry_row(entry: &SessionPlanEntry, cx: &mut Context<AmuxApp>) -> gpui::AnyElement {
        let (glyph, color) = match entry.status {
            SessionPlanStatus::Completed => ("✓", cx.theme().muted_foreground),
            SessionPlanStatus::InProgress => ("●", cx.theme().primary),
            SessionPlanStatus::Pending => ("○", cx.theme().muted_foreground),
        };
        let done = entry.status == SessionPlanStatus::Completed;
        h_flex()
            .items_start()
            .gap_1p5()
            .py_0p5()
            .child(Label::new(glyph).text_sm().text_color(color).w(px(14.0)))
            .child(
                Label::new(entry.content.clone())
                    .text_sm()
                    .when(done, |l| l.text_color(cx.theme().muted_foreground))
                    .flex_1(),
            )
            .into_any_element()
    }

    /// 终端面板：当前会话上下文的终端标签 + 活动终端视图（仅普通会话入口可达）。
    pub(crate) fn render_terminal_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((machine_idx, session_id)) = self.open_session_target() else {
            return v_flex()
                .w_full()
                .h_full()
                .p_3()
                .bg(cx.theme().popover)
                .child(Label::new("未选择会话"))
                .into_any();
        };
        let (terminals, active_terminal, online) = self
            .machine(machine_idx)
            .map(|m| {
                (
                    m.terminals
                        .iter()
                        .filter(|t| t.session_id == session_id)
                        .cloned()
                        .collect::<Vec<_>>(),
                    m.active_terminal.clone(),
                    m.status.online(),
                )
            })
            .unwrap_or_default();

        // 终端标签行：点击切换 + 关闭；末尾“新建终端”
        let mut tabs = h_flex().flex_wrap().gap_1();
        for entry in &terminals {
            let entry_id = entry.id.clone();
            let entry_close = entry.id.clone();
            let tab_id = entry.id.clone();
            let active = active_terminal.as_deref() == Some(entry.id.as_str());
            tabs = tabs.child(
                h_flex()
                    .id(format!("terminal-tab-{entry_id}"))
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(active, |d| d.bg(cx.theme().list_active))
                    .when(!active, |d| d.bg(cx.theme().muted.opacity(0.35)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some(m) = this.machine_mut(machine_idx) {
                            m.active_terminal = Some(entry_id.clone());
                        }
                        let focus = this
                            .machine(machine_idx)
                            .and_then(|m| m.terminals.iter().find(|t| t.id == entry_id))
                            .map(|t| t.view.read(cx).focus.clone());
                        if let Some(focus) = focus {
                            window.focus(&focus, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        Label::new(entry.title.clone())
                            .text_xs()
                            .max_w_24()
                            .truncate(),
                    )
                    .child(
                        Button::new(format!("terminal-tab-close-{tab_id}"))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.close_terminal(cx, machine_idx, entry_close.clone());
                            })),
                    ),
            );
        }
        tabs = tabs.child(
            Button::new("terminal-new")
                .small()
                .ghost()
                .icon(IconName::Plus)
                .tooltip("新建终端")
                .disabled(!online)
                .on_click(cx.listener(move |this, _ev, window, cx| {
                    this.spawn_terminal(window, cx, machine_idx);
                })),
        );

        let active_view = terminals
            .iter()
            .find(|t| active_terminal.as_deref() == Some(t.id.as_str()))
            .map(|t| t.view.clone());

        let has_active = active_view.is_some();
        v_flex()
            .w_full()
            .h_full()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        Label::new("终端")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-terminal")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭面板")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(h_flex().flex_wrap().gap_1().px_3().pb_2().child(tabs))
            .child(
                // v_flex：终端视图依赖 flex 拉伸撑满剩余高度（块级 div 会让
                // 子容器高度塌缩成内容行数）
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when_some(active_view, |body, view| body.child(view))
                    .when(!has_active, |body| {
                        body.child(
                            v_flex().flex_1().items_center().justify_center().child(
                                Label::new(if online {
                                    "暂无终端，点击 + 新建"
                                } else {
                                    "机器离线，无法使用终端"
                                })
                                .text_sm()
                                .text_color(cx.theme().muted_foreground),
                            ),
                        )
                    }),
            )
            .into_any()
    }
    pub(crate) fn load_workspace_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
        offset: usize,
    ) {
        let Some((selected_machine, session_id)) = self.open_session_target() else {
            return;
        };
        if selected_machine != machine {
            return;
        }
        let Some(client) = self.machine(machine).map(|m| m.client.clone()) else {
            return;
        };
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.workspace_list_request_id = m.workspace_list_request_id.saturating_add(1);
                m.workspace_list_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.workspace_loading.insert(path.clone());
        }
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let directory_path = path.clone();
            let params = json!({
                "sessionId": session_id.clone(),
                "path": if path.is_empty() { None } else { Some(path.clone()) },
                "offset": offset,
                "limit": 200,
            });
            let res = client
                .request::<_, WorkspaceListResult>(protocol::method::WORKSPACE_LIST, Some(params))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                let selected_session_matches = this.is_selected_session(machine, &session_id);
                let Some(m) = this.machines.get_mut(machine) else {
                    return;
                };
                if !selected_session_matches || m.workspace_list_request_id != request_id {
                    return;
                }
                m.workspace_loading.remove(&directory_path);
                match res {
                    Ok(result) => {
                        if offset == 0 {
                            m.workspace_directories.insert(
                                directory_path.clone(),
                                WorkspaceDirectory {
                                    entries: result.entries,
                                    has_more: result.has_more,
                                    next_offset: result.next_offset,
                                },
                            );
                        } else {
                            let directory = m
                                .workspace_directories
                                .entry(directory_path.clone())
                                .or_default();
                            directory.entries.extend(result.entries);
                            directory.has_more = result.has_more;
                            directory.next_offset = result.next_offset;
                        }
                        m.workspace_error = None;
                    }
                    Err(error) => {
                        m.workspace_error = Some(format!("工作目录列表失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn load_workspace_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
        offset: usize,
    ) {
        let Some((selected_machine, session_id)) = self.open_session_target() else {
            return;
        };
        if selected_machine != machine {
            return;
        }
        let Some(client) = self.machine(machine).map(|m| m.client.clone()) else {
            return;
        };
        let request_id = self
            .machines
            .get_mut(machine)
            .map(|m| {
                m.workspace_read_request_id = m.workspace_read_request_id.saturating_add(1);
                m.workspace_read_request_id
            })
            .unwrap_or_default();
        if let Some(m) = self.machines.get_mut(machine) {
            m.workspace_read_loading = true;
            m.workspace_error = None;
            if offset == 0 {
                m.workspace_file = Some(path.clone());
                m.workspace_content.clear();
                m.workspace_read_has_more = false;
                m.workspace_read_next_offset = 0;
            }
        }
        cx.notify();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = WorkspaceReadParams {
                session_id: session_id.clone(),
                path,
                offset,
                limit: 400,
            };
            let res = client
                .request::<_, WorkspaceReadResult>(protocol::method::WORKSPACE_READ, Some(params))
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                let selected_session_matches = this.is_selected_session(machine, &session_id);
                let Some(m) = this.machines.get_mut(machine) else {
                    return;
                };
                if !selected_session_matches || m.workspace_read_request_id != request_id {
                    return;
                }
                m.workspace_read_loading = false;
                match res {
                    Ok(result) => {
                        if offset == 0 {
                            m.workspace_content = result.content;
                        } else {
                            m.workspace_content.push_str(&result.content);
                        }
                        m.workspace_file = Some(result.path);
                        m.workspace_error = None;
                        m.workspace_read_has_more = result.has_more;
                        m.workspace_read_next_offset = result.next_offset;
                    }
                    Err(error) => {
                        m.workspace_error = Some(format!("读取文件失败：{error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn render_workspace_tree(
        &self,
        machine_idx: usize,
        path: &str,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(machine) = self.machine(machine_idx) else {
            return Vec::new();
        };
        let Some(directory) = machine.workspace_directories.get(path) else {
            return if machine.workspace_loading.contains(path) {
                vec![Label::new("加载中…")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element()]
            } else {
                Vec::new()
            };
        };
        let entries = directory.entries.clone();
        let has_more = directory.has_more;
        let next_offset = directory.next_offset;
        let loading = machine.workspace_loading.contains(path);
        let expanded_paths = machine.workspace_expanded.clone();
        let selected_file = machine.workspace_file.as_deref();
        let mut children = Vec::new();

        for entry in entries {
            let entry_path = entry.path.clone();
            let is_dir = entry.is_dir;
            let expanded = is_dir && expanded_paths.contains(&entry_path);
            let selected = !is_dir && selected_file == Some(entry_path.as_str());
            let click_path = entry_path.clone();
            // Button 保留了树节点的整行热区，同时提供焦点、Tab 和键盘激活语义。
            // 通过全宽子布局抵消 Button 内容槽的居中默认值，让树仍沿层级脊柱左对齐。
            let row = Button::new(format!("workspace-entry-{entry_path}"))
                .small()
                .ghost()
                .w_full()
                .selected(selected)
                .on_click(cx.listener(move |this, _ev, window, cx| {
                    let Some(machine) = this.active_machine() else {
                        return;
                    };
                    if is_dir {
                        if !this
                            .machines
                            .get_mut(machine)
                            .is_some_and(|m| m.workspace_expanded.remove(&click_path))
                        {
                            if let Some(m) = this.machines.get_mut(machine) {
                                m.workspace_expanded.insert(click_path.clone());
                            }
                            let needs_load = this
                                .machine(machine)
                                .map(|m| !m.workspace_directories.contains_key(&click_path))
                                .unwrap_or(false);
                            if needs_load {
                                this.load_workspace_list(
                                    window,
                                    cx,
                                    machine,
                                    click_path.clone(),
                                    0,
                                );
                            }
                        }
                    } else {
                        this.load_workspace_file(window, cx, machine, click_path.clone(), 0);
                    }
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .w_full()
                        .justify_start()
                        .gap_1p5()
                        .pl(rems(0.5 + depth as f32 * 0.875))
                        .child(
                            Icon::new(if is_dir {
                                if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                }
                            } else {
                                IconName::File
                            })
                            .xsmall()
                            .flex_none()
                            .text_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().muted_foreground
                            }),
                        )
                        .child(
                            Label::new(entry.name.clone())
                                .text_sm()
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        ),
                );
            let mut node = v_flex().child(row);
            if expanded {
                node = node.children(self.render_workspace_tree(
                    machine_idx,
                    &entry_path,
                    depth + 1,
                    cx,
                ));
            }
            children.push(node.into_any_element());
        }

        if has_more {
            let path_for_click = path.to_string();
            children.push(
                Button::new(format!("workspace-load-more-{path}"))
                    .small()
                    .ghost()
                    .label(if loading {
                        "加载中…"
                    } else {
                        "加载更多"
                    })
                    .disabled(loading)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        if let Some(machine) = this.active_machine() {
                            this.load_workspace_list(
                                window,
                                cx,
                                machine,
                                path_for_click.clone(),
                                next_offset,
                            );
                        }
                    }))
                    .into_any_element(),
            );
        }
        if children.is_empty() && !loading {
            children.push(
                Label::new(if depth == 0 {
                    "（目录为空）"
                } else {
                    "（空目录）"
                })
                .px_2()
                .pl(rems(0.5 + depth as f32 * 0.875)) // 目录树缩进：层级间距随 rem 缩放
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .into_any_element(),
            );
        }
        children
    }

    pub(crate) fn render_workspace_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(machine_idx) = self.active_machine() else {
            return v_flex()
                .w_full()
                .h_full()
                .p_3()
                .bg(cx.theme().popover)
                .child(Label::new("未选择会话"))
                .into_any();
        };
        let Some(machine) = self.machine(machine_idx) else {
            return div().into_any();
        };
        let workspace_file = machine.workspace_file.clone();
        let workspace_content = machine.workspace_content.clone();
        let workspace_error = machine.workspace_error.clone();
        let workspace_read_loading = machine.workspace_read_loading;
        let read_has_more = machine.workspace_read_has_more;
        let read_next_offset = machine.workspace_read_next_offset;
        let file = workspace_file.clone();
        // 折叠时左侧文件树整个不渲染，展开/折叠由工具栏按钮控制
        let tree = (!machine.workspace_tree_collapsed).then(|| {
            v_flex()
                .gap_0()
                .w_56() // 文件树面板宽度：随 rem 缩放
                .p_1()
                .bg(cx.theme().muted.opacity(0.35))
                .rounded_md()
                .overflow_y_scrollbar()
                .children(self.render_workspace_tree(machine_idx, "", 0, cx))
        });

        let mut content = v_flex().flex_1().min_w_0().h_full().gap_2().child(
            Label::new(
                workspace_file
                    .clone()
                    .unwrap_or_else(|| "选择文件查看内容".to_string()),
            )
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground),
        );
        if let Some(error) = workspace_error {
            content = content.child(Label::new(error).text_sm().text_color(cx.theme().danger));
        } else if workspace_read_loading {
            content = content.child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .child(Spinner::new())
                    .child(
                        Label::new("正在读取文件…")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
        } else if let Some(path) = file {
            content = content.child(
                TextView::markdown(
                    "workspace-file-content",
                    format!("```text\n{}\n```", workspace_content),
                )
                .selectable(true),
            );
            if read_has_more {
                content = content.child(
                    Button::new("workspace-read-more")
                        .small()
                        .ghost()
                        .label(if workspace_read_loading {
                            "读取中…"
                        } else {
                            "加载更多内容"
                        })
                        .disabled(workspace_read_loading)
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(machine) = this.active_machine() {
                                this.load_workspace_file(
                                    window,
                                    cx,
                                    machine,
                                    path.clone(),
                                    read_next_offset,
                                );
                            }
                        })),
                );
            }
        } else {
            content = content.child(
                Label::new("选择文件查看文本内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
        }

        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Label::new("工作目录")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("workspace-toggle-tree")
                            .small()
                            .ghost()
                            .label(if machine.workspace_tree_collapsed {
                                "展开文件树"
                            } else {
                                "折叠文件树"
                            })
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                if let Some(machine) = this.active_machine() {
                                    if let Some(m) = this.machines.get_mut(machine) {
                                        m.workspace_tree_collapsed = !m.workspace_tree_collapsed;
                                    }
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("close-panel-workspace")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭面板")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .children(tree)
                    .child(content),
            )
            .into_any()
    }

    pub(crate) fn render_detail_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(meta) = self.selected_meta() else {
            return div().w_full().child(Label::new("未选择会话")).into_any();
        };
        // 工作流会话详情数据仅来自应用侧会话元数据（OrcSession，见 wfstore）：
        // 无工作目录等普通会话字段，下方对应行按选中类型过滤
        let is_workflow = matches!(self.selected, Some(Selected::Workflow { .. }));
        let mut body = v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("会话详情")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel2")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭面板")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(info_row(
                "ID",
                &meta.id,
                cx.theme().muted_foreground,
                cx.theme().foreground,
            ))
            .child(info_row(
                "Agent",
                &meta.agent,
                cx.theme().muted_foreground,
                cx.theme().foreground,
            ))
            // 工作目录（仅普通会话展示：工作流会话无工作目录）
            .when(!is_workflow, |view| {
                view.child(
                    div()
                        .debug_selector(|| "detail-cwd-row".into())
                        .child(info_row(
                            "工作目录",
                            &meta.cwd,
                            cx.theme().muted_foreground,
                            cx.theme().foreground,
                        )),
                )
            })
            .when(!is_workflow && !meta.worktree_dir.is_empty(), |view| {
                // worktree 会话：agent 实际工作在工作树内，展示以便定位
                view.child(info_row(
                    "worktree",
                    &meta.worktree_dir,
                    cx.theme().muted_foreground,
                    cx.theme().foreground,
                ))
            })
            .child(info_row(
                "状态",
                if meta.state == SessionState::Busy {
                    "工作中"
                } else {
                    "空闲"
                },
                cx.theme().muted_foreground,
                cx.theme().foreground,
            ))
            .when(
                matches!(self.selected, Some(Selected::Session { .. }))
                    && context_usage_text(meta.context_size, meta.context_window_size).is_some(),
                |view| {
                    // 会话上下文占用（docs/DESIGN.md：usage_update 记录已用/窗口，token）
                    view.child(info_row(
                        "上下文",
                        &context_usage_text(meta.context_size, meta.context_window_size)
                            .expect("上方已判非 None"),
                        cx.theme().muted_foreground,
                        cx.theme().foreground,
                    ))
                },
            )
            .child(info_row(
                "创建时间",
                &format_local_time(meta.created_at, TimePrecision::Seconds),
                cx.theme().muted_foreground,
                cx.theme().foreground,
            ));
        if let Some(Selected::Session { machine, .. }) = &self.selected {
            if let Some(machine_view) = self.machine(*machine) {
                body = body
                    .child(info_row(
                        "机器",
                        &machine_view.config.name,
                        cx.theme().muted_foreground,
                        cx.theme().foreground,
                    ))
                    .child(info_row(
                        "机器状态",
                        &machine_view.status.label(),
                        cx.theme().muted_foreground,
                        cx.theme().foreground,
                    ));
            }
        }
        let title = if meta.title.is_empty() {
            "（未命名）".to_string()
        } else {
            meta.title.clone()
        };
        body = body.child(info_row(
            "标题",
            &title,
            cx.theme().muted_foreground,
            cx.theme().foreground,
        ));
        if let Some(Selected::Workflow { id }) = &self.selected {
            // 关联普通会话（仅工作流会话展示）：列表数据仅来自应用侧会话元数据
            // （OrcSession.children，含会话 ID 与所属机器名），不查询各机器状态
            if let Some(wf) = self.workflow(id) {
                let children = wf.children();
                body = body.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .debug_selector(|| "wf-detail-children".into())
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Label::new("关联普通会话")
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(cx.theme().foreground),
                                )
                                .child(
                                    Label::new(children.len().to_string())
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                        .children(children.into_iter().map(|c| {
                            h_flex()
                                .w_full()
                                .gap_1p5()
                                .items_center()
                                .debug_selector(|| "wf-detail-child".into())
                                .child(
                                    Icon::new(IconName::SquareTerminal)
                                        .small()
                                        .text_color(cx.theme().muted_foreground),
                                )
                                .child(Label::new(c.id).text_sm().flex_1().min_w_0().truncate())
                                .child(
                                    Label::new(c.machine_name)
                                        .text_xs()
                                        .flex_none()
                                        .text_color(cx.theme().muted_foreground),
                                )
                        })),
                );
            }
        }
        body.into_any()
    }

    /// 右缘悬浮面板切换栏：图标 + 微标签的纵向导航条（活动栏样式）。
    /// 面板入口按会话类型过滤（docs/PRD.md「右侧面板」）：工作目录/文件改动/
    /// 会话计划/终端仅普通会话展示，工作流会话仅详情与活动。
    pub(crate) fn render_floating_buttons(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_session = matches!(self.selected, Some(Selected::Session { .. }));
        v_flex()
            .gap_0p5()
            .p_1()
            .justify_center()
            .bg(cx.theme().popover)
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .shadow_sm()
            // 工作目录（仅普通会话展示）
            .when(is_session, |rail| {
                rail.child(self.render_rail_button(
                    Panel::Workspace,
                    "float-workspace",
                    "目录",
                    |color| {
                        Icon::new(IconName::FolderOpen)
                            .text_color(color)
                            .into_any_element()
                    },
                    cx,
                ))
            })
            // 改动按钮：文件 diff 图标（内含 +/−）；仅普通会话展示
            .when(is_session, |rail| {
                rail.child(self.render_rail_button(
                    Panel::Diff,
                    "float-diff",
                    "改动",
                    |color| Icon::new(FileDiffIcon).text_color(color).into_any_element(),
                    cx,
                ))
            })
            .child(self.render_rail_button(
                Panel::Detail,
                "float-detail",
                "详情",
                |color| {
                    Icon::new(IconName::Info)
                        .text_color(color)
                        .into_any_element()
                },
                cx,
            ))
            .child(self.render_rail_button(
                Panel::Activities,
                "float-activities",
                "活动",
                |color| {
                    Icon::new(IconName::Inbox)
                        .text_color(color)
                        .into_any_element()
                },
                cx,
            ))
            // 会话计划（仅普通会话展示）
            .when(is_session, |rail| {
                rail.child(self.render_rail_button(
                    Panel::Plan,
                    "float-plan",
                    "计划",
                    |color| {
                        Icon::new(IconName::Map)
                            .text_color(color)
                            .into_any_element()
                    },
                    cx,
                ))
            })
            // 终端（仅普通会话展示）
            .when(is_session, |rail| {
                rail.child(self.render_rail_button(
                    Panel::Terminal,
                    "float-terminal",
                    "终端",
                    |color| {
                        Icon::new(IconName::SquareTerminal)
                            .text_color(color)
                            .into_any_element()
                    },
                    cx,
                ))
            })
    }

    /// 单个面板切换入口（icon-only，悬浮弹出文字标签）：再次点击同一面板即关闭；
    /// 打开工作目录/改动面板时顺带加载。
    /// `icon` 按当前着色构造（普通按钮为单一图标，改动按钮为 +/− 组合）。
    pub(crate) fn render_rail_button(
        &self,
        panel: Panel,
        id: &str,
        label: &'static str,
        icon: impl Fn(Hsla) -> AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = self.panel == Some(panel);
        let icon_color = if active {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };
        div()
            .id(id.to_string())
            .v_flex()
            .items_center()
            .p_2()
            .rounded_md()
            .when(active, |d| d.bg(cx.theme().list_active))
            .hover(|d| d.bg(cx.theme().list_hover))
            .tooltip(move |window, _cx| Tooltip::new(label).build(window, _cx))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                let next = if this.panel == Some(panel) {
                    None
                } else {
                    Some(panel)
                };
                this.set_panel(window, cx, next);
                if next == Some(Panel::Workspace) {
                    if let Some(machine) = this.active_machine() {
                        this.load_workspace_list(window, cx, machine, String::new(), 0);
                    }
                }
                if next == Some(Panel::Diff) {
                    if let Some(Selected::Session { machine, .. }) = this.selected.clone() {
                        this.load_diff(window, cx, machine);
                    }
                }
                // 打开即拉取：refresh_activities 有「面板已打开」守卫，周期轮询
                // 不会补上首次打开前的数据；set_panel 已置位，此处守卫可通过
                if next == Some(Panel::Activities) {
                    if let Some((machine, id)) = this.open_session_target() {
                        this.refresh_activities(window, cx, machine, id);
                    }
                }
                if next == Some(Panel::Plan) {
                    if let Some((machine, id)) = this.open_session_target() {
                        this.refresh_plan(window, cx, machine, id);
                    }
                }
            }))
            .child(icon(icon_color))
            .into_any_element()
    }
}
