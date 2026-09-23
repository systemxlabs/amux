//! 中间面板：新建会话视图与会话交互视图（对话、实时活动、输入区、会话选项）。

use amux_common::api::RecentWorkspace;
use amux_common::domain::{HistoryItem, SessionConfigKind, SessionConfigOptionValue, SessionState};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::alert::Alert;
use gpui_component::button::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::Input;
use gpui_component::label::Label;
use gpui_component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, ElementExt as _, Icon, IconName, Selectable,
    Sizable,
};

use crate::app::{AmuxApp, InputResizeDrag, INPUT_RESIZE_HANDLE_HEIGHT};
use crate::state::{Core, SettingsTab};
use crate::ui;

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

/// 工作目录联想的选项行高与最多同时展示的行数（更多靠列表内滚动查看）。
const SUGGEST_ROW_HEIGHT: f32 = 28.0;
const SUGGEST_MAX_ROWS: usize = 8;
/// 联想列表的上下内边距。
const SUGGEST_PADDING: f32 = 4.0;
/// 最近工作目录下拉的展示上限。
const RECENT_WORKSPACE_LIMIT: usize = 20;

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
        .child(
            Label::new("新会话")
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD),
        )
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
            // 切换模式不保留浮层（docs/PRD.md「新建会话视图」：切换模式后回到普通模式由
            // 聚焦输入框重新触发）
            this.workspace_recent_open = false;
            this.with_core(|core| {
                core.new_session.workflow_mode = workflow;
                core.new_session.suggestions.clear();
            });
            if workflow {
                this.load_workflow_setup();
            }
            cx.notify();
        }))
}

/// 普通模式：无连接机器时仅提示，否则机器 / 智能体 / 工作目录 / worktree / 创建。
fn direct_form(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    if core.settings.machines.is_empty() {
        return Label::new("请运行 amux-daemon 程序将机器连接至服务器")
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .into_any_element();
    }
    let machine = core.new_session.machine.clone();
    // 机器与智能体同一行；工作目录另起一行（其输入框占满整行）
    let mut row = h_flex().flex_wrap().gap_6().items_start();
    row = row.child(field("机器", machine_selector(core, cx), cx));
    row = row.child(match &machine {
        Some(machine) => field("智能体", agent_selector(core, machine, cx), cx),
        None => field("智能体", hint_text("请选择机器", cx), cx),
    });

    let workspace = this.workspace_input.read(cx).value().trim().to_string();
    let can_create = !workspace.is_empty() && selected_agent_available(core);

    v_flex()
        .gap_3()
        .child(row)
        .child(workspace_picker(core, this, cx))
        .child(field("项目", project_selector(core, cx), cx))
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

/// 工作流模式：工作计划 / 创建（未配置内置智能体时引导去设置）。
fn workflow_form(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    // 配置尚未拉取回来时不做判断，避免把「还没拉取」误报成「未配置」
    if core.settings.orchestrator_loaded && core.settings.orchestrator.is_none() {
        return v_flex()
            .gap_2()
            .child(
                Alert::warning(
                    "ns-no-orchestrator-alert",
                    "请先在设置 → 内置智能体中配置大模型供应商连接。",
                )
                .title("内置智能体尚未配置"),
            )
            .child(
                Button::new("ns-goto-orchestrator-settings")
                    .small()
                    .primary()
                    .label("去配置内置智能体")
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
        // 必须用 Input 组件渲染：直接挂 InputState 实体只会渲染成不可交互的文本
        .child(Input::new(&this.plan_input).w_full())
        .child(field("项目", project_selector(core, cx), cx))
        .child(
            Label::new("创建后由工作流智能体按计划推进；可随时输入指令调整调度。")
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
                    let plan = plan.clone();
                    move |this, _, window, cx| this.set_plan(plan.clone(), window, cx)
                })),
        );
    }
    Some(row.into_any_element())
}

/// 项目选择：未选择即未归属，点击已选项目取消选择（docs/PRD.md「新建会话视图」）。
fn project_selector(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let selected = core.new_session.project.clone();
    let mut row = h_flex().flex_wrap().gap_1();
    for project in core.settings.projects.clone() {
        let name = project.name.clone();
        let is_selected = selected.as_deref() == Some(name.as_str());
        row = row.child(
            Button::new(format!("ns-project-{name}"))
                .small()
                .label(ui::truncate(&name, 24))
                .selected(is_selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.with_core(|core| {
                        core.new_session.project = (!is_selected).then(|| name.clone());
                    });
                    cx.notify();
                })),
        );
    }
    row.into_any_element()
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

/// 机器选择：一行按钮，无已接入机器时给出提示。
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
                    // 换机器后最近目录与联想项都属于上一台机器：收起浮层重新点选
                    this.workspace_recent_open = false;
                    this.with_core(|core| {
                        core.new_session.machine = Some(name.clone());
                        core.new_session.agent = None;
                        core.new_session.suggestions.clear();
                        core.new_session.suggestion = None;
                    });
                    cx.notify();
                })),
        );
    }
    if core.settings.machines.is_empty() {
        return row.into_any_element();
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
        row = row.child(hint_text("该机器未发现智能体", cx));
    }
    row.into_any_element()
}

/// 字段内的灰提示文本（如「请选择机器」）。
fn hint_text(text: &str, cx: &Context<AmuxApp>) -> AnyElement {
    Label::new(text.to_string())
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .into_any_element()
}

/// 工作目录：可手动输入（前缀联想）或从最近目录中选择。
///
/// 该机器有最近目录时，输入框尾部带上拉箭头，点击输入框即在输入框上方展开最近目录上拉框
/// （行内展示完整路径、溢出从头部截断）；手动输入时上拉框换成输入框下方的前缀匹配下拉框，
/// 输入框失焦或点击浮层之外则上下拉框都收起；没有最近目录时就是普通输入框
/// （docs/PRD.md「新建会话视图」）。
fn workspace_picker(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let recent: Vec<RecentWorkspace> = core
        .recent_workspaces
        .iter()
        .filter(|workspace| core.new_session.machine.as_deref() == Some(workspace.machine.as_str()))
        .take(RECENT_WORKSPACE_LIMIT)
        .cloned()
        .collect();

    let theme = ui::Colors::of(cx.theme());
    let app = cx.entity();
    let has_recent = !recent.is_empty();
    let open = has_recent && this.workspace_recent_open;
    let input_bounds = this.workspace_input_bounds;

    let mut input_row =
        h_flex()
            .w_full()
            .min_w_0()
            .child(
                Input::new(&this.workspace_input)
                    .w_full()
                    .when(has_recent, |input| {
                        input.suffix(
                            Icon::new(IconName::ChevronUp)
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                    }),
            );
    let bounds_app = app.clone();
    input_row = input_row.on_prepaint(move |bounds, _, cx| {
        bounds_app.update(cx, |this, _| this.workspace_input_bounds = Some(bounds));
    });
    if has_recent {
        // 点击输入框弹出最近目录上拉框（鼠标事件先到输入框自身，再冒泡到这里，不影响编辑；
        // 列表项挂在输入行之外，点选项不会经过这里）。收起由点击浮层之外或输入框失焦触发
        // （docs/PRD.md「新建会话视图」）
        let app = app.clone();
        input_row = input_row.on_mouse_down(MouseButton::Left, move |_, _, cx| {
            app.update(cx, |this, cx| this.show_recent_workspaces(cx));
        });
    }
    let mut wrap = h_flex()
        .id("ns-workspace-wrap")
        .relative()
        .w_full()
        .min_w_0()
        .child(input_row);

    // 最近目录：以输入行左缘为锚向上展开（deferred 保持浮层置顶），高度按条目数
    // 自适应、超过上限时列表内滚动查看（带滚动条）
    if let Some(bounds) = input_bounds.filter(|_| open) {
        let hover_bg = theme.accent;
        let visible = recent.len().min(SUGGEST_MAX_ROWS);
        let height = px(SUGGEST_PADDING * 2.0 + SUGGEST_ROW_HEIGHT * visible as f32);
        let mut rows = v_flex()
            .id("ns-workspace-recent")
            .w_full()
            .flex_1()
            .min_h_0()
            .p(px(SUGGEST_PADDING))
            .overflow_y_scrollbar();
        for workspace in recent {
            let path = workspace.workspace.clone();
            let app = app.clone();
            let value = workspace.clone();
            let delete_app = app.clone();
            let delete_target = workspace.clone();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("ns-workspace-recent-{path}")))
                    .w_full()
                    .h(px(SUGGEST_ROW_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .overflow_hidden()
                    .hover(move |row| row.bg(hover_bg))
                    .on_click(move |_, window, cx| {
                        let value = value.clone();
                        app.update(cx, |this, cx| {
                            this.select_recent_workspace(value.clone(), window, cx)
                        });
                    })
                    .child(
                        Label::new(path.clone())
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start(),
                    )
                    .child(
                        Button::new(SharedString::from(format!("ns-recent-del-{path}")))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("删除最近工作目录")
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                delete_app.update(cx, |this, cx| {
                                    this.delete_recent_workspace(delete_target.clone(), cx)
                                });
                            }),
                    ),
            );
        }
        wrap = wrap.child(deferred(
            anchored()
                .anchor(Anchor::BottomLeft)
                .position(point(bounds.left(), bounds.top() - px(SUGGEST_PADDING)))
                .child(
                    v_flex()
                        .id("ns-workspace-recent-panel")
                        .w(bounds.size.width)
                        .h(height)
                        .overflow_hidden()
                        .relative()
                        .bg(theme.popover)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(
                            cx.listener(|this, _, _, cx| this.dismiss_workspace_popups(cx)),
                        )
                        .child(rows),
                ),
        ));
    }

    // 前缀联想：以输入行左缘为锚向下展开（deferred 保持浮层置顶）；高度按条目数
    // 自适应，超过上限时列表内滚动查看（带滚动条）
    if let Some(bounds) = input_bounds.filter(|_| !core.new_session.suggestions.is_empty()) {
        let visible = core.new_session.suggestions.len().min(SUGGEST_MAX_ROWS);
        let height = px(SUGGEST_PADDING * 2.0 + SUGGEST_ROW_HEIGHT * visible as f32);
        let mut rows = v_flex()
            .id("ns-workspace-suggest")
            .w_full()
            .flex_1()
            .min_h_0()
            .p(px(SUGGEST_PADDING))
            .overflow_y_scrollbar();
        for entry in core.new_session.suggestions.clone() {
            let path = entry.path.clone();
            let value = format!("{}/", path.trim_end_matches('/'));
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("ns-suggest-{path}")))
                    .w_full()
                    .h(px(SUGGEST_ROW_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .overflow_hidden()
                    .hover(|row| row.bg(theme.accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_workspace(value.clone(), window, cx)
                    }))
                    .child(
                        Label::new(path.clone())
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start(),
                    ),
            );
        }
        wrap = wrap.child(deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .position(point(bounds.left(), bounds.bottom() + px(SUGGEST_PADDING)))
                .child(
                    v_flex()
                        .id("ns-workspace-suggest-panel")
                        .w(bounds.size.width)
                        .h(height)
                        .overflow_hidden()
                        .relative()
                        .bg(theme.popover)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_lg()
                        .shadow_lg()
                        .on_mouse_down_out(
                            cx.listener(|this, _, _, cx| this.dismiss_workspace_popups(cx)),
                        )
                        .child(rows),
                ),
        ));
    }

    field("工作目录", wrap.into_any_element(), cx)
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
        .is_some_and(|(_, agents)| {
            agents
                .iter()
                .any(|item| item.name == agent && item.available)
        })
}

// ---------- 会话交互视图 ----------

/// 会话区：顶部信息 + 对话历史（flex_1 自行收缩）。
fn session_view(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
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
        .child(ui::availability_tag(available, &theme))
        .when_some(
            core.view
                .session
                .as_ref()
                .map(|session| session.root_dir().to_string()),
            |header, workdir| {
                header.child(
                    Label::new(workdir)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
            },
        )
        .when_some(
            core.view
                .workflow
                .as_ref()
                .map(|workflow| workflow.plan.clone())
                .filter(|plan| !plan.is_empty()),
            |header, plan| {
                header.child(
                    div()
                        .id("interaction-workflow-plan")
                        .flex_1()
                        .min_w_0()
                        .tooltip({
                            let plan = plan.clone();
                            move |window, cx| Tooltip::new(plan.clone()).build(window, cx)
                        })
                        .child(
                            Label::new(plan)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate(),
                        ),
                )
            },
        );
    v_flex()
        .flex_1()
        .min_h_0()
        .child(header)
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(dialog(core, this, cx)),
        )
        .into_any_element()
}

/// 对话历史：气泡列表 + 覆盖式滚动条（滚动容器右侧预留滚动条沟槽），
/// 滚动到顶部一页之内时自动加载更早一页（docs/DESIGN.md「对话滚动机制」）。
fn dialog(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut rows: Vec<AnyElement> = Vec::new();
    for item in &core.view.detail.history {
        rows.push(match item {
            HistoryItem::UserMessage {
                content, timestamp, ..
            } => user_bubble(content, *timestamp, cx),
            HistoryItem::AgentMessage {
                content, timestamp, ..
            } => agent_bubble(content, *timestamp, cx),
        });
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
    if this.dialog_scroll_on_entry || scroll_at_bottom(&this.dialog_scroll) {
        this.dialog_scroll.scroll_to_bottom();
        this.dialog_scroll_on_entry = false;
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
        crate::theme::font_body().as_f32(),
        BUBBLE_MIN_WIDTH,
        BUBBLE_MAX_WIDTH,
    );
    if !images.is_empty() {
        width = width.max(px(280.));
    }
    let code_foreground = cx.theme().accent_foreground;
    let theme = ui::Colors::of(cx.theme());
    div()
        .id(("user-row", timestamp))
        .w_full()
        .child(
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
                            .style(
                                TextViewStyle::default()
                                    .code_block(
                                        StyleRefinement::default().text_color(code_foreground),
                                    )
                                    .inline_code(HighlightStyle {
                                        color: Some(code_foreground),
                                        ..Default::default()
                                    }),
                            )
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
        crate::theme::font_body().as_f32(),
        BUBBLE_MIN_WIDTH,
        BUBBLE_MAX_WIDTH,
    );
    if !images.is_empty() {
        width = width.max(px(280.));
    }
    let theme = ui::Colors::of(cx.theme());
    div()
        .id(("agent-row", timestamp))
        .w_full()
        .child(
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
    let project = core
        .view
        .session
        .as_ref()
        .and_then(|session| session.project.as_deref())
        .or_else(|| {
            core.view
                .workflow
                .as_ref()
                .and_then(|workflow| workflow.project.as_deref())
        });
    for command in core
        .settings
        .quick_commands
        .iter()
        .filter(|command| command.project.is_none() || command.project.as_deref() == project)
        .cloned()
    {
        row = row.child(
            Button::new(format!("qc-{:?}-{}", command.project, command.name))
                .small()
                .ghost()
                .label(ui::truncate(&command.name, 16))
                .tooltip(command.prompt.clone())
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
    let pending_attachments = this.with_core(|core| core.composer_attachments.clone());
    let attachment_count = pending_attachments.len();

    let mut chips = h_flex().flex_wrap().gap_1();
    for (index, attachment) in pending_attachments.iter().enumerate() {
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

    // 高度拖拽手柄：贴输入框上沿，向上拖变高、向下拖变矮（docs/PRD.md「会话交互视图」：
    // 多行输入框，可拖拽高度）。外层保留鼠标命中区，视觉上仅显示一条细线。
    let resize_line = theme.border;
    let resize_hover = theme.primary.opacity(0.08);
    let resize_active = theme.primary.opacity(0.16);
    let resize_handle = div()
        .id("composer-resize-handle")
        .w_full()
        .h(px(INPUT_RESIZE_HANDLE_HEIGHT))
        .flex()
        .items_center()
        .cursor(CursorStyle::ResizeRow)
        .hover(move |handle| handle.bg(resize_hover))
        .active(move |handle| handle.bg(resize_active))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, _, _| {
                this.begin_composer_resize(event.position.y.as_f32());
            }),
        )
        .on_drag(InputResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
        .on_drag_move(
            cx.listener(|this, event: &DragMoveEvent<InputResizeDrag>, window, cx| {
                this.resize_composer(event.event.position.y.as_f32(), window, cx);
            }),
        )
        .child(
            div()
                .w_full()
                .h(px(1.))
                .rounded_full()
                .bg(resize_line.opacity(0.6)),
        );

    let mut editor = v_flex()
        .id("input-drop-zone")
        .relative()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .bg(theme.background)
        .can_drop(|dragged, _, _| dragged.is::<ExternalPaths>())
        .on_drop(
            cx.listener(|this, paths: &ExternalPaths, _, cx| this.attach_paths(paths.paths(), cx)),
        )
        .child(resize_handle)
        .child(
            Input::new(&this.input)
                .appearance(false)
                .w_full()
                .h(px(this.composer_height)),
        )
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .pb_2()
                .child(
                    Button::new("attach-files")
                        .small()
                        .ghost()
                        .icon(IconName::File)
                        .label("附件")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.pick_attachments(window, cx)),
                        ),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
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
                        )
                        .child(
                            Button::new("send")
                                .small()
                                .primary()
                                .label("发送")
                                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                        ),
                ),
        );
    editor = editor.children(slash_menu(this, cx));

    v_flex()
        .id("composer")
        .gap_2()
        .capture_action(
            cx.listener(|this, action: &gpui_component::input::Paste, _, cx| {
                this.paste_into_composer(action, cx)
            }),
        )
        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            this.composer_key_down(event, window, cx)
        }))
        .when(attachment_count > 0, |composer| composer.child(chips))
        .child(editor)
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
fn config_options(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
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
pub(crate) fn scroll_at_bottom(handle: &ScrollHandle) -> bool {
    handle.offset().y <= -handle.max_offset().y + px(1.0)
}

/// 会话状态是否为工作中（列表行与详情共用）。
pub fn is_busy(state: SessionState) -> bool {
    state == SessionState::Busy
}
