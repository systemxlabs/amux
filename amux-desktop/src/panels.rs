//! 三面板渲染：左侧会话列表、中间会话交互、右侧辅助面板。

use amux_common::api::TerminalState;
use amux_common::domain::SessionState;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::*;
use gpui_component::label::Label;
use gpui_component::*;
use gpui_component::{h_flex, v_flex, ActiveTheme, IconName, Sizable};

use crate::app::{text_input, AmuxApp};
use crate::state::{ListEntry, OpenTarget, SidePanel};
use crate::ui;

/// 左侧面板宽度。
const LEFT_WIDTH: f32 = 260.0;
/// 右侧面板宽度。
const RIGHT_WIDTH: f32 = 380.0;

/// 左侧面板：新建会话、会话列表（普通会话 + 工作流会话）、设置入口。
pub fn render_left(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let mut list = v_flex()
        .id("session-list")
        .flex_1()
        .w_full()
        .gap_1()
        .overflow_y_scroll();

    for entry in core.entries.clone() {
        match &entry {
            ListEntry::Session(session) => {
                list = list.child(session_row(
                    &session.id,
                    &session.title,
                    session.state,
                    session.updated_at,
                    false,
                    core,
                    this,
                    cx,
                ));
            }
            ListEntry::Workflow(workflow) => {
                let expanded = core.expanded_workflows.contains(&workflow.id);
                list = list.child(workflow_row(workflow, expanded, core, this, cx));
                if expanded {
                    for linked in &workflow.linked_sessions {
                        list = list.child(div().pl_6().child(session_row(
                            &linked.id,
                            &linked.title,
                            linked.state,
                            linked.updated_at,
                            false,
                            core,
                            this,
                            cx,
                        )));
                    }
                }
            }
        }
    }
    if core.entries.is_empty() {
        list = list.child(ui::empty_hint("暂无会话", cx.theme()));
    }

    let paging = h_flex()
        .gap_2()
        .p_2()
        .child(
            Button::new("load-more")
                .small()
                .ghost()
                .label("加载更多")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_core(|core| core.list_limit += 20);
                    this.with_core(|core| core.last.list = None);
                    cx.notify();
                })),
        )
        .child(
            Button::new("collapse")
                .small()
                .ghost()
                .label("收起")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_core(|core| {
                        core.list_limit = 20;
                        core.last.list = None;
                    });
                    cx.notify();
                })),
        );

    v_flex()
        .w(px(LEFT_WIDTH))
        .h_full()
        .bg(cx.theme().sidebar)
        .border_r_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .p_2()
                .gap_2()
                .items_center()
                .child(Label::new("amux").font_weight(FontWeight::SEMIBOLD))
                .child(div().flex_1())
                .child(
                    Button::new("new-session")
                        .small()
                        .primary()
                        .label("+")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.with_core(|core| {
                                core.open = None;
                                core.view = Default::default();
                                core.new_session.workflow_mode = false;
                            });
                            cx.notify();
                        })),
                ),
        )
        .child(list)
        .child(paging)
        .child(
            h_flex().px_2().gap_2().items_center().child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(this.status_label()),
            ),
        )
        .child(
            h_flex().p_2().child(
                Button::new("open-settings")
                    .small()
                    .ghost()
                    .icon(IconName::Settings)
                    .label("设置")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.with_core(|core| core.settings_open = true);
                        cx.notify();
                    })),
            ),
        )
        .into_any()
}

/// 单条会话行：标题（或重命名输入框）、状态徽章、操作按钮。
#[allow(clippy::too_many_arguments)]
fn session_row(
    id: &str,
    title: &str,
    state: SessionState,
    updated_at: u64,
    is_workflow: bool,
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let open = match (is_workflow, core.open.as_ref()) {
        (true, Some(OpenTarget::Workflow(current))) => current == id,
        (false, Some(OpenTarget::Session(current))) => current == id,
        _ => false,
    };
    let title = if title.trim().is_empty() {
        "未命名会话".to_string()
    } else {
        title.to_string()
    };
    let renaming = this.renaming_id.as_deref() == Some(id);

    let mut row = h_flex()
        .id(format!("session-{id}"))
        .w_full()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded_md()
        .when(open, |row| row.bg(cx.theme().accent))
        .hover(|row| row.bg(cx.theme().accent))
        .on_click(cx.listener({
            let id = id.to_string();
            move |this, _, _, cx| {
                this.open_entry(&id, cx);
            }
        }));

    if renaming {
        row = row.child(text_input(&this.rename_input));
        row = row.child(
            Button::new(format!("rename-ok-{id}"))
                .xsmall()
                .primary()
                .label("确定")
                .on_click(cx.listener(|this, _, _, cx| this.commit_rename(cx))),
        );
    } else {
        row = row.child(
            div()
                .flex_1()
                .overflow_hidden()
                .child(Label::new(ui::truncate(&title, 28)).text_sm()),
        );
    }
    row = row.child(ui::state_badge(state, cx.theme()));
    row = row.child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(ui::timestamp(updated_at)),
    );
    row = row.child(
        Button::new(format!("rename-{id}"))
            .xsmall()
            .ghost()
            .label("改名")
            .on_click(cx.listener({
                let id = id.to_string();
                move |this, _, window, cx| {
                    this.begin_rename(&id, window, cx);
                }
            })),
    );
    row = row.child(
        Button::new(format!("delete-{id}"))
            .xsmall()
            .ghost()
            .label("删除")
            .on_click(cx.listener({
                let entry = if is_workflow {
                    ListEntry::Workflow(amux_common::api::Workflow {
                        id: id.to_string(),
                        title: title.clone(),
                        state,
                        plan: String::new(),
                        created_at: 0,
                        updated_at,
                        linked_sessions: Vec::new(),
                    })
                } else {
                    ListEntry::Session(amux_common::api::Session {
                        id: id.to_string(),
                        title: title.clone(),
                        state,
                        machine: String::new(),
                        agent: String::new(),
                        workspace: String::new(),
                        worktree_dir: String::new(),
                        created_at: 0,
                        updated_at,
                    })
                };
                move |this, _, window, cx| {
                    this.confirm_delete(entry.clone(), window, cx);
                }
            })),
    );
    row.into_any()
}

/// 工作流会话行（标题带「工作流」标记，可展开/折叠关联普通会话）。
fn workflow_row(
    workflow: &amux_common::api::Workflow,
    expanded: bool,
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let row = session_row(
        &workflow.id,
        &workflow.title,
        workflow.state,
        workflow.updated_at,
        true,
        core,
        this,
        cx,
    );
    let toggle = Button::new(format!("toggle-{}", workflow.id))
        .xsmall()
        .ghost()
        .icon(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .on_click(cx.listener({
            let id = workflow.id.clone();
            move |this, _, _, cx| {
                this.with_core(|core| {
                    if core.expanded_workflows.contains(&id) {
                        core.expanded_workflows.remove(&id);
                    } else {
                        core.expanded_workflows.insert(id.clone());
                    }
                });
                cx.notify();
            }
        }));
    h_flex()
        .w_full()
        .gap_1()
        .items_center()
        .child(toggle)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().primary)
                .child("工作流"),
        )
        .child(div().flex_1().child(row))
        .into_any()
}

/// 中间面板：新建会话视图或会话交互视图。
pub fn render_middle(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    match core.open.clone() {
        None => new_session_view(core, this, cx),
        Some(_) => interaction_view(core, this, cx),
    }
}

/// 新建会话视图：普通 / 工作流模式。
fn new_session_view(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let workflow_mode = core.new_session.workflow_mode;
    let mut mode_row = h_flex().gap_2().p_3().child(
        {
            let button = Button::new("mode-session").small().label("普通会话");
            if workflow_mode {
                button
            } else {
                button.primary()
            }
        }
        .on_click(cx.listener(|this, _, _, cx| {
            this.with_core(|core| core.new_session.workflow_mode = false);
            cx.notify();
        })),
    );
    let workflow_button = {
        let button = Button::new("mode-workflow").small().label("工作流会话");
        if workflow_mode {
            button.primary()
        } else {
            button
        }
    };
    mode_row = mode_row.child(workflow_button.on_click(cx.listener(|this, _, _, cx| {
        this.with_core(|core| core.new_session.workflow_mode = true);
        cx.notify();
    })));

    let mut body = v_flex().flex_1().gap_3().p_3();
    if workflow_mode {
        let configured = core.settings.orchestrator.is_some();
        body = body.child(Label::new("工作计划（选择已保存计划或直接输入）"));
        if !core.settings.plans.is_empty() {
            let mut plans = h_flex().gap_2().flex_wrap();
            for plan in core.settings.plans.clone() {
                plans = plans.child(
                    Button::new(format!("plan-{}", plan.name))
                        .small()
                        .ghost()
                        .label(ui::truncate(&plan.name, 18))
                        .on_click(cx.listener({
                            let text = plan.plan.clone();
                            move |this, _, window, cx| {
                                this.plan_input.update(cx, |state, cx| {
                                    state.set_value(text.clone(), window, cx)
                                });
                                cx.notify();
                            }
                        })),
                );
            }
            body = body.child(plans);
        }
        body = body.child(text_input(&this.plan_input));
        if !configured {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .child("编排智能体未配置，请在设置中配置后创建"),
            );
        }
        body = body.child(
            Button::new("create-workflow")
                .small()
                .primary()
                .label("创建工作流会话")
                .on_click(cx.listener(|this, _, _, cx| this.create_session(cx))),
        );
    } else {
        body = body.child(Label::new("机器与 agent"));
        let mut agents_row = h_flex().gap_2().flex_wrap();
        for machine in core.settings.machines.clone() {
            let selected = core.new_session.machine.as_deref() == Some(machine.name.as_str());
            agents_row = agents_row.child(
                Button::new(format!("machine-{}", machine.name))
                    .small()
                    .when(selected, |button| button.primary())
                    .label(machine.name.clone())
                    .on_click(cx.listener({
                        let name = machine.name.clone();
                        move |this, _, _, cx| {
                            this.with_core(|core| {
                                core.new_session.machine = Some(name.clone());
                                core.new_session.agent = None;
                            });
                            cx.notify();
                        }
                    })),
            );
        }
        if core.settings.machines.is_empty() {
            agents_row = agents_row.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("暂无已连接机器"),
            );
        }
        body = body.child(agents_row);

        if let Some(machine) = core.new_session.machine.clone() {
            let agents = core
                .settings
                .agents
                .iter()
                .find(|(name, _)| name == &machine)
                .map(|(_, agents)| agents.clone())
                .unwrap_or_default();
            let mut row = h_flex().gap_2().flex_wrap();
            for agent in agents {
                let selected = core.new_session.agent.as_deref() == Some(agent.name.as_str());
                let button = Button::new(format!("agent-{}", agent.name))
                    .small()
                    .disabled(!agent.available)
                    .label(format!(
                        "{}{}",
                        agent.name,
                        if agent.available {
                            ""
                        } else {
                            "（不可用）"
                        }
                    ))
                    .on_click(cx.listener({
                        let name = agent.name.clone();
                        move |this, _, _, cx| {
                            this.with_core(|core| core.new_session.agent = Some(name.clone()));
                            cx.notify();
                        }
                    }));
                let button = if selected { button.primary() } else { button };
                row = row.child(button);
            }
            body = body.child(row);
        }

        body = body.child(Label::new("工作目录"));
        body = body.child(text_input(&this.workspace_input));
        if !core.recent_workspaces.is_empty() {
            let mut recent = h_flex().gap_2().flex_wrap();
            for workspace in core
                .recent_workspaces
                .iter()
                .filter(|workspace| {
                    core.new_session.machine.as_deref() == Some(workspace.machine.as_str())
                })
                .take(8)
            {
                recent = recent.child(
                    Button::new(format!("recent-{}", workspace.workspace))
                        .small()
                        .ghost()
                        .label(ui::truncate(&workspace.workspace, 24))
                        .on_click(cx.listener({
                            let path = workspace.workspace.clone();
                            move |this, _, window, cx| {
                                this.workspace_input.update(cx, |state, cx| {
                                    state.set_value(path.clone(), window, cx)
                                });
                                cx.notify();
                            }
                        })),
                );
            }
            body = body.child(recent);
        }
        body = body.child(
            h_flex()
                .gap_2()
                .items_center()
                .child({
                    let button = Button::new("worktree-toggle").small().label(format!(
                        "worktree：{}",
                        if core.new_session.use_worktree {
                            "开"
                        } else {
                            "关"
                        }
                    ));
                    let button = if core.new_session.use_worktree {
                        button.primary()
                    } else {
                        button
                    };
                    button.on_click(cx.listener(|this, _, _, cx| {
                        this.with_core(|core| {
                            core.new_session.use_worktree = !core.new_session.use_worktree
                        });
                        cx.notify();
                    }))
                })
                .child(
                    Button::new("create-session")
                        .small()
                        .primary()
                        .label("创建会话")
                        .on_click(cx.listener(|this, _, _, cx| this.create_session(cx))),
                ),
        );
    }

    v_flex()
        .flex_1()
        .h_full()
        .child(mode_row)
        .child(body)
        .into_any()
}

/// 会话交互视图：标题栏、对话历史、实时活动、输入区、会话选项、悬浮按钮。
fn interaction_view(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let title = core.view.title();
    let title = if title.trim().is_empty() {
        "未命名会话".to_string()
    } else {
        title
    };
    let header = h_flex()
        .p_2()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(Label::new(ui::truncate(&title, 40)))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(core.view.subtitle()),
        )
        .child(ui::state_badge(core.view.state(), cx.theme()))
        .child(div().flex_1());

    let mut history = v_flex()
        .id("history")
        .flex_1()
        .gap_2()
        .p_3()
        .overflow_y_scroll();
    for item in &core.view.detail.history {
        let (bubble_bg, role) = match item {
            amux_common::domain::HistoryItem::UserMessage { .. } => (cx.theme().accent, "我"),
            amux_common::domain::HistoryItem::AgentMessage { .. } => {
                (cx.theme().secondary, "agent")
            }
        };
        history = history.child(
            v_flex()
                .max_w(relative(0.8))
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(bubble_bg)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(role),
                )
                .child(div().text_sm().child(ui::history_text(item))),
        );
    }
    if core.view.detail.history.is_empty() {
        history = history.child(ui::empty_hint("暂无对话", cx.theme()));
    }

    let ongoing = core
        .view
        .detail
        .ongoing
        .as_ref()
        .map(ui::activity_line)
        .unwrap_or_default();

    let mut quick = h_flex().gap_2().flex_wrap().px_3();
    for command in core.settings.quick_commands.clone() {
        quick = quick.child(
            Button::new(format!("quick-{}", command.name))
                .xsmall()
                .ghost()
                .label(ui::truncate(&command.name, 12))
                .on_click(cx.listener({
                    let prompt = command.prompt.clone();
                    move |this, _, window, cx| {
                        this.input
                            .update(cx, |state, cx| state.set_value(prompt.clone(), window, cx));
                        cx.notify();
                    }
                })),
        );
    }

    let mut options = h_flex().gap_2().flex_wrap().px_3();
    for option in core.view.detail.config_options.clone() {
        let current = match &option.kind {
            amux_common::domain::SessionConfigKind::Select { current_value, .. } => {
                current_value.clone()
            }
            amux_common::domain::SessionConfigKind::Boolean { current_value } => {
                current_value.to_string()
            }
        };
        options = options.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{}：{}", option.name, current)),
        );
    }

    let footer = v_flex()
        .gap_2()
        .p_3()
        .border_t_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(if ongoing.is_empty() {
                    String::new()
                } else {
                    format!("实时活动：{ongoing}")
                }),
        )
        .child(quick)
        .child(options)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().child(text_input(&this.input)))
                .child(
                    Button::new("send")
                        .small()
                        .primary()
                        .label("发送")
                        .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                )
                .child(
                    Button::new("cancel-work")
                        .small()
                        .label("取消")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                ),
        );

    let floating = v_flex().absolute().top_12().right_3().gap_1().children(
        crate::state::SidePanel::ALL.iter().map(|panel| {
            let label = panel.label();
            let panel = *panel;
            Button::new(format!("panel-{}", label))
                .xsmall()
                .ghost()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.with_core(|core| {
                        core.side_panel = Some(panel);
                        match panel {
                            SidePanel::Activities => core.last.activities = None,
                            SidePanel::Plan | SidePanel::Detail => core.last.plan = None,
                            _ => {}
                        }
                    });
                    if panel == SidePanel::Terminal {
                        this.open_terminal(cx);
                    }
                    if panel == SidePanel::Diff {
                        this.refresh_diff(cx);
                    }
                    cx.notify();
                }))
        }),
    );

    v_flex()
        .relative()
        .flex_1()
        .h_full()
        .child(header)
        .child(history)
        .child(footer)
        .child(floating)
        .into_any()
}

/// 右侧面板：工作目录 / 改动 / 详情 / 活动 / 计划 / 终端。
pub fn render_right(
    core: &crate::state::Core,
    panel: SidePanel,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let header = h_flex()
        .p_2()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(Label::new(panel.label()))
        .child(div().flex_1())
        .child(
            Button::new("close-panel")
                .xsmall()
                .ghost()
                .icon(IconName::Close)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_core(|core| core.side_panel = None);
                    cx.notify();
                })),
        );

    let mut body = v_flex()
        .id("side-body")
        .flex_1()
        .gap_2()
        .p_3()
        .overflow_y_scroll();
    match panel {
        SidePanel::Detail => {
            if let Some(session) = &core.view.session {
                body = body
                    .child(detail_row("会话 ID", &session.id))
                    .child(detail_row("所属机器", &session.machine))
                    .child(detail_row("所属 agent", &session.agent))
                    .child(detail_row("工作目录", &session.workspace))
                    .child(detail_row("worktree", &session.worktree_dir))
                    .child(detail_row("创建时间", &ui::timestamp(session.created_at)))
                    .child(detail_row("最近活跃", &ui::timestamp(session.updated_at)))
                    .child(detail_row(
                        "上下文",
                        &format!(
                            "{} / {} tokens",
                            core.view.detail.context_size, core.view.detail.context_window_size
                        ),
                    ));
            } else if let Some(workflow) = &core.view.workflow {
                body = body
                    .child(detail_row("工作流 ID", &workflow.id))
                    .child(detail_row("计划", &workflow.plan))
                    .child(detail_row("创建时间", &ui::timestamp(workflow.created_at)))
                    .child(detail_row("最近活跃", &ui::timestamp(workflow.updated_at)));
            } else {
                body = body.child(ui::empty_hint("未选择会话", cx.theme()));
            }
        }
        SidePanel::Activities => {
            for activity in core.view.detail.activities.iter().rev() {
                body = body.child(
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().secondary)
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(ui::timestamp(ui::activity_timestamp(activity))),
                        )
                        .child(div().text_sm().child(ui::activity_line(activity))),
                );
            }
            if core.view.detail.activities.is_empty() {
                body = body.child(ui::empty_hint("暂无活动", cx.theme()));
            }
        }
        SidePanel::Plan => {
            for entry in &core.view.detail.plan {
                let glyph = match entry.status {
                    amux_common::domain::SessionPlanStatus::Completed => "✓",
                    amux_common::domain::SessionPlanStatus::InProgress => "●",
                    amux_common::domain::SessionPlanStatus::Pending => "○",
                };
                body = body.child(
                    h_flex()
                        .gap_2()
                        .child(div().child(glyph))
                        .child(div().text_sm().child(entry.content.clone())),
                );
            }
            if core.view.detail.plan.is_empty() {
                body = body.child(ui::empty_hint("暂无计划", cx.theme()));
            }
        }
        SidePanel::Diff => {
            body = body.child(
                Button::new("refresh-diff")
                    .xsmall()
                    .ghost()
                    .label("刷新改动")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_diff(cx))),
            );
            match &core.view.detail.diff {
                Some(diff) if diff.not_repo => {
                    body = body.child(ui::empty_hint("工作目录不是 git 仓库", cx.theme()));
                }
                Some(diff) => {
                    for file in &diff.files {
                        let mut file_block =
                            v_flex()
                                .gap_1()
                                .p_2()
                                .rounded_md()
                                .bg(cx.theme().secondary)
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(file.path.clone()),
                                        )
                                        .child(div().text_xs().child(format!(
                                            "+{} -{}",
                                            file.additions, file.deletions
                                        )))
                                        .child(div().flex_1())
                                        .child(
                                            Button::new(format!("restore-file-{}", file.path))
                                                .xsmall()
                                                .ghost()
                                                .label("撤销文件")
                                                .on_click(cx.listener({
                                                    let path = file.path.clone();
                                                    move |this, _, _, cx| {
                                                        this.restore(Some(path.clone()), None, cx)
                                                    }
                                                })),
                                        ),
                                );
                        for hunk in &file.hunks {
                            file_block =
                                file_block.child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(hunk.header.clone()),
                                        )
                                        .child(
                                            Button::new(format!(
                                                "restore-hunk-{}-{}",
                                                file.path, hunk.header
                                            ))
                                            .xsmall()
                                            .ghost()
                                            .label("撤销代码块")
                                            .on_click(cx.listener({
                                                let path = file.path.clone();
                                                let patch = hunk.patch.clone();
                                                move |this, _, _, cx| {
                                                    this.restore(
                                                        Some(path.clone()),
                                                        Some(patch.clone()),
                                                        cx,
                                                    )
                                                }
                                            })),
                                        ),
                                );
                        }
                        body = body.child(file_block);
                    }
                    if diff.files.is_empty() {
                        body = body.child(ui::empty_hint("没有改动", cx.theme()));
                    }
                }
                None => body = body.child(ui::empty_hint("点击「刷新改动」加载", cx.theme())),
            }
        }
        SidePanel::Workspace => {
            let (machine, path) = core
                .view
                .session
                .as_ref()
                .map(|session| {
                    (
                        session.machine.clone(),
                        if session.worktree_dir.is_empty() {
                            session.workspace.clone()
                        } else {
                            session.worktree_dir.clone()
                        },
                    )
                })
                .unzip();
            match (machine, path) {
                (Some(machine), Some(path)) => {
                    body = body.child(detail_row("路径", &path));
                    for entry in core.view.detail.workspace_entries.clone() {
                        let mut row = h_flex()
                            .gap_2()
                            .child(div().child(if entry.is_dir { "📁" } else { "📄" }))
                            .child(div().text_sm().child(entry.name.clone()))
                            .child(div().flex_1())
                            .child(div().text_xs().child(format!("{}", entry.size)));
                        if !entry.is_dir {
                            row = row.child(
                                Button::new(format!("read-{}", entry.path))
                                    .xsmall()
                                    .ghost()
                                    .label("查看")
                                    .on_click(cx.listener({
                                        let machine = machine.clone();
                                        let path = entry.path.clone();
                                        move |this, _, _, cx| {
                                            this.read_file(machine.clone(), path.clone(), cx)
                                        }
                                    })),
                            );
                        }
                        body = body.child(row);
                    }
                    if let Some(content) = core.view.detail.file_content.clone() {
                        body = body.child(
                            v_flex()
                                .gap_1()
                                .p_2()
                                .rounded_md()
                                .bg(cx.theme().secondary)
                                .child(div().text_xs().child("文件内容"))
                                .child(div().text_xs().child(ui::truncate(&content, 4000))),
                        );
                    }
                    if core.view.detail.workspace_entries.is_empty() {
                        body = body.child(
                            Button::new("load-workspace")
                                .xsmall()
                                .ghost()
                                .label("加载工作目录")
                                .on_click(cx.listener({
                                    let machine = machine.clone();
                                    move |this, _, _, cx| {
                                        this.load_workspace(machine.clone(), cx);
                                    }
                                })),
                        );
                    }
                }
                _ => body = body.child(ui::empty_hint("未选择会话", cx.theme())),
            }
        }
        SidePanel::Terminal => {
            body = body.child(
                Button::new("open-terminal")
                    .xsmall()
                    .ghost()
                    .label("新建终端")
                    .on_click(cx.listener(|this, _, _, cx| this.open_terminal(cx))),
            );
            let active = core.view.detail.active_terminal.clone();
            for terminal in &core.view.detail.terminals {
                let id = terminal.id.clone();
                body = body.child(
                    h_flex()
                        .gap_2()
                        .child(div().text_xs().child(ui::truncate(&id, 12)))
                        .child(div().text_xs().child(format!(
                            "{}×{} {}",
                            terminal.cols,
                            terminal.rows,
                            match terminal.state {
                                TerminalState::Running => "运行中",
                                TerminalState::Exited => "已退出",
                            }
                        )))
                        .text_color(if active.as_deref() == Some(id.as_str()) {
                            cx.theme().primary
                        } else {
                            cx.theme().foreground
                        })
                        .child(div().flex_1())
                        .child(
                            Button::new(format!("select-terminal-{id}"))
                                .xsmall()
                                .ghost()
                                .label("切换")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        this.select_terminal(id.clone(), cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new(format!("resize-terminal-{id}"))
                                .xsmall()
                                .ghost()
                                .label("尺寸 -")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        this.resize_terminal(id.clone(), 80, 24, cx)
                                    }
                                })),
                        )
                        .child(
                            Button::new(format!("close-terminal-{id}"))
                                .xsmall()
                                .ghost()
                                .label("关闭")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| this.close_terminal(id.clone(), cx)
                                })),
                        ),
                );
            }
            body = body.child(crate::terminal_view::render(core, this, cx));
        }
    }

    v_flex()
        .w(px(RIGHT_WIDTH))
        .h_full()
        .bg(cx.theme().popover)
        .border_l_1()
        .border_color(cx.theme().border)
        .child(header)
        .child(body)
        .into_any()
}

fn detail_row(label: &str, value: &str) -> AnyElement {
    h_flex()
        .gap_2()
        .child(div().w(px(84.0)).text_xs().child(label.to_string()))
        .child(div().flex_1().text_sm().child(value.to_string()))
        .into_any()
}
