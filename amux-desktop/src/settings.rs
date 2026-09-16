//! 设置浮窗：连接设置、机器管理、编排智能体、快捷指令、技能、工作流计划。

use amux_common::api::{ApiFormat, Skill};
use amux_common::domain::ContentBlock;
use gpui::*;
use gpui_component::button::*;
use gpui_component::label::Label;
use gpui_component::*;
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable};

use crate::app::{text_input, AmuxApp};
use crate::client::Client;
use crate::dialog::{self, FormTarget};
use crate::state::{Core, SettingsTab, SharedCore};
use crate::ui;

/// 覆盖整个窗口的设置浮窗。
pub fn render_overlay(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut sidebar = v_flex().w(px(150.0)).h_full().gap_1().p_2();
    for tab in SettingsTab::ALL {
        let selected = core.settings_tab == tab;
        sidebar = sidebar.child(
            {
                let button = Button::new(format!("settings-{}", tab.label()))
                    .small()
                    .label(tab.label());
                if selected {
                    button.primary()
                } else {
                    button
                }
            }
            .on_click(cx.listener(move |this, _, window, cx| {
                // 仅切换分类时重新预填表单，避免重复点击同一分类覆盖未保存的编辑
                let switched = this.with_core(|core| {
                    let switched = core.settings_tab != tab;
                    core.settings_tab = tab;
                    switched
                });
                if switched && tab == SettingsTab::Orchestrator {
                    this.load_orchestrator_form(window, cx);
                }
                cx.notify();
            })),
        );
    }

    let content = match core.settings_tab {
        SettingsTab::Connection => connection_tab(core, this, cx),
        SettingsTab::Machines => machines_tab(core, this, cx),
        SettingsTab::Orchestrator => orchestrator_tab(core, this, cx),
        SettingsTab::QuickCommands => quick_commands_tab(core, this, cx),
        SettingsTab::Skills => skills_tab(core, this, cx),
        SettingsTab::WorkflowPlans => plans_tab(core, this, cx),
    };

    div()
        .id("settings-backdrop")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.35))
        .on_click(cx.listener(|this, _, _, cx| {
            this.with_core(|core| core.settings_open = false);
            cx.notify();
        }))
        .child(
            h_flex()
                .id("settings-panel")
                .w(px(860.0))
                .h(px(600.0))
                .rounded_lg()
                .overflow_hidden()
                .bg(cx.theme().popover)
                .border_1()
                .border_color(cx.theme().border)
                .child(sidebar)
                .child(
                    v_flex()
                        .id("settings-content")
                        .flex_1()
                        .h_full()
                        .p_3()
                        .gap_3()
                        .overflow_y_scroll()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new(core.settings_tab.label())
                                        .font_weight(FontWeight::SEMIBOLD),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("close-settings")
                                        .small()
                                        .ghost()
                                        .label("关闭")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.with_core(|core| core.settings_open = false);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(content),
                ),
        )
        .into_any()
}

/// 连接设置（docs/DESIGN.md「连接设置」「连接存储」）。
fn connection_tab(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let status_color = match core.status {
        crate::state::ConnectionStatus::Online => cx.theme().success,
        crate::state::ConnectionStatus::Failed(_) => cx.theme().danger,
        _ => cx.theme().muted_foreground,
    };
    v_flex()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Label::new("连接状态"))
                .child(div().text_color(status_color).child(core.status.label())),
        )
        .child(Label::new("Server 地址"))
        .child(text_input(&this.server_input))
        .child(Label::new("认证 token"))
        .child(text_input(&this.token_input))
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("save-connection")
                        .small()
                        .primary()
                        .disabled(!this.settings_dirty)
                        .label("保存")
                        .on_click(cx.listener(|this, _, _, cx| this.save_connection(cx))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "存储位置：{}",
                            crate::config::connection_path().display()
                        )),
                ),
        )
        .into_any()
}

/// 机器管理：机器卡片（名称 + 重新发现）与 agent 行（状态 + 重启），两个操作都需弹窗确认。
fn machines_tab(_core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let (machines, agents_by_machine) = {
        let core = this.core.lock();
        (core.settings.machines.clone(), core.settings.agents.clone())
    };
    let mut body = v_flex().gap_3();
    let mut has_machine = false;
    for machine in machines {
        has_machine = true;
        let agents = agents_by_machine
            .iter()
            .find(|(name, _)| name == &machine.name)
            .map(|(_, agents)| agents.clone())
            .unwrap_or_default();
        let mut card = v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(machine.name.clone()).font_weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("rediscover-{}", machine.name))
                            .xsmall()
                            .ghost()
                            .label("重新发现")
                            .on_click(cx.listener({
                                let name = machine.name.clone();
                                move |_, _, window, cx| {
                                    dialog::confirm(
                                        window,
                                        cx,
                                        format!("重新发现「{name}」上的 agent？"),
                                        "将重新扫描该机器上已安装的 agent。".to_string(),
                                        "重新发现",
                                        false,
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
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} {} {}",
                        machine.os, machine.arch, machine.hostname
                    )),
            );
        for agent in agents {
            card = card.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child(agent.name.clone()))
                    .child(div().text_xs().child(if agent.available {
                        "可用"
                    } else {
                        "不可用"
                    }))
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("restart-{}-{}", machine.name, agent.name))
                            .xsmall()
                            .ghost()
                            .label("重启")
                            .on_click(cx.listener({
                                let machine = machine.name.clone();
                                let agent = agent.name.clone();
                                move |_, _, window, cx| {
                                    dialog::confirm(
                                        window,
                                        cx,
                                        format!("重启 {agent}@{machine}？"),
                                        "该 agent 上正在进行的会话会被中断。".to_string(),
                                        "重启",
                                        false,
                                        {
                                            let machine = machine.clone();
                                            let agent = agent.clone();
                                            move |this, cx| {
                                                this.restart_agent(
                                                    machine.clone(),
                                                    agent.clone(),
                                                    cx,
                                                )
                                            }
                                        },
                                    )
                                }
                            })),
                    ),
            );
        }
        body = body.child(card);
    }
    if !has_machine {
        body = body.child(ui::empty_hint("暂无已连接机器", cx.theme()));
    }
    body.into_any()
}

/// 编排智能体设置。
fn orchestrator_tab(_core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let format = this.current_orchestrator_format();
    let mut formats = h_flex().gap_2();
    for (value, label) in [
        (ApiFormat::ChatCompletions, "chat_completions"),
        (ApiFormat::Responses, "responses"),
        (ApiFormat::Messages, "messages"),
    ] {
        formats = formats.child(
            {
                let button = Button::new(format!("format-{label}")).small().label(label);
                if format == value {
                    button.primary()
                } else {
                    button
                }
            }
            .on_click(cx.listener(move |this, _, _, cx| {
                this.orchestrator_format = Some(value);
                cx.notify();
            })),
        );
    }

    v_flex()
        .gap_3()
        .child(Label::new("API 格式"))
        .child(formats)
        .child(Label::new("Base URL"))
        .child(text_input(&this.orch_base_url))
        .child(Label::new("API Key"))
        .child(text_input(&this.orch_api_key))
        .child(Label::new("模型名称"))
        .child(text_input(&this.orch_model))
        .child(Label::new("推理级别"))
        .child(text_input(&this.orch_effort))
        .child(
            Button::new("save-orchestrator")
                .small()
                .primary()
                .label("保存")
                .on_click(cx.listener(|this, _, _, cx| this.save_orchestrator(cx))),
        )
        .into_any()
}

/// 快捷指令设置：卡片（名称 + 内容 + 编辑/删除），右上方「+」弹窗新增。
fn quick_commands_tab(core: &Core, _this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for command in &core.settings.quick_commands {
        let name = command.name.clone();
        list = list.child(card(
            &command.name,
            &command.prompt,
            vec![
                Button::new(format!("quick-edit-{}", command.name))
                    .xsmall()
                    .ghost()
                    .label("编辑")
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| {
                            this.open_quick_command_form(FormTarget::Edit(name.clone()), window, cx)
                        }
                    }))
                    .into_any_element(),
                Button::new(format!("quick-remove-{}", command.name))
                    .xsmall()
                    .ghost()
                    .label("删除")
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| {
                            this.delete_quick_command(name.clone(), window, cx)
                        }
                    }))
                    .into_any_element(),
            ],
            cx,
        ));
    }
    if core.settings.quick_commands.is_empty() {
        list = list.child(ui::empty_hint("暂无快捷指令", cx.theme()));
    }
    v_flex()
        .gap_3()
        .child(card_header("快捷指令", cx, |this, window, cx| {
            this.open_quick_command_form(FormTarget::New, window, cx)
        }))
        .child(list)
        .into_any()
}

/// 技能设置：卡片（安装/更新/卸载 + 编辑/删除），右上方「+」弹窗新增。
fn skills_tab(core: &Core, _this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for skill in &core.settings.skills {
        let name = skill.name.clone();
        let mut actions: Vec<AnyElement> = Vec::new();
        for action in ["安装", "更新", "卸载"] {
            actions.push(
                Button::new(format!("{action}-{}", skill.name))
                    .xsmall()
                    .ghost()
                    .label(action)
                    .on_click(cx.listener({
                        let skill = skill.clone();
                        move |this, _, _, cx| this.apply_skill(skill.clone(), action, cx)
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            Button::new(format!("skill-edit-{}", skill.name))
                .xsmall()
                .ghost()
                .label("编辑")
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, window, cx| {
                        this.open_skill_form(FormTarget::Edit(name.clone()), window, cx)
                    }
                }))
                .into_any_element(),
        );
        actions.push(
            Button::new(format!("skill-remove-{}", skill.name))
                .xsmall()
                .ghost()
                .label("删除")
                .on_click(cx.listener({
                    let name = name.clone();
                    move |this, _, window, cx| this.delete_skill(name.clone(), window, cx)
                }))
                .into_any_element(),
        );
        list = list.child(card(&skill.name, &skill.description, actions, cx));
    }
    if core.settings.skills.is_empty() {
        list = list.child(ui::empty_hint("暂无技能", cx.theme()));
    }
    v_flex()
        .gap_3()
        .child(card_header("技能", cx, |this, window, cx| {
            this.open_skill_form(FormTarget::New, window, cx)
        }))
        .child(list)
        .into_any()
}

/// 工作流计划设置：卡片（名称 + 内容 + 编辑/删除），右上方「+」弹窗新增。
fn plans_tab(core: &Core, _this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for plan in &core.settings.plans {
        let name = plan.name.clone();
        list = list.child(card(
            &plan.name,
            &plan.plan,
            vec![
                Button::new(format!("plan-edit-{}", plan.name))
                    .xsmall()
                    .ghost()
                    .label("编辑")
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| {
                            this.open_plan_form(FormTarget::Edit(name.clone()), window, cx)
                        }
                    }))
                    .into_any_element(),
                Button::new(format!("plan-remove-{}", plan.name))
                    .xsmall()
                    .ghost()
                    .label("删除")
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| this.delete_plan(name.clone(), window, cx)
                    }))
                    .into_any_element(),
            ],
            cx,
        ));
    }
    if core.settings.plans.is_empty() {
        list = list.child(ui::empty_hint("暂无工作流计划", cx.theme()));
    }
    v_flex()
        .gap_3()
        .child(card_header("工作流计划", cx, |this, window, cx| {
            this.open_plan_form(FormTarget::New, window, cx)
        }))
        .child(list)
        .into_any()
}

/// 卡片区头部：标题 + 右上角「+」按钮（点击弹出表单）。
fn card_header(
    title: &str,
    cx: &mut Context<AmuxApp>,
    add: impl Fn(&mut AmuxApp, &mut Window, &mut Context<AmuxApp>) + 'static,
) -> AnyElement {
    h_flex()
        .items_center()
        .gap_2()
        .child(Label::new(title.to_string()).font_weight(FontWeight::SEMIBOLD))
        .child(div().flex_1())
        .child(
            Button::new(format!("add-{title}"))
                .small()
                .ghost()
                .label("+")
                .on_click(cx.listener(move |this, _, window, cx| add(this, window, cx))),
        )
        .into_any()
}

/// 卡片：名称、截断展示的内容与操作按钮。
fn card(name: &str, detail: &str, actions: Vec<AnyElement>, cx: &Context<AmuxApp>) -> AnyElement {
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(cx.theme().secondary)
        .child(Label::new(name.to_string()))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(ui::truncate(detail, 120)),
        )
        .child(h_flex().gap_2().children(actions))
        .into_any()
}

/// 技能安装/更新/卸载：创建临时目录普通会话并发送指令（docs/DESIGN.md「技能操作」）。
pub fn install_skill(
    client: Client,
    runtime: tokio::runtime::Handle,
    core: SharedCore,
    skill: Skill,
    action: String,
) {
    runtime.spawn(async move {
        let machines = match client.machines().await {
            Ok(machines) => machines,
            Err(error) => {
                core.lock().note(format!("读取机器失败：{error}"));
                return;
            }
        };
        let mut target = None;
        for machine in &machines {
            let agents = client.agents(&machine.name).await.unwrap_or_default();
            if let Some(agent) = agents.into_iter().find(|agent| agent.available) {
                target = Some((machine.name.clone(), agent.name));
                break;
            }
        }
        let Some((machine, agent)) = target else {
            core.lock().note("没有可用的机器与 agent");
            return;
        };
        let workspace = std::env::temp_dir().to_string_lossy().to_string();
        let session = match client
            .create_session(&amux_common::api::CreateSessionRequest {
                machine,
                agent,
                workspace,
                use_worktree: false,
            })
            .await
        {
            Ok(session) => session,
            Err(error) => {
                core.lock().note(format!("创建技能会话失败：{error}"));
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
            core.lock().note(format!("发送技能指令失败：{error}"));
            return;
        }
        {
            let mut core = core.lock();
            core.note(format!("已发起技能{action}会话"));
            crate::poll::open_session(&mut core, &session.id);
        }
    });
}
