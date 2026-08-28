use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, input::Input, label::Label, notification::Notification as UiNotification,
    radio::RadioGroup, *,
};

use crate::config::{ApiFormat, OrchestratorConfig, SkillEntry, WorkflowTemplate};
use crate::display::machine_status_badge;

use crate::app::{AmuxApp, CloseSettingsOverlay, SettingsCategory, SkillAction};

impl AmuxApp {
    pub(crate) fn setup_orch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.store.orchestrator();
        self.orch_api_format = cfg.api_format;
        self.orch_base_input
            .update(cx, |s, cx| s.set_value(&cfg.base_url, window, cx));
        self.orch_key_input
            .update(cx, |s, cx| s.set_value(&cfg.api_key, window, cx));
        self.orch_model_input
            .update(cx, |s, cx| s.set_value(&cfg.model, window, cx));
    }

    pub(crate) fn save_orchestrator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let api_format = self.orch_api_format;
        let base_url = self.orch_base_input.read(cx).value().trim().to_owned();
        let api_key = self.orch_key_input.read(cx).value().trim().to_owned();
        let model = self.orch_model_input.read(cx).value().trim().to_owned();
        let error = if base_url.is_empty() {
            Some("请输入 Base URL。")
        } else if api_key.is_empty() {
            Some("请输入 API Key。")
        } else if model.is_empty() {
            Some("请输入模型名称。")
        } else {
            None
        };
        if let Some(error) = error {
            self.orchestrator_form_error = Some(error.into());
            self.orchestrator_form_status = None;
            window.push_notification(
                UiNotification::error(error).title("编排智能体设置保存失败"),
                cx,
            );
        } else {
            let result = self.store.save_orchestrator(&OrchestratorConfig {
                api_format,
                base_url,
                api_key,
                model,
            });
            match result {
                Ok(()) => {
                    self.orchestrator_form_error = None;
                    self.orchestrator_form_status = Some("已保存。".into());
                    window.push_notification(
                        UiNotification::success("编排智能体设置已保存").title("保存成功"),
                        cx,
                    );
                }
                Err(error) => {
                    let message = format!("保存失败：{error}");
                    self.orchestrator_form_error = Some(message.clone());
                    self.orchestrator_form_status = None;
                    window.push_notification(
                        UiNotification::error(message).title("编排智能体设置保存失败"),
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// 打开设置浮窗并把焦点移入：Escape（绑定 SettingsOverlay key_context）
    /// 只有焦点落在浮窗内部时才会派发到 on_action。
    pub(crate) fn open_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        category: Option<SettingsCategory>,
    ) {
        self.show_settings = true;
        if let Some(category) = category {
            self.settings_category = category;
        }
        window.focus(&self.settings_focus, cx);
        cx.notify();
    }

    pub(crate) fn open_quick_command_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<(String, String)>,
    ) {
        self.qc_edit_target = target.as_ref().map(|(name, _)| name.clone());
        self.qc_name_input.update(cx, |s, cx| {
            s.set_value(
                target.as_ref().map(|(name, _)| name.as_str()).unwrap_or(""),
                window,
                cx,
            );
        });
        self.qc_prompt_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|(_, prompt)| prompt.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_quick_command_form = true;
        cx.notify();
    }

    pub(crate) fn close_quick_command_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_quick_command_form = false;
        self.qc_edit_target = None;
        self.qc_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.qc_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.qc_name_input.read(cx).value().trim().to_owned();
        let prompt = self.qc_prompt_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入指令名称。".into());
        } else if prompt.is_empty() {
            self.settings_form_error = Some("请输入指令内容。".into());
        } else {
            if let Some(old) = self.qc_edit_target.clone() {
                if old != name {
                    self.store.remove_quick_command(&old);
                    self.store.add_quick_command(&name, &prompt);
                } else {
                    self.store.update_quick_command(&old, &prompt);
                }
            } else {
                self.store.add_quick_command(&name, &prompt);
            }
            self.close_quick_command_form(window, cx);
            return;
        }
        cx.notify();
    }

    pub(crate) fn confirm_remove_quick_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除快捷指令",
            format!("确定删除快捷指令「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_quick_command(&name);
                cx.notify();
            },
        );
    }

    pub(crate) fn open_skill_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<SkillEntry>,
    ) {
        self.skill_edit_target = target.as_ref().map(|skill| skill.name.clone());
        self.skill_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.skill_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.description.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_skill_form = true;
        cx.notify();
    }

    pub(crate) fn close_skill_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_skill_form = false;
        self.skill_edit_target = None;
        self.skill_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.skill_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.skill_name_input.read(cx).value().trim().to_owned();
        let description = self.skill_desc_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入技能名称。".into());
        } else {
            if let Some(old) = self.skill_edit_target.clone() {
                if old != name {
                    self.store.remove_skill(&old);
                    self.store.add_skill(&name, &description);
                } else {
                    self.store.update_skill(&old, &description);
                }
            } else {
                self.store.add_skill(&name, &description);
            }
            self.close_skill_form(window, cx);
            return;
        }
        cx.notify();
    }

    pub(crate) fn confirm_remove_skill(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除技能",
            format!("确定删除技能「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_skill(&name);
                cx.notify();
            },
        );
    }

    pub(crate) fn open_skill_action_dialog(
        &mut self,
        skill: SkillEntry,
        action: SkillAction,
        cx: &mut Context<Self>,
    ) {
        self.skill_action_dialog = Some((skill, action));
        cx.notify();
    }

    pub(crate) fn open_template_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<WorkflowTemplate>,
    ) {
        self.tpl_edit_target = target.as_ref().map(|template| template.name.clone());
        self.tpl_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.tpl_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.plan.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings_form_error = None;
        self.show_template_form = true;
        cx.notify();
    }

    pub(crate) fn close_template_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_template_form = false;
        self.tpl_edit_target = None;
        self.tpl_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tpl_desc_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.settings_form_error = None;
        cx.notify();
    }

    pub(crate) fn save_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.tpl_name_input.read(cx).value().trim().to_owned();
        let plan = self.tpl_desc_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.settings_form_error = Some("请输入计划名称。".into());
        } else if plan.is_empty() {
            self.settings_form_error = Some("请输入计划内容。".into());
        } else {
            if let Some(old) = self.tpl_edit_target.clone() {
                if old != name {
                    self.store.remove_template(&old);
                    self.store.add_template(&name, &plan);
                } else {
                    self.store.update_template(&old, &plan);
                }
            } else {
                self.store.add_template(&name, &plan);
            }
            self.close_template_form(window, cx);
            return;
        }
        cx.notify();
    }

    pub(crate) fn confirm_remove_template(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除工作流计划",
            format!("确定删除工作流计划「{name}」吗？"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_template(&name);
                cx.notify();
            },
        );
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
                // 域标识：按钮 id 用机器名而非下标（机器顺序变化不影响身份）
                let machine_name = m.config.name.clone();
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
                                Button::new(format!("restart-agent-{machine_name}-{agent}"))
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

    pub(crate) fn render_quick_commands_settings(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
