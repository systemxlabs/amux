use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, label::Label, scroll::ScrollableElement, separator::Separator, spinner::Spinner,
    text::TextView, *,
};

use serde_json::json;

use protocol::{SessionState, WorkspaceListResult, WorkspaceReadParams, WorkspaceReadResult};

use crate::display::info_row;
use crate::logic::context_usage_text;
use crate::machine::WorkspaceDirectory;
use crate::text::{format_local_time, TimePrecision};

use crate::app::{AmuxApp, Panel, Selected};

impl AmuxApp {
    pub(crate) fn load_workspace_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        path: String,
        offset: usize,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine => id.clone(),
            _ => return,
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
                let selected_session_matches = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                );
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
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let session_id = match &self.selected {
            Some(Selected::Session {
                id,
                machine: selected_machine,
            }) if *selected_machine == machine => id.clone(),
            _ => return,
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
                let selected_session_matches = matches!(
                    &this.selected,
                    Some(Selected::Session {
                        machine: selected_machine,
                        id
                    }) if *selected_machine == machine && id == &session_id
                );
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
            // 手搓行而非 Button：库 Button 内容层硬编码 justify_center，全宽行的
            // 「图标+名称」会被整体居中（第一层级缩进小、错位最明显），无法左对齐
            let row = div()
                .id(format!("workspace-entry-{entry_path}"))
                .w_full()
                .h_6()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .pl(px(8. + depth as f32 * 14.)) // 目录树缩进：随层级深度计算的运行时几何
                .rounded_sm()
                .cursor_pointer()
                .when(selected, |d| d.bg(cx.theme().list_active))
                .hover(|d| d.bg(cx.theme().list_hover))
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
                .pl(px(8. + depth as f32 * 14.)) // 目录树缩进：随层级深度计算的运行时几何
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
        let tree = v_flex()
            .gap_0()
            .w(px(220.0)) // 文件树面板固定宽度
            .p_1()
            .bg(cx.theme().muted.opacity(0.35))
            .rounded_md()
            .overflow_y_scrollbar()
            .children(self.render_workspace_tree(machine_idx, "", 0, cx));

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
                    .child(tree)
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
            .child(info_row(
                "工作目录",
                &meta.cwd,
                cx.theme().muted_foreground,
                cx.theme().foreground,
            ))
            .when(!meta.worktree_dir.is_empty(), |view| {
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
        if let Some(Selected::Workflow { id }) = self.selected.clone() {
            // 无运行中的推进时无需取消（工作流无终态，空闲即可直接下发新指令）
            let busy = self
                .workflow(&id)
                .is_some_and(|w| w.state() == SessionState::Busy);
            let id_cancel = id.clone();
            let id_delete = id.clone();
            body = body
                .child(Separator::horizontal().label("工作流会话"))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("wf-cancel")
                                .small()
                                .when(!busy, |b| b.disabled(true))
                                .label("取消")
                                .on_click(cx.listener(move |this, _ev, window, cx| {
                                    this.cancel_workflow(window, cx, id_cancel.clone());
                                })),
                        )
                        .child(
                            Button::new("wf-delete")
                                .small()
                                .danger()
                                .label("删除工作流")
                                .on_click(cx.listener(move |this, _ev, window, cx| {
                                    this.confirm_delete_workflow(window, cx, id_delete.clone());
                                })),
                        ),
                )
                .child(
                    Label::new(format!(
                        "子会话 {}",
                        self.workflow(&id).map(|w| w.child_count()).unwrap_or(0)
                    ))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground),
                );
            for c in self.workflow(&id).map(|w| w.children()).unwrap_or_default() {
                let step: SharedString = self
                    .machine(c.machine_idx)
                    .and_then(|m| m.sessions.iter().find(|s| s.id == c.id))
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| "（会话不存在）".into())
                    .into();
                body = body.child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .items_center()
                        .child(
                            Icon::new(IconName::SquareTerminal)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(Label::new(step).text_sm().flex_1().min_w_0().truncate())
                        .child(
                            Label::new(c.id)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground),
                        ),
                );
            }
        }
        body.into_any()
    }

    /// 右缘悬浮面板切换栏：图标 + 微标签的纵向导航条（活动栏样式）。
    pub(crate) fn render_floating_buttons(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .p_1()
            .justify_center()
            .bg(cx.theme().popover)
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .shadow_sm()
            .child(self.render_rail_button(
                Panel::Workspace,
                "float-workspace",
                "目录",
                IconName::FolderOpen,
                cx,
            ))
            .child(self.render_rail_button(Panel::Diff, "float-diff", "改动", IconName::File, cx))
            .child(self.render_rail_button(
                Panel::Detail,
                "float-detail",
                "详情",
                IconName::Info,
                cx,
            ))
            .child(self.render_rail_button(
                Panel::Activities,
                "float-activities",
                "活动",
                IconName::Inbox,
                cx,
            ))
    }

    /// 单个面板切换入口：再次点击同一面板即关闭；打开工作目录/改动面板时顺带加载。
    pub(crate) fn render_rail_button(
        &self,
        panel: Panel,
        id: &str,
        label: &'static str,
        icon: IconName,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = self.panel == Some(panel);
        let (icon_color, label_color) = if active {
            (cx.theme().primary, cx.theme().foreground)
        } else {
            (cx.theme().muted_foreground, cx.theme().muted_foreground)
        };
        div()
            .id(id.to_string())
            .v_flex()
            .items_center()
            .gap_0p5()
            .px_2()
            .py_1p5()
            .rounded_md()
            .when(active, |d| d.bg(cx.theme().list_active))
            .hover(|d| d.bg(cx.theme().list_hover))
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
            }))
            .child(Icon::new(icon).text_color(icon_color))
            .child(Label::new(label).text_xs().text_color(label_color))
            .into_any_element()
    }
}
