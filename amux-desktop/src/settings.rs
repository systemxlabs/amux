//! 设置浮窗：连接设置、机器管理、编排智能体、快捷指令、技能、工作流计划。

use std::sync::Arc;

use amux_common::api::{ApiFormat, OrchestratorConfig, QuickCommand, Skill};
use amux_common::domain::ContentBlock;
use gpui::*;
use gpui_component::button::*;
use gpui_component::label::Label;
use gpui_component::*;
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable};

use crate::app::{text_input, AmuxApp};
use crate::client::Client;
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.with_core(|core| core.settings_tab = tab);
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

/// 机器管理：机器卡片（名称 + 重新发现）与 agent 行（状态 + 重启）。
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
                                move |this, _, _, cx| this.rediscover(name.clone(), cx)
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
                                move |this, _, _, cx| {
                                    this.restart_agent(machine.clone(), agent.clone(), cx)
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
    let format = this
        .orchestrator_form
        .as_ref()
        .map(|config| config.api_format)
        .unwrap_or(ApiFormat::ChatCompletions);
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
                let base_url = this.orch_base_url.read(cx).value().to_string();
                let api_key = this.orch_api_key.read(cx).value().to_string();
                let model = this.orch_model.read(cx).value().to_string();
                let effort = this.orch_effort.read(cx).value().to_string();
                this.orchestrator_form = Some(OrchestratorConfig {
                    api_format: value,
                    base_url,
                    api_key,
                    model,
                    effort,
                });
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
                .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
        )
        .into_any()
}

/// 快捷指令设置。
fn quick_commands_tab(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for command in &core.settings.quick_commands {
        list = list.child(list_row(
            &command.name,
            &command.prompt,
            format!("quick-remove-{}", command.name),
            {
                let name = command.name.clone();
                let core = Arc::clone(&this.core);
                let client = this.core.lock().client.clone();
                let runtime = this.runtime.clone();
                move || {
                    let Some(client) = client.clone() else { return };
                    let mut remaining: Vec<QuickCommand> = core
                        .lock()
                        .settings
                        .quick_commands
                        .iter()
                        .filter(|item| item.name != name)
                        .cloned()
                        .collect();
                    remaining.shrink_to_fit();
                    runtime.spawn(async move {
                        let _ = client.set_quick_commands(&remaining).await;
                    });
                }
            },
            cx,
        ));
    }
    v_flex()
        .gap_3()
        .child(list)
        .child(Label::new("新增快捷指令"))
        .child(text_input(&this.quick_name))
        .child(text_input(&this.quick_prompt))
        .child(
            Button::new("add-quick")
                .small()
                .primary()
                .label("保存")
                .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
        )
        .into_any()
}

/// 技能设置：卡片 + 安装/更新/卸载（由应用侧发起临时目录会话）。
fn skills_tab(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for skill in &core.settings.skills {
        let mut actions = h_flex().gap_2();
        for action in ["安装", "更新", "卸载"] {
            actions = actions.child(
                Button::new(format!("{action}-{}", skill.name))
                    .xsmall()
                    .ghost()
                    .label(action)
                    .on_click(cx.listener({
                        let skill = skill.clone();
                        move |this, _, _, cx| this.apply_skill(skill.clone(), action, cx)
                    })),
            );
        }
        list = list.child(
            v_flex()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(cx.theme().secondary)
                .child(Label::new(skill.name.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(ui::truncate(&skill.description, 120)),
                )
                .child(actions),
        );
    }
    v_flex()
        .gap_3()
        .child(list)
        .child(Label::new("新增技能"))
        .child(text_input(&this.skill_name))
        .child(text_input(&this.skill_desc))
        .child(
            Button::new("add-skill")
                .small()
                .primary()
                .label("保存")
                .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
        )
        .into_any()
}

/// 工作流计划设置。
fn plans_tab(core: &Core, this: &mut AmuxApp, cx: &mut Context<AmuxApp>) -> AnyElement {
    let mut list = v_flex().gap_2();
    for plan in &core.settings.plans {
        list = list.child(
            v_flex()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(cx.theme().secondary)
                .child(Label::new(plan.name.clone()))
                .child(div().text_xs().child(ui::truncate(&plan.plan, 200))),
        );
    }
    v_flex()
        .gap_3()
        .child(list)
        .child(Label::new("新增工作流计划"))
        .child(text_input(&this.plan_name))
        .child(text_input(&this.plan_plan))
        .child(
            Button::new("add-plan")
                .small()
                .primary()
                .label("保存")
                .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx))),
        )
        .into_any()
}

fn list_row(
    name: &str,
    detail: &str,
    button_id: String,
    on_remove: impl Fn() + 'static,
    cx: &Context<AmuxApp>,
) -> AnyElement {
    h_flex()
        .gap_2()
        .p_2()
        .rounded_md()
        .bg(cx.theme().secondary)
        .child(Label::new(name.to_string()))
        .child(
            div()
                .flex_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(ui::truncate(detail, 80)),
        )
        .child(
            Button::new(button_id)
                .xsmall()
                .ghost()
                .label("删除")
                .on_click(move |_, _, _| on_remove()),
        )
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
