//! 设置浮窗：分类导航 + 连接设置 / 机器管理 / 内置智能体 / 快捷指令 / 技能管理 / 工作流计划。

use amux_common::api::{ApiFormat, Machine, Skill};
use amux_common::domain::ContentBlock;
use gpui::*;
use gpui_component::button::*;
use gpui_component::input::Input;
use gpui_component::label::Label;
use gpui_component::radio::RadioGroup;
use gpui_component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, FocusTrapElement as _, Icon, IconName,
    Selectable, Sizable,
};

use crate::app::{AmuxApp, CloseSettingsOverlay, QuickCommandFormTarget};
use crate::client::Client;
use crate::dialog::{self, FormTarget};
use crate::state::{Core, SettingsTab, SharedCore};
use crate::ui;

#[derive(Clone)]
struct ProjectOrderDrag(String);

/// 内置智能体 API 格式的顺序（索引与单选组一一对应）。
const API_FORMATS: [ApiFormat; 3] = [
    ApiFormat::ChatCompletions,
    ApiFormat::Responses,
    ApiFormat::Messages,
];

/// 设置浮窗：半透明遮罩 + 分类导航侧边栏 + 右侧内容（类似系统设置）。
pub fn render_overlay(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let overlay = theme.overlay;
    let popover = theme.popover;
    let border = theme.border;
    let focus = this.settings_focus.clone();

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
                .bg(overlay)
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
        )
        .child(
            h_flex()
                .id("settings-card")
                .w_full()
                .max_w(rems(65.714))
                .h_full()
                .max_h(rems(45.714))
                .overflow_hidden()
                .bg(popover)
                .rounded_lg()
                .border_1()
                .border_color(border)
                .shadow_lg()
                // 遮罩点击关闭：卡片内的点击不穿透到遮罩
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .track_focus(&focus)
                .focus_trap("settings-trap", &focus)
                .key_context("SettingsOverlay")
                .on_action(
                    cx.listener(|this, _: &CloseSettingsOverlay, _, cx| this.close_settings(cx)),
                )
                .child(render_nav(core, this, cx))
                .child(render_content(core, this, cx)),
        )
        .into_any_element()
}

/// 左侧分类导航：标题 + 关闭、分类项、底部关闭按钮。
fn render_nav(core: &Core, _this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut nav = v_flex()
        .id("settings-nav")
        .w(rems(11.875))
        .h_full()
        .gap_2()
        .p_2()
        .bg(theme.sidebar)
        .child(
            h_flex()
                .items_center()
                .px_1()
                .py_1()
                .child(
                    Label::new("设置")
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground),
                )
                .child(div().flex_1())
                .child(
                    Button::new("settings-close")
                        .small()
                        .ghost()
                        .icon(IconName::Close)
                        .tooltip("关闭设置")
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
                ),
        );
    for tab in SettingsTab::ALL {
        nav = nav.child(nav_item(tab, core.settings_tab == tab, cx));
    }
    nav.child(div().flex_1())
        .child(
            Button::new("settings-back")
                .small()
                .label("关闭")
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
        )
        .into_any_element()
}

/// 单个分类项：图标 + 名称，选中态用 ghost+selected（弱于实心主色按钮）。
fn nav_item(tab: SettingsTab, selected: bool, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let icon_color = if selected {
        theme.primary
    } else {
        theme.muted_foreground
    };
    Button::new(SharedString::from(format!("settings-{}", tab.label())))
        .w_full()
        .ghost()
        .selected(selected)
        .on_click(cx.listener(move |this, _, window, cx| this.select_settings_tab(tab, window, cx)))
        .child(
            h_flex()
                .w_full()
                .justify_start()
                .gap_2()
                .items_center()
                .child(Icon::new(tab.icon()).small().text_color(icon_color))
                .child(
                    Label::new(tab.label())
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                ),
        )
        .into_any_element()
}

/// 右侧内容区：分类标题 + 分类页面。
fn render_content(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let (title, subtitle) = tab_heading(core.settings_tab);
    let page = match core.settings_tab {
        SettingsTab::Connection => connection_tab(this, cx),
        SettingsTab::Machines => machines_tab(this, cx),
        SettingsTab::Orchestrator => orchestrator_tab(this, cx),
        SettingsTab::QuickCommands => quick_commands_tab(core, cx),
        SettingsTab::Skills => skills_tab(core, cx),
        SettingsTab::WorkflowPlans => plans_tab(core, cx),
        SettingsTab::Projects => projects_tab(core, cx),
    };
    v_flex()
        .id("settings-content")
        .flex_1()
        .min_w_0()
        .h_full()
        .gap_2()
        .p_4()
        .overflow_y_scroll()
        .child(
            h_flex()
                .w_full()
                .items_start()
                .gap_2()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            Label::new(title)
                                .text_lg()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground),
                        )
                        .child(
                            Label::new(subtitle)
                                .text_sm()
                                .text_color(theme.muted_foreground),
                        ),
                )
                .children(tab_action(core, cx)),
        )
        .child(page)
        .into_any_element()
}

/// 分类右上角的「+」新增按钮（只有需要新增的分类有）。
fn tab_action(core: &Core, cx: &mut Context<AmuxApp>) -> Option<AnyElement> {
    let tab = core.settings_tab;
    let (id, tooltip) = match tab {
        SettingsTab::QuickCommands => ("qc-add", "添加快捷指令"),
        SettingsTab::Skills => ("skill-add", "添加技能"),
        SettingsTab::WorkflowPlans => ("plan-add", "添加计划"),
        SettingsTab::Projects => ("project-add", "添加项目"),
        _ => return None,
    };
    Some(
        Button::new(id)
            .small()
            .primary()
            .icon(IconName::Plus)
            .tooltip(tooltip)
            .on_click(cx.listener(move |this, _, window, cx| this.open_new_form(tab, window, cx)))
            .into_any_element(),
    )
}

/// 分类标题与说明。
fn tab_heading(tab: SettingsTab) -> (&'static str, &'static str) {
    match tab {
        SettingsTab::Connection => ("连接设置", "配置 Server 地址与认证 token"),
        SettingsTab::Machines => ("机器管理", "查看已接入机器与其 agent，可重启 / 重新发现"),
        SettingsTab::Orchestrator => ("内置智能体", "配置内置智能体使用的大模型供应商连接信息"),
        SettingsTab::QuickCommands => ("快捷指令", "自定义快捷指令，在会话输入区上方一键发送"),
        SettingsTab::Skills => ("技能管理", "集中管理各机器各 agent 上的技能"),
        SettingsTab::WorkflowPlans => ("工作流计划", "可复用的工作流计划，发起工作流会话时选用"),
        SettingsTab::Projects => ("项目管理", "项目用于将普通会话与工作流会话分组聚合展示"),
    }
}

/// 连接设置：Server 地址 / token，改动后才可保存（docs/PRD.md「连接设置」）。
fn connection_tab(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    v_flex()
        .gap_2()
        .child(
            v_flex()
                .gap_1()
                .p_3()
                .bg(theme.muted)
                .rounded_md()
                .child(
                    Label::new("Server 地址")
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(Input::new(&this.server_input).w_full())
                .child(
                    Label::new("认证 token")
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
                .child(Input::new(&this.token_input).w_full())
                .child(
                    h_flex().justify_end().child(
                        Button::new("save-connection")
                            .small()
                            .primary()
                            .label("保存")
                            .disabled(!this.settings_dirty)
                            .on_click(cx.listener(|this, _, _, cx| this.save_connection(cx))),
                    ),
                ),
        )
        .child(
            Label::new(format!(
                "连接信息存储于 {}",
                crate::config::connection_path().display()
            ))
            .text_xs()
            .text_color(theme.muted_foreground),
        )
        .into_any_element()
}

/// 机器管理：每台机器一张卡片（名称 + 重新发现、机器信息、agent 行）。
fn machines_tab(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let (machines, agents_by_machine) = {
        let core = this.core.lock();
        (core.settings.machines.clone(), core.settings.agents.clone())
    };
    let mut cards = v_flex().gap_2();
    for machine in &machines {
        let agents = agents_by_machine
            .iter()
            .find(|(name, _)| name == &machine.name)
            .map(|(_, agents)| agents.clone())
            .unwrap_or_default();
        cards = cards.child(machine_card(machine, agents, this, cx));
    }
    if machines.is_empty() {
        cards = cards.child(
            Label::new("还没有已接入的机器。请在机器上启动 daemon 并连接到 Server。")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    cards.into_any_element()
}

fn machine_card(
    machine: &Machine,
    agents: Vec<amux_common::api::Agent>,
    _this: &mut AmuxApp,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let name = machine.name.clone();
    let mut card = v_flex()
        .gap_1()
        .p_2()
        .bg(theme.muted)
        .rounded_md()
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Label::new(name.clone())
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground),
                )
                .child(div().flex_1())
                .child(
                    Button::new(SharedString::from(format!("rediscover-{name}")))
                        .small()
                        .label("重新发现")
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |_, _, window, cx| {
                                dialog::confirm(
                                    window,
                                    cx,
                                    "重新发现 agents",
                                    format!("确定重新扫描机器「{name}」上的 agent 吗？"),
                                    "重新发现",
                                    ButtonVariant::Primary,
                                    {
                                        let name = name.clone();
                                        move |this, cx| this.rediscover(name.clone(), cx)
                                    },
                                )
                            }
                        })),
                ),
        )
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    Label::new(format!("{} {}", machine.os, machine.arch))
                        .text_xs()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Label::new(format!("{} · amux {}", machine.hostname, machine.version))
                        .text_xs()
                        .text_color(theme.muted_foreground),
                ),
        );
    for agent in agents {
        let machine_name = name.clone();
        let agent_name = agent.name.clone();
        card = card.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Label::new(agent_name.clone())
                        .text_sm()
                        .text_color(theme.foreground),
                )
                .child(ui::availability_tag(agent.available, &theme))
                .child(div().flex_1())
                .child(
                    Button::new(SharedString::from(format!(
                        "restart-agent-{machine_name}-{agent_name}"
                    )))
                    .small()
                    .label("重启")
                    .on_click(cx.listener(move |_, _, window, cx| {
                        dialog::confirm(
                            window,
                            cx,
                            "重启 agent",
                            format!("确定重启 agent「{agent_name}」吗？"),
                            "重启",
                            ButtonVariant::Primary,
                            {
                                let machine = machine_name.clone();
                                let agent = agent_name.clone();
                                move |this, cx| {
                                    this.restart_agent(machine.clone(), agent.clone(), cx)
                                }
                            },
                        )
                    })),
                ),
        );
    }
    card.into_any_element()
}

/// 内置智能体：API 格式单选 + 连接信息表单，改动后才可保存。
fn orchestrator_tab(this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let selected = API_FORMATS
        .iter()
        .position(|format| *format == this.current_orchestrator_format());
    let radio = RadioGroup::horizontal("orch-api-format")
        .children(["chat_completions", "responses", "messages"])
        .selected_index(selected)
        .on_click(cx.listener(move |this, index: &usize, _, cx| {
            let Some(format) = API_FORMATS.get(*index) else {
                return;
            };
            this.orchestrator_format = Some(*format);
            cx.notify();
        }));
    let muted = theme.muted_foreground;
    v_flex()
        .gap_2()
        .child(
            v_flex()
                .gap_1()
                .p_3()
                .bg(theme.muted)
                .rounded_md()
                .child(Label::new("API 格式").text_sm().text_color(muted))
                .child(radio)
                .child(Label::new("Base URL").text_sm().text_color(muted))
                .child(Input::new(&this.orch_base_url).w_full())
                .child(Label::new("API Key").text_sm().text_color(muted))
                .child(Input::new(&this.orch_api_key).w_full())
                .child(Label::new("模型名称").text_sm().text_color(muted))
                .child(Input::new(&this.orch_model).w_full())
                .child(Label::new("推理级别").text_sm().text_color(muted))
                .child(Input::new(&this.orch_effort).w_full())
                .child(
                    h_flex().justify_end().child(
                        Button::new("orch-save")
                            .small()
                            .primary()
                            .label("保存")
                            .disabled(!this.orchestrator_dirty(cx))
                            .on_click(cx.listener(|this, _, _, cx| this.save_orchestrator(cx))),
                    ),
                ),
        )
        .into_any_element()
}

/// 快捷指令：卡片列表（名称 + 内容 + 编辑/删除），右上角「+」新增。
fn quick_commands_tab(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut list = v_flex().gap_2();
    for command in &core.settings.quick_commands {
        let name = command.name.clone();
        let project = command.project.clone();
        let key = format!("{project:?}-{name}");
        list = list.child(
            h_flex()
                .gap_2()
                .items_center()
                .p_2()
                .bg(theme.muted)
                .rounded_md()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(name.clone())
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground),
                        )
                        .child(
                            Label::new(match &project {
                                Some(project) => format!("项目：{project}"),
                                None => "通用".to_string(),
                            })
                            .text_xs()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            Label::new(command.prompt.clone())
                                .text_xs()
                                .line_clamp(2)
                                .text_color(theme.muted_foreground),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("qc-edit-{key}")))
                        .small()
                        .label("编辑")
                        .on_click(cx.listener({
                            let name = name.clone();
                            let project = project.clone();
                            move |this, _, window, cx| {
                                this.open_quick_command_form(
                                    QuickCommandFormTarget::Edit {
                                        project: project.clone(),
                                        name: name.clone(),
                                    },
                                    window,
                                    cx,
                                )
                            }
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("qc-del-{key}")))
                        .small()
                        .label("删除")
                        .on_click(cx.listener({
                            let name = name.clone();
                            let project = project.clone();
                            move |this, _, window, cx| {
                                this.delete_quick_command(project.clone(), name.clone(), window, cx)
                            }
                        })),
                ),
        );
    }
    if core.settings.quick_commands.is_empty() {
        list = list.child(
            Label::new("还没有快捷指令。添加后会显示在会话输入区上方。")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    list.into_any_element()
}

/// 技能：卡片两层（名称/描述 + 编辑/删除，安装/更新/卸载），右上角「+」新增。
fn skills_tab(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut list = v_flex().gap_2();
    for skill in &core.settings.skills {
        let name = skill.name.clone();
        let mut actions = h_flex().gap_1().flex_wrap();
        for action in ["安装", "更新", "卸载"] {
            actions = actions.child(
                Button::new(SharedString::from(format!("skill-action-{action}-{name}")))
                    .small()
                    .label(action)
                    .on_click(cx.listener({
                        let skill = skill.clone();
                        move |this, _, window, cx| {
                            this.open_skill_action_form(skill.clone(), action, window, cx)
                        }
                    })),
            );
        }
        list = list.child(
            v_flex()
                .gap_2()
                .p_2()
                .bg(theme.muted)
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
                                    Label::new(name.clone())
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.foreground),
                                )
                                .child(
                                    Label::new(skill.description.clone())
                                        .text_xs()
                                        .line_clamp(2)
                                        .text_color(theme.muted_foreground),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("skill-edit-{name}")))
                                .small()
                                .label("编辑")
                                .on_click(cx.listener({
                                    let name = name.clone();
                                    move |this, _, window, cx| {
                                        this.open_skill_form(
                                            FormTarget::Edit(name.clone()),
                                            window,
                                            cx,
                                        )
                                    }
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("skill-del-{name}")))
                                .small()
                                .label("删除")
                                .on_click(cx.listener({
                                    let name = name.clone();
                                    move |this, _, window, cx| {
                                        this.delete_skill(name.clone(), window, cx)
                                    }
                                })),
                        ),
                )
                .child(actions),
        );
    }
    if core.settings.skills.is_empty() {
        list = list.child(
            Label::new("还没有技能。点击右上角 + 添加技能说明。")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    list.into_any_element()
}

/// 工作流计划：卡片列表（名称 + 内容 + 编辑/删除），右上角「+」新增。
fn plans_tab(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut list = v_flex().gap_2();
    for plan in &core.settings.plans {
        let name = plan.name.clone();
        list = list.child(
            h_flex()
                .gap_2()
                .items_center()
                .p_2()
                .bg(theme.muted)
                .rounded_md()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(name.clone())
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground),
                        )
                        .child(
                            Label::new(plan.plan.clone())
                                .text_xs()
                                .line_clamp(2)
                                .text_color(theme.muted_foreground),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("plan-edit-{name}")))
                        .small()
                        .label("编辑")
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, window, cx| {
                                this.open_plan_form(FormTarget::Edit(name.clone()), window, cx)
                            }
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("plan-del-{name}")))
                        .small()
                        .label("删除")
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, window, cx| this.delete_plan(name.clone(), window, cx)
                        })),
                ),
        );
    }
    if core.settings.plans.is_empty() {
        list = list.child(
            Label::new("还没有工作流计划。点击右上角 + 创建一个可复用计划。")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    list.into_any_element()
}

/// 项目管理：卡片列表（名称 + 描述 + 编辑/删除 + 上移/下移调整顺序），右上角「+」新增。
fn projects_tab(core: &Core, cx: &mut Context<AmuxApp>) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut list = v_flex().gap_2();
    let count = core.settings.projects.len();
    for (index, project) in core.settings.projects.iter().enumerate() {
        let name = project.name.clone();
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("project-card-{name}")))
                .gap_2()
                .items_center()
                .p_2()
                .bg(theme.muted)
                .rounded_md()
                .on_drag(ProjectOrderDrag(name.clone()), |_, _, _, cx| {
                    cx.new(|_| Empty)
                })
                .on_drop(cx.listener({
                    let target = name.clone();
                    move |this, drag: &ProjectOrderDrag, _, cx| {
                        this.reorder_project(drag.0.clone(), target.clone(), cx)
                    }
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(name.clone())
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground),
                        )
                        .child(
                            Label::new(project.description.clone())
                                .text_xs()
                                .line_clamp(2)
                                .text_color(theme.muted_foreground),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("project-up-{name}")))
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronUp)
                        .disabled(index == 0)
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, _, cx| this.move_project(name.clone(), -1, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("project-down-{name}")))
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronDown)
                        .disabled(index + 1 >= count)
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, _, cx| this.move_project(name.clone(), 1, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("project-edit-{name}")))
                        .small()
                        .label("编辑")
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, window, cx| {
                                this.open_project_form(FormTarget::Edit(name.clone()), window, cx)
                            }
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("project-del-{name}")))
                        .small()
                        .label("删除")
                        .on_click(cx.listener({
                            let name = name.clone();
                            move |this, _, window, cx| this.delete_project(name.clone(), window, cx)
                        })),
                ),
        );
    }
    if core.settings.projects.is_empty() {
        list = list.child(
            Label::new("还没有项目。点击右上角 + 创建一个项目。")
                .text_sm()
                .text_color(theme.muted_foreground),
        );
    }
    list.into_any_element()
}

/// 技能安装/更新/卸载：在指定机器与 agent 上创建临时目录普通会话并发送指令
/// （docs/DESIGN.md「技能操作」）。
pub fn manage_skill(
    client: Client,
    runtime: tokio::runtime::Handle,
    core: SharedCore,
    skill: Skill,
    action: String,
    machine: String,
    agent: String,
) {
    runtime.spawn(async move {
        // 工作目录取该机器的系统临时目录（docs/DESIGN.md「技能操作」）
        let workspace = match client.machines().await {
            Ok(machines) => match machines.into_iter().find(|item| item.name == machine) {
                Some(machine) => machine.temp_dir,
                None => {
                    core.lock().error(format!("找不到机器 {machine}"));
                    return;
                }
            },
            Err(error) => {
                core.lock().error(format!("读取机器信息失败：{error}"));
                return;
            }
        };
        let session = match client
            .create_session(&amux_common::api::CreateSessionRequest {
                machine,
                agent,
                workspace,
                use_worktree: false,
                project: None,
            })
            .await
        {
            Ok(session) => session,
            Err(error) => {
                core.lock().error(format!("创建技能会话失败：{error}"));
                return;
            }
        };
        let prompt = format!(
            "以下是技能 {} 的描述，请{}此技能\n> {}",
            skill.name, action, skill.description
        );
        if let Err(error) = client
            .prompt(&session.id, vec![ContentBlock::Text { text: prompt }])
            .await
        {
            core.lock().error(format!("发送技能指令失败：{error}"));
            return;
        }
        {
            let mut core = core.lock();
            core.success(format!("已发起技能{action}会话"));
            crate::poll::open_session(&mut core, &session.id);
        }
    });
}
