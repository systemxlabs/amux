//! 三面板渲染：左侧会话列表、中间会话交互、右侧辅助面板。

use amux_common::api::TerminalState;
use amux_common::domain::{
    GitDiffFile, GitDiffHunk, SessionConfigKind, SessionConfigOptionValue, SessionState,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::label::Label;
use gpui_component::select::Select;
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::*;
use gpui_component::{h_flex, v_flex, ActiveTheme, IconName, Sizable};

use crate::app::{text_input, AmuxApp};
use crate::difftree::{self, DiffNode};
use crate::state::{ListEntry, OpenTarget, SettingsTab, SidePanel, WorkspaceNode};
use crate::ui;

/// 左侧面板宽度与可拖拽范围。
pub const LEFT_WIDTH: f32 = 260.0;
pub const LEFT_MIN_WIDTH: f32 = 180.0;
pub const LEFT_MAX_WIDTH: f32 = 420.0;
/// 右侧面板宽度与可拖拽范围。
pub const RIGHT_WIDTH: f32 = 400.0;
pub const RIGHT_MIN_WIDTH: f32 = 280.0;
pub const RIGHT_MAX_WIDTH: f32 = 900.0;
/// 改动审查视图左侧文件树宽度。
const DIFF_TREE_WIDTH: f32 = 180.0;

/// 终端尺寸调整步长与下限。
const TERMINAL_COL_STEP: u16 = 10;
const TERMINAL_ROW_STEP: u16 = 5;
const TERMINAL_MIN_COLS: u16 = 20;
const TERMINAL_MIN_ROWS: u16 = 5;

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
            ListEntry::Session(_) => {
                list = list.child(session_row(&entry, core, this, cx));
            }
            ListEntry::Workflow(workflow) => {
                let expanded = core.expanded_workflows.contains(&workflow.id);
                list = list.child(workflow_row(&entry, expanded, core, this, cx));
                if expanded {
                    for linked in &workflow.linked_sessions {
                        list = list.child(div().pl_6().child(session_row(
                            &ListEntry::Session(linked.clone()),
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
        .size_full()
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
fn session_row(
    entry: &ListEntry,
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let id = entry.id();
    let state = entry.state();
    let open = match (entry, core.open.as_ref()) {
        (ListEntry::Workflow(_), Some(OpenTarget::Workflow(current))) => current == id,
        (ListEntry::Session(_), Some(OpenTarget::Session(current))) => current == id,
        _ => false,
    };
    let title = {
        let title = entry.title();
        if title.trim().is_empty() {
            "未命名会话".to_string()
        } else {
            title
        }
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
    if state == SessionState::Busy {
        row = row.child(Spinner::new().xsmall().color(cx.theme().primary));
    }
    row = row.child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(ui::timestamp(entry.updated_at())),
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
                let entry = entry.clone();
                move |this, _, window, cx| {
                    this.confirm_delete(entry.clone(), window, cx);
                }
            })),
    );
    row.into_any()
}

/// 工作流会话行（标题带「工作流」标记，可展开/折叠关联普通会话）。
fn workflow_row(
    entry: &ListEntry,
    expanded: bool,
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let row = session_row(entry, core, this, cx);
    let toggle = Button::new(format!("toggle-{}", entry.id()))
        .xsmall()
        .ghost()
        .icon(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .on_click(cx.listener({
            let id = entry.id().to_string();
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

/// 指定机器上某 agent 是否可用。
fn machine_agent_available(core: &crate::state::Core, machine: &str, agent: &str) -> bool {
    core.settings
        .agents
        .iter()
        .find(|(name, _)| name == machine)
        .is_some_and(|(_, agents)| {
            agents
                .iter()
                .any(|item| item.name == agent && item.available)
        })
}

/// 新建会话表单已选中的 agent 是否可用。
fn selected_agent_available(core: &crate::state::Core) -> bool {
    match (
        core.new_session.machine.as_deref(),
        core.new_session.agent.as_deref(),
    ) {
        (Some(machine), Some(agent)) => machine_agent_available(core, machine, agent),
        _ => false,
    }
}

/// 当前视图的 agent 可用状态：普通会话为其所属 agent，工作流会话为编排智能体。
fn view_agent_available(core: &crate::state::Core) -> bool {
    match &core.view.session {
        Some(session) => machine_agent_available(core, &session.machine, &session.agent),
        None => core.settings.orchestrator.is_some(),
    }
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
        if core.settings.orchestrator.is_none() {
            body = body
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child("编排智能体未配置，请先在设置中完成配置"),
                )
                .child(
                    Button::new("goto-orchestrator")
                        .small()
                        .primary()
                        .label("前往设置")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.with_core(|core| {
                                core.settings_open = true;
                                core.settings_tab = SettingsTab::Orchestrator;
                            });
                            this.load_orchestrator_form(window, cx);
                            cx.notify();
                        })),
                );
        } else {
            let plan = this.plan_input.read(cx).value().trim().to_string();
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
                                move |this, _, window, cx| this.set_plan(text.clone(), window, cx)
                            })),
                    );
                }
                body = body.child(plans);
            }
            body = body.child(text_input(&this.plan_input));
            body = body.child(
                Button::new("create-workflow")
                    .small()
                    .primary()
                    .disabled(plan.is_empty())
                    .label("创建工作流会话")
                    .on_click(cx.listener(|this, _, _, cx| this.create_session(cx))),
            );
        }
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
                                core.new_session.suggestions.clear();
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
        let workspace = this.workspace_input.read(cx).value().trim().to_string();
        if !core.new_session.suggestions.is_empty() {
            let mut suggestions = v_flex().gap_1();
            for entry in &core.new_session.suggestions {
                suggestions = suggestions.child(
                    Button::new(format!("suggest-{}", entry.path))
                        .small()
                        .ghost()
                        .label(ui::truncate(&entry.path, 48))
                        .on_click(cx.listener({
                            let path = entry.path.clone();
                            move |this, _, window, cx| this.set_workspace(path.clone(), window, cx)
                        })),
                );
            }
            body = body.child(suggestions);
        }
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
                            move |this, _, window, cx| this.set_workspace(path.clone(), window, cx)
                        })),
                );
            }
            body = body.child(recent);
        }
        let can_create = !workspace.is_empty() && selected_agent_available(core);
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
                        .disabled(!can_create)
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
        .child(ui::availability_badge(
            view_agent_available(core),
            cx.theme(),
        ))
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
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(role),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(ui::timestamp(ui::history_timestamp(item))),
                        ),
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

    // 快捷指令：点击直接作为用户输入发送（docs/PRD.md「快捷指令」）
    let mut quick = h_flex().gap_2().flex_wrap().px_3();
    for command in core.settings.quick_commands.clone() {
        quick = quick.child(
            Button::new(format!("quick-{}", command.name))
                .xsmall()
                .ghost()
                .label(ui::truncate(&command.name, 12))
                .on_click(cx.listener({
                    let prompt = command.prompt.clone();
                    move |this, _, _, cx| this.send_quick_command(prompt.clone(), cx)
                })),
        );
    }

    let options = session_options(core, this, cx);

    // 输入区：附件、斜杠命令上拉框、多行输入（Enter 发送 / Shift+Enter 换行）、发送与取消
    let row = h_flex()
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
        );
    let mut composer = v_flex().gap_2();
    if !this.attachments.is_empty() {
        let mut attachments = h_flex().gap_2().flex_wrap();
        for (ix, attachment) in this.attachments.iter().enumerate() {
            attachments = attachments.child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().secondary)
                    .child(div().text_xs().child(ui::truncate(&attachment.label, 24)))
                    .child(
                        Button::new(format!("drop-attachment-{ix}"))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("移除附件")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_attachment(ix, cx)),
                            ),
                    ),
            );
        }
        composer = composer.child(attachments);
    }
    let candidates = this.slash_candidates(cx);
    if !candidates.is_empty() {
        let selected = this.slash_selected.min(candidates.len() - 1);
        let mut list = v_flex()
            .id("slash-commands")
            .max_h(rems(12.))
            .overflow_y_scroll()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover);
        for (ix, command) in candidates.iter().enumerate() {
            list = list.child(
                div()
                    .id(format!("slash-{}", command.name))
                    .px_2()
                    .py_1()
                    .when(ix == selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent))
                    .on_click(cx.listener({
                        let command = command.clone();
                        move |this, _, window, cx| this.apply_slash_command(&command, window, cx)
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().child(format!("/{}", command.name)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(ui::truncate(&command.description, 32)),
                            ),
                    ),
            );
        }
        composer = composer.child(list);
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
        .when(!core.view.detail.config_options.is_empty(), |footer| {
            footer.child(options)
        })
        .child(
            // 输入区整体接收粘贴与拖拽：图片/文件成为附件，文本交给输入框
            v_flex()
                .id("composer")
                .gap_2()
                .capture_action(cx.listener(
                    |this, action: &gpui_component::input::Paste, _window, cx| {
                        this.paste_into_composer(action, cx)
                    },
                ))
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.composer_key_down(event, window, cx)
                }))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                    this.attach_paths(paths.paths(), cx)
                }))
                .child(composer)
                .child(row),
        );

    let floating = v_flex().absolute().top_12().right_3().gap_1().children(
        SidePanel::for_session(core.is_workflow())
            .into_iter()
            .map(|panel| {
                let label = panel.label();
                Button::new(format!("panel-{}", label))
                    .xsmall()
                    .ghost()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_side_panel(panel, cx)))
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

    // 改动审查视图自带工具栏与滚动区域，交由它自己管理内边距与溢出
    let mut body = v_flex()
        .id("side-body")
        .flex_1()
        .min_h_0()
        .gap_2()
        .p_3()
        .overflow_y_scroll()
        .when(panel == SidePanel::Diff, |body| {
            body.p_0().overflow_hidden()
        });
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
            body = body.child(diff_review(core, this, cx));
        }
        SidePanel::Workspace => {
            let target = core
                .view
                .session
                .as_ref()
                .map(|session| (session.machine.clone(), session.root_dir().to_string()));
            match target {
                Some((machine, path)) => {
                    body = body.child(detail_row("路径", &path));
                    body = body.children(workspace_nodes(
                        &core.view.detail.workspace_tree,
                        &machine,
                        0,
                        cx,
                    ));
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
                    if core.view.detail.workspace_tree.is_empty() {
                        body = body.child(
                            Button::new("load-workspace")
                                .xsmall()
                                .ghost()
                                .label("加载工作目录")
                                .on_click(cx.listener(|this, _, _, cx| this.load_workspace(cx))),
                        );
                    }
                }
                None => body = body.child(ui::empty_hint("未选择会话", cx.theme())),
            }
        }
        SidePanel::Terminal => {
            body = body.child(
                Button::new("open-terminal")
                    .xsmall()
                    .ghost()
                    .label("新建终端")
                    .on_click(cx.listener(|this, _, _, cx| this.new_terminal(cx))),
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
                            Button::new(format!("shrink-terminal-{id}"))
                                .xsmall()
                                .ghost()
                                .label("尺寸 -")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    let (cols, rows) = (terminal.cols, terminal.rows);
                                    move |this, _, _, cx| {
                                        this.resize_terminal(
                                            id.clone(),
                                            cols.saturating_sub(TERMINAL_COL_STEP)
                                                .max(TERMINAL_MIN_COLS),
                                            rows.saturating_sub(TERMINAL_ROW_STEP)
                                                .max(TERMINAL_MIN_ROWS),
                                            cx,
                                        )
                                    }
                                })),
                        )
                        .child(
                            Button::new(format!("grow-terminal-{id}"))
                                .xsmall()
                                .ghost()
                                .label("尺寸 +")
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    let (cols, rows) = (terminal.cols, terminal.rows);
                                    move |this, _, _, cx| {
                                        this.resize_terminal(
                                            id.clone(),
                                            cols.saturating_add(TERMINAL_COL_STEP),
                                            rows.saturating_add(TERMINAL_ROW_STEP),
                                            cx,
                                        )
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
        .size_full()
        .bg(cx.theme().popover)
        .border_l_1()
        .border_color(cx.theme().border)
        .child(header)
        .child(body)
        .into_any()
}

/// 工作目录树：目录行可折叠/展开（未加载的子目录在展开时拉取），文件行可查看内容。
fn workspace_nodes(
    nodes: &[WorkspaceNode],
    machine: &str,
    depth: usize,
    cx: &mut Context<AmuxApp>,
) -> Vec<AnyElement> {
    let indent = px(depth as f32 * 12.0);
    let mut rows = Vec::new();
    for node in nodes {
        let entry = &node.entry;
        if !entry.is_dir {
            rows.push(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(indent))
                    .child(div().text_sm().child(entry.name.clone()))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}", entry.size)),
                    )
                    .child(
                        Button::new(format!("read-{}", entry.path))
                            .xsmall()
                            .ghost()
                            .label("查看")
                            .on_click(cx.listener({
                                let machine = machine.to_string();
                                let path = entry.path.clone();
                                move |this, _, _, cx| {
                                    this.read_file(machine.clone(), path.clone(), cx)
                                }
                            })),
                    )
                    .into_any(),
            );
            continue;
        }
        rows.push(
            h_flex()
                .gap_1()
                .items_center()
                .child(div().w(indent))
                .child(
                    Button::new(format!("toggle-{}", entry.path))
                        .xsmall()
                        .ghost()
                        .label(if node.expanded { "▾" } else { "▸" })
                        .on_click(cx.listener({
                            let path = entry.path.clone();
                            move |this, _, _, cx| this.toggle_workspace_dir(path.clone(), cx)
                        })),
                )
                .child(div().text_sm().child(entry.name.clone()))
                .into_any(),
        );
        if node.expanded {
            if let Some(children) = &node.children {
                rows.extend(workspace_nodes(children, machine, depth + 1, cx));
            }
        }
    }
    rows
}

fn detail_row(label: &str, value: &str) -> AnyElement {
    h_flex()
        .gap_2()
        .child(div().w(rems(5.25)).text_xs().child(label.to_string()))
        .child(div().flex_1().text_sm().child(value.to_string()))
        .into_any()
}

/// 会话选项控件：按选项类型渲染下拉框或开关（docs/PRD.md「会话选项」）。
fn session_options(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let mut row = h_flex().gap_3().flex_wrap().px_3();
    for option in core.view.detail.config_options.clone() {
        let label = div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(option.name.clone());
        match &option.kind {
            SessionConfigKind::Boolean { current_value } => {
                let id = option.id.clone();
                let checked = *current_value;
                row = row.child(
                    h_flex().gap_2().items_center().child(label).child(
                        Switch::new(format!("config-{}", option.id))
                            .checked(checked)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.apply_config_option(
                                    id.clone(),
                                    SessionConfigOptionValue::Boolean { value: *checked },
                                    cx,
                                )
                            })),
                    ),
                );
            }
            SessionConfigKind::Select { .. } => {
                // 选项值由轮询周期同步（app.rs sync_config_options）
                let Some(select) = this.config_select(&option.id).cloned() else {
                    continue;
                };
                row = row.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(label)
                        .child(Select::new(&select).small()),
                );
            }
        }
    }
    row.into_any()
}

/// 文件改动审查视图：顶部工具栏、左侧文件树、右侧 inline 改动、选择后发送给 agent
/// （docs/PRD.md「文件改动审查视图」）。
fn diff_review(
    core: &crate::state::Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let mut view = v_flex().flex_1().h_full().gap_2();
    let all_collapsed = core.view.detail.diff.as_ref().is_some_and(|diff| {
        !diff.files.is_empty()
            && diff
                .files
                .iter()
                .all(|file| this.diff_collapsed_files.contains(&file.path))
    });
    view = view.child(
        h_flex()
            .px_3()
            .pt_3()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                Button::new("toggle-diff-tree")
                    .xsmall()
                    .ghost()
                    .label(if this.diff_tree_visible {
                        "折叠文件树"
                    } else {
                        "展开文件树"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_diff_tree(cx))),
            )
            .child(
                Button::new("toggle-diff-all")
                    .xsmall()
                    .ghost()
                    .label(if all_collapsed {
                        "展开改动"
                    } else {
                        "折叠改动"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_all_diffs(cx))),
            )
            .child(
                Button::new("refresh-diff")
                    .xsmall()
                    .ghost()
                    .label("刷新改动")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_diff(cx))),
            ),
    );

    match &core.view.detail.diff {
        Some(diff) if diff.not_repo => {
            view = view.child(ui::empty_hint("工作目录不是 git 仓库", cx.theme()));
        }
        Some(diff) if diff.files.is_empty() => {
            view = view.child(ui::empty_hint("没有改动", cx.theme()));
        }
        Some(diff) => {
            let files = diff.files.clone();
            let mut content = h_flex().flex_1().min_h_0().gap_2();
            if this.diff_tree_visible {
                content = content.child(diff_tree_pane(&files, this, cx));
            }
            content = content.child(diff_inline_pane(&files, this, cx));
            view = view.child(content).child(diff_review_footer(this, cx));
        }
        None => {
            view = view.child(ui::empty_hint("点击「刷新改动」加载", cx.theme()));
        }
    }
    view.into_any()
}

/// 左侧文件树：只含改动文件，点击文件滚动到对应改动。
fn diff_tree_pane(
    files: &[GitDiffFile],
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let nodes = difftree::build(files);
    let mut pane = v_flex()
        .id("diff-tree")
        .w(px(DIFF_TREE_WIDTH))
        .h_full()
        .gap_1()
        .overflow_y_scroll();
    let mut rows = Vec::new();
    for node in &nodes {
        push_tree_rows(node, 0, this, cx, &mut rows);
    }
    pane = pane.children(rows);
    pane.into_any()
}

/// 文件树节点入列：目录可整体折叠/展开，合并节点作为一个节点处理。
fn push_tree_rows(
    node: &DiffNode,
    depth: usize,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
    rows: &mut Vec<AnyElement>,
) {
    let indent = rems(0.75 * depth as f32);
    if let Some(ix) = node.file_ix {
        rows.push(
            h_flex()
                .pl(indent)
                .child(
                    Button::new(format!("diff-file-{}", node.key))
                        .xsmall()
                        .ghost()
                        .icon(IconName::File)
                        .label(ui::truncate(&node.name, 24))
                        .on_click(cx.listener(move |this, _, _, cx| this.scroll_to_file(ix, cx))),
                )
                .into_any(),
        );
        return;
    }

    let collapsed = this.diff_collapsed_dirs.contains(&node.key);
    rows.push(
        h_flex()
            .pl(indent)
            .child(
                Button::new(format!("diff-dir-{}", node.key))
                    .xsmall()
                    .ghost()
                    .icon(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .label(ui::truncate(&node.name, 22))
                    .on_click(cx.listener({
                        let key = node.key.clone();
                        move |this, _, _, cx| this.toggle_diff_dir(key.clone(), cx)
                    })),
            )
            .into_any(),
    );
    if collapsed {
        return;
    }
    for child in &node.children {
        push_tree_rows(child, depth + 1, this, cx, rows);
    }
}

/// 右侧 inline 改动：文件名 + 可折叠的 hunk 行内容 + 文件/代码块撤销与选择。
fn diff_inline_pane(
    files: &[GitDiffFile],
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let mut pane = v_flex()
        .id("diff-inline")
        .flex_1()
        .min_w_0()
        .h_full()
        .gap_3()
        .overflow_y_scroll()
        .track_scroll(&this.diff_scroll);

    for file in files {
        let collapsed = this.diff_collapsed_files.contains(&file.path);
        let selected = this.diff_selected_files.contains(&file.path);
        let mut block = v_flex()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Checkbox::new(format!("diff-select-file-{}", file.path))
                            .checked(selected)
                            .on_click(cx.listener({
                                let path = file.path.clone();
                                move |this, checked: &bool, _, cx| {
                                    if *checked != this.diff_selected_files.contains(&path) {
                                        this.toggle_diff_file_selected(path.clone(), cx);
                                    }
                                }
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(file.path.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .child(format!("+{} -{}", file.additions, file.deletions)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("diff-toggle-file-{}", file.path))
                            .xsmall()
                            .ghost()
                            .label(if collapsed { "展开" } else { "折叠" })
                            .on_click(cx.listener({
                                let path = file.path.clone();
                                move |this, _, _, cx| this.toggle_diff_file(path.clone(), cx)
                            })),
                    )
                    .child(
                        Button::new(format!("restore-file-{}", file.path))
                            .xsmall()
                            .ghost()
                            .label("撤销文件")
                            .on_click(cx.listener({
                                let path = file.path.clone();
                                move |this, _, _, cx| this.restore(Some(path.clone()), None, cx)
                            })),
                    ),
            );
        if !collapsed {
            for hunk in &file.hunks {
                block = block.child(hunk_view(file, hunk, this, cx));
            }
        }
        pane = pane.child(block);
    }
    pane.into_any()
}

/// 单个代码块：@@ 头、行内容、选择与撤销。
fn hunk_view(
    file: &GitDiffFile,
    hunk: &GitDiffHunk,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let key = (file.path.clone(), hunk.header.clone());
    let selected = this.diff_selected_hunks.contains(&key);
    let mut lines = v_flex().gap_0p5();
    for line in &hunk.lines {
        let text = if line.text.is_empty() {
            format!("{}\u{00a0}", line.kind.prefix())
        } else {
            format!("{}{}", line.kind.prefix(), line.text)
        };
        lines = lines.child(
            div()
                .text_xs()
                .text_color(ui::diff_line_color(line.kind, cx.theme()))
                .child(text),
        );
    }
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Checkbox::new(format!("diff-select-hunk-{}-{}", file.path, hunk.header))
                        .checked(selected)
                        .on_click(cx.listener({
                            let path = file.path.clone();
                            let header = hunk.header.clone();
                            move |this, checked: &bool, _, cx| {
                                let key = (path.clone(), header.clone());
                                if *checked != this.diff_selected_hunks.contains(&key) {
                                    this.toggle_diff_hunk_selected(
                                        path.clone(),
                                        header.clone(),
                                        cx,
                                    );
                                }
                            }
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hunk.header.clone()),
                )
                .child(div().flex_1())
                .child(
                    Button::new(format!("restore-hunk-{}-{}", file.path, hunk.header))
                        .xsmall()
                        .ghost()
                        .label("撤销代码块")
                        .on_click(cx.listener({
                            let path = file.path.clone();
                            let patch = hunk.patch.clone();
                            move |this, _, _, cx| {
                                this.restore(Some(path.clone()), Some(patch.clone()), cx)
                            }
                        })),
                ),
        )
        .child(lines)
        .into_any()
}

/// 审查视图底部：选中统计、指令输入与发送。
fn diff_review_footer(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let has_selection =
        !this.diff_selected_files.is_empty() || !this.diff_selected_hunks.is_empty();
    let selected_hunks = this.diff_selected_hunks.len();
    let selected_files = this.diff_selected_files.len();
    h_flex()
        .px_3()
        .pb_3()
        .gap_2()
        .items_center()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "已选 {selected_files} 个文件 · {selected_hunks} 个代码块"
                )),
        )
        .child(div().flex_1().child(text_input(&this.diff_instruction)))
        .child(
            Button::new("send-diff-review")
                .small()
                .label("发送给 agent")
                .disabled(!has_selection)
                .on_click(cx.listener(|this, _, window, cx| this.send_diff_review(window, cx))),
        )
        .into_any()
}
