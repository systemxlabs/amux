//! `AmuxApp` 的视图渲染方法（`render_*`），拆自 `app.rs`。
//! 保持与业务/状态方法分离：本模块只承担 GPUI 视图构建，无状态修改副作用。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    alert::Alert,
    button::*,
    checkbox::Checkbox,
    collapsible::Collapsible,
    input::Input,
    label::Label,
    menu::{ContextMenuExt, PopupMenuItem},
    popover::Popover,
    progress::Progress,
    radio::RadioGroup,
    scroll::ScrollableElement,
    separator::Separator,
    spinner::Spinner,
    tag::Tag,
    text::TextView,
    tooltip::Tooltip,
    *,
};

use serde_json::json;

use protocol::{
    Activity, GitChangeStatus, SessionConfigKind, SessionConfigOptionValue, SessionMeta,
    SessionResult, SessionState,
};

use crate::app::{AmuxApp, CloseSettingsOverlay, NewSessionMode, Panel, Selected, SessionListItem, SettingsCategory, SkillAction};
use crate::config::ApiFormat;
use crate::diff::{diff_lines, DiffLineKind};
use crate::display::{info_row, machine_status_badge, short_cwd};
use crate::logic::{
    activity_kind_detail, build_changed_file_tree, context_percent, context_usage_text,
    external_path_attachment, DialogMsg, DiffFileTreeNode, InputAttachment,
};
use crate::machine::MachineStatus;
use crate::text::{block_text, format_local_time, one_line, TimePrecision};
impl AmuxApp {
    pub(crate) fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = cx.theme().sidebar;
        let sidebar_border = cx.theme().sidebar_border;
        let foreground = cx.theme().foreground;
        let sidebar_width = self.sidebar_width_px / window.scale_factor();
        let sidebar_content = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_3()
            .bg(sidebar)
            .border_r_1()
            .border_color(sidebar_border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size_2().rounded_full().bg(cx.theme().primary))
                    .child(
                        Label::new("amux")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("goto-new-session")
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("新会话 / 工作流")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.selected = None;
                                this.set_panel(window, cx, None);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("sidebar-sessions")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_2()
                    .children(self.render_session_list(cx)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("settings")
                        .small()
                        .ghost()
                        .label("设置")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.open_settings(window, cx, None);
                            if let Some(i) = this.active_machine() {
                                this.refresh_sessions(i, window, cx);
                            }
                        })),
                ),
            );
        let resize_handle = div()
            .id("sidebar-resize-handle")
            .w(px(5.0)) // 拖拽手柄宽度：物理命中区域
            .h_full()
            .bg(sidebar_border.opacity(0.6))
            .hover(|d| d.bg(cx.theme().primary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                    this.sidebar_resize_origin = Some(event.position.x.as_f32());
                    this.sidebar_resize_initial = this.sidebar_width_px;
                }),
            )
            .on_drag((), |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<()>, window, cx| {
                let Some(origin) = this.sidebar_resize_origin else {
                    return;
                };
                let next = (this.sidebar_resize_initial
                    + (event.event.position.x.as_f32() - origin))
                    .clamp(180.0 * window.scale_factor(), 420.0 * window.scale_factor());
                this.sidebar_width_px = next;
                cx.notify();
            }));
        h_flex()
            .w(px(sidebar_width)) // 拖拽解析出的运行时宽度（随 pointer 事件更新）
            .h_full()
            .child(sidebar_content)
            .child(resize_handle)
    }

    pub(crate) fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 子会话只挂在工作流会话下，顶层列表跳过
        let child_ids: std::collections::HashSet<String> = self
            .workflows
            .iter()
            .flat_map(|wf| {
                wf.session
                    .read()
                    .unwrap()
                    .children
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut items: Vec<(u64, SessionListItem)> = Vec::new();
        for (mi, m) in self.machines.iter().enumerate() {
            // 离线/认证失败/连接中的机器不展示其会话：数据是上次刷新的陈旧缓存
            // 且不可操作；机器恢复在线后随 10s 定时刷新自动重现
            if !matches!(m.status, MachineStatus::Online) {
                continue;
            }
            for s in &m.sessions {
                if child_ids.contains(s.id.as_str()) {
                    continue;
                }
                items.push((
                    s.last_active_at,
                    SessionListItem::Session {
                        machine: mi,
                        meta: s.clone(),
                    },
                ));
            }
        }
        for (wi, wf) in self.workflows.iter().enumerate() {
            let s_guard = wf.snapshot();
            let mut recency = s_guard.updated_at;
            for c in &s_guard.children {
                if let Some(mm) = self.machines.get(c.machine_idx) {
                    if let Some(s) = mm.sessions.iter().find(|s| s.id == c.id) {
                        recency = recency.max(s.last_active_at);
                    }
                }
            }
            items.push((recency, SessionListItem::Workflow { idx: wi }));
        }
        items.sort_by_key(|(rec, _)| std::cmp::Reverse(*rec));

        let mut rows: Vec<gpui::AnyElement> = items
            .into_iter()
            .map(|(_, item)| match item {
                SessionListItem::Session { machine, meta } => {
                    self.render_session_row(cx, machine, &meta)
                }
                SessionListItem::Workflow { idx } => self.render_workflow_row(cx, idx),
            })
            .collect();

        for (mi, m) in self.machines.iter().enumerate() {
            if m.sessions_has_more {
                let name = m.config.name.clone();
                rows.push(
                    Button::new(format!("sessions-more-{mi}"))
                        .small()
                        .label(format!("加载更早会话（{name}）"))
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.load_more_sessions(window, cx, mi);
                        }))
                        .into_any_element(),
                );
            }
        }
        rows
    }

    pub(crate) fn render_session_row(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        s: &SessionMeta,
    ) -> gpui::AnyElement {
        let sid = s.id.clone();
        let sel = self.selected
            == Some(Selected::Session {
                machine,
                id: sid.clone(),
            });
        let title = if s.title.is_empty() {
            format!("（未命名）{}", short_cwd(&s.cwd))
        } else {
            s.title.clone()
        };
        // 重命名预填用原始标题（title 是含「（未命名）/目录」回退的展示文案）
        let raw_title = s.title.clone();
        let busy = s.state == SessionState::Busy;
        let label: SharedString = title.clone().into();
        let active = cx.theme().list_active;
        let border = cx.theme().list_active_border;

        if self.renaming_session.as_ref() == Some(&(machine, sid.clone())) {
            let sid2 = sid.clone();
            return v_flex()
                .gap_1()
                .child(Input::new(&self.title_input))
                .child(
                    Button::new(format!("rename-save-{sid}"))
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            let title = this.title_input.read(cx).value().to_string();
                            this.rename_session(window, cx, machine, sid2.clone(), title);
                        })),
                )
                .into_any_element();
        }

        let sid_open = sid.clone();
        // 右键菜单交给 ContextMenu 组件：外点/Esc 关闭、键盘导航、焦点恢复由其负责。
        // 菜单构建闭包与各条目回调均为 Fn，逐层持有独立克隆
        let app = cx.entity();
        let sid_menu = sid.clone();
        div()
            .id(format!("sess-row-{machine}-{sid}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(active.opacity(if sel { 1.0 } else { 0.0 }))
            .when(sel, |d| d.border_1().border_color(border))
            .hover(|d| d.bg(cx.theme().list_hover))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                this.open_session(window, cx, machine, sid_open.clone());
            }))
            .context_menu(move |menu, _window, _cx| {
                menu.item(PopupMenuItem::new("重命名").on_click({
                    let app = app.clone();
                    let sid = sid_menu.clone();
                    let raw_title = raw_title.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.selected = Some(Selected::Session {
                                machine,
                                id: sid.clone(),
                            });
                            this.renaming_session = Some((machine, sid.clone()));
                            this.title_input
                                .update(cx, |s, cx| s.set_value(&raw_title, window, cx));
                            cx.notify();
                        });
                    }
                }))
                .item(PopupMenuItem::new("删除会话").on_click({
                    let app = app.clone();
                    let sid = sid_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.confirm_delete_session(window, cx, machine, sid.clone());
                        });
                    }
                }))
            })
            .child(
                h_flex()
                    .w_full()
                    .h_8()
                    .px_1()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .small()
                            .text_color(if sel {
                                cx.theme().primary
                            } else {
                                cx.theme().muted_foreground
                            }),
                    )
                    .child(
                        h_flex()
                            .id(format!("sess-title-{machine}-{sid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .child(Label::new(label).text_sm().flex_1().min_w_0().truncate()),
                    )
                    // 会话上下文占用（docs/DESIGN.md：usage_update 记录的已用/窗口）
                    .when(
                        context_percent(s.context_size, s.context_window_size).is_some(),
                        |row| {
                            let percent = context_percent(s.context_size, s.context_window_size)
                                .expect("上方已判非 None");
                            let percent_text: SharedString = format!("{percent:.0}%").into();
                            row.child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div().w(px(40.)).child(
                                            Progress::new(format!("sess-ctx-{machine}-{sid}"))
                                                .value(percent)
                                                .xsmall()
                                                .color(cx.theme().primary),
                                        ),
                                    )
                                    .child(
                                        Label::new(percent_text)
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                        },
                    )
                    .when(s.last_active_at > 0, |row| {
                        row.child(
                            Label::new(format_local_time(s.last_active_at, TimePrecision::Compact))
                                .text_xs()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .child(if busy {
                        Spinner::new()
                            .xsmall()
                            .color(cx.theme().primary)
                            .into_any_element()
                    } else {
                        div().size_2().into_any_element()
                    }),
            )
            .into_any_element()
    }

    pub(crate) fn render_workflow_row(&self, cx: &mut Context<Self>, wi: usize) -> gpui::AnyElement {
        let Some(wf) = self.workflows.get(wi) else {
            return div().into_any();
        };
        let wf_sel = self.selected
            == Some(Selected::Workflow {
                id: wf.id(),
            });
        let wf_id = wf.id();
        let (title, busy) = {
            let title = if wf.title_is_empty() {
                "新工作流".to_string()
            } else {
                wf.title()
            };
            (title, wf.state() == SessionState::Busy)
        };
        let expanded = self.expanded_workflows.contains(&wf_id);
        // 各回调闭包（Fn）逐个持有独立克隆，避免互抢所有权
        let wf_id_title = wf_id.clone();
        let wf_id_toggle = wf_id.clone();
        let header = h_flex()
            .gap_1()
            .items_center()
            .child(
                h_flex()
                    .id(format!("wf-title-{wf_id}"))
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.open_workflow(window, cx, wf_id_title.clone());
                    }))
                    .child(Icon::new(IconName::Network).small().text_color(if wf_sel {
                        cx.theme().primary
                    } else {
                        cx.theme().muted_foreground
                    }))
                    .child(
                        Label::new(title.as_str())
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .child(if busy {
                        Tag::warning()
                            .small()
                            .rounded_full()
                            .child(Label::new("编排中…").text_xs())
                            .into_any_element()
                    } else {
                        Tag::secondary()
                            .outline()
                            .small()
                            .rounded_full()
                            .child(Label::new("空闲").text_xs())
                            .into_any_element()
                    }),
            )
            .child(
                Button::new(format!("wf-toggle-{wf_id}"))
                    .small()
                    .ghost()
                    .icon(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if !this.expanded_workflows.insert(wf_id_toggle.clone()) {
                            this.expanded_workflows.remove(&wf_id_toggle);
                        }
                        cx.notify();
                    })),
            )
            .child(if busy {
                Spinner::new()
                    .xsmall()
                    .color(cx.theme().primary)
                    .into_any_element()
            } else {
                div().size_2().into_any_element()
            });

        // 子会话默认折叠、可展开下钻。标题/忙闲联表本机会话缓存（权威在 server）
        let mut children = wf.children();
        children.sort_by_key(|child| {
            std::cmp::Reverse(
                self.machine(child.machine_idx)
                    .and_then(|machine| {
                        machine
                            .sessions
                            .iter()
                            .find(|session| session.id == child.id)
                    })
                    .map(|session| session.last_active_at)
                    .unwrap_or(0),
            )
        });
        let mut content = v_flex().gap_1();
        for c in &children {
            let cid = c.id.clone();
            let cid_open = cid.clone();
            let machine_name = c.machine_name.clone();
            let machine_click = machine_name.clone();
            let meta = self
                .machine(c.machine_idx)
                .and_then(|machine| machine.sessions.iter().find(|s| s.id == c.id));
            let step = meta
                .map(|m| m.title.clone())
                .unwrap_or_else(|| "（会话不存在）".into());
            let agent = meta.map(|m| m.agent.clone()).unwrap_or_default();
            let busy = meta.is_some_and(|m| m.state == SessionState::Busy);
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Label::new("↳").text_color(cx.theme().muted_foreground))
                    .child(
                        h_flex()
                            .id(format!("wf-child-title-{wf_id}-{cid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let mi = this
                                    .machines
                                    .iter()
                                    .position(|mm| mm.config.name == machine_click)
                                    .unwrap_or(0);
                                this.open_session(window, cx, mi, cid_open.clone());
                            }))
                            .child(Label::new(step).text_sm().flex_1().min_w_0().truncate())
                            .child(
                                Label::new(format!("{agent}@{machine_name}"))
                                    .text_xs()
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .child(if busy {
                        Spinner::new()
                            .xsmall()
                            .color(cx.theme().primary)
                            .into_any_element()
                    } else {
                        div().size_2().into_any_element()
                    }),
            );
        }

        if self.renaming_workflow.as_deref() == Some(wf_id.as_str()) {
            let wf_id2 = wf_id.clone();
            return v_flex()
                .gap_1()
                .p_2()
                .bg(cx.theme().popover)
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(Input::new(&self.title_input))
                .child(
                    Button::new(format!("wf-rename-save-{wf_id}"))
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            let title = this.title_input.read(cx).value().to_string();
                            this.rename_workflow(cx, &wf_id2.clone(), title);
                        })),
                )
                .into_any_element();
        }

        let row = v_flex().gap_1().p_1().rounded_md().child(header).child(
            Collapsible::new()
                .open(self.expanded_workflows.contains(&wf_id))
                .content(content),
        );

        // 右键菜单交给 ContextMenu 组件（同普通会话行）；逐层持有独立克隆
        let app = cx.entity();
        let wf_id_menu = wf_id.clone();
        let title_menu = title.clone();
        div()
            .id(format!("wf-row-{wf_id}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(cx
                .theme()
                .list_active
                .opacity(if wf_sel { 1.0 } else { 0.0 }))
            .when(wf_sel, |d| {
                d.border_1().border_color(cx.theme().list_active_border)
            })
            .hover(|d| d.bg(cx.theme().list_hover))
            .context_menu(move |menu, _window, _cx| {
                menu.item(PopupMenuItem::new("重命名").on_click({
                    let app = app.clone();
                    let wf_id = wf_id_menu.clone();
                    let title = title_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.selected = Some(Selected::Workflow { id: wf_id.clone() });
                            this.renaming_workflow = Some(wf_id.clone());
                            this.title_input
                                .update(cx, |s, cx| s.set_value(&title, window, cx));
                            cx.notify();
                        });
                    }
                }))
                .item(PopupMenuItem::new("删除工作流").on_click({
                    let app = app.clone();
                    let wf_id = wf_id_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.confirm_delete_workflow(window, cx, wf_id.clone());
                        });
                    }
                }))
            })
            .child(row)
            .into_any_element()
    }

    pub(crate) fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.selected.is_none() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .p_3()
                .child(self.render_center(window, cx))
                .into_any();
        }
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(self.render_center(window, cx))
            .child(self.render_quick_buttons(cx))
            .child(self.render_activity_bar(cx))
            .child(self.render_input(window, cx))
            .into_any()
    }

    pub(crate) fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.selected.is_none() {
            return self.render_new_session_view(window, cx);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.render_session_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_dialog(window, cx))
                    .child(self.render_floating_buttons(window, cx)),
            )
            .into_any()
    }

    pub(crate) fn render_session_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, status) = match &self.selected {
            Some(Selected::Session { machine, id }) => {
                let Some(machine_view) = self.machine(*machine) else {
                    return h_flex().into_any();
                };
                let Some(session) = machine_view.sessions.iter().find(|s| s.id == *id) else {
                    return h_flex().into_any();
                };
                let available = machine_view.status.online()
                    && machine_view
                        .agents
                        .iter()
                        .any(|agent| agent.name == session.agent && agent.available);
                (
                    format!("{}@{}", session.agent, machine_view.config.name),
                    if available { "可用" } else { "不可用" },
                )
            }
            Some(Selected::Workflow { id }) => {
                let Some(workflow) = self.workflow(id) else {
                    return h_flex().into_any();
                };
                (
                    "编排智能体".to_string(),
                    if workflow.state() == SessionState::Busy {
                        "工作中"
                    } else if self.store.orchestrator().is_configured() {
                        "可用"
                    } else {
                        "不可用"
                    },
                )
            }
            None => return h_flex().into_any(),
        };
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Label::new(label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(if status == "可用" {
                Tag::success()
                    .small()
                    .rounded_full()
                    .child(Label::new(status).text_xs())
                    .into_any_element()
            } else {
                Tag::danger()
                    .small()
                    .rounded_full()
                    .child(Label::new(status).text_xs())
                    .into_any_element()
            })
            .into_any()
    }

    pub(crate) fn render_new_session_view(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mode = self.new_session_mode;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let mut card = v_flex()
            .w_full()
            .max_w(px(640.)) // 新建会话卡片最大宽度（固定容器尺寸）
            .gap_3()
            .p_4()
            .bg(popover)
            .rounded_lg()
            .border_1()
            .border_color(border)
            .shadow_lg()
            .child(
                Label::new("新会话")
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(foreground),
            )
            .child(
                // 分段单选：模式切换用库 ButtonGroup（选中态/圆角拼接由其负责）
                ButtonGroup::new("ns-mode")
                    .small()
                    .w_full()
                    .child(
                        Button::new("ns-mode-direct")
                            .flex_1()
                            .label("普通")
                            .selected(mode == NewSessionMode::Direct),
                    )
                    .child(
                        Button::new("ns-mode-tpl")
                            .flex_1()
                            .label("工作流")
                            .selected(mode == NewSessionMode::Workflow),
                    )
                    .on_click(cx.listener(|this, clicks: &Vec<usize>, _window, cx| {
                        if clicks.contains(&0) {
                            this.new_session_mode = NewSessionMode::Direct;
                        } else if clicks.contains(&1) {
                            this.new_session_mode = NewSessionMode::Workflow;
                        }
                        cx.notify();
                    })),
            );
        match mode {
            NewSessionMode::Direct => {
                if self.machines.is_empty() {
                    // 无机器时提示并引导到设置。
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .child(
                                Alert::warning(
                                    "ns-no-machines-alert",
                                    "请先在设置 → 机器管理中注册一台 amux server。",
                                )
                                .title("尚未注册机器"),
                            )
                            .child(
                                Button::new("ns-goto-machine-settings")
                                    .small()
                                    .primary()
                                    .label("去注册机器")
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.open_settings(
                                            window,
                                            cx,
                                            Some(SettingsCategory::Machines),
                                        );
                                    })),
                            ),
                    );
                } else {
                    card = card
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_6()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("机器")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_machine_selector(cx)),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("Agent")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_harness_selector(cx)),
                                ),
                        )
                        .child(self.render_workspace_picker(cx))
                        // worktree 开关（docs/PRD.md）：勾选后 agent 在独立工作树中
                        // 工作，主仓库工作区不受影响；路径由 server 统一分配
                        .child(
                            Checkbox::new("ns-worktree-toggle")
                                .label("使用 worktree")
                                .checked(self.new_session_worktree)
                                .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                                    this.new_session_worktree = *checked;
                                    cx.notify();
                                })),
                        )
                        .when_some(self.new_session_error.clone(), |view, error| {
                            view.child(Alert::error("ns-create-error", error))
                        })
                        .child(
                            Button::new("ns-create")
                                .primary()
                                .mt_2()
                                .label("创建会话")
                                .on_click(cx.listener(|this, _ev, window, cx| {
                                    this.create_session_only(window, cx);
                                })),
                        );
                }
            }
            NewSessionMode::Workflow => {
                if !self.store.orchestrator().is_configured() {
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .child(
                                Alert::warning(
                                    "ns-no-orch-alert",
                                    "请先配置 API 格式、Base URL、API Key 和模型名称。",
                                )
                                .title("编排智能体尚未配置"),
                            )
                            .child(
                                Button::new("ns-goto-orch-settings")
                                    .small()
                                    .primary()
                                    .label("去配置编排 agent")
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.open_settings(
                                            window,
                                            cx,
                                            Some(SettingsCategory::Orchestrator),
                                        );
                                    })),
                            ),
                    );
                } else {
                    card = card
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("工作流计划")
                                        .text_sm()
                                        .text_color(muted_foreground),
                                )
                                .child(self.render_template_selector(cx)),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new(if self.workflow_template.is_some() {
                                        "本次工作流目标（可留空，稍后在会话中输入）"
                                    } else {
                                        "自然语言执行计划"
                                    })
                                    .text_sm()
                                    .text_color(muted_foreground),
                                )
                                .child(Input::new(&self.workflow_input)),
                        );
                    if let Some(err) = &self.workflow_error {
                        card = card.child(Alert::error("ns-wf-error", err.clone()));
                    }
                    card = card.child(
                        Button::new("ns-create-workflow")
                            .primary()
                            .label("创建工作流会话")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.create_workflow(window, cx);
                            })),
                    );
                }
            }
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(card)
            .into_any()
    }

    pub(crate) fn render_workspace_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return v_flex().into_any();
        };
        let dirs = self.store.recent_workspaces_for_machine(&m.config.name);
        // 有最近目录时输入框本身即 Popover 触发器（Input 实现 Selectable，
        // 开启态由组件在触发器上呈现选中样式），点击即弹出最近目录；
        // 外点/Esc 关闭、选项回填后经 on_open_change 回写关闭
        let app = cx.entity();
        let open = self.show_workspace_dropdown;
        let store = self.store.clone();
        let machine_name = m.config.name.clone();
        v_flex()
            .gap_1()
            .child(
                Label::new("工作目录")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .when(dirs.is_empty(), |view| {
                view.child(Input::new(&self.session_cwd_input))
            })
            .when(!dirs.is_empty(), |view| {
                view.child(
                    Popover::new("workspace-picker")
                        .anchor(Anchor::BottomLeft)
                        .open(open)
                        .on_open_change({
                            let app = app.clone();
                            move |is_open, _window, cx| {
                                app.update(cx, |this, cx| {
                                    this.show_workspace_dropdown = *is_open;
                                    cx.notify();
                                });
                            }
                        })
                        .trigger(
                            Input::new(&self.session_cwd_input).suffix(
                                Icon::new(IconName::ChevronDown)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                        )
                        .content(move |_, _window, cx| {
                            // 受控开启：选项点击后经 AmuxApp 关闭（on_open_change 回写）。
                            // 选项为手搓行而非 Button——库 Button 内容层硬编码居中，
                            // 全宽下拉项无法左对齐（同目录树行）
                            let dirs = store.recent_workspaces_for_machine(&machine_name);
                            let hover_bg = cx.theme().accent;
                            v_flex()
                                .id("workspace-picker-list")
                                .w(rems(26.))
                                .max_h(rems(16.))
                                .overflow_y_scroll()
                                .gap_0p5()
                                .children(dirs.into_iter().map(|dir| {
                                    let app = app.clone();
                                    let dir_val = dir.clone();
                                    div()
                                        .id(format!("ns-workspace-option-{dir}"))
                                        .w_full()
                                        .h_6()
                                        .flex()
                                        .items_center()
                                        .px_2()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(move |d| d.bg(hover_bg))
                                        .on_click(move |_, window, cx| {
                                            app.update(cx, |this, cx| {
                                                this.session_cwd_input.update(cx, |s, cx| {
                                                    s.set_value(&dir_val, window, cx)
                                                });
                                                this.show_workspace_dropdown = false;
                                                this.new_session_error = None;
                                                cx.notify();
                                            });
                                        })
                                        .child(
                                            // 展示完整路径；溢出时头部截断——路径尾部
                                            // （最具体的目录段）始终可见
                                            Label::new(dir.clone())
                                                .text_sm()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis_start(),
                                        )
                                }))
                        }),
                )
            })
            .into_any()
    }

    pub(crate) fn render_machine_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        if self.machines.is_empty() {
            return Label::new("（请先在设置中添加机器）").into_any_element();
        }
        // ButtonGroup 单选组：子按钮 on_click 由组统一接管（按下索引回传）
        ButtonGroup::new("ns-machine-group")
            .small()
            .flex_wrap()
            .children(self.machines.iter().enumerate().map(|(i, m)| {
                Button::new(format!("ns-machine-{i}"))
                    .label(m.config.name.clone())
                    .selected(selected_machine == Some(i))
            }))
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, _window, cx| {
                let Some(&ix) = clicks.first() else {
                    return;
                };
                this.new_session_machine = Some(ix);
                this.new_session_agent = None;
                this.new_session_error = None;
                cx.notify();
            }))
            .into_any_element()
    }

    pub(crate) fn render_harness_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        let mut row = h_flex().gap_1().flex_wrap();
        let Some(mi) = machine else {
            return row.child(Label::new("（无机器）"));
        };
        let agents = self
            .machine(mi)
            .map(|m| m.agents.clone())
            .unwrap_or_default();
        if agents.is_empty() {
            return row.child(Label::new("（未发现 agent）"));
        }
        for a in &agents {
            let name = a.name.clone();
            let name_click = name.clone();
            let selected = self.new_session_agent.as_deref() == Some(name.as_str());
            let mut btn = Button::new(format!("ns-agent-{name}"))
                .small()
                .label(name)
                .when(selected, |b| b.primary());
            if !a.available {
                btn = btn.disabled(true);
            }
            let available = a.available;
            row = row.child(btn.on_click(cx.listener(move |this, _ev, _window, cx| {
                if available {
                    this.new_session_agent = Some(name_click.clone());
                    cx.notify();
                }
            })));
        }
        row
    }

    pub(crate) fn render_template_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        if templates.is_empty() {
            return Label::new("（无计划，可在设置中添加）").into_any_element();
        }
        ButtonGroup::new("ns-tpl-group")
            .small()
            .flex_wrap()
            .children(templates.iter().map(|t| {
                let sel = self
                    .workflow_template
                    .as_ref()
                    .is_some_and(|x| x.name == t.name);
                Button::new(format!("ns-tpl-{}", t.name))
                    .label(t.name.clone())
                    .selected(sel)
            }))
            // 单选组回传按下索引；再次点击已选模板取消选择
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, _window, cx| {
                let Some(&ix) = clicks.first() else {
                    return;
                };
                if let Some(t) = this.store.list_templates().get(ix) {
                    if this
                        .workflow_template
                        .as_ref()
                        .is_some_and(|x| x.name == t.name)
                    {
                        this.workflow_template = None;
                    } else {
                        this.workflow_template = Some(t.clone());
                    }
                }
                cx.notify();
            }))
            .into_any_element()
    }

    pub(crate) fn render_dialog(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialog: Vec<DialogMsg> = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .map(|v| v.dialog.clone())
                .unwrap_or_default(),
            Some(Selected::Workflow { id }) => self
                .workflow(id)
                .map(|w| {
                    let sg = w.snapshot();
                    let all = sg.to_dialog();
                    let start = all.len().saturating_sub(self.workflow_dialog_limit);
                    all[start..].to_vec()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let agent_label: SharedString = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| {
                    let machine_name = m.config.name.clone();
                    m.sessions
                        .iter()
                        .find(|s| &s.id == id)
                        .map(|s| format!("{}@{machine_name}", s.agent).into())
                })
                .unwrap_or_else(|| "Agent".into()),
            Some(Selected::Workflow { .. }) => "编排".into(),
            None => "Agent".into(),
        };
        let primary = cx.theme().primary;
        let primary_foreground = cx.theme().primary_foreground;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let muted_foreground = cx.theme().muted_foreground;
        let rows = dialog
            .iter()
            .map(|item| match item {
                DialogMsg::UserMessage { content, timestamp } => {
                    let text = block_text(content);
                    // 气泡贴内容：按最长行估算宽度，短消息收拢；长消息触顶换行。
                    // 下限需容纳「我 + 时间戳」头部行
                    let bubble_w = crate::text::estimate_bubble_width(
                        &text,
                        crate::theme::FONT_BODY.as_f32(),
                        132.,
                        720., // 消息气泡最大宽度（内容可读性上限）
                    );
                    div().id(("row", *timestamp)).w_full().child(
                        div()
                            .ml_auto()
                            .flex_none()
                            .w(bubble_w)
                            .overflow_hidden()
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(primary)
                            .shadow_sm()
                            .child(
                                Label::new("我")
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(primary_foreground),
                            )
                            .child(
                                Label::new(format_local_time(*timestamp, TimePrecision::Seconds))
                                    .text_xs()
                                    .text_color(cx.theme().primary_foreground.opacity(0.78)),
                            )
                            .child(
                                TextView::markdown(format!("umd-{timestamp}"), text)
                                    .selectable(true)
                                    .text_color(primary_foreground),
                            ),
                    )
                }
                DialogMsg::AgentMessage { content, timestamp } => {
                    let text = block_text(content);
                    let bubble_w = crate::text::estimate_bubble_width(
                        &text,
                        crate::theme::FONT_BODY.as_f32(),
                        132.,
                        720., // 消息气泡最大宽度（内容可读性上限）
                    );
                    div().id(("row", *timestamp)).w_full().child(
                        div()
                            .flex_none()
                            .w(bubble_w)
                            .overflow_hidden()
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .shadow_sm()
                            .child(
                                Label::new(agent_label.clone())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted_foreground),
                            )
                            .child(
                                Label::new(format_local_time(*timestamp, TimePrecision::Seconds))
                                    .text_xs()
                                    .text_color(muted_foreground),
                            )
                            .child(
                                TextView::markdown(format!("amd-{timestamp}"), block_text(content))
                                    .selectable(true),
                            ),
                    )
                }
            })
            .collect::<Vec<_>>();
        let history_has_more = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .map(|v| v.history_has_more)
                .unwrap_or(false),
            _ => false,
        };
        let mut content = Vec::new();
        if let Some(Selected::Workflow { id }) = &self.selected {
            let total = self
                .workflow(id)
                .map(|w| w.snapshot().transcript.len())
                .unwrap_or(0);
            if total > self.workflow_dialog_limit {
                content.push(
                    Button::new("load-more-workflow-history")
                        .small()
                        .ghost()
                        .label(format!("加载更早消息（共 {total} 条）"))
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.workflow_dialog_limit += 100;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        if history_has_more {
            content.push(
                Button::new("load-more-history")
                    .small()
                    .ghost()
                    .label("加载更早消息")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_history(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        content.extend(rows.into_iter().map(|r| r.into_any_element()));
        if content.is_empty() {
            // 空态：图标 + 引导文案居中，弱化存在感
            div()
                .id("dialog-empty")
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Inbox)
                        .large()
                        .text_color(muted_foreground.opacity(0.55)),
                )
                .child(
                    Label::new("选择左侧会话查看对话，或输入消息开始")
                        .text_sm()
                        .text_color(muted_foreground),
                )
                .into_any()
        } else {
            div()
                .id("dialog")
                .v_flex()
                .flex_1()
                .gap_4()
                .p_2()
                .overflow_y_scroll()
                .track_scroll(&self.dialog_scroll)
                .children(content)
                .into_any()
        }
    }

    pub(crate) fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current: Option<Activity> = match &self.selected {
            Some(Selected::Session { machine, id }) => self
                .machine(*machine)
                .and_then(|m| m.views.get(id))
                .and_then(|v| v.live.clone()),
            Some(Selected::Workflow { id }) => {
                let busy = self
                    .workflow(id)
                    .is_some_and(|wf| wf.state() == SessionState::Busy);
                if busy {
                    Some(Activity::Thinking {
                        timestamp: 0,
                        content: "正在编排决策/推进…".into(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        let warning = cx.theme().warning;
        let warning_foreground = cx.theme().warning_foreground;
        let danger = cx.theme().danger;
        match &current {
            Some(Activity::Thinking { content, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!("思考中：{}", one_line(content, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::ToolCall { name, title, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!(
                        "工具调用：{} {}",
                        name,
                        one_line(title.as_deref().unwrap_or(""), 120)
                    ))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Compaction { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("上下文压缩：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Error { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(danger.opacity(0.12))
                .border_1()
                .border_color(danger.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("错误：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(danger),
                )
                .into_any(),
            None => div().id("activity-bar-empty").into_any(),
        }
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

    pub(crate) fn render_quick_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commands = self.store.list_quick_commands();
        // ghost：输入区上方的快捷入口应视觉后退，不与发送按钮争夺注意力
        let mut row = h_flex().flex_wrap().gap_1();
        for c in commands {
            let name = c.name.clone();
            let cmd = c.clone();
            row = row.child(
                Button::new(format!("qc-{}", c.name))
                    .small()
                    .ghost()
                    .label(name)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.quick_command(window, cx, &cmd);
                    })),
            );
        }
        row
    }

    pub(crate) fn render_input(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            // 附件 chips：点击单个 chip 即移除该附件（原仅支持一键清空）
            .when(!self.input_attachments.is_empty(), |view| {
                view.child(h_flex().flex_wrap().gap_1().children(
                    self.input_attachments.iter().enumerate().map(|(i, a)| {
                        let label: SharedString = match a {
                            InputAttachment::Path { path, .. } => path.clone().into(),
                            InputAttachment::Image { name, .. } => name.clone().into(),
                        };
                        let tooltip_label = label.clone();
                        div()
                            .id(format!("attachment-chip-{i}"))
                            .max_w(px(280.))
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .hover(|d| d.bg(cx.theme().secondary_hover))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_label.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.input_attachments.remove(i);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_1()
                                    .overflow_hidden()
                                    .child(
                                        Icon::new(IconName::Close)
                                            .xsmall()
                                            .text_color(muted_foreground),
                                    )
                                    .child(
                                        Label::new(label.clone())
                                            .text_xs()
                                            .text_color(muted_foreground)
                                            .truncate(),
                                    ),
                            )
                    }),
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(96.)) // 输入区最小高度（宽松命中区域）
                            .id("input-drop-zone")
                            .child(Input::new(&self.input_state))
                            .can_drop(|dragged, _window, _cx| dragged.is::<ExternalPaths>())
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    for p in paths.paths() {
                                        this.input_attachments.push(external_path_attachment(
                                            &p.display().to_string(),
                                        ));
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        Button::new("send")
                            .primary()
                            .label("发送")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.send_prompt(window, cx);
                            })),
                    )
                    // 常驻取消：不随忙闲出现/消失（此前条件渲染让「想取消时
                    // 找不到按钮」）；空闲时点击为无害操作——会话侧静默吞掉
                    // ACP 的无可取消错误，工作流侧走既有注入取消指令机制
                    .child(
                        Button::new("cancel-work")
                            .small()
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(gpui::transparent_black())
                                    .foreground(cx.theme().danger)
                                    .hover(cx.theme().danger.opacity(0.12))
                                    .active(cx.theme().danger.opacity(0.2)),
                            )
                            .icon(IconName::Close)
                            .label("取消")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.cancel_work(window, cx);
                            })),
                    )
                    .when(self.input_attachments.len() > 1, |row| {
                        row.child(
                            Button::new("clear-attachments")
                                .small()
                                .ghost()
                                .label("清空附件")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.input_attachments.clear();
                                    cx.notify();
                                })),
                        )
                    }),
            )
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
                        self.workflow(&id)
                            .map(|w| w.child_count())
                            .unwrap_or(0)
                    ))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground),
                );
            for c in self
                .workflow(&id)
                .map(|w| w.children())
                .unwrap_or_default()
            {
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
        // 会话选项（docs/DESIGN.md「ACP 通信」：选项由 ACP 会话提供，用户可基于
        // 当前会话可选项设置）。select 点击展开候选，boolean 直接开关。
        if let Some(Selected::Session { machine, id }) = self.selected.clone() {
            if !meta.config_options.is_empty() {
                body = body.child(Separator::horizontal().label("会话选项"));
                for opt in &meta.config_options {
                    let cfg_key = format!("{machine}:{id}:{}", opt.id);
                    let cfg_name = opt.name.clone();
                    match &opt.kind {
                        SessionConfigKind::Select {
                            current_value,
                            options,
                        } => {
                            let current_label = options
                                .iter()
                                .find(|o| o.value == *current_value)
                                .map(|o| o.name.clone())
                                .unwrap_or_else(|| current_value.clone());
                            let expanded = self.expanded_config_options.contains(&cfg_key);
                            let key_toggle = cfg_key.clone();
                            let cid = cfg_key.clone();
                            body = body.child(
                                h_flex()
                                    .id(format!("cfg-row-{cfg_key}"))
                                    .w_full()
                                    .gap_1p5()
                                    .items_center()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(cx.theme().muted))
                                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                                        if !this.expanded_config_options.remove(&key_toggle) {
                                            this.expanded_config_options.insert(key_toggle.clone());
                                        }
                                        cx.notify();
                                    }))
                                    .child(Label::new(cfg_name).text_sm().flex_none())
                                    .child(
                                        Label::new(current_label)
                                            .text_sm()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .xsmall()
                                        .flex_none()
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            );
                            if expanded {
                                for o in options {
                                    let sid = id.clone();
                                    let oid = opt.id.clone();
                                    let oval = o.value.clone();
                                    let oname = o.name.clone();
                                    let btn_id = format!("cfg-opt-{cid}-{}", o.value);
                                    body = body.child(
                                        h_flex().w_full().pl_4().child(
                                            Button::new(btn_id)
                                                .small()
                                                .ghost()
                                                .when(o.value == *current_value, |b| b.primary())
                                                .label(oname)
                                                .on_click(cx.listener(
                                                    move |this, _ev, window, cx| {
                                                        this.set_session_config_option(
                                                            window,
                                                            cx,
                                                            machine,
                                                            sid.clone(),
                                                            oid.clone(),
                                                            SessionConfigOptionValue::ValueId {
                                                                value: oval.clone(),
                                                            },
                                                        );
                                                    },
                                                )),
                                        ),
                                    );
                                }
                            }
                        }
                        SessionConfigKind::Boolean { current_value } => {
                            let sid = id.clone();
                            let oid = opt.id.clone();
                            let checked = *current_value;
                            body = body.child(
                                h_flex()
                                    .w_full()
                                    .gap_1p5()
                                    .items_center()
                                    .child(Label::new(cfg_name).text_sm().flex_1())
                                    .child(
                                        Checkbox::new(format!("cfg-bool-{cfg_key}"))
                                            .checked(checked)
                                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                                this.set_session_config_option(
                                                    window,
                                                    cx,
                                                    machine,
                                                    sid.clone(),
                                                    oid.clone(),
                                                    SessionConfigOptionValue::Boolean {
                                                        value: !checked,
                                                    },
                                                );
                                            })),
                                    ),
                            );
                        }
                    }
                }
            }
        }
        body.into_any()
    }

    /// 设置会话配置选项（docs/DESIGN.md：Server 向 ACP Server 发送
    /// `session/set_config_option`）。成功后刷新会话列表带回最新选项。
    pub(crate) fn set_session_config_option(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
        config_id: String,
        value: SessionConfigOptionValue,
    ) {
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let client = m.client.clone();
        let params = json!({
            "sessionId": session_id,
            "configId": config_id,
            "value": value,
        });
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if client
                .request::<_, SessionResult>(
                    protocol::method::SESSION_SET_CONFIG_OPTION,
                    Some(params),
                )
                .await
                .is_ok()
            {
                let _ = this.update_in(cx, |this, w, cx| {
                    this.refresh_sessions(machine, w, cx);
                });
            }
        })
        .detach();
    }

    /// 活动行身份：语义键（aggregate::activity_key）+ 前缀，而非下标——加载更早
    /// 活动会前移插入，下标键会让展开态漂移到其他条目。工具调用附名称以区分
    /// 同毫秒的多个调用。
    pub(crate) fn activity_row_key(prefix: &str, a: &Activity) -> String {
        let (kind, ts) = crate::aggregate::activity_key(a);
        match a {
            Activity::ToolCall { name, .. } => format!("{prefix}-{kind}-{ts}-{name}"),
            _ => format!("{prefix}-{kind}-{ts}"),
        }
    }

    pub(crate) fn activity_row(&self, prefix: &str, a: &Activity, cx: &mut Context<Self>) -> gpui::AnyElement {
        let key_toggle = Self::activity_row_key(prefix, a);
        let expanded = self.expanded_activities.contains(&key_toggle);
        // 整卡可点击切换展开：折叠恒为一行（截断省略），展开显示全文（可换行）。
        // 不再用字符数阈值裁剪——截断交给样式层，展开态即原始 detail。
        let (_, ts) = crate::aggregate::activity_key(a);
        let (kind, detail) = activity_kind_detail(a);
        div()
            .id(key_toggle.clone())
            .w_full()
            .p_2()
            .bg(cx.theme().muted.opacity(0.55))
            .rounded_md()
            .cursor_pointer()
            .hover(|d| d.bg(cx.theme().muted))
            .on_click(cx.listener(move |this, _ev, _window, cx| {
                if !this.expanded_activities.remove(&key_toggle) {
                    this.expanded_activities.insert(key_toggle.clone());
                }
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        Label::new(format_local_time(ts, TimePrecision::Seconds))
                            .text_xs()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Label::new(kind.to_string())
                            .text_xs()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Label::new(detail.to_string())
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .when(!expanded, |l| l.truncate()),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_activities_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut activities_has_more = false;
        match &self.selected {
            Some(Selected::Session { machine, id }) => {
                let view = self.machine(*machine).and_then(|m| m.views.get(id));
                let activities = view.map(|v| v.activities.clone()).unwrap_or_default();
                activities_has_more = view.map(|v| v.activities_has_more).unwrap_or(false);
                rows = activities
                    .iter()
                    .map(|a| self.activity_row("act", a, cx))
                    .collect();
            }
            Some(Selected::Workflow { id }) => {
                if let Some(wf) = self.workflow(id) {
                    let sg = wf.snapshot();
                    rows = sg
                        .activities
                        .iter()
                        .map(|a| self.activity_row("wf-act", a, cx))
                        .collect();
                }
            }
            _ => {}
        }
        let total = rows.len();
        let start = if activities_has_more {
            0
        } else {
            total.saturating_sub(self.activities_limit)
        };
        let has_more = activities_has_more || start > 0;
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if has_more {
            children.push(
                Button::new("load-more-activities")
                    .small()
                    .ghost()
                    .label("加载更早活动")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_activities(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        children.extend(rows.into_iter().skip(start));
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
                        Label::new("会话活动历史")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-activities")
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
                // v_flex 让卡片间 gap 生效（原为普通 div，gap 无效导致卡片贴叠）
                div()
                    .id("activities-panel")
                    .flex_1()
                    .v_flex()
                    .gap_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.activities_scroll)
                    .children(children),
            )
            .into_any()
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
                    let diff_scroll = self.diff_scroll.clone();
                    out.push(
                        h_flex()
                            .items_center()
                            .pl(px((depth * 12 + 4) as f32))
                            .min_w_0()
                            .child(
                                Button::new(format!("diff-tree-{fi}"))
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

    pub(crate) fn render_diff_panel(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
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
        for (fi, f) in files.iter().enumerate() {
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
                            Button::new(format!("restore-hunk-{fi}-{hi}"))
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
                    .id(format!("diff-file-{fi}"))
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

    pub(crate) fn render_settings_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("settings-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.show_settings = false;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .id("settings-card")
                    .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                        cx.stop_propagation();
                    })
                    // Escape 关闭：焦点锚在卡片上，动作按叠层从顶到底关闭
                    .track_focus(&self.settings_focus)
                    .key_context("SettingsOverlay")
                    .on_action(cx.listener(|this, _: &CloseSettingsOverlay, window, cx| {
                        if this.show_add_machine_form {
                            this.close_add_machine_form(window, cx);
                        } else if this.show_quick_command_form {
                            this.close_quick_command_form(window, cx);
                        } else if this.show_skill_form {
                            this.close_skill_form(window, cx);
                        } else if this.show_template_form {
                            this.close_template_form(window, cx);
                        } else if this.skill_action_dialog.take().is_some() {
                            // 已取走即完成关闭
                        } else {
                            this.show_settings = false;
                        }
                        cx.notify();
                    }))
                    .w_full()
                    .max_w(px(880.)) // 设置浮窗最大尺寸（固定容器）
                    .h_full()
                    .max_h(px(620.))
                    .overflow_hidden()
                    .bg(cx.theme().popover)
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
            )
            .when(self.show_add_machine_form, |overlay| {
                overlay.child(self.render_add_machine_dialog(window, cx))
            })
            .when(self.show_quick_command_form, |overlay| {
                overlay.child(self.render_quick_command_dialog(window, cx))
            })
            .when(self.show_skill_form, |overlay| {
                overlay.child(self.render_skill_dialog(window, cx))
            })
            .when(self.show_template_form, |overlay| {
                overlay.child(self.render_template_dialog(window, cx))
            })
            .when(self.skill_action_dialog.is_some(), |overlay| {
                overlay.child(self.render_skill_action_dialog(window, cx))
            })
    }

    pub(crate) fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-nav")
            .w(px(190.)) // 设置导航面板固定宽度
            .h_full()
            .gap_1()
            .p_2()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .items_center()
                    .px_1()
                    .py_1()
                    .child(
                        Label::new("设置")
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-close")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭设置")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.show_settings = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(self.settings_nav_item(
                SettingsCategory::Machines,
                "cat-machines",
                "机器管理",
                IconName::HardDrive,
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::Orchestrator,
                "cat-orch",
                "编排智能体",
                IconName::Bot,
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::QuickCommands,
                "cat-qc",
                "快捷指令",
                IconName::Play,
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::Skills,
                "cat-skills",
                "技能管理",
                IconName::BookOpen,
                cx,
            ))
            .child(self.settings_nav_item(
                SettingsCategory::Templates,
                "cat-tpl",
                "工作流计划",
                IconName::File,
                cx,
            ))
            .child(div().flex_1())
            .child(
                Button::new("settings-back")
                    .small()
                    .label("关闭")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.show_settings = false;
                        cx.notify();
                    })),
            )
    }

    pub(crate) fn settings_nav_item(
        &self,
        target: SettingsCategory,
        id: &str,
        label: &str,
        icon: IconName,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id_owned = id.to_string();
        let label_owned = label.to_string();
        // ghost + selected：选中态由组件以 secondary_active 底色呈现，弱于 primary 实心
        Button::new(id_owned)
            .small()
            .ghost()
            .icon(icon)
            .label(label_owned)
            .selected(self.settings_category == target)
            .on_click(cx.listener(move |this, _ev, _window, cx| {
                this.settings_category = target;
                cx.notify();
            }))
    }

    pub(crate) fn render_settings_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .p_4()
            .overflow_y_scroll()
            .child(match self.settings_category {
                SettingsCategory::Machines => self.render_machines_settings(cx),
                SettingsCategory::Orchestrator => self.render_orchestrator_settings(cx),
                SettingsCategory::QuickCommands => self.render_quick_commands_settings(cx),
                SettingsCategory::Skills => self.render_skills_settings(cx),
                SettingsCategory::Templates => self.render_templates_settings(cx),
            })
    }

    pub(crate) fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines = self
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut item = v_flex()
                    .gap_1()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                Label::new(&m.config.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(machine_status_badge(
                                (&m.status, m.notice.as_deref()),
                                cx.theme().success,
                                cx.theme().danger,
                                cx.theme().warning,
                            ))
                            .child(div().flex_1())
                            .child(
                                Button::new(format!("restart-m-{i}"))
                                    .small()
                                    .label("重连")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = this.machines[i].config.name.clone();
                                        this.confirm_reconnect_machine(window, cx, i, name);
                                    })),
                            )
                            .child(
                                Button::new(format!("remove-{i}"))
                                    .small()
                                    .label("移除")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = this.machines[i].config.name.clone();
                                        this.confirm_remove_machine(window, cx, i, name);
                                    })),
                            ),
                    )
                    .child(
                        Label::new(&m.config.url)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate(),
                    );
                for a in &m.agents {
                    let available = a.available;
                    let agent = a.name.clone();
                    let agent_restart = agent.clone();
                    item = item.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new(format!(
                                    "{} · {}",
                                    agent,
                                    if available { "可用" } else { "不可用" }
                                ))
                                .text_sm(),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new(format!("restart-agent-{i}-{agent}"))
                                    .small()
                                    .label("重启")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let agent = agent_restart.clone();
                                        this.confirm_restart_agent(window, cx, i, agent);
                                    })),
                            ),
                    );
                }
                item
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "机器管理",
                        "接入 / 移除机器；每台机器自动发现 ACP agent，可重启",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-add-open")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .tooltip("添加机器")
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.show_add_machine_form = true;
                                this.machine_form_error = None;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.machines.is_empty(), |view| {
                view.child(
                    Label::new("还没有注册机器。点击右上角 + 添加 amux server。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(machines)
            .into_any()
    }

    pub(crate) fn render_add_machine_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut card = v_flex()
            .id("add-machine-card")
            .relative()
            .w(px(460.)) // 对话框固定宽度
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("添加机器")
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("add-machine-close")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("取消")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.close_add_machine_form(window, cx);
                            })),
                    ),
            )
            .child(
                Label::new("注册一台 amux server，保存后会立即建立连接。")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                Label::new("名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_name_input))
            .child(
                Label::new("连接地址")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_url_input))
            .child(
                Label::new("Token")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.machine_token_input));
        if let Some(error) = &self.machine_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("add-machine-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_add_machine_form(window, cx);
                        })),
                )
                .child(
                    Button::new("add-machine-submit")
                        .small()
                        .primary()
                        .label("添加机器")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            let name = this.machine_name_input.read(cx).value().to_string();
                            let url = this.machine_url_input.read(cx).value().to_string();
                            let token = this.machine_token_input.read(cx).value().to_string();
                            if this.add_machine(window, cx, name, url, token) {
                                this.close_add_machine_form(window, cx);
                            }
                        })),
                ),
        );
        div()
            .id("add-machine-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("add-machine-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_add_machine_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    pub(crate) fn render_quick_command_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.qc_edit_target.is_some() {
            "编辑快捷指令"
        } else {
            "新增快捷指令"
        };
        let mut card = v_flex()
            .id("quick-command-card")
            .relative()
            .w(px(520.)) // 对话框固定宽度
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                Label::new(title)
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("指令名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.qc_name_input))
            .child(
                Label::new("指令内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.qc_prompt_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("qc-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_quick_command_form(window, cx);
                        })),
                )
                .child(
                    Button::new("qc-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_quick_command(window, cx);
                        })),
                ),
        );
        div()
            .id("quick-command-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("quick-command-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_quick_command_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    pub(crate) fn render_skill_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.skill_edit_target.is_some() {
            "编辑技能"
        } else {
            "新增技能"
        };
        let mut card = v_flex()
            .id("skill-form-card")
            .relative()
            .w(px(520.)) // 对话框固定宽度
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                Label::new(title)
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("技能名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.skill_name_input))
            .child(
                Label::new("技能描述")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.skill_desc_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("skill-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_skill_form(window, cx);
                        })),
                )
                .child(
                    Button::new("skill-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_skill(window, cx);
                        })),
                ),
        );
        div()
            .id("skill-form-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("skill-form-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_skill_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    pub(crate) fn render_skill_action_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((skill, action)) = &self.skill_action_dialog else {
            return div().into_any_element();
        };
        let action = *action;
        let mut targets = Vec::new();
        for (machine_idx, machine) in self.machines.iter().enumerate() {
            for agent in &machine.agents {
                let skill = skill.clone();
                let agent_name = agent.name.clone();
                let label = format!("{} · {}", machine.config.name, agent.name);
                targets.push(
                    Button::new(format!(
                        "skill-target-{machine_idx}-{}-{}",
                        action.label(),
                        agent.name
                    ))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.skill_action_dialog = None;
                        this.manage_skill_on_agent(
                            window,
                            cx,
                            machine_idx,
                            agent_name.clone(),
                            skill.clone(),
                            action,
                        );
                        cx.notify();
                    })),
                );
            }
        }
        let mut card = v_flex()
            .id("skill-action-card")
            .relative()
            .w(px(520.)) // 对话框固定宽度
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                Label::new(format!("{}技能", action.label()))
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new(format!("选择执行技能「{}」的机器和 agent", skill.name))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            );
        if targets.is_empty() {
            card = card.child(
                Label::new("当前没有可用的机器 agent。")
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        } else {
            card = card.child(h_flex().gap_1().flex_wrap().children(targets));
        }
        card = card.child(
            h_flex().justify_end().child(
                Button::new("skill-action-cancel")
                    .small()
                    .label("取消")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.skill_action_dialog = None;
                        cx.notify();
                    })),
            ),
        );
        div()
            .id("skill-action-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("skill-action-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.skill_action_dialog = None;
                        cx.notify();
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    pub(crate) fn render_template_dialog(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if self.tpl_edit_target.is_some() {
            "编辑工作流计划"
        } else {
            "新增工作流计划"
        };
        let mut card = v_flex()
            .id("template-form-card")
            .relative()
            .w(px(560.)) // 对话框固定宽度
            .gap_2()
            .p_4()
            .bg(cx.theme().popover)
            .rounded_lg()
            .shadow_lg()
            .child(
                Label::new(title)
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("计划名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.tpl_name_input))
            .child(
                Label::new("计划内容")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.tpl_desc_input));
        if let Some(error) = &self.settings_form_error {
            card = card.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        card = card.child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("template-form-cancel")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.close_template_form(window, cx);
                        })),
                )
                .child(
                    Button::new("template-form-save")
                        .small()
                        .primary()
                        .label("保存")
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.save_template(window, cx);
                        })),
                ),
        );
        div()
            .id("template-form-dialog")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("template-form-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.close_template_form(window, cx);
                    })),
            )
            .child(card.on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                cx.stop_propagation();
            }))
    }

    pub(crate) fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_api_format = match self.orch_api_format {
            ApiFormat::ChatCompletions => Some(0),
            ApiFormat::Responses => Some(1),
            ApiFormat::Messages => Some(2),
        };
        let api_format_options = RadioGroup::horizontal("orch-api-format")
            .children(["chat_completions", "responses", "messages"])
            .selected_index(selected_api_format)
            .on_click(cx.listener(|this, selected: &usize, _window, cx| {
                this.orch_api_format = match *selected {
                    0 => ApiFormat::ChatCompletions,
                    1 => ApiFormat::Responses,
                    2 => ApiFormat::Messages,
                    _ => return,
                };
                this.orchestrator_form_error = None;
                this.orchestrator_form_status = None;
                cx.notify();
            }));
        let mut form = v_flex()
            .gap_1()
            .p_3()
            .bg(cx.theme().muted)
            .rounded_md()
            .child(
                Label::new("API 格式")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(api_format_options)
            .child(
                Label::new("Base URL")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_base_input))
            .child(
                Label::new("API Key")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_key_input))
            .child(
                Label::new("模型名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.orch_model_input));
        if let Some(error) = &self.orchestrator_form_error {
            form = form.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        if let Some(status) = &self.orchestrator_form_status {
            form = form.child(
                Label::new(status.clone())
                    .text_sm()
                    .text_color(cx.theme().success),
            );
        }
        form = form.child(
            h_flex().justify_end().child(
                Button::new("orch-save")
                    .small()
                    .primary()
                    .label("保存")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.save_orchestrator(window, cx);
                    })),
            ),
        );
        v_flex()
            .gap_2()
            .child(self.settings_header(
                "编排智能体",
                "配置工作流编排使用的大模型供应商连接信息",
                cx.theme().muted_foreground,
            ))
            .child(form)
            .into_any()
    }

    pub(crate) fn render_quick_commands_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commands = self.store.list_quick_commands();
        let items = commands
            .iter()
            .map(|command| {
                let edit = command.clone();
                let delete = command.name.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(&command.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(&command.prompt)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .line_clamp(2),
                            ),
                    )
                    .child(
                        Button::new(format!("qc-edit-{}", command.name))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_quick_command_form(
                                    window,
                                    cx,
                                    Some((edit.name.clone(), edit.prompt.clone())),
                                );
                            })),
                    )
                    .child(
                        Button::new(format!("qc-del-{}", command.name))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.confirm_remove_quick_command(window, cx, delete.clone());
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "快捷指令",
                        "自定义快捷指令，输入区上方一键发送",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("qc-add-open")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .tooltip("添加快捷指令")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_quick_command_form(window, cx, None);
                            })),
                    ),
            )
            .when(commands.is_empty(), |view| {
                view.child(
                    Label::new("还没有快捷指令。添加后会显示在会话输入区上方。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    pub(crate) fn render_skills_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let skills = self.store.list_skills();
        let items = skills
            .iter()
            .map(|skill| {
                let edit = skill.clone();
                let delete = skill.name.clone();
                let mut actions = Vec::new();
                for action in [
                    SkillAction::Install,
                    SkillAction::Update,
                    SkillAction::Uninstall,
                ] {
                    let skill = skill.clone();
                    actions.push(
                        Button::new(format!("skill-action-{}-{}", action.label(), skill.name))
                            .small()
                            .label(action.label())
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.open_skill_action_dialog(skill.clone(), action, cx);
                            })),
                    );
                }
                v_flex()
                    .gap_2()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Label::new(&skill.name)
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM),
                                    )
                                    .child(
                                        Label::new(&skill.description)
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .line_clamp(2),
                                    ),
                            )
                            .child(
                                Button::new(format!("skill-edit-{}", skill.name))
                                    .small()
                                    .label("编辑")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.open_skill_form(window, cx, Some(edit.clone()));
                                    })),
                            )
                            .child(
                                Button::new(format!("skill-del-{}", skill.name))
                                    .small()
                                    .label("删除")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.confirm_remove_skill(window, cx, delete.clone());
                                    })),
                            ),
                    )
                    .child(h_flex().gap_1().flex_wrap().children(actions))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "技能管理",
                        "已安装 / 管理的 ACP skills 清单",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("skill-add-open")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .tooltip("添加技能")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_skill_form(window, cx, None);
                            })),
                    ),
            )
            .when(skills.is_empty(), |view| {
                view.child(
                    Label::new("还没有技能。点击右上角 + 添加技能说明。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    pub(crate) fn render_templates_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        let items = templates
            .iter()
            .map(|template| {
                let edit = template.clone();
                let delete = template.name.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .bg(cx.theme().muted)
                    .rounded_md()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(&template.name)
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(&template.plan)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .line_clamp(2),
                            ),
                    )
                    .child(
                        Button::new(format!("tpl-edit-{}", template.name))
                            .small()
                            .label("编辑")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_template_form(window, cx, Some(edit.clone()));
                            })),
                    )
                    .child(
                        Button::new(format!("tpl-del-{}", template.name))
                            .small()
                            .label("删除")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.confirm_remove_template(window, cx, delete.clone());
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .child(self.settings_header(
                        "工作流计划",
                        "计划内容会作为工作流编排的系统指令注入",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("tpl-add-open")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .tooltip("添加计划")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_template_form(window, cx, None);
                            })),
                    ),
            )
            .when(templates.is_empty(), |view| {
                view.child(
                    Label::new("还没有工作流计划。点击右上角 + 创建一个可复用计划。")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .children(items)
            .into_any()
    }

    pub(crate) fn settings_header(
        &self,
        title: &str,
        subtitle: &str,
        muted_foreground: Hsla,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(title).text_lg().font_weight(FontWeight::MEDIUM))
            .child(Label::new(subtitle).text_sm().text_color(muted_foreground))
    }
}

