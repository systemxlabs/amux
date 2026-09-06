use std::sync::Arc;

use gpui::prelude::FluentBuilder;

use crate::ws::WsClient;
use gpui::*;
use gpui_component::{
    button::*,
    collapsible::Collapsible,
    input::Input,
    label::Label,
    menu::{ContextMenuExt, PopupMenuItem},
    popover::Popover,
    spinner::Spinner,
    *,
};

use protocol::{SessionIdParams, SessionState};

use crate::machine::MachineStatus;
use crate::workflow::{OrcBackend, RigBackend, WorkflowEngine};

use crate::app::{run_engine_on_tokio, AmuxApp, DraftKey, Selected};

fn is_linked_session_draft(
    machine: &str,
    session_id: &str,
    linked_sessions: &[(String, String)],
) -> bool {
    linked_sessions
        .iter()
        .any(|(linked_machine, linked_id)| linked_machine == machine && linked_id == session_id)
}

fn is_deleted_selected_session(
    selected: Option<(String, String)>,
    deleted_sessions: &[(String, String)],
) -> bool {
    selected.is_some_and(|(machine, id)| {
        deleted_sessions
            .iter()
            .any(|(deleted_machine, deleted_id)| deleted_machine == &machine && deleted_id == &id)
    })
}

impl AmuxApp {
    pub(crate) fn open_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        if let Some(wf) = self.workflow(&wf_id) {
            // 惰性加载：仅在打开会话渲染对话/活动视图时，从 JSONL 按需补齐 payload。
            if let Err(e) = wf.backfill(&self.data_dir) {
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
        let (sessions, has_more) = match WorkflowEngine::load_window(
            &self.data_dir,
            self.list_pages.saturating_mul(crate::app::PAGE_LIMIT),
        ) {
            Ok(result) => result,
            Err(e) => {
                log::error!("加载工作流失败：{e}");
                return;
            }
        };
        self.workflow_has_more = has_more;
        self.visible_workflows = sessions.iter().map(|session| session.id.clone()).collect();
        for session in sessions {
            if self.workflow_idx(&session.id).is_some() {
                continue;
            }
            // 每个工作流独立 backend：共享实例会在并发推进时互相覆盖
            //（A 可能取到 B 的关联普通会话快照）
            self.workflows.push(WorkflowEngine::restore(
                session,
                self.orchestrator_backend(),
                self.machine_hub.clone(),
                &self.data_dir,
            ));
        }
        cx.notify();
    }

    pub(crate) fn create_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.workflow_input.read(cx).value().trim().to_string();
        if goal.is_empty() {
            self.workflow_error = Some("请先手动输入工作流计划，或从下拉选择已保存的工作流".into());
            cx.notify();
            return;
        }
        let description = goal;
        self.create_workflow_with(window, cx, description, None);
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
        let backend = self.orchestrator_backend();
        let engine = WorkflowEngine::new(
            &description,
            "",
            preamble.as_deref().unwrap_or(""),
            backend,
            self.machine_hub.clone(),
            &self.data_dir,
        );
        let wi = self.workflows.len();
        let data_dir = self.data_dir.clone();
        self.workflows.push(engine);
        let wf_id = self.workflows[wi].id();
        self.visible_workflows.insert(wf_id.clone());
        // 新建仅落元数据（含执行计划），不驱动：用户在对话界面输入消息后
        // 经 record_user 触发推进（同普通会话的对话驱动模式）
        self.set_selected(Some(Selected::Workflow { id: wf_id }), window, cx);
        if let Some(wf) = self.workflows.get(wi) {
            wf.persist_in_background(data_dir.clone());
        }
        cx.notify();
    }

    pub(crate) fn cancel_workflow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        wf_id: String,
    ) {
        let data_dir = self.data_dir.clone();
        let Some(engine) = self.workflow_idx(&wf_id) else {
            return;
        };
        let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
            let should_advance = wf.cancel();
            if should_advance {
                wf.begin_busy();
            }
            wf.persist_in_background(data_dir.clone());
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
                    if let Err(e) = wf.persist(&data_dir) {
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
        let linked_session_count = self
            .workflow_idx(&wf_id)
            .and_then(|idx| self.workflows.get(idx))
            .map(|w| w.linked_session_count())
            .unwrap_or(0);
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            ButtonVariant::Danger,
            "删除工作流会话",
            format!(
                "确定删除该工作流会话吗？将同时删除其 {linked_session_count} 个关联普通会话，不可恢复。"
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
        let workflow = self.workflows[idx].clone();
        let linked_sessions: Vec<(String, String)> = workflow
            .linked_sessions()
            .iter()
            .map(|c| (c.machine_name.clone(), c.id.clone()))
            .collect();
        if workflow.state() == SessionState::Busy {
            self.workflow_error = Some("请先取消正在执行的工作流，再删除工作流会话。".into());
            cx.notify();
            return;
        }
        workflow.mark_deleted();
        // 异步删除期间其它机器可能被移除，导致 machines Vec 重排；只携带稳定机器名，
        // 完成回调按名称重新解析，禁止把旧下标误当成另一台机器。
        #[derive(Clone)]
        struct RemoteSessionTarget {
            machine: String,
            client: WsClient,
            session_id: String,
            generation: u64,
        }
        let mut targets: Vec<RemoteSessionTarget> = Vec::new();
        let mut unavailable = Vec::new();
        for (machine_name, sid) in &linked_sessions {
            match self.machine_by_name(machine_name) {
                Some(m) => targets.push(RemoteSessionTarget {
                    machine: m.config.name.clone(),
                    client: m.client.clone(),
                    session_id: sid.clone(),
                    generation: m.connection_generation,
                }),
                None => unavailable.push(format!("机器 {machine_name} 不可用，无法删除会话 {sid}")),
            }
        }
        let target_connections: Vec<(String, u64)> = targets
            .iter()
            .map(|t| (t.machine.clone(), t.generation))
            .collect();
        let remote_targets = targets.clone();
        let data_dir = self.data_dir.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = run_engine_on_tokio(async move {
                let mut deleted = Vec::new();
                let mut failures = unavailable;
                for t in &remote_targets {
                    let params = SessionIdParams {
                        session_id: t.session_id.clone(),
                    };
                    match t
                        .client
                        .request_ok(protocol::method::SESSION_DELETE, Some(params))
                        .await
                    {
                        Ok(()) => deleted.push((t.machine.clone(), t.session_id.clone())),
                        Err(error) if error.code == protocol::server_error::SESSION_NOT_FOUND => {
                            deleted.push((t.machine.clone(), t.session_id.clone()));
                        }
                        Err(error) => failures
                            .push(format!("删除关联普通会话 {} 失败：{error}", t.session_id)),
                    }
                }
                Ok::<_, String>((deleted, failures))
            })
            .await;
            let _ = this.update_in(cx, |this, w, cx| {
                let targets_current = target_connections.iter().all(|(name, generation)| {
                    this.is_current_machine_connection(name, *generation)
                });
                if !targets_current {
                    workflow.unmark_deleted();
                    return;
                }
                match result {
                    Some(Ok((deleted, failures))) => {
                        for (machine_name, sid) in &deleted {
                            if let Some(m) = this.machine_mut_by_name(machine_name) {
                                m.sessions.retain(|session| session.id != *sid);
                                m.views.remove(sid);
                            }
                        }
                        let deleted_selected_session =
                            is_deleted_selected_session(this.open_session_target(), &deleted);
                        if deleted_selected_session {
                            this.set_selected(None, w, cx);
                        }
                        if failures.is_empty() {
                            match workflow.remove_deleted(&data_dir) {
                                Ok(()) => {
                                    this.workflows.retain(|workflow| workflow.id() != wf_id);
                                    this.visible_workflows.remove(&wf_id);
                                    if this.selected
                                        == Some(Selected::Workflow { id: wf_id.clone() })
                                    {
                                        this.set_selected(None, w, cx);
                                    }
                                    this.drafts.retain(|key, _| match key {
                                        DraftKey::Workflow { id } => id != &wf_id,
                                        DraftKey::Session { machine, id } => {
                                            !is_linked_session_draft(machine, id, &linked_sessions)
                                        }
                                    });
                                }
                                Err(error) => {
                                    workflow.unmark_deleted();
                                    this.workflow_error =
                                        Some(format!("删除工作流持久化记录失败：{error}"));
                                }
                            }
                        } else {
                            workflow.unmark_deleted();
                            this.workflow_error = Some(format!(
                                "工作流部分删除完成，剩余关联会话可重试：{}",
                                failures.join("；")
                            ));
                        }
                    }
                    Some(Err(error)) => {
                        workflow.unmark_deleted();
                        this.workflow_error = Some(format!("删除工作流任务失败：{error}"));
                    }
                    None => {
                        workflow.unmark_deleted();
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

    pub(crate) fn rename_workflow(&mut self, cx: &mut Context<Self>, wf_id: &str, title: String) {
        if let Some(wf) = self
            .workflow_idx(wf_id)
            .and_then(|wi| self.workflows.get_mut(wi))
        {
            wf.session.write().title = title.trim().to_string();
            wf.persist_in_background(self.data_dir.clone());
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
                    ), // 状态展示同普通会话行：工作中（编排调度中或关联
                       // 普通会话工作中）显示转圈，空闲无指示
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

        // 关联普通会话默认折叠、可展开下钻。标题/忙闲联表本机会话缓存（权威在 server）
        let mut linked_sessions = wf.linked_sessions();
        linked_sessions.sort_by_key(|linked| {
            std::cmp::Reverse(
                self.machine_idx_by_name(&linked.machine_name)
                    .and_then(|idx| self.machine(idx))
                    .and_then(|machine| {
                        machine
                            .sessions
                            .iter()
                            .find(|session| session.id == linked.id)
                    })
                    .map(|session| session.last_active_at)
                    .unwrap_or(0),
            )
        });
        let mut content = v_flex()
            .gap_1()
            .debug_selector(|| "wf-linked-sessions-list".into());
        for c in &linked_sessions {
            let cid = c.id.clone();
            let machine_name = c.machine_name.clone();
            let machine = self
                .machine_idx_by_name(&machine_name)
                .map(|idx| &self.machines[idx]);
            let meta = machine.and_then(|machine| machine.sessions.iter().find(|s| s.id == cid));
            let available = machine.is_some_and(|machine| {
                machine.status == MachineStatus::Online
                    && !machine.unavailable_workflow_sessions.contains(&cid)
            }) && meta.is_some();
            let step = if available {
                meta.map(|m| m.title.clone()).unwrap_or_default()
            } else {
                crate::logic::unavailable_workflow_session_title(&cid, &machine_name)
            };
            let busy = available && meta.is_some_and(|m| m.state == SessionState::Busy);
            let mut title = h_flex()
                .id(format!("wf-linked-session-title-{wf_id}-{cid}"))
                .flex_1()
                .min_w_0()
                .items_center()
                // 会话列表每行只展示标题与状态；
                // agent@机器 属于对话视图 header，不在此处重复
                .child(Label::new(step).text_sm().flex_1().min_w_0().truncate());
            if available {
                let cid_open = cid.clone();
                let machine_name_open = machine_name.clone();
                title = title.on_click(cx.listener(move |this, _ev, window, cx| {
                    // 机器已移除时 open_session 按名字解析不到即静默返回
                    this.open_session(window, cx, &machine_name_open, cid_open.clone());
                }));
            }
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Label::new("↳").text_color(cx.theme().muted_foreground))
                    .child(title)
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
            .debug_selector(|| "wf-row".into())
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
                            this.set_selected(
                                Some(Selected::Workflow { id: wf_id.clone() }),
                                window,
                                cx,
                            );
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

    /// 工作流计划输入：输入框可手动输入；有已保存工作流时输入框本身即
    /// Popover 触发器（同工作目录选择），下拉项展示名称与计划（截断），
    /// 选择后把工作流计划内容复制到输入框。
    pub(crate) fn render_template_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let templates = self.store.list_templates();
        let app = cx.entity();
        let open = self.show_workflow_dropdown;
        let content = if templates.is_empty() {
            Input::new(&self.workflow_input).into_any_element()
        } else {
            Popover::new("workflow-plan-picker")
                .anchor(Anchor::BottomLeft)
                .open(open)
                .on_open_change({
                    let app = app.clone();
                    move |is_open, _window, cx| {
                        app.update(cx, |this, cx| {
                            this.show_workflow_dropdown = *is_open;
                            cx.notify();
                        });
                    }
                })
                .trigger(
                    Input::new(&self.workflow_input).suffix(
                        Icon::new(IconName::ChevronDown)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    ),
                )
                .content({
                    let store = self.store.clone();
                    move |_, _window, cx| {
                        // 受控开启：选项点击后经 AmuxApp 关闭（on_open_change 回写）
                        let hover_bg = cx.theme().accent;
                        v_flex()
                            .id("workflow-plan-picker-list")
                            .w(rems(30.))
                            .max_h(rems(16.))
                            .overflow_y_scroll()
                            .gap_0p5()
                            .children(store.list_templates().into_iter().map(|t| {
                                let app = app.clone();
                                let name = t.name.clone();
                                let plan = t.plan.clone();
                                let plan_head = plan.lines().next().unwrap_or_default().to_string();
                                div()
                                    .id(format!("ns-tpl-option-{name}"))
                                    .w_full()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(move |d| d.bg(hover_bg))
                                    .on_click(move |_, window, cx| {
                                        // 把工作流计划内容复制到输入框，供手动微调
                                        app.update(cx, |this, cx| {
                                            this.workflow_input
                                                .update(cx, |s, cx| s.set_value(&plan, window, cx));
                                            this.show_workflow_dropdown = false;
                                            this.workflow_error = None;
                                            cx.notify();
                                        });
                                    })
                                    .child(
                                        v_flex()
                                            .gap_0p5()
                                            .min_w_0()
                                            .child(
                                                Label::new(name)
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM),
                                            )
                                            .child(
                                                // 计划为多行文本：仅展示首行并截断
                                                Label::new(plan_head)
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis(),
                                            ),
                                    )
                            }))
                    }
                })
                .into_any_element()
        };
        v_flex().gap_1().child(content).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{is_deleted_selected_session, is_linked_session_draft};

    #[test]
    fn linked_session_draft_requires_machine_and_id() {
        let linked = vec![("machine-a".to_string(), "same-id".to_string())];

        assert!(is_linked_session_draft("machine-a", "same-id", &linked));
        assert!(!is_linked_session_draft("machine-b", "same-id", &linked));
        assert!(!is_linked_session_draft("machine-a", "other-id", &linked));
    }

    #[test]
    fn deleted_selected_session_requires_machine_and_id() {
        let deleted = vec![("machine-a".to_string(), "same-id".to_string())];

        assert!(is_deleted_selected_session(
            Some(("machine-a".into(), "same-id".into())),
            &deleted
        ));
        assert!(!is_deleted_selected_session(
            Some(("machine-b".into(), "same-id".into())),
            &deleted
        ));
        assert!(!is_deleted_selected_session(
            Some(("machine-a".into(), "other-id".into())),
            &deleted
        ));
        assert!(!is_deleted_selected_session(None, &deleted));
    }
}
