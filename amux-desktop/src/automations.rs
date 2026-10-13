//! 自动化列表、编辑与运行详情（docs/PRD.md「自动化列表视图」等）。

use std::str::FromStr;
use std::sync::Arc;

use amux_common::api::{
    Automation, AutomationInput, AutomationResultLabel, AutomationRun, AutomationRunState,
    AutomationSessionOptions, AutomationTrigger,
};
use croner::Cron;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::label::Label;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::spinner::Spinner;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, Icon, IconName, Selectable, Sizable,
};

use crate::app::{AmuxApp, PanelResizeDrag, PANEL_RESIZE_HANDLE_WIDTH};
use crate::dialog;
use crate::state::{
    AutomationPanel, AutomationSessionMode, AutomationTriggerMode, Core, OpenTarget,
    WorkspacePickerTarget,
};
use crate::ui;

const AUTOMATION_PANEL_WIDTH: f32 = 460.0;

#[derive(Clone, Copy)]
enum AutomationTargetKind {
    Session,
    Workflow,
}

/// 自动化列表位于中间面板。
pub fn render_main(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut groups = Vec::new();
    for project in &core.settings.projects {
        groups.push((
            project.name.clone(),
            if project.description.trim().is_empty() {
                project.name.clone()
            } else {
                format!("{} · {}", project.name, project.description)
            },
            core.automations
                .iter()
                .filter(|automation| automation.project.as_deref() == Some(project.name.as_str()))
                .cloned()
                .collect::<Vec<_>>(),
        ));
    }
    groups.push((
        String::new(),
        String::new(),
        core.automations
            .iter()
            .filter(|automation| automation.project.is_none())
            .cloned()
            .collect(),
    ));

    let mut body = v_flex().gap_3();
    for (key, label, automations) in groups {
        if automations.is_empty() {
            continue;
        }
        let mut group = v_flex().gap_1();
        if key.is_empty() && !core.settings.projects.is_empty() {
            group = group.child(div().my_1().border_t_1().border_color(cx.theme().border));
        } else if !label.is_empty() {
            group = group.child(
                Label::new(label)
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .truncate(),
            );
        }
        for automation in automations {
            group = group.child(automation_row(automation, core, cx));
        }
        body = body.child(group);
    }
    if core.automations.is_empty() {
        body = body.child(
            v_flex().flex_1().items_center().justify_center().child(
                Label::new("暂无自动化")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            ),
        );
    }

    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .bg(cx.theme().background)
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    Label::new("自动化列表")
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    Button::new("new-automation")
                        .small()
                        .outline()
                        .icon(IconName::Plus)
                        .tooltip("新建自动化")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_new_automation(window, cx)),
                        ),
                ),
        )
        .child(
            v_flex()
                .id("automation-list-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .p_3()
                .child(body),
        )
        .into_any_element()
}

/// 自动化详情/编辑位于右侧面板。
pub fn render_panel(
    core: &Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> Option<AnyElement> {
    let panel = core.automation_panel.clone()?;
    let content = match panel {
        AutomationPanel::New => editor(core, this, cx),
        AutomationPanel::Detail(id) => {
            let automation = core
                .automations
                .iter()
                .find(|automation| automation.id == id)
                .cloned();
            match automation {
                Some(_automation) if core.automation_form.editing_id.as_deref() == Some(&id) => {
                    editor(core, this, cx)
                }
                Some(automation) => detail(&automation, core, cx),
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new("自动化不存在")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element(),
            }
        }
    };
    let theme = cx.theme();
    let border = theme.border;
    let primary = theme.primary;
    let handle = div()
        .id("automation-panel-resize-handle")
        .w(px(PANEL_RESIZE_HANDLE_WIDTH))
        .h_full()
        .bg(border.opacity(0.35))
        .hover(move |handle| handle.bg(primary))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, _, _| {
                this.panel_drag = Some((event.position.x.as_f32(), this.panel_width));
            }),
        )
        .on_drag(PanelResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
        .on_drag_move(
            cx.listener(|this, event: &DragMoveEvent<PanelResizeDrag>, window, cx| {
                let Some((origin, initial)) = this.panel_drag else {
                    return;
                };
                this.panel_width = (initial + (origin - event.event.position.x.as_f32()))
                    .clamp(0.0, window.bounds().size.width.as_f32());
                cx.notify();
            }),
        );
    Some(
        h_flex()
            .h_full()
            .child(handle)
            .child(
                v_flex()
                    .w(px(this.panel_width.max(AUTOMATION_PANEL_WIDTH)))
                    .h_full()
                    .min_w_0()
                    .bg(cx.theme().background)
                    .child(content),
            )
            .into_any_element(),
    )
}

fn automation_row(automation: Automation, core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let id = automation.id.clone();
    let title = automation.title.clone();
    let selected = core.automation_panel == Some(AutomationPanel::Detail(id.clone()));
    let enabled = automation.enabled;
    let trigger = trigger_summary(&automation);
    let last = run_summary(automation.last_run.as_ref());
    let app = cx.entity();
    let action_id = id.clone();
    let toggle_id = id.clone();
    let delete_id = id.clone();
    let delete_title = title.clone();
    let row = h_flex()
        .id(SharedString::from(format!("automation-row-{id}")))
        .w_full()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .rounded_md()
        .when(selected, |row| row.bg(cx.theme().accent))
        .hover(|row| row.bg(cx.theme().accent))
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| this.open_automation_detail(id.clone(), cx)
        }))
        .context_menu(move |menu, _, _| {
            let run_id = action_id.clone();
            let run_app = app.clone();
            let toggle_id = toggle_id.clone();
            let toggle_app = app.clone();
            let delete_id = delete_id.clone();
            let delete_title = delete_title.clone();
            let delete_app = app.clone();
            menu.item(PopupMenuItem::new("立即运行").on_click(move |_, _, cx| {
                let id = run_id.clone();
                let app = run_app.clone();
                app.update(cx, |this, cx| this.trigger_automation(id.clone(), cx));
            }))
            .item(
                PopupMenuItem::new(if enabled { "暂停" } else { "恢复" }).on_click(
                    move |_, _, cx| {
                        let id = toggle_id.clone();
                        let app = toggle_app.clone();
                        app.update(cx, |this, cx| this.toggle_automation(id.clone(), cx));
                    },
                ),
            )
            .item(PopupMenuItem::new("删除").on_click(move |_, window, cx| {
                let id = delete_id.clone();
                let title = delete_title.clone();
                let app = delete_app.clone();
                app.update(cx, |this, cx| {
                    this.confirm_delete_automation(id.clone(), title.clone(), window, cx)
                });
            }))
        })
        .child(
            Button::new(SharedString::from(format!("automation-toggle-{id}")))
                .xsmall()
                .ghost()
                .icon(if enabled {
                    IconName::Pause
                } else {
                    IconName::Play
                })
                .tooltip(if enabled { "暂停" } else { "恢复" })
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_automation(id.clone(), cx);
                    }
                })),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(title)
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .truncate(),
                )
                .child(
                    Label::new(trigger)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate(),
                )
                .child(
                    Label::new(format!("最近运行：{last}"))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate(),
                ),
        )
        .child(
            Icon::new(match &automation.trigger {
                AutomationTrigger::Schedule { .. } => IconName::Calendar,
                AutomationTrigger::GithubWebhook { .. } => IconName::Github,
            })
            .small()
            .text_color(cx.theme().muted_foreground),
        );
    row.into_any_element()
}

fn detail(automation: &Automation, core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let id = automation.id.clone();
    let enabled = automation.enabled;
    let mut body = v_flex()
        .flex_1()
        .min_h_0()
        .overflow_y_scrollbar()
        .gap_4()
        .p_4()
        .child(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .justify_between()
                        .items_start()
                        .child(
                            Label::new(automation.title.clone())
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .flex_1()
                                .min_w_0()
                                .truncate(),
                        )
                        .child(
                            Button::new(SharedString::from(format!("automation-edit-{id}")))
                                .small()
                                .outline()
                                .label("编辑")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, window, cx| {
                                        this.edit_automation(id.clone(), window, cx)
                                    }
                                })),
                        ),
                )
                .child(
                    Label::new(run_summary(automation.last_run.as_ref()))
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new(SharedString::from(format!("automation-run-{id}")))
                        .small()
                        .outline()
                        .icon(IconName::Play)
                        .label("立即运行")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| this.trigger_automation(id.clone(), cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("automation-detail-toggle-{id}")))
                        .small()
                        .outline()
                        .icon(if enabled {
                            IconName::Pause
                        } else {
                            IconName::Play
                        })
                        .label(if enabled { "暂停" } else { "恢复" })
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| this.toggle_automation(id.clone(), cx)
                        })),
                ),
        )
        .child(detail_fields(automation, cx))
        .child(
            Label::new("运行记录")
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD),
        );

    if core.automation_runs_loading
        && core
            .automation_runs
            .get(&automation.id)
            .is_none_or(Vec::is_empty)
    {
        body = body.child(Spinner::new().small().color(cx.theme().muted_foreground));
    } else if core
        .automation_runs
        .get(&automation.id)
        .is_none_or(Vec::is_empty)
    {
        body = body.child(
            Label::new("暂无运行记录")
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        );
    } else {
        let mut runs = v_flex().gap_2();
        for run in core
            .automation_runs
            .get(&automation.id)
            .into_iter()
            .flatten()
            .cloned()
        {
            let target = run
                .session_id
                .clone()
                .map(OpenTarget::Session)
                .or_else(|| run.workflow_id.clone().map(OpenTarget::Workflow));
            let app = cx.entity();
            runs = runs.child(
                v_flex()
                    .id(SharedString::from(format!("automation-run-{}", run.id)))
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .child(Label::new(run_summary(Some(&run))).text_sm())
                            .child(
                                Label::new(ui::format_local_time(
                                    run.started_at.unwrap_or(run.created_at),
                                    ui::TimePrecision::Seconds,
                                ))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .when_some(run.error.clone(), |card, error| {
                        card.child(Label::new(error).text_xs().text_color(cx.theme().danger))
                    })
                    .on_click(move |_, _, cx| {
                        if let Some(target) = target.clone() {
                            app.update(cx, |this, cx| this.open_target(target.clone(), cx));
                        }
                    }),
            );
        }
        body = body.child(runs);
    }

    v_flex()
        .h_full()
        .child(panel_header("自动化详情", cx))
        .child(body)
        .into_any_element()
}

fn detail_fields(automation: &Automation, cx: &Context<AmuxApp>) -> AnyElement {
    let session = match &automation.session_options {
        AutomationSessionOptions::ExistingSession { session_id } => {
            format!("已有普通会话 · {session_id}")
        }
        AutomationSessionOptions::ExistingWorkflow { workflow_id } => {
            format!("已有工作流会话 · {workflow_id}")
        }
        AutomationSessionOptions::NewSession {
            machine,
            agent,
            workspace,
            ..
        } => format!("新建普通会话 · {agent}@{machine} · {workspace}"),
        AutomationSessionOptions::NewWorkflow { plan } => {
            format!("新建工作流会话 · {plan}")
        }
    };
    v_flex()
        .gap_2()
        .child(detail_line("触发方式", trigger_summary(automation), cx))
        .child(detail_line("任务", automation.task.clone(), cx))
        .child(detail_line(
            "项目",
            automation
                .project
                .clone()
                .unwrap_or_else(|| "未归属".into()),
            cx,
        ))
        .child(detail_line("新建会话选项", session, cx))
        .child(detail_line(
            "结果分类",
            if automation.result_labels.is_empty() {
                "未设置".to_string()
            } else {
                automation
                    .result_labels
                    .iter()
                    .map(|label| format!("{}：{}", label.name, label.description))
                    .collect::<Vec<_>>()
                    .join("；")
            },
            cx,
        ))
        .into_any_element()
}

fn detail_line(label: &str, value: String, cx: &Context<AmuxApp>) -> AnyElement {
    h_flex()
        .items_start()
        .gap_3()
        .child(
            Label::new(label.to_string())
                .text_sm()
                .w_24()
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            Label::new(value)
                .text_sm()
                .flex_1()
                .min_w_0()
                .whitespace_normal(),
        )
        .into_any_element()
}

fn editor(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let form = core.automation_form.clone();
    let title = this.automation_title.read(cx).value().trim().to_string();
    let task = this.automation_task.read(cx).value().trim().to_string();
    let cron = this.automation_cron.read(cx).value().trim().to_string();
    let repository = this
        .automation_repository
        .read(cx)
        .value()
        .trim()
        .to_string();
    let workspace = this
        .automation_workspace
        .read(cx)
        .value()
        .trim()
        .to_string();
    let plan = this.automation_plan.read(cx).value().trim().to_string();
    let label1_name = this
        .automation_label1_name
        .read(cx)
        .value()
        .trim()
        .to_string();
    let label1_description = this
        .automation_label1_description
        .read(cx)
        .value()
        .trim()
        .to_string();
    let label2_name = this
        .automation_label2_name
        .read(cx)
        .value()
        .trim()
        .to_string();
    let label2_description = this
        .automation_label2_description
        .read(cx)
        .value()
        .trim()
        .to_string();
    let can_save = !title.is_empty()
        && !task.is_empty()
        && match form.trigger_mode {
            AutomationTriggerMode::Schedule => cron_next_runs(&cron).is_ok(),
            AutomationTriggerMode::Webhook => !repository.is_empty(),
        }
        && match form.session_mode {
            AutomationSessionMode::ExistingSession => form.existing_session_target.is_some(),
            AutomationSessionMode::ExistingWorkflow => form.existing_workflow_target.is_some(),
            AutomationSessionMode::NewSession => {
                form.machine.is_some() && form.agent.is_some() && !workspace.is_empty()
            }
            AutomationSessionMode::NewWorkflow => !plan.is_empty(),
        }
        && !label1_name.is_empty()
        && !label1_description.is_empty()
        && !label2_name.is_empty()
        && !label2_description.is_empty();

    let mut content = v_flex()
        .id("automation-editor-scroll")
        .flex_1()
        .min_h_0()
        .overflow_y_scrollbar()
        .gap_4()
        .p_4()
        .child(field(
            "标题",
            Input::new(&this.automation_title)
                .w_full()
                .into_any_element(),
            cx,
        ))
        .child(field(
            "任务",
            Input::new(&this.automation_task)
                .w_full()
                .into_any_element(),
            cx,
        ))
        .child(project_selector(core, cx))
        .child(trigger_mode_selector(core, cx))
        .child(match form.trigger_mode {
            AutomationTriggerMode::Schedule => {
                let mut control = v_flex()
                    .w_full()
                    .gap_2()
                    .child(Input::new(&this.automation_cron).w_full());
                if let Some(error) = form.cron_error.clone() {
                    control =
                        control.child(Label::new(error).text_xs().text_color(cx.theme().danger));
                } else if !form.cron_next_runs.is_empty() {
                    control = control.child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("最近 5 次触发时间")
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .children(form.cron_next_runs.iter().map(|time| {
                                Label::new(ui::format_local_time(*time, ui::TimePrecision::Seconds))
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .into_any_element()
                            })),
                    );
                }
                field("cron 表达式", control.into_any_element(), cx)
            }
            AutomationTriggerMode::Webhook => v_flex()
                .gap_3()
                .child(field(
                    "仓库",
                    Input::new(&this.automation_repository)
                        .w_full()
                        .into_any_element(),
                    cx,
                ))
                .child(field(
                    "事件类型",
                    Input::new(&this.automation_events)
                        .w_full()
                        .into_any_element(),
                    cx,
                ))
                .into_any_element(),
        })
        .child(session_mode_selector(core, cx));

    content = match form.session_mode {
        AutomationSessionMode::ExistingSession | AutomationSessionMode::ExistingWorkflow => {
            content.child(existing_session_selector(core, this, cx))
        }
        AutomationSessionMode::NewSession => content
            .child(machine_selector(core, cx))
            .child(agent_selector(core, cx))
            .child(crate::sessions::workspace_picker(
                core,
                this,
                WorkspacePickerTarget::Automation,
                cx,
            ))
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new("使用 worktree")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Checkbox::new("automation-worktree")
                            .checked(form.use_worktree)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                this.with_core(|core| core.automation_form.use_worktree = *checked);
                                this.refresh_automation_worktree_branches(cx);
                                cx.notify();
                            })),
                    ),
            )
            .when(form.use_worktree, |content| {
                content.child(field(
                    "worktree 基分支",
                    automation_worktree_base_menu(core, cx),
                    cx,
                ))
            }),
        AutomationSessionMode::NewWorkflow => content.child(field(
            "工作流计划",
            v_flex()
                .w_full()
                .gap_2()
                .when_some(automation_plan_selector(core, cx), |form, plans| {
                    form.child(plans)
                })
                .child(Input::new(&this.automation_plan).w_full())
                .into_any_element(),
            cx,
        )),
    };

    content = content
        .child(field(
            "运行结果分类",
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&this.automation_label1_name).w_full())
                        .child(Input::new(&this.automation_label1_description).w_full())
                        .into_any_element(),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&this.automation_label2_name).w_full())
                        .child(Input::new(&this.automation_label2_description).w_full())
                        .into_any_element(),
                )
                .into_any_element(),
            cx,
        ))
        .child(
            Button::new("save-automation")
                .primary()
                .w_full()
                .label("保存")
                .disabled(!can_save)
                .on_click(cx.listener(|this, _, _, cx| this.save_automation(cx))),
        );

    v_flex()
        .h_full()
        .child(panel_header(
            if form.editing_id.is_some() {
                "编辑自动化"
            } else {
                "新建自动化"
            },
            cx,
        ))
        .child(content)
        .into_any_element()
}

fn trigger_mode_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    field(
        "触发方式",
        TabBar::new("automation-trigger-mode")
            .small()
            .segmented()
            .w_full()
            .selected_index(match core.automation_form.trigger_mode {
                AutomationTriggerMode::Schedule => 0,
                AutomationTriggerMode::Webhook => 1,
            })
            .child(Tab::new().label("定时"))
            .child(Tab::new().label("Github Webhook"))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.with_core(|core| {
                    core.automation_form.trigger_mode = if *index == 1 {
                        AutomationTriggerMode::Webhook
                    } else {
                        AutomationTriggerMode::Schedule
                    };
                });
                cx.notify();
            }))
            .into_any_element(),
        cx,
    )
}

fn session_mode_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    field(
        "会话选项",
        TabBar::new("automation-session-mode")
            .small()
            .segmented()
            .w_full()
            .selected_index(match core.automation_form.session_mode {
                AutomationSessionMode::ExistingSession => 0,
                AutomationSessionMode::ExistingWorkflow => 1,
                AutomationSessionMode::NewSession => 2,
                AutomationSessionMode::NewWorkflow => 3,
            })
            .child(Tab::new().label("已有普通会话"))
            .child(Tab::new().label("已有工作流会话"))
            .child(Tab::new().label("新建普通会话"))
            .child(Tab::new().label("新建工作流会话"))
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                this.dismiss_automation_workspace_popups(cx);
                this.with_core(|core| {
                    core.automation_form.session_mode = match *index {
                        0 => AutomationSessionMode::ExistingSession,
                        1 => AutomationSessionMode::ExistingWorkflow,
                        2 => AutomationSessionMode::NewSession,
                        _ => AutomationSessionMode::NewWorkflow,
                    };
                });
                cx.notify();
            }))
            .into_any_element(),
        cx,
    )
}

fn existing_session_selector(core: &Core, this: &AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    match core.automation_form.session_mode {
        AutomationSessionMode::ExistingSession => field(
            "普通会话",
            automation_target_popover(core, this, AutomationTargetKind::Session, cx),
            cx,
        ),
        AutomationSessionMode::ExistingWorkflow => field(
            "工作流会话",
            automation_target_popover(core, this, AutomationTargetKind::Workflow, cx),
            cx,
        ),
        AutomationSessionMode::NewSession | AutomationSessionMode::NewWorkflow => {
            div().into_any_element()
        }
    }
}

fn automation_target_popover(
    core: &Core,
    this: &AmuxApp,
    kind: AutomationTargetKind,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let selected = match kind {
        AutomationTargetKind::Session => core.automation_form.existing_session_target.clone(),
        AutomationTargetKind::Workflow => core.automation_form.existing_workflow_target.clone(),
    };
    let app = cx.entity();
    let sessions = core.automation_sessions.clone();
    let workflows = core.automation_workflows.clone();
    let (id, label, has_more, loading, scroll_handle) = match kind {
        AutomationTargetKind::Session => (
            "automation-existing-session-list",
            selected
                .as_ref()
                .and_then(|id| sessions.iter().find(|session| &session.id == id))
                .map(|session| session.title.clone())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "未选择".to_string()),
            core.automation_sessions_has_more,
            core.automation_sessions_loading,
            this.automation_session_scroll.clone(),
        ),
        AutomationTargetKind::Workflow => (
            "automation-existing-workflow-list",
            selected
                .as_ref()
                .and_then(|id| workflows.iter().find(|workflow| &workflow.id == id))
                .map(|workflow| workflow.title.clone())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "未选择".to_string()),
            core.automation_workflows_has_more,
            core.automation_workflows_loading,
            this.automation_workflow_scroll.clone(),
        ),
    };
    let selected_for_menu = selected.clone();
    Popover::new(id)
        .w(px(320.))
        .max_h(px(280.))
        .trigger(
            Button::new(format!("{id}-trigger"))
                .small()
                .outline()
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                ),
        )
        .content(move |_, _, cx| {
            let load_app = app.clone();
            let load_scroll = scroll_handle.clone();
            let popover = cx.entity();
            let mut body = v_flex()
                .id(SharedString::from(id))
                .w_full()
                .max_h(px(260.))
                .overflow_y_scroll()
                .track_scroll(&scroll_handle);
            match kind {
                AutomationTargetKind::Session => {
                    for session in &sessions {
                        let id = session.id.clone();
                        let app = app.clone();
                        let popover = popover.clone();
                        let selected = selected_for_menu.as_deref() == Some(id.as_str());
                        let label = if session.pinned {
                            format!("置顶 · {}", session.title)
                        } else if session.title.is_empty() {
                            "未命名会话".to_string()
                        } else {
                            session.title.clone()
                        };
                        body = body.child(
                            Button::new(SharedString::from(format!(
                                "automation-session-option-{id}"
                            )))
                            .small()
                            .ghost()
                            .w_full()
                            .justify_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label),
                            )
                            .selected(selected)
                            .on_click(move |_, _, cx| {
                                let id = id.clone();
                                app.update(cx, |this, cx| {
                                    this.with_core(|core| {
                                        core.automation_form.existing_session_target = Some(id);
                                        core.automation_form.session_mode =
                                            AutomationSessionMode::ExistingSession;
                                    });
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.emit(DismissEvent));
                            }),
                        );
                    }
                }
                AutomationTargetKind::Workflow => {
                    for workflow in &workflows {
                        let id = workflow.id.clone();
                        let app = app.clone();
                        let popover = popover.clone();
                        let selected = selected_for_menu.as_deref() == Some(id.as_str());
                        let label = if workflow.pinned {
                            format!("置顶 · {}", workflow.title)
                        } else if workflow.title.is_empty() {
                            "未命名工作流".to_string()
                        } else {
                            workflow.title.clone()
                        };
                        body = body.child(
                            Button::new(SharedString::from(format!(
                                "automation-workflow-option-{id}"
                            )))
                            .small()
                            .ghost()
                            .w_full()
                            .justify_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label),
                            )
                            .selected(selected)
                            .on_click(move |_, _, cx| {
                                let id = id.clone();
                                app.update(cx, |this, cx| {
                                    this.with_core(|core| {
                                        core.automation_form.existing_workflow_target = Some(id);
                                        core.automation_form.session_mode =
                                            AutomationSessionMode::ExistingWorkflow;
                                    });
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.emit(DismissEvent));
                            }),
                        );
                    }
                }
            }
            if loading {
                body = body.child(Spinner::new().small().color(cx.theme().muted_foreground));
            } else if has_more {
                body = body.child(
                    Label::new("滚动加载更多")
                        .text_xs()
                        .text_color(cx.theme().muted_foreground),
                );
            }
            body.on_scroll_wheel(move |_, _, cx| {
                let near_bottom = load_scroll.offset().y + load_scroll.max_offset().y <= px(24.);
                if !near_bottom {
                    return;
                }
                load_app.update(cx, |this, cx| match kind {
                    AutomationTargetKind::Session => {
                        this.load_more_automation_sessions(cx);
                    }
                    AutomationTargetKind::Workflow => {
                        this.load_more_automation_workflows(cx);
                    }
                });
            })
        })
        .into_any_element()
}

fn project_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    field(
        "项目",
        option_group(
            core.settings
                .projects
                .iter()
                .map(|project| {
                    let name = project.name.clone();
                    let selected =
                        core.automation_form.project.as_deref() == Some(project.name.as_str());
                    Button::new(SharedString::from(format!("automation-project-{name}")))
                        .small()
                        .label(ui::truncate(&project.name, 24))
                        .selected(selected)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.with_core(|core| {
                                core.automation_form.project = (!selected).then(|| name.clone());
                            });
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
        ),
        cx,
    )
}

fn machine_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    field(
        "机器",
        option_group(
            core.settings
                .machines
                .iter()
                .map(|machine| {
                    let name = machine.name.clone();
                    let selected =
                        core.automation_form.machine.as_deref() == Some(machine.name.as_str());
                    Button::new(SharedString::from(format!("automation-machine-{name}")))
                        .small()
                        .label(name.clone())
                        .selected(selected)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.with_core(|core| {
                                core.automation_form.machine = Some(name.clone());
                                core.automation_form.agent = None;
                            });
                            this.refresh_automation_workspace_suggestions(cx);
                            this.refresh_automation_worktree_branches(cx);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
        ),
        cx,
    )
}

fn agent_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let agents = core
        .automation_form
        .machine
        .as_ref()
        .and_then(|machine| {
            core.settings
                .agents
                .iter()
                .find(|(name, _)| name == machine)
                .map(|(_, agents)| agents.clone())
        })
        .unwrap_or_default();
    field(
        "智能体",
        option_group(
            agents
                .into_iter()
                .map(|agent| {
                    let name = agent.name.clone();
                    let selected =
                        core.automation_form.agent.as_deref() == Some(agent.name.as_str());
                    Button::new(SharedString::from(format!("automation-agent-{name}")))
                        .small()
                        .label(name.clone())
                        .selected(selected)
                        .disabled(!agent.available)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.with_core(|core| core.automation_form.agent = Some(name.clone()));
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
        ),
        cx,
    )
}

fn automation_worktree_base_menu(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let current = core.automation_form.worktree_base_branch.clone();
    let branches = core.automation_form.worktree_branches.clone();
    let app = cx.entity();
    Button::new("automation-worktree-base-select")
        .small()
        .outline()
        .child(
            div()
                .max_w(rems(14.))
                .line_height(relative(1.))
                .truncate()
                .child(if current == "HEAD" {
                    "HEAD（当前提交）".to_string()
                } else {
                    current.clone()
                }),
        )
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut menu = menu;
            let head_selected = current == "HEAD";
            let head_app = app.clone();
            menu = menu.item(
                PopupMenuItem::new("HEAD（当前提交）")
                    .checked(head_selected)
                    .on_click(move |_, _, cx| {
                        head_app.update(cx, |this, cx| {
                            this.with_core(|core| {
                                core.automation_form.worktree_base_branch = "HEAD".to_string()
                            });
                            cx.notify();
                        })
                    }),
            );
            for branch in &branches {
                let value = branch.name.clone();
                let checked = current == value;
                let branch_app = app.clone();
                menu = menu.item(
                    PopupMenuItem::new(worktree_base_branch_label(branch))
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            branch_app.update(cx, |this, cx| {
                                this.with_core(|core| {
                                    core.automation_form.worktree_base_branch = value.clone()
                                });
                                cx.notify();
                            })
                        }),
                );
            }
            menu
        })
        .into_any_element()
}

fn worktree_base_branch_label(branch: &amux_common::domain::GitBranch) -> String {
    match (branch.is_worktree_base, branch.is_default) {
        (true, true) => format!("{}（worktree 基分支、默认分支）", branch.name),
        (true, false) => format!("{}（worktree 基分支）", branch.name),
        (false, true) => format!("{}（默认分支）", branch.name),
        (false, false) => branch.name.clone(),
    }
}

fn automation_plan_selector(core: &Core, cx: &mut Context<AmuxApp>) -> Option<AnyElement> {
    if core.settings.plans.is_empty() {
        return None;
    }
    let mut row = h_flex().flex_wrap().gap_1();
    for plan in core.settings.plans.clone() {
        let project = plan.last_used_project.clone().filter(|project| {
            core.settings
                .projects
                .iter()
                .any(|candidate| &candidate.name == project)
        });
        row = row.child(
            Button::new(SharedString::from(format!("automation-plan-{}", plan.name)))
                .small()
                .label(ui::truncate(&plan.name, 18))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.automation_plan.update(cx, |state, cx| {
                        state.set_value(plan.plan.clone(), window, cx)
                    });
                    this.with_core(|core| core.automation_form.project = project.clone());
                    cx.notify();
                })),
        );
    }
    Some(row.into_any_element())
}

fn option_group(items: Vec<AnyElement>) -> AnyElement {
    let mut row = h_flex().flex_wrap().gap_1();
    for item in items {
        row = row.child(item);
    }
    row.into_any_element()
}

fn panel_header(title: &str, cx: &mut Context<AmuxApp>) -> AnyElement {
    h_flex()
        .w_full()
        .justify_between()
        .items_center()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            Label::new(title.to_string())
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            Button::new("automation-panel-close")
                .xsmall()
                .ghost()
                .icon(IconName::Close)
                .tooltip("关闭")
                .on_click(cx.listener(|this, _, _, cx| this.close_automation_panel(cx))),
        )
        .into_any_element()
}

fn field(label: &str, control: AnyElement, cx: &Context<AmuxApp>) -> AnyElement {
    v_flex()
        .gap_1()
        .items_start()
        .child(
            Label::new(label.to_string())
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
        .child(control)
        .into_any_element()
}

fn trigger_summary(automation: &Automation) -> String {
    match &automation.trigger {
        AutomationTrigger::Schedule { cron } => match automation.next_run_at {
            Some(next) => format!(
                "定时 · 下次 {}",
                ui::format_local_time(next, ui::TimePrecision::Compact)
            ),
            None => format!("定时 · {cron}"),
        },
        AutomationTrigger::GithubWebhook { repository, events } => {
            let events = if events.is_empty() {
                "全部事件".to_string()
            } else {
                events.join("、")
            };
            format!("GitHub · {repository} · {events}")
        }
    }
}

fn run_summary(run: Option<&AutomationRun>) -> String {
    let Some(run) = run else {
        return "尚未运行".to_string();
    };
    let state = match run.state {
        AutomationRunState::Queued => "排队中",
        AutomationRunState::Running => "运行中",
        AutomationRunState::Finished => "已完成",
        AutomationRunState::Failed => "失败",
    };
    match run.result_label.as_deref().or(run.error.as_deref()) {
        Some(detail) => format!("{state} · {detail}"),
        None => state.to_string(),
    }
}

impl AmuxApp {
    pub fn open_automation_list(&mut self, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.automation_open = true;
            core.automation_panel = None;
            core.automation_form = Default::default();
            core.open = None;
            core.side_panel = None;
            core.loaded_view = None;
        });
        self.load_automations();
        cx.notify();
    }

    pub fn open_new_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_automation_form(None, window, cx);
        self.with_core(|core| {
            core.automation_open = true;
            core.automation_panel = Some(AutomationPanel::New);
            core.open = None;
            core.side_panel = None;
        });
        self.panel_width = AUTOMATION_PANEL_WIDTH;
        self.load_automations();
        cx.notify();
    }

    pub fn open_automation_detail(&mut self, id: String, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.automation_open = true;
            core.automation_panel = Some(AutomationPanel::Detail(id.clone()));
            core.automation_form.editing_id = None;
            core.open = None;
            core.side_panel = None;
        });
        self.panel_width = AUTOMATION_PANEL_WIDTH;
        self.load_automation_runs(id);
        cx.notify();
    }

    pub fn close_automation_panel(&mut self, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.automation_panel = None;
            core.automation_form.editing_id = None;
        });
        cx.notify();
    }

    pub fn edit_automation(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let automation = self.with_core(|core| {
            core.automations
                .iter()
                .find(|automation| automation.id == id)
                .cloned()
        });
        if let Some(automation) = automation {
            self.reset_automation_form(Some(&automation), window, cx);
            self.with_core(|core| {
                core.automation_panel = Some(AutomationPanel::Detail(id.clone()));
            });
            cx.notify();
        }
    }

    pub fn save_automation(&mut self, cx: &mut Context<Self>) {
        let (client, editing_id, form, automation_enabled) = self.with_core(|core| {
            (
                core.client.clone(),
                core.automation_form.editing_id.clone(),
                core.automation_form.clone(),
                core.automation_form
                    .editing_id
                    .as_ref()
                    .and_then(|id| {
                        core.automations
                            .iter()
                            .find(|automation| &automation.id == id)
                    })
                    .map(|automation| automation.enabled)
                    .unwrap_or(true),
            )
        });
        let Some(client) = client else { return };
        let title = self.automation_title.read(cx).value().trim().to_string();
        let task = self.automation_task.read(cx).value().trim().to_string();
        let trigger = match form.trigger_mode {
            AutomationTriggerMode::Schedule => AutomationTrigger::Schedule {
                cron: self.automation_cron.read(cx).value().trim().to_string(),
            },
            AutomationTriggerMode::Webhook => AutomationTrigger::GithubWebhook {
                repository: self
                    .automation_repository
                    .read(cx)
                    .value()
                    .trim()
                    .to_string(),
                events: self
                    .automation_events
                    .read(cx)
                    .value()
                    .split(',')
                    .map(|event| event.trim().to_string())
                    .filter(|event| !event.is_empty())
                    .collect(),
            },
        };
        let session_options = match form.session_mode {
            AutomationSessionMode::ExistingSession => AutomationSessionOptions::ExistingSession {
                session_id: form.existing_session_target.clone().unwrap_or_default(),
            },
            AutomationSessionMode::ExistingWorkflow => AutomationSessionOptions::ExistingWorkflow {
                workflow_id: form.existing_workflow_target.clone().unwrap_or_default(),
            },
            AutomationSessionMode::NewSession => AutomationSessionOptions::NewSession {
                machine: form.machine.clone().unwrap_or_default(),
                agent: form.agent.clone().unwrap_or_default(),
                workspace: self
                    .automation_workspace
                    .read(cx)
                    .value()
                    .trim()
                    .to_string(),
                use_worktree: form.use_worktree,
                worktree_base_branch: form
                    .use_worktree
                    .then(|| form.worktree_base_branch.clone())
                    .filter(|branch| branch != "HEAD"),
            },
            AutomationSessionMode::NewWorkflow => AutomationSessionOptions::NewWorkflow {
                plan: self.automation_plan.read(cx).value().trim().to_string(),
            },
        };
        let label =
            |name: &Entity<InputState>, description: &Entity<InputState>| AutomationResultLabel {
                name: name.read(cx).value().trim().to_string(),
                description: description.read(cx).value().trim().to_string(),
            };
        let input = AutomationInput {
            title,
            enabled: automation_enabled,
            task,
            project: form.project,
            trigger,
            session_options,
            result_labels: vec![
                label(
                    &self.automation_label1_name,
                    &self.automation_label1_description,
                ),
                label(
                    &self.automation_label2_name,
                    &self.automation_label2_description,
                ),
            ],
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match editing_id.as_deref() {
                Some(id) => client.update_automation(id, &input).await,
                None => client.create_automation(&input).await,
            };
            match result {
                Ok(automation) => {
                    let runs = client.automation_runs(&automation.id, 100, 0).await.ok();
                    let mut core = core.lock();
                    if let Some(existing) = core
                        .automations
                        .iter_mut()
                        .find(|item| item.id == automation.id)
                    {
                        *existing = automation.clone();
                    } else {
                        core.automations.insert(0, automation.clone());
                    }
                    core.automation_panel = Some(AutomationPanel::Detail(automation.id.clone()));
                    core.automation_form.editing_id = None;
                    if let Some(page) = runs {
                        core.automation_runs
                            .insert(automation.id.clone(), page.runs);
                    }
                    core.success("自动化已保存");
                }
                Err(error) => core.lock().error(format!("保存自动化失败：{error}")),
            }
        });
        cx.notify();
    }

    pub fn toggle_automation(&mut self, id: String, cx: &mut Context<Self>) {
        let (client, automation) = self.with_core(|core| {
            (
                core.client.clone(),
                core.automations
                    .iter()
                    .find(|automation| automation.id == id)
                    .cloned(),
            )
        });
        let (Some(client), Some(automation)) = (client, automation) else {
            return;
        };
        let input = automation_input(&automation, !automation.enabled);
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.update_automation(&automation.id, &input).await {
                Ok(updated) => {
                    let mut core = core.lock();
                    if let Some(existing) = core.automations.iter_mut().find(|item| item.id == id) {
                        *existing = updated;
                    }
                }
                Err(error) => core.lock().error(format!("更新自动化失败：{error}")),
            }
        });
        cx.notify();
    }

    pub fn trigger_automation(&mut self, id: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.trigger_automation(&id).await {
                Ok(_) => {
                    core.lock().success("自动化已触发");
                    if let Ok(automations) = client.automations().await {
                        core.lock().automations = automations;
                    }
                    if let Ok(page) = client.automation_runs(&id, 100, 0).await {
                        core.lock().automation_runs.insert(id.clone(), page.runs);
                    }
                }
                Err(error) => core.lock().error(format!("触发自动化失败：{error}")),
            }
        });
        cx.notify();
    }

    pub fn confirm_delete_automation(
        &mut self,
        id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        dialog::confirm(
            window,
            cx,
            "删除自动化？",
            format!("「{title}」及其全部运行记录都会被删除。"),
            "删除",
            ButtonVariant::Danger,
            move |this, cx| this.delete_automation(id.clone(), cx),
        );
    }

    fn delete_automation(&mut self, id: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.delete_automation(&id).await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.automations.retain(|automation| automation.id != id);
                    core.automation_runs.remove(&id);
                    if core.automation_panel == Some(AutomationPanel::Detail(id.clone())) {
                        core.automation_panel = None;
                    }
                    core.success("自动化已删除");
                }
                Err(error) => core.lock().error(format!("删除自动化失败：{error}")),
            }
        });
        cx.notify();
    }

    pub fn load_automations(&mut self) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            crate::poll::refresh_new_session(&client, &core).await;
            crate::poll::refresh_list(&client, &core).await;
            crate::poll::refresh_automations(&client, &core).await;
            crate::poll::reset_automation_targets(&client, &core).await;
        });
    }

    pub fn refresh_automation_workspace_suggestions(&mut self, cx: &mut Context<Self>) {
        self.refresh_workspace_suggestions_for(WorkspacePickerTarget::Automation, cx);
    }

    pub fn refresh_automation_worktree_branches(&mut self, cx: &mut Context<Self>) {
        self.refresh_worktree_branches_for(WorkspacePickerTarget::Automation, cx);
    }

    pub fn validate_automation_cron(&mut self, cx: &mut Context<Self>) {
        let value = self.automation_cron.read(cx).value().trim().to_string();
        let preview = cron_next_runs(&value);
        self.with_core(|core| match preview {
            Ok(runs) => {
                core.automation_form.cron_error = None;
                core.automation_form.cron_next_runs = runs;
            }
            Err(error) => {
                core.automation_form.cron_error = Some(error);
                core.automation_form.cron_next_runs.clear();
            }
        });
        cx.notify();
    }

    pub fn dismiss_automation_workspace_popups(&mut self, cx: &mut Context<Self>) {
        self.dismiss_workspace_popups_for(WorkspacePickerTarget::Automation, cx);
    }

    pub fn load_more_automation_sessions(&mut self, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            crate::poll::load_more_automation_sessions(&client, &core).await;
        });
        cx.notify();
    }

    pub fn load_more_automation_workflows(&mut self, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            crate::poll::load_more_automation_workflows(&client, &core).await;
        });
        cx.notify();
    }

    pub fn load_automation_runs(&mut self, id: String) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.with_core(|core| core.automation_runs_loading = true);
        self.runtime.spawn(async move {
            let result = client.automation_runs(&id, 100, 0).await;
            let mut core = core.lock();
            core.automation_runs_loading = false;
            match result {
                Ok(page) => {
                    core.automation_runs.insert(id, page.runs);
                }
                Err(error) => core.error(format!("读取运行记录失败：{error}")),
            }
        });
    }

    fn reset_automation_form(
        &mut self,
        automation: Option<&Automation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut form = crate::state::AutomationForm {
            editing_id: automation.map(|automation| automation.id.clone()),
            ..Default::default()
        };
        if let Some(automation) = automation {
            form.project = automation.project.clone();
            form.trigger_mode = match &automation.trigger {
                AutomationTrigger::Schedule { .. } => AutomationTriggerMode::Schedule,
                AutomationTrigger::GithubWebhook { .. } => AutomationTriggerMode::Webhook,
            };
            form.session_mode = match &automation.session_options {
                AutomationSessionOptions::ExistingSession { .. } => {
                    AutomationSessionMode::ExistingSession
                }
                AutomationSessionOptions::ExistingWorkflow { .. } => {
                    AutomationSessionMode::ExistingWorkflow
                }
                AutomationSessionOptions::NewSession { .. } => AutomationSessionMode::NewSession,
                AutomationSessionOptions::NewWorkflow { .. } => AutomationSessionMode::NewWorkflow,
            };
            match &automation.session_options {
                AutomationSessionOptions::ExistingSession { session_id } => {
                    form.existing_session_target = Some(session_id.clone());
                }
                AutomationSessionOptions::ExistingWorkflow { workflow_id } => {
                    form.existing_workflow_target = Some(workflow_id.clone());
                }
                AutomationSessionOptions::NewSession {
                    machine,
                    agent,
                    use_worktree,
                    worktree_base_branch,
                    ..
                } => {
                    form.machine = Some(machine.clone());
                    form.agent = Some(agent.clone());
                    form.use_worktree = *use_worktree;
                    form.worktree_base_branch = worktree_base_branch
                        .clone()
                        .unwrap_or_else(|| "HEAD".to_string());
                }
                AutomationSessionOptions::NewWorkflow { .. } => {}
            }
        }
        self.with_core(|core| core.automation_form = form);
        let set = |input: &Entity<InputState>,
                   value: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            input.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
        };
        set(
            &self.automation_title,
            automation.map(|a| a.title.clone()).unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.automation_task,
            automation.map(|a| a.task.clone()).unwrap_or_default(),
            window,
            cx,
        );
        let cron = automation
            .and_then(|automation| match &automation.trigger {
                AutomationTrigger::Schedule { cron } => Some(cron.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let validate_cron = !cron.trim().is_empty();
        set(&self.automation_cron, cron, window, cx);
        let (repository, events) = automation
            .and_then(|automation| match &automation.trigger {
                AutomationTrigger::GithubWebhook { repository, events } => {
                    Some((repository.clone(), events.join(", ")))
                }
                _ => None,
            })
            .unwrap_or_else(|| (String::new(), "push".to_string()));
        set(&self.automation_repository, repository, window, cx);
        set(&self.automation_events, events, window, cx);
        let (workspace, plan) = automation
            .map(|automation| match &automation.session_options {
                AutomationSessionOptions::NewSession { workspace, .. } => {
                    (workspace.clone(), String::new())
                }
                AutomationSessionOptions::NewWorkflow { plan } => (String::new(), plan.clone()),
                AutomationSessionOptions::ExistingSession { .. }
                | AutomationSessionOptions::ExistingWorkflow { .. } => {
                    (String::new(), String::new())
                }
            })
            .unwrap_or_default();
        set(&self.automation_workspace, workspace, window, cx);
        set(&self.automation_plan, plan, window, cx);
        let labels = automation
            .map(|automation| automation.result_labels.clone())
            .unwrap_or_else(|| {
                vec![
                    AutomationResultLabel {
                        name: "通过".into(),
                        description: "任务成功完成".into(),
                    },
                    AutomationResultLabel {
                        name: "失败".into(),
                        description: "任务未成功完成".into(),
                    },
                ]
            });
        set(
            &self.automation_label1_name,
            labels
                .first()
                .map(|label| label.name.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.automation_label1_description,
            labels
                .first()
                .map(|label| label.description.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.automation_label2_name,
            labels
                .get(1)
                .map(|label| label.name.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.automation_label2_description,
            labels
                .get(1)
                .map(|label| label.description.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        self.refresh_automation_worktree_branches(cx);
        if validate_cron {
            self.validate_automation_cron(cx);
        }
    }
}

fn cron_next_runs(expression: &str) -> Result<Vec<u64>, String> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err("请输入 cron 表达式".to_string());
    }
    let cron = Cron::from_str(expression).map_err(|error| format!("cron 表达式无效: {error}"))?;
    let mut current = jiff::Zoned::now();
    let mut runs = Vec::with_capacity(5);
    for _ in 0..5 {
        current = cron
            .find_next_occurrence(&current, false)
            .map_err(|error| format!("计算触发时间失败: {error}"))?;
        runs.push(current.timestamp().as_millisecond().max(0) as u64);
    }
    Ok(runs)
}

fn automation_input(automation: &Automation, enabled: bool) -> AutomationInput {
    AutomationInput {
        title: automation.title.clone(),
        enabled,
        task: automation.task.clone(),
        project: automation.project.clone(),
        trigger: automation.trigger.clone(),
        session_options: automation.session_options.clone(),
        result_labels: automation.result_labels.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::cron_next_runs;

    #[test]
    fn cron_preview_returns_five_increasing_runs() {
        let runs = cron_next_runs("0 9 * * *").unwrap();
        assert_eq!(runs.len(), 5);
        assert!(runs.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn cron_preview_rejects_invalid_expression() {
        assert!(cron_next_runs("not a cron").is_err());
    }
}
