use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*, input::Input, input::InputState, label::Label,
    notification::Notification as UiNotification, radio::RadioGroup, FocusTrapElement as _, *,
};

use crate::config::{ApiFormat, OrchestratorConfig, SkillEntry, WorkflowTemplate};
use crate::display::machine_status_badge;

use crate::app::{AmuxApp, CloseSettingsOverlay, SettingsCategory, SkillAction};

/// 设置域状态：浮窗开关与导航、全部表单（机器 / 编排智能体 / 快捷指令 /
/// 技能 / 工作流计划）的输入、编辑目标与校验错误。所有权与逻辑归本模块。
pub(crate) struct SettingsState {
    /// 设置浮窗焦点锚：打开时把焦点移入浮窗，Escape 动作（绑定
    /// SettingsOverlay key_context）才能被派发到 on_action
    pub(crate) focus: gpui::FocusHandle,
    pub(crate) show: bool,
    pub(crate) category: SettingsCategory,
    // 机器表单
    pub(crate) machine_name_input: Entity<InputState>,
    pub(crate) machine_url_input: Entity<InputState>,
    pub(crate) machine_token_input: Entity<InputState>,
    pub(crate) machine_form_error: Option<String>,
    // 编排智能体表单
    pub(crate) orch_api_format: ApiFormat,
    pub(crate) orch_base_input: Entity<InputState>,
    pub(crate) orch_key_input: Entity<InputState>,
    pub(crate) orch_model_input: Entity<InputState>,
    pub(crate) orchestrator_form_error: Option<String>,
    pub(crate) orchestrator_form_status: Option<String>,
    // 快捷指令 / 技能 / 工作流计划（双字段同构表单）
    pub(crate) qc_name_input: Entity<InputState>,
    pub(crate) qc_prompt_input: Entity<InputState>,
    pub(crate) qc_edit_target: Option<String>,
    pub(crate) skill_name_input: Entity<InputState>,
    pub(crate) skill_desc_input: Entity<InputState>,
    pub(crate) skill_edit_target: Option<String>,
    pub(crate) tpl_name_input: Entity<InputState>,
    pub(crate) tpl_desc_input: Entity<InputState>,
    pub(crate) tpl_edit_target: Option<String>,
    /// 双字段表单对话框当前的校验错误（三张表单共享，同时只开一张）
    pub(crate) form_error: Option<String>,
}

impl SettingsState {
    pub(crate) fn new(window: &mut Window, cx: &mut gpui::Context<crate::app::AmuxApp>) -> Self {
        let machine_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("名称，如 localpc"));
        let machine_url_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("连接地址 ws://host:port"));
        let machine_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Token")
                .masked(true)
        });
        let qc_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("名称")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let qc_prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("提示词（发给 agent 的一段话）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let skill_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("名称")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let skill_desc_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("描述（仓库/资源 URL 或安装方法）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let tpl_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("计划名")
                .multi_line(true)
                .auto_grow(2, 4)
        });
        let tpl_desc_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("执行计划（自然语言描述）")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let orch_base_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Base URL（如 https://api…/v1）"));
        let orch_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("API Key")
                .masked(true)
        });
        let orch_model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("模型名（如 gpt-4.1）"));
        SettingsState {
            focus: cx.focus_handle(),
            show: false,
            category: SettingsCategory::Machines,
            machine_name_input,
            machine_url_input,
            machine_token_input,
            machine_form_error: None,
            orch_api_format: ApiFormat::ChatCompletions,
            orch_base_input,
            orch_key_input,
            orch_model_input,
            orchestrator_form_error: None,
            orchestrator_form_status: None,
            qc_name_input,
            qc_prompt_input,
            qc_edit_target: None,
            skill_name_input,
            skill_desc_input,
            skill_edit_target: None,
            tpl_name_input,
            tpl_desc_input,
            tpl_edit_target: None,
            form_error: None,
        }
    }
}

impl AmuxApp {
    pub(crate) fn setup_orch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.store.orchestrator();
        self.settings.orch_api_format = cfg.api_format;
        self.settings
            .orch_base_input
            .update(cx, |s, cx| s.set_value(&cfg.base_url, window, cx));
        self.settings
            .orch_key_input
            .update(cx, |s, cx| s.set_value(&cfg.api_key, window, cx));
        self.settings
            .orch_model_input
            .update(cx, |s, cx| s.set_value(&cfg.model, window, cx));
    }

    pub(crate) fn save_orchestrator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let api_format = self.settings.orch_api_format;
        let base_url = self
            .settings
            .orch_base_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let api_key = self
            .settings
            .orch_key_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let model = self
            .settings
            .orch_model_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
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
            self.settings.orchestrator_form_error = Some(error.into());
            self.settings.orchestrator_form_status = None;
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
                    self.settings.orchestrator_form_error = None;
                    self.settings.orchestrator_form_status = Some("已保存。".into());
                    window.push_notification(
                        UiNotification::success("编排智能体设置已保存").title("保存成功"),
                        cx,
                    );
                }
                Err(error) => {
                    let message = format!("保存失败：{error}");
                    self.settings.orchestrator_form_error = Some(message.clone());
                    self.settings.orchestrator_form_status = None;
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
        self.settings.show = true;
        if let Some(category) = category {
            self.settings.category = category;
        }
        window.focus(&self.settings.focus, cx);
        cx.notify();
    }

    /// 统一表单对话框：gpui-component Dialog 承载——Escape 关闭、遮罩点击关闭、
    /// 焦点陷阱与恢复由库提供。库版 Dialog 不渲染 button_props 的确定/取消
    /// 按钮（仅 AlertDialog 会），必须自带 footer；字段区每次渲染重新求值，
    /// 校验错误随状态即时刷新，`on_ok` 返回 false 表示校验失败，对话框保持打开。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_form_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        title: &'static str,
        ok_label: &'static str,
        width_rems: f32,
        build_fields: impl Fn(&Self, &mut Context<Self>) -> AnyElement + 'static,
        on_ok: impl Fn(&mut Self, &mut Window, &mut Context<Self>) -> bool + Clone + 'static,
    ) {
        let width = rems(width_rems).to_pixels(window.rem_size());
        let app = cx.entity();
        let build_fields = std::rc::Rc::new(build_fields);
        let app_rc = std::rc::Rc::new(app);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let app_for_ok = app_rc.clone();
            let app = app_rc.clone();
            let on_ok = on_ok.clone();
            let build_fields = build_fields.clone();
            dialog
                .title(title)
                .width(width)
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("form-dialog-cancel")
                                .small()
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("form-dialog-ok")
                                .small()
                                .primary()
                                .label(ok_label)
                                .on_click({
                                    let on_ok = on_ok.clone();
                                    let app_for_ok = app_for_ok.clone();
                                    move |_, window, cx| {
                                        let mut ok = false;
                                        app_for_ok.update(cx, |this, cx| {
                                            ok = on_ok(this, window, cx);
                                        });
                                        if ok {
                                            window.close_dialog(cx);
                                        }
                                    }
                                }),
                        ),
                )
                .content(move |content, _window, cx| {
                    app.update(cx, |this, cx| content.child(build_fields(this, cx)))
                })
        });
        cx.notify();
    }

    /// 双字段表单的公共字段区（快捷指令 / 技能 / 工作流计划表单同构）。
    fn render_two_field_form(
        &self,
        cx: &mut Context<Self>,
        name_label: &'static str,
        name_input: &Entity<InputState>,
        content_label: &'static str,
        content_input: &Entity<InputState>,
        error: Option<&String>,
    ) -> AnyElement {
        let mut fields = v_flex()
            .gap_2()
            .child(
                Label::new(name_label)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(name_input))
            .child(
                Label::new(content_label)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(content_input));
        if let Some(error) = error {
            fields = fields.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        fields.into_any_element()
    }

    pub(crate) fn open_add_machine_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings
            .machine_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.settings
            .machine_url_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.settings
            .machine_token_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.settings.machine_form_error = None;
        self.open_form_dialog(
            window,
            cx,
            "添加机器",
            "添加机器",
            28.75,
            |this, cx| this.render_add_machine_fields(cx),
            |this, window, cx| {
                let name = this
                    .settings
                    .machine_name_input
                    .read(cx)
                    .value()
                    .to_string();
                let url = this.settings.machine_url_input.read(cx).value().to_string();
                let token = this
                    .settings
                    .machine_token_input
                    .read(cx)
                    .value()
                    .to_string();
                this.add_machine(window, cx, name, url, token)
            },
        );
    }

    fn render_add_machine_fields(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut fields = v_flex()
            .gap_2()
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
            .child(Input::new(&self.settings.machine_name_input))
            .child(
                Label::new("连接地址")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.settings.machine_url_input))
            .child(
                Label::new("Token")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.settings.machine_token_input));
        if let Some(error) = &self.settings.machine_form_error {
            fields = fields.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        fields.into_any_element()
    }

    pub(crate) fn open_quick_command_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<(String, String)>,
    ) {
        self.settings.qc_edit_target = target.as_ref().map(|(name, _)| name.clone());
        self.settings.qc_name_input.update(cx, |s, cx| {
            s.set_value(
                target.as_ref().map(|(name, _)| name.as_str()).unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.qc_prompt_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|(_, prompt)| prompt.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.form_error = None;
        let title = if self.settings.qc_edit_target.is_some() {
            "编辑快捷指令"
        } else {
            "新增快捷指令"
        };
        self.open_form_dialog(
            window,
            cx,
            title,
            "保存",
            32.5,
            |this, cx| {
                this.render_two_field_form(
                    cx,
                    "指令名称",
                    &this.settings.qc_name_input,
                    "指令内容",
                    &this.settings.qc_prompt_input,
                    this.settings.form_error.as_ref(),
                )
            },
            |this, _window, cx| this.save_quick_command(cx),
        );
    }

    pub(crate) fn save_quick_command(&mut self, cx: &mut Context<Self>) -> bool {
        let name = self
            .settings
            .qc_name_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let prompt = self
            .settings
            .qc_prompt_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.settings.form_error = Some("请输入指令名称。".into());
        } else if prompt.is_empty() {
            self.settings.form_error = Some("请输入指令内容。".into());
        } else {
            if let Some(old) = self.settings.qc_edit_target.take() {
                if old != name {
                    self.store.remove_quick_command(&old);
                    self.store.add_quick_command(&name, &prompt);
                } else {
                    self.store.update_quick_command(&old, &prompt);
                }
            } else {
                self.store.add_quick_command(&name, &prompt);
            }
            cx.notify();
            return true;
        }
        cx.notify();
        false
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
            "删除",
            true,
            "删除快捷指令",
            format!("快捷指令「{name}」将被删除，此操作不可撤销。"),
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
        self.settings.skill_edit_target = target.as_ref().map(|skill| skill.name.clone());
        self.settings.skill_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.skill_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|skill| skill.description.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.form_error = None;
        let title = if self.settings.skill_edit_target.is_some() {
            "编辑技能"
        } else {
            "新增技能"
        };
        self.open_form_dialog(
            window,
            cx,
            title,
            "保存",
            32.5,
            |this, cx| {
                this.render_two_field_form(
                    cx,
                    "技能名称",
                    &this.settings.skill_name_input,
                    "技能描述",
                    &this.settings.skill_desc_input,
                    this.settings.form_error.as_ref(),
                )
            },
            |this, _window, cx| this.save_skill(cx),
        );
    }

    pub(crate) fn save_skill(&mut self, cx: &mut Context<Self>) -> bool {
        let name = self
            .settings
            .skill_name_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let description = self
            .settings
            .skill_desc_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.settings.form_error = Some("请输入技能名称。".into());
        } else {
            if let Some(old) = self.settings.skill_edit_target.take() {
                if old != name {
                    self.store.remove_skill(&old);
                    self.store.add_skill(&name, &description);
                } else {
                    self.store.update_skill(&old, &description);
                }
            } else {
                self.store.add_skill(&name, &description);
            }
            cx.notify();
            return true;
        }
        cx.notify();
        false
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
            "删除",
            true,
            "删除技能",
            format!("技能「{name}」将被删除，此操作不可撤销。"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_skill(&name);
                cx.notify();
            },
        );
    }

    /// 技能安装/更新/卸载目标选择对话框：无表单字段，自定义 footer 只有取消。
    pub(crate) fn open_skill_action_dialog(
        &mut self,
        skill: SkillEntry,
        action: SkillAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = cx.entity();
        let title: SharedString = format!("{}技能", action.label()).into();
        let skill = std::rc::Rc::new(skill);
        let width = rems(32.5).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let app = app.clone();
            let title = title.clone();
            let skill = skill.clone();
            dialog
                .title(title)
                .width(width)
                .footer(
                    Button::new("skill-action-cancel")
                        .small()
                        .label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .content(move |content, _window, cx| {
                    app.update(cx, |this, cx| {
                        content.child(this.render_skill_action_targets(&skill, action, cx))
                    })
                })
        });
        cx.notify();
    }

    fn render_skill_action_targets(
        &self,
        skill: &SkillEntry,
        action: SkillAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut targets = Vec::new();
        for machine in &self.machines {
            for agent in &machine.agents {
                let skill = skill.clone();
                let agent_name = agent.name.clone();
                let machine_name = machine.config.name.clone();
                let label = format!("{} · {}", machine.config.name, agent.name);
                targets.push(
                    Button::new(format!(
                        "skill-target-{machine_name}-{}-{agent_name}",
                        action.label()
                    ))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.manage_skill_on_agent(
                            window,
                            cx,
                            &machine_name,
                            agent_name.clone(),
                            skill.clone(),
                            action,
                        );
                        window.close_dialog(cx);
                    })),
                );
            }
        }
        let mut card = v_flex().gap_2().child(
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
        card.into_any_element()
    }

    pub(crate) fn open_template_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<WorkflowTemplate>,
    ) {
        self.settings.tpl_edit_target = target.as_ref().map(|template| template.name.clone());
        self.settings.tpl_name_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.name.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.tpl_desc_input.update(cx, |s, cx| {
            s.set_value(
                target
                    .as_ref()
                    .map(|template| template.plan.as_str())
                    .unwrap_or(""),
                window,
                cx,
            );
        });
        self.settings.form_error = None;
        let title = if self.settings.tpl_edit_target.is_some() {
            "编辑工作流计划"
        } else {
            "新增工作流计划"
        };
        self.open_form_dialog(
            window,
            cx,
            title,
            "保存",
            35.0,
            |this, cx| {
                this.render_two_field_form(
                    cx,
                    "计划名称",
                    &this.settings.tpl_name_input,
                    "计划内容",
                    &this.settings.tpl_desc_input,
                    this.settings.form_error.as_ref(),
                )
            },
            |this, _window, cx| this.save_template(cx),
        );
    }

    pub(crate) fn save_template(&mut self, cx: &mut Context<Self>) -> bool {
        let name = self
            .settings
            .tpl_name_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        let plan = self
            .settings
            .tpl_desc_input
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.settings.form_error = Some("请输入计划名称。".into());
        } else if plan.is_empty() {
            self.settings.form_error = Some("请输入计划内容。".into());
        } else {
            if let Some(old) = self.settings.tpl_edit_target.take() {
                if old != name {
                    self.store.remove_template(&old);
                    self.store.add_template(&name, &plan);
                } else {
                    self.store.update_template(&old, &plan);
                }
            } else {
                self.store.add_template(&name, &plan);
            }
            cx.notify();
            return true;
        }
        cx.notify();
        false
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
            "删除",
            true,
            "删除工作流计划",
            format!("工作流计划「{name}」将被删除，此操作不可撤销。"),
            move |this, _window, cx| {
                let name = name.clone();
                this.store.remove_template(&name);
                cx.notify();
            },
        );
    }

    pub(crate) fn render_settings_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                        this.settings.show = false;
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
                    .track_focus(&self.settings.focus)
                    .focus_trap("settings-trap", &self.settings.focus)
                    .key_context("SettingsOverlay")
                    // 表单对话框已迁移到 window.open_dialog（库自管 Escape），
                    // 此处的 Escape 只负责关闭设置浮窗本身
                    .on_action(cx.listener(|this, _: &CloseSettingsOverlay, _window, cx| {
                        this.settings.show = false;
                        cx.notify();
                    }))
                    .w_full()
                    .max_w(rems(55.)) // 设置浮窗最大尺寸，随 rem 缩放
                    .h_full()
                    .max_h(rems(38.75))
                    .overflow_hidden()
                    .bg(cx.theme().popover)
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
            )
    }

    pub(crate) fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-nav")
            .w(rems(11.875)) // 设置导航面板宽度：随 rem 缩放
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
                                this.settings.show = false;
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
                        this.settings.show = false;
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
            .selected(self.settings.category == target)
            .on_click(cx.listener(move |this, _ev, _window, cx| {
                this.settings.category = target;
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
            .child(match self.settings.category {
                SettingsCategory::Machines => self.render_machines_settings(cx),
                SettingsCategory::Orchestrator => self.render_orchestrator_settings(cx),
                SettingsCategory::QuickCommands => self.render_quick_commands_settings(cx),
                SettingsCategory::Skills => self.render_skills_settings(cx),
                SettingsCategory::Templates => self.render_templates_settings(cx),
            })
    }

    pub(crate) fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines =
            self.machines
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    // 域标识：按钮 id 与回调捕获均用机器名而非下标（机器顺序变化不影响身份）
                    let machine_name = m.config.name.clone();
                    let reconnect_name = machine_name.clone();
                    let remove_name = machine_name.clone();
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
                                    Button::new(SharedString::from(format!(
                                        "reconnect-m-{machine_name}"
                                    )))
                                    .small()
                                    .label("重连")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = reconnect_name.clone();
                                        this.confirm_reconnect_machine(window, cx, name);
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "rediscover-m-{machine_name}"
                                    )))
                                    .small()
                                    .label("重新发现")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.confirm_rediscover_agents(window, cx, i);
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "remove-m-{machine_name}"
                                    )))
                                    .small()
                                    .label("移除")
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        let name = remove_name.clone();
                                        this.confirm_remove_machine(window, cx, name);
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
                        "接入 / 移除机器；每台机器自动发现 ACP agent，可重启 / 重新发现",
                        cx.theme().muted_foreground,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-add-open")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .tooltip("添加机器")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_add_machine_form(window, cx);
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

    pub(crate) fn render_orchestrator_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_api_format = match self.settings.orch_api_format {
            ApiFormat::ChatCompletions => Some(0),
            ApiFormat::Responses => Some(1),
            ApiFormat::Messages => Some(2),
        };
        let api_format_options = RadioGroup::horizontal("orch-api-format")
            .children(["chat_completions", "responses", "messages"])
            .selected_index(selected_api_format)
            .on_click(cx.listener(|this, selected: &usize, _window, cx| {
                this.settings.orch_api_format = match *selected {
                    0 => ApiFormat::ChatCompletions,
                    1 => ApiFormat::Responses,
                    2 => ApiFormat::Messages,
                    _ => return,
                };
                this.settings.orchestrator_form_error = None;
                this.settings.orchestrator_form_status = None;
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
            .child(Input::new(&self.settings.orch_base_input))
            .child(
                Label::new("API Key")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.settings.orch_key_input))
            .child(
                Label::new("模型名称")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(Input::new(&self.settings.orch_model_input));
        if let Some(error) = &self.settings.orchestrator_form_error {
            form = form.child(
                Label::new(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger),
            );
        }
        if let Some(status) = &self.settings.orchestrator_form_status {
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
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                this.open_skill_action_dialog(skill.clone(), action, window, cx);
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
