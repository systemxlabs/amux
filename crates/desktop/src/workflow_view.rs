use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    collapsible::Collapsible,
    input::Input,
    label::Label,
    menu::{ContextMenuExt, PopupMenuItem},
    spinner::Spinner,
    tag::Tag,
    *,
};

use protocol::{SessionIdParams, SessionState};

use crate::logic::{parse_at_references, read_path_context};
use crate::workflow::{AgentSlot, MachineSummary, OrcBackend, RigBackend, WorkflowEngine};
use crate::ws::WsClient;

use crate::app::{run_engine_on_tokio, AmuxApp, DraftKey, Selected};

impl AmuxApp {
    pub(crate) fn open_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        if let Some(wf) = self.workflow(&wf_id) {
            // 惰性加载：仅在打开会话渲染对话/活动视图时，从 JSONL 按需补齐 payload。
            if let Err(e) = wf.backfill(&self.session_dir) {
                log::error!("补齐工作流历史失败：{e}");
            }
        }
        self.set_selected(Some(Selected::Workflow { id: wf_id }), window, cx);
        // 面板打开状态跨会话切换保持；活动/详情面板均支持工作流视图，
        // 工作目录/改动面板沿用机器级状态（与在会话面板上点击侧栏按钮一致）。
        self.workflow_dialog_limit = 50;
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn restore_workflows(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let sessions = match WorkflowEngine::load_all(&self.session_dir) {
            Ok(sessions) => sessions,
            Err(e) => {
                log::error!("加载工作流失败：{e}");
                return;
            }
        };
        if sessions.is_empty() {
            return;
        }
        for s in sessions {
            // 每个工作流独立 backend：RigBackend 的 synced_children/synced_activities
            // 是单轮 decide 的回传槽位，共享实例会在并发推进时互相覆盖
            // （A 可能取到 B 的子会话快照）
            self.workflows.push(WorkflowEngine::restore(
                s,
                self.orchestrator_backend(),
                self.machine_hub.clone(),
                &self.session_dir,
            ));
        }
        cx.notify();
    }

    pub(crate) fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.workflow_input.read(cx).value().to_string();
        let template = self.workflow_template.take();
        let description = if goal.trim().is_empty() {
            template
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default()
        } else {
            goal.trim().to_string()
        };
        if template.is_none() && description.is_empty() {
            self.workflow_error = Some("请先用自然语言描述执行计划".into());
            cx.notify();
            return;
        }
        let preamble = template.map(|t| t.plan);
        self.create_workflow_with(window, cx, description, preamble);
        self.workflow_input
            .update(cx, |s, cx| s.set_value("", window, cx));
    }

    pub(crate) fn create_workflow_with(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        description: String,
        preamble: Option<String>,
    ) {
        if !self.store.orchestrator().is_configured() {
            self.workflow_error = Some(
                "编排 agent 未配置 API（Base URL / API key / 模型）。请先在 设置 → 编排 agent 中配置。"
                    .into(),
            );
            cx.notify();
            return;
        }
        self.workflow_error = None;
        let (clean, refs) = parse_at_references(&description);
        let context = refs
            .iter()
            .map(|r| read_path_context(r))
            .collect::<Vec<_>>()
            .join("\n");
        let backend = self.orchestrator_backend();
        let engine = WorkflowEngine::new(
            &clean,
            &context,
            preamble.as_deref().unwrap_or(""),
            backend,
            self.machine_hub.clone(),
            &self.session_dir,
        );
        let wi = self.workflows.len();
        let session_dir = self.session_dir.clone();
        self.workflows.push(engine);
        let wf_id = self.workflows[wi].id();
        self.set_selected(Some(Selected::Workflow { id: wf_id }), window, cx);
        let should_advance =
            !clean.trim().is_empty() || preamble.as_deref().is_some_and(|p| !p.trim().is_empty());
        if should_advance {
            if let Some(wf) = self.workflows.get_mut(wi) {
                wf.begin_busy();
            }
        }
        if let Some(wf) = self.workflows.get(wi) {
            wf.persist_in_background(session_dir.clone());
        }
        if should_advance {
            let wf = self.workflows[wi].clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                run_engine_on_tokio(async move {
                    if let Err(e) = wf.advance().await {
                        log::error!("推进工作流失败 {}: {e}", wf.id());
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                    }
                })
                .await;
                let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
            });
            self._tasks.push(t);
        }
        cx.notify();
    }

    pub(crate) fn cancel_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let session_dir = self.session_dir.clone();
        let Some(engine) = self.workflow_idx(&wf_id) else {
            return;
        };
        let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
            let should_advance = wf.cancel();
            if should_advance {
                wf.begin_busy();
            }
            wf.persist_in_background(session_dir.clone());
            should_advance
        } else {
            false
        };
        if should_advance {
            let wf = self.workflows[engine].clone();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                run_engine_on_tokio(async move {
                    if let Err(e) = wf.advance().await {
                        log::error!("取消推进工作流失败 {}: {e}", wf.id());
                    }
                    if let Err(e) = wf.persist(&session_dir) {
                        log::error!("工作流状态持久化失败 {}: {e}", wf.id());
                    }
                })
                .await;
                let _ = this.update_in(cx, |_this, _w, cx| cx.notify());
            });
            self._tasks.push(t);
        }
    }

    pub(crate) fn confirm_delete_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let child_count = self
            .workflow_idx(&wf_id)
            .and_then(|idx| self.workflows.get(idx))
            .map(|w| w.child_count())
            .unwrap_or(0);
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除工作流会话",
            format!(
                "确定删除该工作流会话吗？将同时删除其 {child_count} 个关联普通会话，不可恢复。"
            ),
            move |this, window, cx| {
                let wf_id = wf_id.clone();
                this.delete_workflow(window, cx, wf_id);
            },
        );
    }

    pub(crate) fn delete_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let Some(idx) = self.workflow_idx(&wf_id) else {
            return;
        };
        let children: Vec<(usize, String)> = self
            .workflows
            .get(idx)
            .map(|w| {
                w.children()
                    .iter()
                    .map(|c| (c.machine_idx, c.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if self
            .workflows
            .get(idx)
            .is_some_and(|workflow| workflow.state() == SessionState::Busy)
        {
            self.workflow_error = Some("请先取消正在执行的工作流，再删除工作流会话。".into());
            cx.notify();
            return;
        }
        let targets: Vec<(usize, WsClient, String)> = children
            .iter()
            .filter_map(|(machine, sid)| {
                self.machine(*machine)
                    .map(|m| (*machine, m.client.clone(), sid.clone()))
            })
            .collect();
        let missing_machine = targets.len() != children.len();
        let remote_targets = targets.clone();
        let session_dir = self.session_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                if missing_machine {
                    return Err("关联普通会话所属机器已移除，无法完成远端删除".to_string());
                }
                for (_, client, sid) in &remote_targets {
                    let params = SessionIdParams {
                        session_id: sid.clone(),
                    };
                    client
                        .request_ok(protocol::method::SESSION_DELETE, Some(params))
                        .await
                        .map_err(|error| format!("删除关联普通会话 {sid} 失败：{error}"))?;
                }
                Ok(())
            })
            .await;
            let _ = this.update_in(cx, |this, w, cx| {
                match result {
                    Some(Ok(())) => {
                        for (machine, _, sid) in &targets {
                            if let Some(m) = this.machine_mut(*machine) {
                                m.sessions.retain(|session| session.id != *sid);
                                m.views.remove(sid);
                            }
                        }
                        match WorkflowEngine::remove(&session_dir, &wf_id) {
                            Ok(()) => {
                                // 选中态以工作流会话 ID 为身份：删除后无需平移其他引用
                                this.workflows.retain(|workflow| workflow.id() != wf_id);
                                if this.selected == Some(Selected::Workflow { id: wf_id.clone() }) {
                                    this.set_selected(None, w, cx);
                                }
                                this.drafts.retain(|key, _| match key {
                                    DraftKey::Workflow { id } => id != &wf_id,
                                    // 关联的普通会话已一并删除，草稿随之清理
                                    DraftKey::Session { id, .. } => {
                                        !children.iter().any(|(_, sid)| sid == id)
                                    }
                                });
                            }
                            Err(error) => {
                                this.workflow_error =
                                    Some(format!("删除工作流持久化记录失败：{error}"));
                            }
                        }
                    }
                    Some(Err(error)) => {
                        this.workflow_error = Some(format!("删除工作流失败：{error}"));
                    }
                    None => {
                        this.workflow_error = Some("删除工作流任务未能执行".into());
                    }
                }
                cx.notify();
            });
        });
        self._tasks.push(t);
    }

    pub(crate) fn orchestrator_backend(&self) -> Arc<dyn OrcBackend> {
        let cfg = self.store.orchestrator();
        Arc::new(RigBackend::new(cfg))
    }

    pub(crate) fn machine_summaries(&self) -> Vec<MachineSummary> {
        // 全量透传（含不可用 agent 的真实 available）：编排 LLM 需要看到
        // 「某 agent 不可用」才能避让或上报，预先过滤会让该事实消失
        self.machines
            .iter()
            .map(|m| MachineSummary {
                name: m.config.name.clone(),
                online: m.status.online(),
                agents: m
                    .agents
                    .iter()
                    .map(|a| AgentSlot {
                        name: a.name.clone(),
                        available: a.available,
                    })
                    .collect(),
            })
            .collect()
    }

    pub(crate) fn rename_workflow(&mut self, cx: &mut Context<Self>, wf_id: &str, title: String) {
        if let Some(wf) = self
            .workflow_idx(wf_id)
            .and_then(|wi| self.workflows.get_mut(wi))
        {
            wf.session.write().unwrap().title = title.trim().to_string();
            wf.persist_in_background(self.session_dir.clone());
        }
        self.renaming_workflow = None;
        cx.notify();
    }

    pub(crate) fn render_workflow_row(
        &self,
        cx: &mut Context<Self>,
        wi: usize,
    ) -> gpui::AnyElement {
        let Some(wf) = self.workflows.get(wi) else {
            return div().into_any();
        };
        let wf_sel = self.selected == Some(Selected::Workflow { id: wf.id() });
        let wf_id = wf.id();
        let (title, busy) = {
            let title = if wf.title_is_empty() {
                "新工作流".to_string()
            } else {
                wf.title()
            };
            (title, wf.state() == SessionState::Busy)
        };
        let expanded = self.expanded_workflows.contains(&wf_id);
        // 各回调闭包（Fn）逐个持有独立克隆，避免互抢所有权
        let wf_id_title = wf_id.clone();
        let wf_id_toggle = wf_id.clone();
        let header = h_flex()
            .gap_1()
            .items_center()
            .child(
                h_flex()
                    .id(format!("wf-title-{wf_id}"))
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.open_workflow(window, cx, wf_id_title.clone());
                    }))
                    .child(Icon::new(IconName::Network).small().text_color(if wf_sel {
                        cx.theme().primary
                    } else {
                        cx.theme().muted_foreground
                    }))
                    .child(
                        Label::new(title.as_str())
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .child(if busy {
                        Tag::warning()
                            .small()
                            .rounded_full()
                            .child(Label::new("编排中…").text_xs())
                            .into_any_element()
                    } else {
                        Tag::secondary()
                            .outline()
                            .small()
                            .rounded_full()
                            .child(Label::new("空闲").text_xs())
                            .into_any_element()
                    }),
            )
            .child(
                Button::new(format!("wf-toggle-{wf_id}"))
                    .small()
                    .ghost()
                    .icon(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        if !this.expanded_workflows.insert(wf_id_toggle.clone()) {
                            this.expanded_workflows.remove(&wf_id_toggle);
                        }
                        cx.notify();
                    })),
            )
            .child(if busy {
                Spinner::new()
                    .xsmall()
                    .color(cx.theme().primary)
                    .into_any_element()
            } else {
                div().size_2().into_any_element()
            });

        // 子会话默认折叠、可展开下钻。标题/忙闲联表本机会话缓存（权威在 server）
        let mut children = wf.children();
        children.sort_by_key(|child| {
            std::cmp::Reverse(
                self.machine(child.machine_idx)
                    .and_then(|machine| {
                        machine
                            .sessions
                            .iter()
                            .find(|session| session.id == child.id)
                    })
                    .map(|session| session.last_active_at)
                    .unwrap_or(0),
            )
        });
        let mut content = v_flex().gap_1();
        for c in &children {
            let cid = c.id.clone();
            let cid_open = cid.clone();
            let machine_name = c.machine_name.clone();
            let machine_click = machine_name.clone();
            let meta = self
                .machine(c.machine_idx)
                .and_then(|machine| machine.sessions.iter().find(|s| s.id == c.id));
            let step = meta
                .map(|m| m.title.clone())
                .unwrap_or_else(|| "（会话不存在）".into());
            let agent = meta.map(|m| m.agent.clone()).unwrap_or_default();
            let busy = meta.is_some_and(|m| m.state == SessionState::Busy);
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Label::new("↳").text_color(cx.theme().muted_foreground))
                    .child(
                        h_flex()
                            .id(format!("wf-child-title-{wf_id}-{cid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let mi = this
                                    .machines
                                    .iter()
                                    .position(|mm| mm.config.name == machine_click)
                                    .unwrap_or(0);
                                this.open_session(window, cx, mi, cid_open.clone());
                            }))
                            .child(Label::new(step).text_sm().flex_1().min_w_0().truncate())
                            .child(
                                Label::new(format!("{agent}@{machine_name}"))
                                    .text_xs()
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .child(if busy {
                        Spinner::new()
                            .xsmall()
                            .color(cx.theme().primary)
                            .into_any_element()
                    } else {
                        div().size_2().into_any_element()
                    }),
            );
        }

        if self.renaming_workflow.as_deref() == Some(wf_id.as_str()) {
            let wf_id2 = wf_id.clone();
            return v_flex()
                .gap_1()
                .p_2()
                .bg(cx.theme().popover)
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(Input::new(&self.title_input))
                .child(
                    h_flex().gap_1().child(
                        Button::new(format!("wf-rename-save-{wf_id}"))
                            .small()
                            .primary()
                            .flex_1()
                            .label("保存")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                let title = this.title_input.read(cx).value().to_string();
                                this.rename_workflow(cx, &wf_id2.clone(), title);
                            })),
                    ),
                )
                .child(
                    Button::new(format!("wf-rename-cancel-{wf_id}"))
                        .small()
                        .ghost()
                        .flex_1()
                        .label("取消")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.renaming_workflow = None;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }

        let row = v_flex().gap_1().p_1().rounded_md().child(header).child(
            Collapsible::new()
                .open(self.expanded_workflows.contains(&wf_id))
                .content(content),
        );

        // 右键菜单交给 ContextMenu 组件（同普通会话行）；逐层持有独立克隆
        let app = cx.entity();
        let wf_id_menu = wf_id.clone();
        let title_menu = title.clone();
        div()
            .id(format!("wf-row-{wf_id}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(cx
                .theme()
                .list_active
                .opacity(if wf_sel { 1.0 } else { 0.0 }))
            .when(wf_sel, |d| {
                d.border_1().border_color(cx.theme().list_active_border)
            })
            .hover(|d| d.bg(cx.theme().list_hover))
            .context_menu(move |menu, _window, _cx| {
                menu.item(PopupMenuItem::new("重命名").on_click({
                    let app = app.clone();
                    let wf_id = wf_id_menu.clone();
                    let title = title_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.selected = Some(Selected::Workflow { id: wf_id.clone() });
                            this.renaming_workflow = Some(wf_id.clone());
                            this.title_input
                                .update(cx, |s, cx| s.set_value(&title, window, cx));
                            cx.notify();
                        });
                    }
                }))
                .item(PopupMenuItem::new("删除工作流").on_click({
                    let app = app.clone();
                    let wf_id = wf_id_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.confirm_delete_workflow(window, cx, wf_id.clone());
                        });
                    }
                }))
            })
            .child(row)
            .into_any_element()
    }

    pub(crate) fn render_template_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        if templates.is_empty() {
            return Label::new("（无计划，可在设置中添加）").into_any_element();
        }
        ButtonGroup::new("ns-tpl-group")
            .small()
            .flex_wrap()
            .children(templates.iter().map(|t| {
                let sel = self
                    .workflow_template
                    .as_ref()
                    .is_some_and(|x| x.name == t.name);
                Button::new(format!("ns-tpl-{}", t.name))
                    .label(t.name.clone())
                    .selected(sel)
            }))
            // 单选组回传按下索引；再次点击已选模板取消选择
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, _window, cx| {
                let Some(&ix) = clicks.first() else {
                    return;
                };
                if let Some(t) = this.store.list_templates().get(ix) {
                    if this
                        .workflow_template
                        .as_ref()
                        .is_some_and(|x| x.name == t.name)
                    {
                        this.workflow_template = None;
                    } else {
                        this.workflow_template = Some(t.clone());
                    }
                }
                cx.notify();
            }))
            .into_any_element()
    }
}
