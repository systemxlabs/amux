//! 中间面板：新建会话视图与会话交互视图（对话、实时活动、输入区、会话选项）。

use amux_common::domain::{HistoryItem, SessionConfigKind, SessionConfigOptionValue, SessionState};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::label::Label;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::scroll::Scrollbar;
use gpui_component::spinner::Spinner;
use gpui_component::input::Input;
use gpui_component::switch::Switch;
use gpui_component::text::TextView;
use gpui_component::{h_flex, v_flex, ActiveTheme, Disableable as _, Icon, IconName, Selectable, Sizable};
use gpui_component::alert::Alert;

use crate::app::AmuxApp;
use crate::state::{Core, SettingsTab};
use crate::ui;

/// 输入框最小高度：宽松的命中区域，随 auto_grow 继续增高。
const INPUT_MIN_HEIGHT: f32 = 96.0;
/// 消息气泡宽度上下限（下限需容纳时间戳行）。
const BUBBLE_MIN_WIDTH: f32 = 132.0;
const BUBBLE_MAX_WIDTH: f32 = 720.0;

/// 中间面板：未打开会话时为新建会话视图，否则为会话交互视图。
pub fn render_main(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    if core.open.is_none() {
        return v_flex()
            .flex_1()
            .min_w_0()
            .p_3()
            .child(new_session_view(core, this, cx))
            .into_any_element();
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
        // 两个容器：上方对话区（flex_1 自行收缩），下方输入区独立成卡，
        // 仅占自然高度，不叠加在对话区之上
        .child(session_view(core, this, cx))
        .child(input_bar(core, this, cx))
        .into_any_element()
}

// ---------- 新建会话视图 ----------

fn new_session_view(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let workflow_mode = core.new_session.workflow_mode;
    let card = v_flex()
        .w_full()
        .max_w(px(640.))
        .gap_3()
        .p_4()
        .bg(cx.theme().popover)
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .shadow_lg()
        .child(Label::new("新会话").text_xl().font_weight(FontWeight::SEMIBOLD))
        .child(mode_switch(workflow_mode, cx))
        .child(if workflow_mode {
            workflow_form(core, this, cx)
        } else {
            direct_form(core, this, cx)
        });
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .child(card)
        .into_any_element()
}

/// 模式切换：普通 / 工作流（整行等分）。
fn mode_switch(workflow_mode: bool, cx: &mut Context<AmuxApp>) -> impl IntoElement {
    ButtonGroup::new("ns-mode")
        .small()
        .w_full()
        .child(
            Button::new("ns-mode-direct")
                .flex_1()
                .label("普通")
                .selected(!workflow_mode),
        )
        .child(
            Button::new("ns-mode-workflow")
                .flex_1()
                .label("工作流")
                .selected(workflow_mode),
        )
        .on_click(cx.listener(move |this, clicks: &Vec<usize>, _, cx| {
            let workflow = clicks.contains(&1);
            this.with_core(|core| {
                core.new_session.workflow_mode = workflow;
                core.new_session.suggestions.clear();
            });
            cx.notify();
        }))
}

/// 普通模式：机器 / agent / 工作目录 / worktree / 创建。
fn direct_form(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    if core.settings.machines.is_empty() {
        return v_flex()
            .gap_2()
            .child(Alert::warning(
                "ns-no-machines-alert",
                "请先在设置 → 机器管理中确认已接入的机器。",
            )
            .title("尚未注册机器"))
            .child(
                Button::new("ns-goto-machine-settings")
                    .small()
                    .primary()
                    .label("去查看机器")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Some(SettingsTab::Machines), window, cx)
                    })),
            )
            .into_any_element();
    }

    let machine = core.new_session.machine.clone();
    let mut fields = h_flex().flex_wrap().gap_6().items_start();
    fields = fields.child(field(
        "机器",
        machine_selector(core, cx),
        cx,
    ));
    if let Some(machine) = &machine {
        fields = fields.child(field("Agent", agent_selector(core, machine, cx), cx));
    }
    fields = fields.child(workspace_picker(core, this, cx));

    let workspace = this.workspace_input.read(cx).value().trim().to_string();
    let can_create = !workspace.is_empty() && selected_agent_available(core);

    v_flex()
        .gap_3()
        .child(fields)
        .child(
            Checkbox::new("ns-worktree-toggle")
                .label("使用 worktree")
                .checked(core.new_session.use_worktree)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.with_core(|core| core.new_session.use_worktree = *checked);
                    cx.notify();
                })),
        )
        .child(
            div().id("ns-create-wrap").child(
                Button::new("ns-create")
                    .primary()
                    .mt_2()
                    .label("创建会话")
                    .disabled(!can_create)
                    .on_click(cx.listener(|this, _, _, cx| this.create_session(cx))),
            ),
        )
        .into_any_element()
}

/// 工作流模式：工作计划 / 创建（未配置编排智能体时引导去设置）。
fn workflow_form(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    if core.settings.orchestrator.is_none() {
        return v_flex()
            .gap_2()
            .child(
                Alert::warning(
                    "ns-no-orchestrator-alert",
                    "请先在设置 → 编排智能体中配置大模型供应商连接。",
                )
                .title("编排智能体尚未配置"),
            )
            .child(
                Button::new("ns-goto-orchestrator-settings")
                    .small()
                    .primary()
                    .label("去配置编排智能体")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Some(SettingsTab::Orchestrator), window, cx)
                    })),
            )
            .into_any_element();
    }

    let plan = this.plan_input.read(cx).value().trim().to_string();
    v_flex()
        .gap_2()
        .child(
            Label::new("工作计划")
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
        .children(plan_selector(core, cx))
        .child(this.plan_input.clone())
        .child(
            Label::new("创建后由编排智能体按计划推进；可随时输入指令调整调度。")
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            Button::new("ns-create-workflow")
                .primary()
                .mt_2()
                .label("创建工作流会话")
                .disabled(plan.is_empty())
                .on_click(cx.listener(|this, _, _, cx| this.create_session(cx))),
        )
        .into_any_element()
}

/// 已保存计划的选择器（无已保存计划时不渲染）。
fn plan_selector(core: &Core, cx: &mut Context<AmuxApp>) -> Option<AnyElement> {
    if core.settings.plans.is_empty() {
        return None;
    }
    let mut row = h_flex().flex_wrap().gap_1();
    for plan in core.settings.plans.clone() {
        row = row.child(
            Button::new(format!("ns-plan-{}", plan.name))
                .small()
                .label(ui::truncate(&plan.name, 18))
                .on_click(cx.listener({
                    let text = plan.plan.clone();
                    move |this, _, window, cx| this.set_plan(text.clone(), window, cx)
                })),
        );
    }
    Some(row.into_any_element())
}

/// 表单字段：灰标签 + 控件。
fn field(label: &str, control: AnyElement, cx: &mut Context<AmuxApp>) -> AnyElement {
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

/// 机器选择：一行按钮，离线的机器置灰。
fn machine_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let selected = core.new_session.machine.clone();
    let mut row = h_flex().flex_wrap().gap_1();
    for machine in core.settings.machines.clone() {
        let name = machine.name.clone();
        row = row.child(
            Button::new(format!("ns-machine-{name}"))
                .small()
                .label(name.clone())
                .selected(selected.as_deref() == Some(name.as_str()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.with_core(|core| {
                        core.new_session.machine = Some(name.clone());
                        core.new_session.agent = None;
                        core.new_session.suggestions.clear();
                    });
                    cx.notify();
                })),
        );
    }
    row.into_any_element()
}

/// agent 选择：不可用的 agent 置灰不可点击。
fn agent_selector(core: &Core, machine: &str, cx: &mut Context<AmuxApp>) -> AnyElement {
    let selected = core.new_session.agent.clone();
    let agents = core
        .settings
        .agents
        .iter()
        .find(|(name, _)| name == machine)
        .map(|(_, agents)| agents.clone())
        .unwrap_or_default();
    let mut row = h_flex().flex_wrap().gap_1();
    let mut any = false;
    for agent in agents {
        any = true;
        let name = agent.name.clone();
        row = row.child(
            Button::new(format!("ns-agent-{name}"))
                .small()
                .label(name.clone())
                .selected(selected.as_deref() == Some(name.as_str()))
                .disabled(!agent.available)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.with_core(|core| core.new_session.agent = Some(name.clone()));
                    cx.notify();
                })),
        );
    }
    if !any {
        row = row.child(
            Label::new("（未发现 agent）")
                .text_sm()
                .text_color(cx.theme().muted_foreground),
        );
    }
    row.into_any_element()
}

/// 工作目录：可手动输入（前缀联想）或从最近目录中选择。
fn workspace_picker(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let recent: Vec<String> = core
        .recent_workspaces
        .iter()
        .filter(|workspace| {
            core.new_session.machine.as_deref() == Some(workspace.machine.as_str())
        })
        .take(20)
        .map(|workspace| workspace.workspace.clone())
        .collect();

    let theme = ui::Colors::of(cx.theme());
    let mut input = h_flex()
        .id("ns-workspace-wrap")
        .relative()
        .gap_1()
        .items_center()
        .w(rems(24.))
        .min_w_0()
        .child(Input::new(&this.workspace_input).w_full());
    if !recent.is_empty() {
        // 最近目录以落位在输入框下方的下拉菜单给出
        let app = cx.entity();
        input = input.child(
            Button::new("ns-workspace-recent")
                .small()
                .ghost()
                .icon(IconName::ChevronDown)
                .tooltip("选择最近使用的工作目录")
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                    let mut menu = menu;
                    for path in recent.clone() {
                        let app = app.clone();
                        let value = path.clone();
                        menu = menu.item(PopupMenuItem::new(path).on_click(move |_, window, cx| {
                            let value = value.clone();
                            app.update(cx, |this, cx| this.set_workspace(value, window, cx));
                        }));
                    }
                    menu
                }),
        );
    }

    // 前缀联想：贴输入框下方展开（deferred 以免撑开表单）
    if !core.new_session.suggestions.is_empty() {
        let mut suggestions = v_flex()
            .id("ns-workspace-suggest")
            .absolute()
            .top(relative(1.0))
            .left_0()
            .right_0()
            .max_h(rems(16.))
            .overflow_y_scroll()
            .p_1()
            .gap_0p5()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg();
        for entry in core.new_session.suggestions.clone() {
            let path = entry.path.clone();
            let value = format!("{}/", path.trim_end_matches('/'));
            suggestions = suggestions.child(
                div()
                    .id(SharedString::from(format!("ns-suggest-{path}")))
                    .w_full()
                    .h_6()
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .overflow_hidden()
                    .hover(|row| row.bg(theme.accent))
                    .on_mouse_down_out(
                        cx.listener(|this, _, _, cx| this.dismiss_workspace_suggestions(cx)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_workspace(value.clone(), window, cx)
                    }))
                    .child(
                        Label::new(path.clone())
                            .text_sm()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start(),
                    ),
            );
        }
        input = input.child(deferred(suggestions));
    }

    field("工作目录", input.into_any_element(), cx)
}

/// 表单已选 agent 是否可用。
fn selected_agent_available(core: &Core) -> bool {
    match (
        core.new_session.machine.as_deref(),
        core.new_session.agent.as_deref(),
    ) {
        (Some(machine), Some(agent)) => machine_agent_available(core, machine, agent),
        _ => false,
    }
}

/// 指定机器上某 agent 是否可用。
pub fn machine_agent_available(core: &Core, machine: &str, agent: &str) -> bool {
    core.settings
        .agents
        .iter()
        .find(|(name, _)| name == machine)
        .is_some_and(|(_, agents)| agents.iter().any(|item| item.name == agent && item.available))
}

// ---------- 会话交互视图 ----------

/// 会话区：顶部信息 + 对话历史（flex_1 自行收缩）。
fn session_view(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let available = match &core.view.session {
        Some(session) => machine_agent_available(core, &session.machine, &session.agent),
        None => core.settings.orchestrator.is_some(),
    };
    let header = h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            Label::new(core.view.subtitle())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground),
        )
        .child(ui::availability_tag(available));
    v_flex()
        .flex_1()
        .min_h_0()
        .child(header)
        .child(h_flex().flex_1().min_h_0().items_stretch().child(dialog(core, this, cx)))
        .into_any_element()
}

/// 对话历史：气泡列表 + 覆盖式滚动条（滚动容器右侧预留滚动条沟槽）。
fn dialog(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    for item in &core.view.detail.history {
        rows.push(match item {
            HistoryItem::UserMessage { content, timestamp } => {
                user_bubble(content, *timestamp, cx)
            }
            HistoryItem::AgentMessage { content, timestamp } => {
                agent_bubble(content, *timestamp, cx)
            }
        });
    }
    if core.view.detail.history_has_more {
        rows.insert(
            0,
            Button::new("load-more-history")
                .small()
                .ghost()
                .label("加载更早消息")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_core(|core| core.last.history = None);
                    cx.notify();
                }))
                .into_any_element(),
        );
    }
    if rows.is_empty() {
        return div()
            .id("dialog-empty")
            .flex_1()
            .min_w_0()
            .child(ui::empty_state(
                "暂无消息，输入消息开始对话",
                IconName::Inbox,
                &ui::Colors::of(cx.theme()),
            ))
            .into_any_element();
    }

    // 贴底跟随在渲染期重申：底部区域高度变化会缩小视口，旧 offset 不再贴底
    if scroll_at_bottom(&this.dialog_scroll) {
        this.dialog_scroll.scroll_to_bottom();
    }
    let scroll = v_flex()
        .id("dialog")
        .w_full()
        .flex_1()
        .h_full()
        .gap_4()
        .p_2()
        .pr_4()
        .track_scroll(&this.dialog_scroll)
        .overflow_y_scroll()
        .children(rows);
    div()
        .id("dialog-wrap")
        .relative()
        .w_full()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(scroll)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(Scrollbar::vertical(&this.dialog_scroll).id("dialog-scrollbar")),
        )
        .into_any_element()
}

/// 用户消息气泡：右对齐、主色填充。
fn user_bubble(
    content: &[amux_common::domain::ContentBlock],
    timestamp: u64,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let text = ui::blocks_text(content);
    let images = ui::message_images(content);
    let mut width = ui::estimate_bubble_width(
        &text,
        crate::theme::FONT_BODY.as_f32(),
        BUBBLE_MIN_WIDTH,
        BUBBLE_MAX_WIDTH,
    );
    if !images.is_empty() {
        width = width.max(px(280.));
    }
    let theme = ui::Colors::of(cx.theme());
    div().id(("user-row", timestamp)).w_full().child(
        v_flex()
            .ml_auto()
            .flex_none()
            .w(width)
            .overflow_hidden()
            .p_3()
            .gap_1()
            .rounded_md()
            .bg(theme.primary)
            .shadow_sm()
            .child(
                Label::new(ui::format_local_time(timestamp, ui::TimePrecision::Seconds))
                    .text_xs()
                    .text_color(theme.primary_foreground.opacity(0.78)),
            )
            .when(!text.is_empty(), |bubble| {
                bubble.child(
                    TextView::markdown(("umd", timestamp), text)
                        .selectable(true)
                        .text_color(theme.primary_foreground),
                )
            })
            .children(images.into_iter().map(|image| {
                img(std::sync::Arc::new(image))
                    .w_full()
                    .max_h(px(240.))
                    .object_fit(ObjectFit::Contain)
                    .rounded_md()
                    .overflow_hidden()
            })),
    )
    .into_any_element()
}

/// agent 消息气泡：左对齐、弹出层底色 + 描边。
fn agent_bubble(
    content: &[amux_common::domain::ContentBlock],
    timestamp: u64,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let text = ui::blocks_text(content);
    let images = ui::message_images(content);
    let mut width = ui::estimate_bubble_width(
        &text,
        crate::theme::FONT_BODY.as_f32(),
        BUBBLE_MIN_WIDTH,
        BUBBLE_MAX_WIDTH,
    );
    if !images.is_empty() {
        width = width.max(px(280.));
    }
    let theme = ui::Colors::of(cx.theme());
    div().id(("agent-row", timestamp)).w_full().child(
        v_flex()
            .flex_none()
            .w(width)
            .overflow_hidden()
            .p_3()
            .gap_1()
            .rounded_md()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_sm()
            .child(
                Label::new(ui::format_local_time(timestamp, ui::TimePrecision::Seconds))
                    .text_xs()
                    .text_color(theme.muted_foreground),
            )
            .when(!text.is_empty(), |bubble| {
                bubble.child(TextView::markdown(("amd", timestamp), text).selectable(true))
            })
            .children(images.into_iter().map(|image| {
                img(std::sync::Arc::new(image))
                    .w_full()
                    .max_h(px(240.))
                    .object_fit(ObjectFit::Contain)
                    .rounded_md()
                    .overflow_hidden()
            })),
    )
    .into_any_element()
}

/// 输入区卡片：快捷指令 + 实时活动 + 输入框 + 会话选项。
fn input_bar(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    v_flex()
        .id("input-bar")
        .flex_none()
        .gap_2()
        .p_2()
        .bg(cx.theme().muted)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_lg()
        .child(quick_buttons(core, cx))
        .child(activity_bar(core, cx))
        .child(composer(this, cx))
        .child(config_options(core, this, cx))
        .into_any_element()
}

/// 快捷指令：点击即作为用户输入发送（docs/PRD.md「快捷指令」）。
fn quick_buttons(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut row = h_flex().flex_wrap().gap_1();
    for command in core.settings.quick_commands.clone() {
        row = row.child(
            Button::new(format!("qc-{}", command.name))
                .small()
                .ghost()
                .label(ui::truncate(&command.name, 16))
                .on_click(cx.listener({
                    let prompt = command.prompt.clone();
                    move |this, _, _, cx| this.send_quick_command(prompt.clone(), cx)
                })),
        );
    }
    row.into_any_element()
}

/// 实时活动条：思考/工具调用为警示色，错误为危险色；无进行中活动时占零高。
fn activity_bar(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let Some(text) = ui::activity_bar_text(core.view.detail.ongoing.as_ref()) else {
        return div().id("activity-bar-empty").into_any_element();
    };
    let theme = ui::Colors::of(cx.theme());
    let failed = matches!(
        core.view.detail.ongoing,
        Some(amux_common::domain::Activity::Error { .. })
    );
    let (background, border, foreground) = if failed {
        (
            theme.danger.opacity(0.12),
            theme.danger.opacity(0.45),
            theme.danger,
        )
    } else {
        (
            theme.warning.opacity(0.16),
            theme.warning.opacity(0.45),
            theme.warning_foreground,
        )
    };
    let spinner = !failed
        && matches!(
            core.view.detail.ongoing,
            Some(amux_common::domain::Activity::Thinking { .. })
                | Some(amux_common::domain::Activity::ToolCall { .. })
        );
    h_flex()
        .w_full()
        .gap_2()
        .p_2()
        .bg(background)
        .border_1()
        .border_color(border)
        .rounded_md()
        .when(spinner, |row| {
            row.child(Spinner::new().xsmall().color(foreground))
        })
        .child(
            Label::new(text)
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(foreground),
        )
        .into_any_element()
}

/// 输入区：附件、多行输入、发送/取消、斜杠命令上拉框。
fn composer(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let danger = theme.danger;
    let transparent = gpui::transparent_black();
    let attachment_count = this.attachments.len();

    let mut chips = h_flex().flex_wrap().gap_1();
    for (index, attachment) in this.attachments.iter().enumerate() {
        let label = attachment.label.clone();
        chips = chips.child(
            div()
                .id(("attachment-chip", index))
                .max_w(px(280.))
                .px_2()
                .py_0p5()
                .rounded_full()
                .bg(theme.muted)
                .hover(|chip| chip.bg(theme.secondary_hover))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(index, cx)))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1()
                        .overflow_hidden()
                        .child(
                            Icon::new(IconName::Close)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            Label::new(ui::truncate(&label, 32))
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate(),
                        ),
                ),
        );
    }

    let mut row = h_flex().relative().gap_2().items_end();
    row = row.child(
        div()
            .id("input-drop-zone")
            .flex_1()
            .min_w_0()
            .can_drop(|dragged, _, _| dragged.is::<ExternalPaths>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.attach_paths(paths.paths(), cx)
            }))
            .child(Input::new(&this.input).min_h(px(INPUT_MIN_HEIGHT))),
    );
    row = row.child(
        v_flex()
            .gap_2()
            .child(
                Button::new("send")
                    .primary()
                    .label("发送")
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .child(
                Button::new("cancel-work")
                    .small()
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(transparent)
                            .foreground(danger)
                            .hover(danger.opacity(0.12))
                            .active(danger.opacity(0.2)),
                    )
                    .icon(IconName::Close)
                    .label("取消")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            ),
    );
    if attachment_count > 1 {
        row = row.child(
            Button::new("clear-attachments")
                .small()
                .ghost()
                .label("清空附件")
                .on_click(cx.listener(|this, _, _, cx| this.clear_attachments(cx))),
        );
    }
    row = row.children(slash_menu(this, cx));

    v_flex()
        .id("composer")
        .gap_2()
        .capture_action(cx.listener(
            |this, action: &gpui_component::input::Paste, _, cx| this.paste_into_composer(action, cx),
        ))
        .capture_key_down(
            cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.composer_key_down(event, window, cx)
            }),
        )
        .when(attachment_count > 0, |composer| composer.child(chips))
        .child(row)
        .into_any_element()
}

/// 斜杠命令上拉框：锚在输入行上沿之上，向上展开。
fn slash_menu(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> Option<AnyElement> {
    let candidates = this.slash_candidates(cx);
    if candidates.is_empty() {
        return None;
    }
    let selected = this.slash_selected.min(candidates.len() - 1);
    let mut rows = v_flex()
        .id("slash-menu")
        .absolute()
        .bottom(relative(1.0))
        .left_0()
        .w(px(480.))
        .max_h(px(280.))
        .overflow_y_scroll()
        .p_1()
        .gap_0p5()
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_lg()
        .shadow_lg();
    for (index, command) in candidates.into_iter().enumerate() {
        let hint = command
            .hint
            .clone()
            .unwrap_or_else(|| command.description.clone());
        let name = command.name.clone();
        let mut row = div()
            .id(("slash-cmd", index))
            .w_full()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().accent))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Label::new(format!("/{name}"))
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(
                        Label::new(ui::truncate(&hint, 48))
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
        let accent = cx.theme().accent;
        if index == selected {
            row = row.bg(accent);
        }
        rows = rows.child(row.on_click(cx.listener({
            let command = command.clone();
            move |this, _, window, cx| this.apply_slash_command(&command, window, cx)
        })));
    }
    Some(deferred(rows).into_any_element())
}

/// 会话选项行：select 用下拉按钮、boolean 用开关（docs/PRD.md「会话选项」）。
fn config_options(
    core: &Core,
    this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    if core.view.detail.config_options.is_empty() {
        return div().id("config-options-empty").into_any_element();
    }
    let mut row = h_flex().flex_wrap().gap_x_3().gap_y_1().items_center();
    for option in core.view.detail.config_options.clone() {
        let label = Label::new(option.name.clone())
            .text_sm()
            .text_color(cx.theme().muted_foreground);
        match &option.kind {
            SessionConfigKind::Boolean { current_value } => {
                let id = option.id.clone();
                row = row.child(
                    h_flex().gap_1().items_center().child(label).child(
                        Switch::new(format!("cfg-switch-{id}"))
                            .small()
                            .checked(*current_value)
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
                row = row.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(label)
                        .child(this.config_option_menu(&option, cx)),
                );
            }
        }
    }
    row.into_any_element()
}

/// 滚动区是否贴底（gpui 的 offset.y 范围为 [-max, 0]，留 1px 浮点容差）。
fn scroll_at_bottom(handle: &ScrollHandle) -> bool {
    handle.offset().y <= -handle.max_offset().y + px(1.0)
}

/// 会话状态是否为工作中（列表行与详情共用）。
pub fn is_busy(state: SessionState) -> bool {
    state == SessionState::Busy
}
