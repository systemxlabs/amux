use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    alert::Alert,
    button::*,
    checkbox::Checkbox,
    input::{Input, Paste},
    label::Label,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    notification::Notification as UiNotification,
    popover::Popover,
    spinner::Spinner,
    switch::Switch,
    tag::Tag,
    text::TextView,
    tooltip::Tooltip,
    *,
};

use protocol::{
    ActivitiesResult, HistoryResult, OngoingActivityResult, OpResult, SessionConfigKind,
    SessionConfigOptionValue, SessionConfigOptionsResult, SessionConfigSetting,
    SessionConfigureParams, SessionIdParams, SessionInfoParams, SessionInfoResult,
    SessionListParams, SessionListResult, SessionMeta, SessionNewParams, SessionPageParams,
    SessionPlanResult, SessionPromptParams, SessionResult, SessionSlashCommandsResult,
    SessionState,
};

use crate::config::QuickCommand;
use crate::display::short_cwd;
use crate::logic::{
    activity_kind_detail, compose_prompt, compose_workflow_text, external_path_attachment,
    filter_slash_commands, image_attachment, merge_session_window, slash_command_prefix, DialogMsg,
    InputAttachment,
};
use crate::machine::MachineStatus;
use crate::text::{block_text, format_local_time, one_line, TimePrecision};
use crate::workflow::now;
use protocol::Activity;

use crate::app::{
    run_engine_on_tokio, AmuxApp, DraftKey, NewSessionMode, Panel, Selected, SelectedConfigOptions,
    SelectedSlashCommands, SessionListItem, SettingsCategory, PAGE_LIMIT,
};

async fn request_session_page<R, T>(
    client: crate::ws::WsClient,
    method: &'static str,
    session_id: String,
    before: Option<u64>,
    decode: impl FnOnce(R) -> (Vec<T>, bool, Option<u64>),
) -> Result<(Vec<T>, bool, Option<usize>), crate::ws::RpcError>
where
    R: serde::de::DeserializeOwned,
{
    let response = client
        .request::<_, R>(
            method,
            Some(SessionPageParams {
                session_id,
                limit: Some(PAGE_LIMIT),
                before,
            }),
        )
        .await?;
    let (items, has_more, next_before) = decode(response);
    Ok((items, has_more, next_before.map(|value| value as usize)))
}

impl AmuxApp {
    pub(crate) fn refresh_sessions(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machines.get_mut(idx) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let request_id = {
            m.sessions_request_id = m.sessions_request_id.saturating_add(1);
            m.sessions_request_id
        };
        let count = self.list_pages * PAGE_LIMIT;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // 滚动查询（docs/DESIGN.md「会话列表滚动查询」）：按数量查询前 N 页。
            let params = SessionListParams { limit: Some(count) };
            let (pages, has_more) = match client
                .request::<_, SessionListResult>(protocol::method::SESSION_LIST, Some(params))
                .await
            {
                // 刷新失败保持现状：离线/断连由列表的机器在线过滤兜底
                Ok(res) => (res.sessions, res.has_more),
                Err(_) => return,
            };

            // 应用查询结果，并计算工作流关联会话中尚未出现在其中的 id。
            // 机器名和连接代次均需匹配，防止删机重排或重连旧响应误写。
            let missing = match this.update_in(cx, |this, _w, _cx| {
                let idx = this.machine_idx_by_name(&machine_name)?;
                let m = this.machines.get_mut(idx)?;
                if m.connection_generation != generation || m.sessions_request_id != request_id {
                    return None;
                }
                m.sessions = pages;
                m.sessions_has_more = has_more;
                crate::logic::sort_sessions_recent(&mut m.sessions);
                let child_ids: Vec<String> = this
                    .workflows
                    .iter()
                    .filter(|wf| this.visible_workflows.contains(&wf.id()))
                    .flat_map(|wf| wf.session.read().children.clone())
                    .filter(|child| child.machine_name == machine_name)
                    .map(|child| child.id)
                    .collect();
                Some(
                    child_ids
                        .into_iter()
                        .filter(|cid| !m.sessions.iter().any(|s| s.id == *cid))
                        .collect::<Vec<_>>(),
                )
            }) {
                Ok(Some(missing)) => missing,
                Ok(None) | Err(_) => return,
            };
            if missing.is_empty() {
                let _ = this.update_in(cx, |_, _, cx| cx.notify());
                return;
            }

            // 第 2 步补齐：批量查询缺失的工作流关联会话
            if let Ok(res) = client
                .request::<_, SessionInfoResult>(
                    protocol::method::SESSION_INFO,
                    Some(SessionInfoParams {
                        session_ids: missing,
                    }),
                )
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    if let Some(m) = this.machines.get_mut(idx) {
                        if m.connection_generation != generation
                            || m.sessions_request_id != request_id
                        {
                            return;
                        }
                        let (list, _) =
                            merge_session_window(&m.sessions, res.sessions, m.sessions_has_more);
                        m.sessions = list;
                        crate::logic::sort_sessions_recent(&mut m.sessions);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let Some(view) = m.views.get_mut(&session_id) else {
            return;
        };
        view.history_request_id = view.history_request_id.saturating_add(1);
        let request_id = view.history_request_id;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: session_id.clone(),
                limit: Some(PAGE_LIMIT),
                before: None,
            };
            if let Ok(res) = client
                .request::<_, HistoryResult>(protocol::method::SESSION_HISTORY, Some(params))
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let was_at_bottom = this.dialog_at_bottom();
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    let Some(m) = this.machines.get_mut(idx) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    let Some(v) = m.views.get_mut(&session_id) else {
                        return;
                    };
                    if v.history_request_id != request_id {
                        return;
                    }
                    v.set_history_page(
                        &res.items,
                        res.has_more,
                        res.next_before.map(|value| value as usize),
                    );
                    if was_at_bottom {
                        this.dialog_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_activities(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        if self.panel != Some(Panel::Activities) {
            return;
        }
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let Some(view) = m.views.get_mut(&session_id) else {
            return;
        };
        view.activities_request_id = view.activities_request_id.saturating_add(1);
        let request_id = view.activities_request_id;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionPageParams {
                session_id: session_id.clone(),
                limit: Some(PAGE_LIMIT),
                before: None,
            };
            if let Ok(res) = client
                .request::<_, ActivitiesResult>(protocol::method::SESSION_ACTIVITIES, Some(params))
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let was_at_bottom = this.activities_at_bottom();
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    let Some(m) = this.machines.get_mut(idx) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    let Some(v) = m.views.get_mut(&session_id) else {
                        return;
                    };
                    if v.activities_request_id != request_id {
                        return;
                    }
                    v.set_activities_page(
                        res.activities,
                        res.has_more,
                        res.next_before.map(|value| value as usize),
                    );
                    if was_at_bottom {
                        this.activities_scroll.scroll_to_bottom();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn refresh_ongoing(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let Some(view) = m.views.get_mut(&session_id) else {
            return;
        };
        view.ongoing_request_id = view.ongoing_request_id.saturating_add(1);
        let request_id = view.ongoing_request_id;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: session_id.clone(),
            };
            if let Ok(res) = client
                .request::<_, OngoingActivityResult>(
                    protocol::method::SESSION_ONGOING_ACTIVITY,
                    Some(params),
                )
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    let Some(m) = this.machines.get_mut(idx) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    let Some(v) = m.views.get_mut(&session_id) else {
                        return;
                    };
                    if v.ongoing_request_id != request_id {
                        return;
                    }
                    v.set_live(res.activity);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 拉取当前会话的 agent 计划（docs/PRD.md「会话计划」面板）。
    /// 面板未打开时不主动刷新，与 refresh_activities 同策略。
    pub(crate) fn refresh_plan(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        if self.panel != Some(Panel::Plan) {
            return;
        }
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let Some(view) = m.views.get_mut(&session_id) else {
            return;
        };
        view.plan_request_id = view.plan_request_id.saturating_add(1);
        let request_id = view.plan_request_id;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: session_id.clone(),
            };
            if let Ok(res) = client
                .request::<_, SessionPlanResult>(protocol::method::SESSION_PLAN, Some(params))
                .await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    let Some(m) = this.machines.get_mut(idx) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    let Some(v) = m.views.get_mut(&session_id) else {
                        return;
                    };
                    if v.plan_request_id != request_id {
                        return;
                    }
                    v.set_plan(res.entries);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_more_selected_page<R, T>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        next_before: fn(&crate::aggregate::SessionView) -> Option<usize>,
        method: &'static str,
        decode: impl FnOnce(R) -> (Vec<T>, bool, Option<u64>) + 'static,
        apply: impl FnOnce(&mut crate::aggregate::SessionView, Vec<T>, bool, Option<usize>) + 'static,
    ) where
        R: serde::de::DeserializeOwned + 'static,
        T: 'static,
    {
        let Some((machine, id)) = self.open_session_target() else {
            return;
        };
        let Some(m) = self.machines.get_mut(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let Some(view) = m.views.get_mut(&id) else {
            return;
        };
        let Some(before) = next_before(view) else {
            return;
        };
        let request_id = if method == protocol::method::SESSION_HISTORY {
            view.history_request_id = view.history_request_id.saturating_add(1);
            view.history_request_id
        } else {
            view.activities_request_id = view.activities_request_id.saturating_add(1);
            view.activities_request_id
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok((items, has_more, next_before)) =
                request_session_page(client, method, id.clone(), Some(before as u64), decode).await
            {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    let Some(m) = this.machines.get_mut(idx) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    let Some(view) = m.views.get_mut(&id) else {
                        return;
                    };
                    let current_request_id = if method == protocol::method::SESSION_HISTORY {
                        view.history_request_id
                    } else {
                        view.activities_request_id
                    };
                    if current_request_id != request_id {
                        return;
                    }
                    apply(view, items, has_more, next_before);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn load_more_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_more_selected_page(
            window,
            cx,
            |view| view.history_next_before,
            protocol::method::SESSION_HISTORY,
            |res: HistoryResult| (res.items, res.has_more, res.next_before),
            |view, items, has_more, next_before| {
                view.prepend_history_page(&items, has_more, next_before)
            },
        );
    }

    pub(crate) fn load_more_activities(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_more_selected_page(
            window,
            cx,
            |view| view.activities_next_before,
            protocol::method::SESSION_ACTIVITIES,
            |res: ActivitiesResult| (res.activities, res.has_more, res.next_before),
            |view, activities, has_more, next_before| {
                view.prepend_activities_page(activities, has_more, next_before)
            },
        );
    }

    /// 「加载更早会话」：滚动查询页数 N +1，所有在线机器统一按前 N 页重新查询，
    /// 随后各自补齐工作流关联会话（docs/DESIGN.md「会话列表滚动查询」）。
    pub(crate) fn load_more_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_pages += 1;
        self.restore_workflows(window, cx);
        self.refresh_all_online(window, cx);
    }

    /// 「收起」：页数归 1（PRD「左侧面板」），重新按单页查询。
    pub(crate) fn collapse_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list_pages == 1 {
            return;
        }
        self.list_pages = 1;
        self.restore_workflows(window, cx);
        self.refresh_all_online(window, cx);
    }

    fn refresh_all_online(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.machines.len() {
            if matches!(self.machines[i].status, MachineStatus::Online) {
                self.refresh_sessions(i, window, cx);
            }
        }
    }

    pub(crate) fn open_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        self.set_selected(
            Some(Selected::Session {
                machine,
                id: session_id.clone(),
            }),
            window,
            cx,
        );
        // 面板打开状态跨会话切换保持（docs/DESIGN.md：右侧上下文面板）。
        if let Some(m) = self.machines.get_mut(machine) {
            m.views.entry(session_id.clone()).or_default();
            m.diff.update(cx, |st, _| {
                st.files.clear();
                st.not_repo = false;
                st.selection.clear();
                st.request_id = st.request_id.saturating_add(1);
                st.loading = false;
                st.error = None;
            });
            m.workspace_directories.clear();
            m.workspace_expanded.clear();
            m.workspace_loading.clear();
            m.workspace_list_request_id = m.workspace_list_request_id.saturating_add(1);
            m.workspace_read_request_id = m.workspace_read_request_id.saturating_add(1);
            m.workspace_file = None;
            m.workspace_content.clear();
            m.workspace_error = None;
            m.workspace_read_loading = false;
            m.workspace_read_has_more = false;
            m.workspace_read_next_offset = 0;
        }
        // 会话级 diff/工作目录数据已清空，打开中的面板需按新会话重新加载；
        // 必须在清除状态之后调用，否则会打乱 diff_request_id/loading 守卫。
        match self.panel {
            Some(Panel::Workspace) => {
                self.load_workspace_list(window, cx, machine, String::new(), 0);
            }
            Some(Panel::Diff) => self.load_diff(window, cx, machine),
            _ => {}
        }
        self.refresh_dialog(window, cx, machine, session_id.clone());
        self.refresh_activities(window, cx, machine, session_id.clone());
        self.refresh_ongoing(window, cx, machine, session_id.clone());
        self.refresh_config_options(cx, machine, session_id.clone());
        self.refresh_slash_commands(cx, machine, session_id);
        self.dialog_scroll.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        let attachments = self.input_attachments.clone();
        if text.trim().is_empty() && attachments.is_empty() {
            return;
        }
        let blocks = compose_prompt(&text, &attachments);
        let Some(target) = self.selected.clone() else {
            cx.notify();
            return;
        };
        match target {
            Selected::Session { machine, id } => {
                let Some(m) = self.machine(machine) else {
                    return;
                };
                let client = m.client.clone();
                let machine_name = m.config.name.clone();
                let generation = m.connection_generation;
                let params = SessionPromptParams {
                    session_id: id.clone(),
                    input: blocks.clone(),
                };
                let optimistic_timestamp = now();
                if let Some(m) = self.machine_mut(machine) {
                    // 本地仅缓存对话视图；会话状态由服务端权威维护，
                    // 经 state_change 推送 / 会话列表轮询同步，应用侧不做乐观改写
                    let v = m.views.entry(id.clone()).or_default();
                    v.dialog.push(DialogMsg::UserMessage {
                        content: blocks.clone(),
                        timestamp: optimistic_timestamp,
                    });
                }
                self.dialog_scroll.scroll_to_bottom();
                let prompt_params = params;
                let original_text = text.clone();
                let original_attachments = attachments.clone();
                let optimistic_blocks = blocks.clone();
                let optimistic_id = id.clone();
                cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                    let result = client
                        .request_ok(protocol::method::SESSION_PROMPT, Some(prompt_params))
                        .await;
                    let _ = this.update_in(cx, |this, w, cx| {
                        if !this.is_current_machine_connection(machine, &machine_name, generation) {
                            return;
                        }
                        match &result {
                            Err(error) => {
                                if let Some(m) = this.machine_mut(machine) {
                                    if let Some(v) = m.views.get_mut(&optimistic_id) {
                                        if let Some(pos) = v.dialog.iter().rposition(|msg| {
                                            matches!(
                                                msg,
                                                DialogMsg::UserMessage {
                                                    content,
                                                    timestamp,
                                                } if *timestamp == optimistic_timestamp
                                                    && *content == optimistic_blocks
                                            )
                                        }) {
                                            v.dialog.remove(pos);
                                        }
                                    }
                                }
                                if matches!(
                                    this.selected,
                                    Some(Selected::Session {
                                        machine: selected_machine,
                                        ref id,
                                    }) if selected_machine == machine && id == &optimistic_id
                                ) && this.input_state.read(cx).value().trim().is_empty()
                                    && this.input_attachments.is_empty()
                                {
                                    this.input_state
                                        .update(cx, |s, cx| s.set_value(&original_text, w, cx));
                                    this.input_attachments = original_attachments.clone();
                                }
                                w.push_notification(
                                    UiNotification::error(format!("发送失败：{error}"))
                                        .title("消息未发送"),
                                    cx,
                                );
                            }
                            // 发送用户消息即触发列表刷新（docs/DESIGN.md 会话列表刷新机制）
                            Ok(()) => {
                                this.refresh_sessions(machine, w, cx);
                            }
                        }
                        this.refresh_dialog(w, cx, machine, optimistic_id);
                        cx.notify();
                    });
                })
                .detach();
            }
            Selected::Workflow { id } => {
                let data_dir = self.data_dir.clone();
                let workflow_text = compose_workflow_text(&text, &attachments);
                let Some(engine) = self.workflow_idx(&id) else {
                    return;
                };
                let should_advance = if let Some(wf) = self.workflows.get_mut(engine) {
                    let should_advance = wf.record_user(&workflow_text);
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
                                log::error!("推进工作流失败 {}: {e}", wf.id());
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
        }
        self.input_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.input_attachments.clear();
    }

    pub(crate) fn quick_command(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        cmd: &QuickCommand,
    ) {
        // 快捷指令等价于把提示词填入输入框后走统一的发送链路
        // （本地回显、贴底滚动、完成后的对话/列表刷新都由 send_prompt 承担）
        self.input_state
            .update(cx, |s, cx| s.set_value(&cmd.prompt, window, cx));
        self.send_prompt(window, cx);
    }

    pub(crate) fn cancel_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Selected::Workflow { id }) = self.selected.clone() {
            self.cancel_workflow(window, cx, id);
            cx.notify();
            return;
        }
        let Some((machine, id)) = self.open_session_target() else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        // 空闲会话本就无可取消：ACP 侧报错属预期，静默忽略以免污染状态徽章；
        // 忙碌中取消失败才值得提示
        let was_busy = m
            .sessions
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.state == SessionState::Busy);
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let sid = id.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: sid.clone(),
            };
            let res = client
                .request_ok(protocol::method::SESSION_CANCEL, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if !this.is_current_machine_connection(machine, &machine_name, generation)
                    || !this.is_selected_session(machine, &sid)
                {
                    return;
                }
                if let Err(error) = &res {
                    if was_busy {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("取消失败（{error}）"));
                        }
                    }
                }
                this.refresh_dialog(w, cx, machine, sid);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn delete_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let sid = session_id.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let params = SessionIdParams {
                session_id: sid.clone(),
            };
            let res = client
                .request_ok(protocol::method::SESSION_DELETE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if !this.is_current_machine_connection(machine, &machine_name, generation) {
                    return;
                }
                match res {
                    Ok(_) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.sessions.retain(|s| s.id != sid);
                            m.views.remove(&sid);
                        }
                        if this.open_session_target().is_some_and(|(_, id)| id == sid) {
                            this.set_selected(None, w, cx);
                        }
                        this.drafts.retain(|key, _| match key {
                            DraftKey::Session { id, machine } => {
                                *id != sid || *machine != machine_name
                            }
                            DraftKey::Workflow { .. } => true,
                        });
                        this.refresh_sessions(machine, w, cx);
                    }
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("删除会话失败（{error}）"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn confirm_delete_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认删除",
            true,
            "删除会话",
            format!("确定删除会话 {session_id} 吗？删除后历史一并移除，不可恢复。"),
            move |this, window, cx| {
                let sid = session_id.clone();
                this.delete_session(window, cx, machine, sid);
            },
        );
    }

    pub(crate) fn rename_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
        title: String,
    ) {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let title_trim = title.trim().to_string();
        let params = SessionConfigureParams {
            session_id: session_id.clone(),
            title: Some(title_trim),
            config: None,
        };
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request_ok(protocol::method::SESSION_CONFIGURE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if !this.is_current_machine_connection(machine, &machine_name, generation) {
                    return;
                }
                match res {
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
                            m.notice = Some(format!("重命名失败（{error}）"));
                        }
                    }
                    Ok(()) => {
                        this.renaming_session = None;
                        // 主动刷新会话列表以体现新标题（修复 M1：重命名后不主动刷新）
                        this.refresh_sessions(machine, w, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn available_agent(&self, idx: usize) -> Option<String> {
        self.machine(idx).and_then(|m| {
            m.agents
                .iter()
                .find(|a| a.available)
                .map(|a| a.name.clone())
        })
    }

    pub(crate) fn selected_meta(&self) -> Option<SessionMeta> {
        if let Some((machine, id)) = self.open_session_target() {
            self.machine(machine)
                .and_then(|m| m.sessions.iter().find(|s| s.id == id))
                .cloned()
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            // 工作流会话详情数据仅来自应用侧会话元数据；cwd/worktree/context
            // 为普通会话字段，工作流会话不适用，置空占位（详情面板按类型过滤展示）
            let wf = self.workflows.get(self.workflow_idx(id)?)?;
            let sg = wf.snapshot();
            Some(SessionMeta {
                id: sg.id.clone(),
                agent: "编排".into(),
                cwd: String::new(),
                state: sg.state,
                title: sg.title.clone(),
                created_at: sg.created_at,
                last_active_at: sg.updated_at,
                worktree_dir: String::new(),
                context_size: 0,
                context_window_size: 0,
            })
        } else {
            None
        }
    }

    pub(crate) fn create_session_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let cwd = self.session_cwd_input.read(cx).value().trim().to_owned();
        if cwd.is_empty() {
            self.new_session_error = Some("请输入工作目录，或选择一个常用工作目录。".into());
            cx.notify();
            return;
        }
        self.new_session_error = None;
        let agent = match self.new_session_agent.clone() {
            Some(a) => a,
            None => match self.available_agent(machine) {
                Some(a) => a,
                None => {
                    if let Some(m) = self.machine_mut(machine) {
                        m.notice = Some("无可用 agent".into());
                    }
                    cx.notify();
                    return;
                }
            },
        };
        let params = SessionNewParams {
            agent: agent.clone(),
            cwd: cwd.clone(),
            use_worktree: self.new_session_worktree,
        };
        let client = self.machines[machine].client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request::<_, SessionResult>(protocol::method::SESSION_NEW, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                if !this.is_current_machine_connection(machine, &machine_name, generation) {
                    return;
                }
                match &res {
                    Ok(res) => {
                        let new_id = res.session.id.clone();
                        if !new_id.is_empty() {
                            this.store
                                .record_recent_workspace(&machine_name, &cwd, now());
                            this.refresh_sessions(machine, w, cx);
                            this.open_session(w, cx, machine, new_id);
                        } else {
                            w.push_notification(
                                UiNotification::error("服务器返回了无效的会话信息")
                                    .title("创建会话失败"),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        w.push_notification(
                            UiNotification::error(format!("无法创建会话：{error}"))
                                .title("创建会话失败"),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn selected_draft_key(&self) -> Option<DraftKey> {
        if let Some((machine, id)) = self.open_session_target() {
            Some(DraftKey::Session {
                machine: self.machines.get(machine)?.config.name.clone(),
                id,
            })
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            Some(DraftKey::Workflow { id: id.clone() })
        } else {
            None
        }
    }

    pub(crate) fn render_session_list(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // 子会话只挂在工作流会话下，顶层列表跳过
        let child_ids: std::collections::HashSet<String> = self
            .workflows
            .iter()
            .filter(|wf| self.visible_workflows.contains(&wf.id()))
            .flat_map(|wf| {
                wf.session
                    .read()
                    .children
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut items: Vec<(u64, SessionListItem)> = Vec::new();
        for (mi, m) in self.machines.iter().enumerate() {
            // 离线/认证失败/连接中的机器不展示其会话：数据是上次刷新的陈旧缓存
            // 且不可操作；机器恢复在线后随 10s 定时刷新自动重现
            if !matches!(m.status, MachineStatus::Online) {
                continue;
            }
            for s in &m.sessions {
                if child_ids.contains(s.id.as_str()) {
                    continue;
                }
                items.push((
                    s.last_active_at,
                    SessionListItem::Session {
                        machine: mi,
                        meta: s.clone(),
                    },
                ));
            }
        }
        for (wi, wf) in self.workflows.iter().enumerate() {
            if !self.visible_workflows.contains(&wf.id()) {
                continue;
            }
            let s_guard = wf.snapshot();
            let mut recency = s_guard.updated_at;
            for c in &s_guard.children {
                if let Some(mm) = self.machines.get(c.machine_idx) {
                    if let Some(s) = mm.sessions.iter().find(|s| s.id == c.id) {
                        recency = recency.max(s.last_active_at);
                    }
                }
            }
            items.push((recency, SessionListItem::Workflow { idx: wi }));
        }
        items.sort_by_key(|(rec, _)| std::cmp::Reverse(*rec));

        let mut rows: Vec<gpui::AnyElement> = items
            .into_iter()
            .map(|(_, item)| match item {
                SessionListItem::Session { machine, meta } => {
                    self.render_session_row(cx, machine, &meta)
                }
                SessionListItem::Workflow { idx } => self.render_workflow_row(cx, idx),
            })
            .collect();

        let any_online = self
            .machines
            .iter()
            .any(|m| matches!(m.status, MachineStatus::Online));
        if self.list_pages > 1 {
            rows.push(
                Button::new("sessions-collapse")
                    .small()
                    .label("收起")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.collapse_sessions(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        let sessions_have_more = any_online
            && self
                .machines
                .iter()
                .any(|m| matches!(m.status, MachineStatus::Online) && m.sessions_has_more);
        if sessions_have_more || self.workflow_has_more {
            rows.push(
                Button::new("sessions-more")
                    .small()
                    .label("加载更早会话")
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.load_more_sessions(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    pub(crate) fn render_session_row(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        s: &SessionMeta,
    ) -> gpui::AnyElement {
        let sid = s.id.clone();
        let sel = self.is_selected_session(machine, &sid);
        let title = if s.title.is_empty() {
            format!("（未命名）{}", short_cwd(&s.cwd))
        } else {
            s.title.clone()
        };
        // 重命名预填用原始标题（title 是含「（未命名）/目录」回退的展示文案）
        let raw_title = s.title.clone();
        let busy = s.state == SessionState::Busy;
        let label: SharedString = title.clone().into();
        let active = cx.theme().list_active;
        let border = cx.theme().list_active_border;

        if self.renaming_session.as_ref() == Some(&(machine, sid.clone())) {
            let sid2 = sid.clone();
            return v_flex()
                .gap_1()
                .child(Input::new(&self.title_input))
                .child(
                    h_flex().gap_1().child(
                        Button::new(format!("rename-save-{sid}"))
                            .small()
                            .primary()
                            .flex_1()
                            .label("保存")
                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                let title = this.title_input.read(cx).value().to_string();
                                this.rename_session(window, cx, machine, sid2.clone(), title);
                            })),
                    ),
                )
                .child(
                    // 取消路径：此前重命名只能保存，Esc 无效会一直挂在编辑态
                    Button::new(format!("rename-cancel-{sid}"))
                        .small()
                        .ghost()
                        .flex_1()
                        .label("取消")
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.renaming_session = None;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let sid_open = sid.clone();
        // 右键菜单交给 ContextMenu 组件：外点/Esc 关闭、键盘导航、焦点恢复由其负责。
        // 菜单构建闭包与各条目回调均为 Fn，逐层持有独立克隆
        let app = cx.entity();
        let sid_menu = sid.clone();
        div()
            .id(format!("sess-row-{machine}-{sid}"))
            .relative()
            .w_full()
            .rounded_md()
            .bg(active.opacity(if sel { 1.0 } else { 0.0 }))
            .when(sel, |d| d.border_1().border_color(border))
            .hover(|d| d.bg(cx.theme().list_hover))
            .on_click(cx.listener(move |this, _ev, window, cx| {
                this.open_session(window, cx, machine, sid_open.clone());
            }))
            .context_menu(move |menu, _window, _cx| {
                menu.item(PopupMenuItem::new("重命名").on_click({
                    let app = app.clone();
                    let sid = sid_menu.clone();
                    let raw_title = raw_title.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.selected = Some(Selected::Session {
                                machine,
                                id: sid.clone(),
                            });
                            this.renaming_session = Some((machine, sid.clone()));
                            this.title_input
                                .update(cx, |s, cx| s.set_value(&raw_title, window, cx));
                            cx.notify();
                        });
                    }
                }))
                .item(PopupMenuItem::new("删除会话").on_click({
                    let app = app.clone();
                    let sid = sid_menu.clone();
                    move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.confirm_delete_session(window, cx, machine, sid.clone());
                        });
                    }
                }))
            })
            .child(
                h_flex()
                    .w_full()
                    .h_8()
                    .px_1()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .small()
                            .text_color(if sel {
                                cx.theme().primary
                            } else {
                                cx.theme().muted_foreground
                            }),
                    )
                    .child(
                        h_flex()
                            .id(format!("sess-title-{machine}-{sid}"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .child(Label::new(label).text_sm().flex_1().min_w_0().truncate()),
                    )
                    .when(s.last_active_at > 0, |row| {
                        row.child(
                            Label::new(format_local_time(s.last_active_at, TimePrecision::Compact))
                                .text_xs()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .child(if busy {
                        Spinner::new()
                            .xsmall()
                            .color(cx.theme().primary)
                            .into_any_element()
                    } else {
                        div().size_2().into_any_element()
                    }),
            )
            .into_any_element()
    }

    pub fn render_dialog(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialog: Vec<DialogMsg> = if let Some((machine, id)) = self.open_session_target() {
            self.machine(machine)
                .and_then(|m| m.views.get(&id))
                .map(|v| v.dialog.clone())
                .unwrap_or_default()
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            self.workflow(id)
                .map(|w| {
                    let sg = w.snapshot();
                    let all = sg.to_dialog();
                    let start = all.len().saturating_sub(self.workflow_dialog_limit);
                    all[start..].to_vec()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let agent_label: SharedString = if let Some((machine, id)) = self.open_session_target() {
            self.machine(machine)
                .and_then(|m| {
                    let machine_name = m.config.name.clone();
                    m.sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| format!("{}@{machine_name}", s.agent).into())
                })
                .unwrap_or_else(|| "Agent".into())
        } else if matches!(self.selected, Some(Selected::Workflow { .. })) {
            "编排".into()
        } else {
            "Agent".into()
        };
        let primary = cx.theme().primary;
        let primary_foreground = cx.theme().primary_foreground;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let muted_foreground = cx.theme().muted_foreground;
        let rows = dialog
            .iter()
            .map(|item| match item {
                DialogMsg::UserMessage { content, timestamp } => {
                    let text = block_text(content);
                    let images = crate::text::message_images(content);
                    // 气泡贴内容：按最长行估算宽度，短消息收拢；长消息触顶换行。
                    // 下限需容纳「我 + 时间戳」头部行
                    let mut bubble_w = crate::text::estimate_bubble_width(
                        &text,
                        crate::theme::FONT_BODY.as_f32(),
                        132.,
                        720., // 消息气泡最大宽度（内容可读性上限）
                    );
                    if !images.is_empty() {
                        // 图片附件需足够宽度展示缩略图
                        bubble_w = bubble_w.max(px(280.));
                    }
                    div().id(("user-row", *timestamp)).w_full().child(
                        div()
                            .ml_auto()
                            .flex_none()
                            .w(bubble_w)
                            .overflow_hidden()
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(primary)
                            .shadow_sm()
                            .child(
                                Label::new("我")
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(primary_foreground),
                            )
                            .child(
                                Label::new(format_local_time(*timestamp, TimePrecision::Seconds))
                                    .text_xs()
                                    .text_color(cx.theme().primary_foreground.opacity(0.78)),
                            )
                            .child(
                                TextView::markdown(format!("umd-{timestamp}"), text)
                                    .selectable(true)
                                    .text_color(primary_foreground),
                            )
                            .children(images.iter().map(|image| {
                                img(std::sync::Arc::new(image.clone()))
                                    .w_full()
                                    .max_h(px(240.)) // 附件缩略图高度上限
                                    .object_fit(ObjectFit::Contain)
                                    .rounded_md()
                                    .overflow_hidden()
                            })),
                    )
                }
                DialogMsg::AgentMessage { content, timestamp } => {
                    let text = block_text(content);
                    let bubble_w = crate::text::estimate_bubble_width(
                        &text,
                        crate::theme::FONT_BODY.as_f32(),
                        132.,
                        720., // 消息气泡最大宽度（内容可读性上限）
                    );
                    div().id(("agent-row", *timestamp)).w_full().child(
                        div()
                            .flex_none()
                            .w(bubble_w)
                            .overflow_hidden()
                            .p_3()
                            .v_flex()
                            .gap_1()
                            .rounded_md()
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .shadow_sm()
                            .child(
                                Label::new(agent_label.clone())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted_foreground),
                            )
                            .child(
                                Label::new(format_local_time(*timestamp, TimePrecision::Seconds))
                                    .text_xs()
                                    .text_color(muted_foreground),
                            )
                            .child(
                                TextView::markdown(format!("amd-{timestamp}"), block_text(content))
                                    .selectable(true),
                            ),
                    )
                }
            })
            .collect::<Vec<_>>();
        let history_has_more = self
            .open_session_target()
            .and_then(|(machine, id)| self.machine(machine).and_then(|m| m.views.get(&id)))
            .map(|v| v.history_has_more)
            .unwrap_or(false);
        let mut content = Vec::new();
        if let Some(Selected::Workflow { id }) = &self.selected {
            let total = self
                .workflow(id)
                .map(|w| w.snapshot().transcript.len())
                .unwrap_or(0);
            if total > self.workflow_dialog_limit {
                content.push(
                    Button::new("load-more-workflow-history")
                        .small()
                        .ghost()
                        .label(format!("加载更早消息（共 {total} 条）"))
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.workflow_dialog_limit += 100;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        if history_has_more {
            content.push(
                Button::new("load-more-history")
                    .small()
                    .ghost()
                    .label("加载更早消息")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_history(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        content.extend(rows.into_iter().map(|r| r.into_any_element()));
        if content.is_empty() {
            // 空态：区分「未选中会话」与「已选中但暂无消息」，避免文案误导
            let hint = if self.selected.is_none() {
                "选择左侧会话查看对话，或输入消息开始"
            } else {
                "暂无消息，输入消息开始对话"
            };
            // 空态：图标 + 引导文案居中，弱化存在感
            div()
                .id("dialog-empty")
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Inbox)
                        .large()
                        .text_color(muted_foreground.opacity(0.55)),
                )
                .child(Label::new(hint).text_sm().text_color(muted_foreground))
                .into_any()
        } else {
            div()
                .id("dialog")
                .v_flex()
                .flex_1()
                .gap_4()
                .p_2()
                .overflow_y_scroll()
                .track_scroll(&self.dialog_scroll)
                .children(content)
                .into_any()
        }
    }

    pub(crate) fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current: Option<Activity> = if let Some((machine, id)) = self.open_session_target() {
            self.machine(machine)
                .and_then(|m| m.views.get(&id))
                .and_then(|v| v.live.clone())
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            // 实时活动只展示编排智能体正在进行的动作（流式思考增量、
            // 执行中的工具调用），动作结束即清除。历史活动不充当实时
            // 展示（修复工具执行完毕后活动条一直转圈）；编排智能体
            // 空闲而关联会话仍工作时，活动条为空。
            self.workflow(id).and_then(|wf| wf.current_activity())
        } else {
            None
        };
        let warning = cx.theme().warning;
        let warning_foreground = cx.theme().warning_foreground;
        let danger = cx.theme().danger;
        match &current {
            Some(Activity::Thinking { content, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!("思考中：{}", one_line(content, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::ToolCall { name, title, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(Spinner::new())
                .child(
                    Label::new(format!(
                        "工具调用：{} {}",
                        name,
                        one_line(title.as_deref().unwrap_or(""), 120)
                    ))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Compaction { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(warning.opacity(0.16))
                .border_1()
                .border_color(warning.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("上下文压缩：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(warning_foreground),
                )
                .into_any(),
            Some(Activity::Error { detail, .. }) => h_flex()
                .w_full()
                .gap_2()
                .p_2()
                .bg(danger.opacity(0.12))
                .border_1()
                .border_color(danger.opacity(0.45))
                .rounded_md()
                .child(
                    Label::new(format!("错误：{}", one_line(detail, 120)))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(danger),
                )
                .into_any(),
            None => div().id("activity-bar-empty").into_any(),
        }
    }

    /// 活动行身份：语义键（aggregate::activity_key）+ 前缀，而非下标——加载更早
    /// 活动会前移插入，下标键会让展开态漂移到其他条目。工具调用附名称以区分
    /// 同毫秒的多个调用。
    pub(crate) fn activity_row_key(prefix: &str, a: &Activity) -> String {
        let (kind, ts) = crate::aggregate::activity_key(a);
        match a {
            Activity::ToolCall { name, .. } => format!("{prefix}-{kind}-{ts}-{name}"),
            _ => format!("{prefix}-{kind}-{ts}"),
        }
    }

    pub(crate) fn activity_row(
        &self,
        prefix: &str,
        a: &Activity,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key_toggle = Self::activity_row_key(prefix, a);
        let expanded = self.expanded_activities.contains(&key_toggle);
        // 整卡可点击切换展开：折叠恒为一行（截断省略），展开显示全文（可换行）。
        // 不再用字符数阈值裁剪——截断交给样式层，展开态即原始 detail。
        let (_, ts) = crate::aggregate::activity_key(a);
        let (kind, detail) = activity_kind_detail(a);
        div()
            .id(key_toggle.clone())
            .w_full()
            .p_2()
            .bg(cx.theme().muted.opacity(0.55))
            .rounded_md()
            .cursor_pointer()
            .hover(|d| d.bg(cx.theme().muted))
            .on_click(cx.listener(move |this, _ev, _window, cx| {
                if !this.expanded_activities.remove(&key_toggle) {
                    this.expanded_activities.insert(key_toggle.clone());
                }
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        Label::new(format_local_time(ts, TimePrecision::Seconds))
                            .text_xs()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Label::new(kind.to_string())
                            .text_xs()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        Label::new(detail.to_string())
                            .text_sm()
                            .flex_1()
                            .min_w_0()
                            .when(!expanded, |l| l.truncate()),
                    ),
            )
            .into_any_element()
    }

    pub fn render_input(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            // 附件 chips：点击单个 chip 即移除该附件（原仅支持一键清空）
            .when(!self.input_attachments.is_empty(), |view| {
                view.child(h_flex().flex_wrap().gap_1().children(
                    self.input_attachments.iter().enumerate().map(|(i, a)| {
                        let label: SharedString = match a {
                            InputAttachment::Path { path, .. } => path.clone().into(),
                            InputAttachment::Image { name, .. } => name.clone().into(),
                        };
                        let tooltip_label = label.clone();
                        div()
                            .id(("attachment-chip", i))
                            .max_w(px(280.))
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .hover(|d| d.bg(cx.theme().secondary_hover))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_label.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.input_attachments.remove(i);
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_1()
                                    .overflow_hidden()
                                    .child(
                                        Icon::new(IconName::Close)
                                            .xsmall()
                                            .text_color(muted_foreground),
                                    )
                                    .child(
                                        Label::new(label.clone())
                                            .text_xs()
                                            .text_color(muted_foreground)
                                            .truncate(),
                                    ),
                            )
                    }),
                ))
            })
            .child(
                h_flex()
                    // relative：斜杠命令上拉框（absolute 定位）的锚点
                    .relative()
                    .gap_2()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .id("input-drop-zone")
                            // 最小高度放在 Input 上而非本容器：容器若高于 Input
                            // （auto_grow 的 3 行自然高 < 96px），底部对齐的按钮列
                            // 会垂到输入框下沿之外（对齐回归见 tests/input_align_layout.rs）
                            .debug_selector(|| "input-align-input".into())
                            .capture_action(cx.listener(|this, _: &Paste, _window, cx| {
                                let Some(item) = cx.read_from_clipboard() else {
                                    cx.propagate();
                                    return;
                                };
                                let mut handled = false;
                                let mut image_index = 0;
                                for entry in item.entries() {
                                    match entry {
                                        ClipboardEntry::Image(image) => {
                                            image_index += 1;
                                            let name = format!(
                                                "paste-{}.{}",
                                                image_index,
                                                image.format.extension()
                                            );
                                            this.input_attachments.push(image_attachment(
                                                &name,
                                                image.format.mime_type(),
                                                &image.bytes,
                                            ));
                                            handled = true;
                                        }
                                        ClipboardEntry::ExternalPaths(paths) => {
                                            for p in paths.paths() {
                                                this.input_attachments.push(
                                                    external_path_attachment(&p.to_string_lossy()),
                                                );
                                            }
                                            handled = true;
                                        }
                                        _ => {}
                                    }
                                }
                                if handled {
                                    cx.notify();
                                } else {
                                    cx.propagate();
                                }
                            }))
                            // 输入区最小高度（宽松命中区域），随 auto_grow 增高
                            .child(Input::new(&self.input_state).min_h(px(96.)))
                            .can_drop(|dragged, _window, _cx| dragged.is::<ExternalPaths>())
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    for p in paths.paths() {
                                        this.input_attachments.push(external_path_attachment(
                                            &p.display().to_string(),
                                        ));
                                    }
                                    cx.notify();
                                },
                            )),
                    )
                    .child(
                        v_flex()
                            .debug_selector(|| "input-align-btn-col".into())
                            .gap_2()
                            .child(Button::new("send").primary().label("发送").on_click(
                                cx.listener(|this, _ev, window, cx| {
                                    this.send_prompt(window, cx);
                                }),
                            ))
                            // 常驻取消：不随忙闲出现/消失（此前条件渲染让「想取消时
                            // 找不到按钮」）；空闲时点击为无害操作——会话侧静默吞掉
                            // ACP 的无可取消错误，工作流侧走既有注入取消指令机制
                            .child(
                                Button::new("cancel-work")
                                    .small()
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(gpui::transparent_black())
                                            .foreground(cx.theme().danger)
                                            .hover(cx.theme().danger.opacity(0.12))
                                            .active(cx.theme().danger.opacity(0.2)),
                                    )
                                    .icon(IconName::Close)
                                    .label("取消")
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.cancel_work(window, cx);
                                    })),
                            ),
                    )
                    .when(self.input_attachments.len() > 1, |row| {
                        row.child(
                            Button::new("clear-attachments")
                                .small()
                                .ghost()
                                .label("清空附件")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.input_attachments.clear();
                                    cx.notify();
                                })),
                        )
                    })
                    // 斜杠命令上拉框（docs/PRD.md「会话交互视图」）
                    .children(self.render_slash_menu(cx)),
            )
            // 会话选项（docs/PRD.md「会话交互视图」：位于输入框下方）
            .children(self.render_config_options_row(cx))
    }

    /// 会话选项行：select 类用下拉按钮、boolean 类用开关（docs/PRD.md
    /// 「会话交互视图」：根据选项类型使用下拉框、开关等组件）。选项数据来自
    /// `session.config_options`，以 Agent 侧数据为权威。
    ///
    /// 菜单项/开关的回调运行在窗口事件分发栈内（`&mut App` 上下文），此时
    /// `weak.update_in` 会因窗口已在 update stack 上而静默失败——必须用
    /// `entity.update`（同会话右键菜单的可用模式），实体更新内再异步发起请求。
    fn render_config_options_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let opts = self.config_options.as_ref()?;
        let (machine, session_id) = self.open_session_target()?;
        if opts.options.is_empty() {
            return None;
        }
        let app = cx.entity();
        let muted = cx.theme().muted_foreground;
        let mut row = h_flex().flex_wrap().gap_x_3().gap_y_1().items_center();
        for opt in &opts.options {
            let opt_id = opt.id.clone();
            match &opt.kind {
                SessionConfigKind::Select {
                    current_value,
                    options,
                } => {
                    let current_label = options
                        .iter()
                        .find(|o| o.value == *current_value)
                        .map(|o| o.name.clone())
                        .unwrap_or_else(|| current_value.clone());
                    // 闭包要求 'static：把候选克隆为值对
                    let entries: Vec<(String, String)> = options
                        .iter()
                        .map(|o| (o.value.clone(), o.name.clone()))
                        .collect();
                    let current_value = current_value.clone();
                    let sid = session_id.clone();
                    let oid = opt_id.clone();
                    let app = app.clone();
                    let dbg_id = opt_id.clone();
                    row = row.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .debug_selector(move || format!("cfg-row-{machine}-{dbg_id}"))
                            .child(Label::new(opt.name.clone()).text_sm().text_color(muted))
                            .child(
                                Button::new(SharedString::from(format!(
                                    "cfg-select-{machine}-{opt_id}"
                                )))
                                .small()
                                .outline()
                                .label(current_label)
                                .dropdown_menu(
                                    move |menu, _window, _cx| {
                                        let mut menu = menu;
                                        for (value, name) in &entries {
                                            let checked = value == &current_value;
                                            let app = app.clone();
                                            let sid = sid.clone();
                                            let oid = oid.clone();
                                            let value = value.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(name.clone())
                                                    .checked(checked)
                                                    .on_click(move |_ev, _window, cx| {
                                                        app.update(cx, |this, cx| {
                                                            this.set_session_config_option(
                                                                cx,
                                                                machine,
                                                                sid.clone(),
                                                                oid.clone(),
                                                                SessionConfigOptionValue::ValueId {
                                                                    value: value.clone(),
                                                                },
                                                            );
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    },
                                ),
                            ),
                    );
                }
                SessionConfigKind::Boolean { current_value } => {
                    let sid = session_id.clone();
                    let oid = opt_id.clone();
                    let checked = *current_value;
                    let app = app.clone();
                    row = row.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(Label::new(opt.name.clone()).text_sm().text_color(muted))
                            .child(
                                Switch::new(SharedString::from(format!(
                                    "cfg-switch-{machine}-{opt_id}"
                                )))
                                .small()
                                .checked(checked)
                                .on_click(
                                    move |_, _window, cx| {
                                        app.update(cx, |this, cx| {
                                            this.set_session_config_option(
                                                cx,
                                                machine,
                                                sid.clone(),
                                                oid.clone(),
                                                SessionConfigOptionValue::Boolean {
                                                    value: !checked,
                                                },
                                            );
                                        });
                                    },
                                ),
                            ),
                    );
                }
            }
        }
        if opts.loading {
            row = row.child(Spinner::new().xsmall());
        }
        Some(row.into_any_element())
    }

    pub(crate) fn render_quick_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commands = self.store.list_quick_commands();
        // ghost：输入区上方的快捷入口应视觉后退，不与发送按钮争夺注意力
        let mut row = h_flex().flex_wrap().gap_1();
        for c in commands {
            let name = c.name.clone();
            let cmd = c.clone();
            row = row.child(
                Button::new(format!("qc-{}", c.name))
                    .small()
                    .ghost()
                    .label(name)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.quick_command(window, cx, &cmd);
                    })),
            );
        }
        row
    }

    pub(crate) fn render_center(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.selected.is_none() {
            return self.render_new_session_view(window, cx);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.render_session_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_dialog(window, cx)),
            )
            .into_any()
    }

    pub(crate) fn render_new_session_view(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mode = self.new_session_mode;
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let mut card = v_flex()
            .w_full()
            .max_w(px(640.)) // 新建会话卡片最大宽度（固定容器尺寸）
            .gap_3()
            .p_4()
            .bg(popover)
            .rounded_lg()
            .border_1()
            .border_color(border)
            .shadow_lg()
            .child(
                Label::new("新会话")
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(foreground),
            )
            .child(
                // 分段单选：模式切换用库 ButtonGroup（选中态/圆角拼接由其负责）
                ButtonGroup::new("ns-mode")
                    .small()
                    .w_full()
                    .child(
                        Button::new("ns-mode-direct")
                            .flex_1()
                            .label("普通")
                            .selected(mode == NewSessionMode::Direct),
                    )
                    .child(
                        Button::new("ns-mode-tpl")
                            .flex_1()
                            .label("工作流")
                            .selected(mode == NewSessionMode::Workflow),
                    )
                    .on_click(cx.listener(|this, clicks: &Vec<usize>, _window, cx| {
                        if clicks.contains(&0) {
                            this.new_session_mode = NewSessionMode::Direct;
                        } else if clicks.contains(&1) {
                            this.new_session_mode = NewSessionMode::Workflow;
                        }
                        cx.notify();
                    })),
            );
        match mode {
            NewSessionMode::Direct => {
                if self.machines.is_empty() {
                    // 无机器时提示并引导到设置。
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .child(
                                Alert::warning(
                                    "ns-no-machines-alert",
                                    "请先在设置 → 机器管理中注册一台 amux server。",
                                )
                                .title("尚未注册机器"),
                            )
                            .child(
                                Button::new("ns-goto-machine-settings")
                                    .small()
                                    .primary()
                                    .label("去注册机器")
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.open_settings(
                                            window,
                                            cx,
                                            Some(SettingsCategory::Machines),
                                        );
                                    })),
                            ),
                    );
                } else {
                    card = card
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_6()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("机器")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_machine_selector(cx)),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("Agent")
                                                .text_sm()
                                                .text_color(muted_foreground),
                                        )
                                        .child(self.render_harness_selector(cx)),
                                ),
                        )
                        .child(self.render_workspace_picker(cx))
                        // worktree 开关（docs/PRD.md）：勾选后 agent 在独立工作树中
                        // 工作，主仓库工作区不受影响；路径由 server 统一分配
                        .child(
                            Checkbox::new("ns-worktree-toggle")
                                .label("使用 worktree")
                                .checked(self.new_session_worktree)
                                .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                                    this.new_session_worktree = *checked;
                                    cx.notify();
                                })),
                        )
                        .when_some(self.new_session_error.clone(), |view, error| {
                            view.child(Alert::error("ns-create-error", error))
                        })
                        .child(
                            Button::new("ns-create")
                                .primary()
                                .mt_2()
                                .label("创建会话")
                                .on_click(cx.listener(|this, _ev, window, cx| {
                                    this.create_session_only(window, cx);
                                })),
                        );
                }
            }
            NewSessionMode::Workflow => {
                if !self.store.orchestrator().is_configured() {
                    card = card.child(
                        v_flex()
                            .gap_2()
                            .child(
                                Alert::warning(
                                    "ns-no-orch-alert",
                                    "请先配置 API 格式、Base URL、API Key 和模型名称。",
                                )
                                .title("编排智能体尚未配置"),
                            )
                            .child(
                                Button::new("ns-goto-orch-settings")
                                    .small()
                                    .primary()
                                    .label("去配置编排 agent")
                                    .on_click(cx.listener(|this, _ev, window, cx| {
                                        this.open_settings(
                                            window,
                                            cx,
                                            Some(SettingsCategory::Orchestrator),
                                        );
                                    })),
                            ),
                    );
                } else {
                    card = card.child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("工作流计划")
                                    .text_sm()
                                    .text_color(muted_foreground),
                            )
                            .child(self.render_template_selector(cx))
                            .child(
                                Label::new("创建后编排 agent 将按计划推进")
                                    .text_sm()
                                    .text_color(muted_foreground),
                            ),
                    );
                    if let Some(err) = &self.workflow_error {
                        card = card.child(Alert::error("ns-wf-error", err.clone()));
                    }
                    card = card.child(
                        Button::new("ns-create-workflow")
                            .primary()
                            .label("创建工作流会话")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.create_workflow(window, cx);
                            })),
                    );
                }
            }
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(card)
            .into_any()
    }

    pub(crate) fn render_workspace_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machine = self.new_session_machine.unwrap_or(0);
        let Some(m) = self.machine(machine) else {
            return v_flex().into_any();
        };
        let dirs = self.store.recent_workspaces_for_machine(&m.config.name);
        // 有最近目录时输入框本身即 Popover 触发器（Input 实现 Selectable，
        // 开启态由组件在触发器上呈现选中样式），点击即弹出最近目录；
        // 外点/Esc 关闭、选项回填后经 on_open_change 回写关闭
        let app = cx.entity();
        let open = self.show_workspace_dropdown;
        let store = self.store.clone();
        let machine_name = m.config.name.clone();
        v_flex()
            .gap_1()
            .child(
                Label::new("工作目录")
                    .text_sm()
                    .text_color(cx.theme().muted_foreground),
            )
            .when(dirs.is_empty(), |view| {
                view.child(Input::new(&self.session_cwd_input))
            })
            .when(!dirs.is_empty(), |view| {
                view.child(
                    Popover::new("workspace-picker")
                        .anchor(Anchor::BottomLeft)
                        .open(open)
                        .on_open_change({
                            let app = app.clone();
                            move |is_open, _window, cx| {
                                app.update(cx, |this, cx| {
                                    this.show_workspace_dropdown = *is_open;
                                    cx.notify();
                                });
                            }
                        })
                        .trigger(
                            Input::new(&self.session_cwd_input).suffix(
                                Icon::new(IconName::ChevronDown)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                        )
                        .content(move |_, _window, cx| {
                            // 受控开启：选项点击后经 AmuxApp 关闭（on_open_change 回写）。
                            // 选项为手搓行而非 Button——库 Button 内容层硬编码居中，
                            // 全宽下拉项无法左对齐（同目录树行）
                            let dirs = store.recent_workspaces_for_machine(&machine_name);
                            let hover_bg = cx.theme().accent;
                            v_flex()
                                .id("workspace-picker-list")
                                .w(rems(26.))
                                .max_h(rems(16.))
                                .overflow_y_scroll()
                                .gap_0p5()
                                .children(dirs.into_iter().map(|dir| {
                                    let app = app.clone();
                                    let dir_val = dir.clone();
                                    div()
                                        .id(format!("ns-workspace-option-{dir}"))
                                        .w_full()
                                        .h_6()
                                        .flex()
                                        .items_center()
                                        .px_2()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .hover(move |d| d.bg(hover_bg))
                                        .on_click(move |_, window, cx| {
                                            app.update(cx, |this, cx| {
                                                this.session_cwd_input.update(cx, |s, cx| {
                                                    s.set_value(&dir_val, window, cx)
                                                });
                                                this.show_workspace_dropdown = false;
                                                this.new_session_error = None;
                                                cx.notify();
                                            });
                                        })
                                        .child(
                                            // 展示完整路径；溢出时头部截断——路径尾部
                                            // （最具体的目录段）始终可见
                                            Label::new(dir.clone())
                                                .text_sm()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis_start(),
                                        )
                                }))
                        }),
                )
            })
            .into_any()
    }

    pub(crate) fn render_machine_selector(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected_machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        if self.machines.is_empty() {
            return Label::new("（请先在设置中添加机器）").into_any_element();
        }
        // ButtonGroup 单选组：子按钮 on_click 由组统一接管（按下索引回传）
        ButtonGroup::new("ns-machine-group")
            .small()
            .flex_wrap()
            .children(self.machines.iter().enumerate().map(|(i, m)| {
                Button::new(format!("ns-machine-{i}"))
                    .label(m.config.name.clone())
                    .selected(selected_machine == Some(i))
            }))
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, _window, cx| {
                let Some(&ix) = clicks.first() else {
                    return;
                };
                this.new_session_machine = Some(ix);
                this.new_session_agent = None;
                this.new_session_error = None;
                cx.notify();
            }))
            .into_any_element()
    }

    pub(crate) fn render_session_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, status) = if let Some((machine, id)) = self.open_session_target() {
            let Some(machine_view) = self.machine(machine) else {
                return h_flex().into_any();
            };
            let Some(session) = machine_view.sessions.iter().find(|s| s.id == id) else {
                return h_flex().into_any();
            };
            let available = machine_view.status.online()
                && machine_view
                    .agents
                    .iter()
                    .any(|agent| agent.name == session.agent && agent.available);
            (
                format!("{}@{}", session.agent, machine_view.config.name),
                if available { "可用" } else { "不可用" },
            )
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            // header 仅标注 agent 名称与可用状态（docs/PRD.md）：工作状态
            // 由会话列表转圈与对话区实时活动表达
            let Some(_workflow) = self.workflow(id) else {
                return h_flex().into_any();
            };
            (
                "编排智能体".to_string(),
                if self.store.orchestrator().is_configured() {
                    "可用"
                } else {
                    "不可用"
                },
            )
        } else {
            return h_flex().into_any();
        };
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Label::new(label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground),
            )
            .child(if status == "可用" {
                Tag::success()
                    .small()
                    .rounded_full()
                    .child(Label::new(status).text_xs())
                    .into_any_element()
            } else {
                Tag::danger()
                    .small()
                    .rounded_full()
                    .child(Label::new(status).text_xs())
                    .into_any_element()
            })
            .into_any()
    }

    pub(crate) fn render_activities_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut activities_has_more = false;
        if let Some((machine, id)) = self.open_session_target() {
            let view = self.machine(machine).and_then(|m| m.views.get(&id));
            let activities = view.map(|v| v.activities.clone()).unwrap_or_default();
            activities_has_more = view.map(|v| v.activities_has_more).unwrap_or(false);
            rows = activities
                .iter()
                .map(|a| self.activity_row("act", a, cx))
                .collect();
        } else if let Some(Selected::Workflow { id }) = &self.selected {
            if let Some(wf) = self.workflow(id) {
                let sg = wf.snapshot();
                rows = sg
                    .activities
                    .iter()
                    .map(|a| self.activity_row("wf-act", a, cx))
                    .collect();
            }
        }
        let total = rows.len();
        let start = if activities_has_more {
            0
        } else {
            total.saturating_sub(self.activities_limit)
        };
        let has_more = activities_has_more || start > 0;
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        if has_more {
            children.push(
                Button::new("load-more-activities")
                    .small()
                    .ghost()
                    .label("加载更早活动")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.load_more_activities(window, cx);
                    }))
                    .into_any_element(),
            );
        }
        children.extend(rows.into_iter().skip(start));
        v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().popover)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .child(
                        Label::new("会话活动历史")
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-panel-activities")
                            .small()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("关闭面板")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.set_panel(window, cx, None);
                            })),
                    ),
            )
            .child(
                // v_flex 让卡片间 gap 生效（原为普通 div，gap 无效导致卡片贴叠）
                div()
                    .id("activities-panel")
                    .flex_1()
                    .v_flex()
                    .gap_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.activities_scroll)
                    .children(children),
            )
            .into_any()
    }

    pub(crate) fn render_harness_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machine = self
            .new_session_machine
            .filter(|i| *i < self.machines.len())
            .or_else(|| (!self.machines.is_empty()).then_some(0));
        let mut row = h_flex().gap_1().flex_wrap();
        let Some(mi) = machine else {
            return row.child(Label::new("（无机器）"));
        };
        let agents = self
            .machine(mi)
            .map(|m| m.agents.clone())
            .unwrap_or_default();
        if agents.is_empty() {
            return row.child(Label::new("（未发现 agent）"));
        }
        for a in &agents {
            let name = a.name.clone();
            let name_click = name.clone();
            let selected = self.new_session_agent.as_deref() == Some(name.as_str());
            let mut btn = Button::new(format!("ns-agent-{name}"))
                .small()
                .label(name)
                .when(selected, |b| b.primary());
            if !a.available {
                btn = btn.disabled(true);
            }
            let available = a.available;
            row = row.child(btn.on_click(cx.listener(move |this, _ev, _window, cx| {
                if available {
                    this.new_session_agent = Some(name_click.clone());
                    cx.notify();
                }
            })));
        }
        row
    }

    fn request_session_data<P, R, F>(
        &self,
        cx: &mut Context<Self>,
        machine: usize,
        method: &'static str,
        params: P,
        apply: F,
    ) where
        P: serde::Serialize + 'static,
        R: serde::de::DeserializeOwned + 'static,
        F: FnOnce(&mut Self, Result<R, crate::ws::RpcError>) + 'static,
    {
        let Some(machine_view) = self.machines.get(machine) else {
            return;
        };
        let client = machine_view.client.clone();
        let machine_name = machine_view.config.name.clone();
        let generation = machine_view.connection_generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let result = client.request::<_, R>(method, Some(params)).await;
            let _ = this.update_in(cx, |this, _w, cx| {
                let current_connection = this.machines.get(machine).is_some_and(|m| {
                    m.config.name == machine_name && m.connection_generation == generation
                });
                if current_connection {
                    apply(this, result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 查询选中会话的会话选项（`session.config_options`；docs/DESIGN.md
    /// 「普通会话选项」：存储在 Server 内存，以 Agent 侧数据为权威）。
    /// 不依赖 window 上下文：供浮层菜单回调等窗口 update stack 内的场景调用。
    pub(crate) fn refresh_config_options(
        &mut self,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        if self.machines.get(machine).is_none() {
            return;
        }
        // 同一会话刷新时保留既有选项展示，避免轮询期间整行闪烁
        let options = match &self.config_options {
            Some(cur) if cur.machine == machine && cur.session_id == session_id => {
                cur.options.clone()
            }
            _ => Vec::new(),
        };
        self.config_options = Some(SelectedConfigOptions {
            machine,
            session_id: session_id.clone(),
            loading: true,
            options,
        });
        cx.notify();
        self.request_session_data(
            cx,
            machine,
            protocol::method::SESSION_CONFIG_OPTIONS,
            SessionIdParams {
                session_id: session_id.clone(),
            },
            move |this, result: Result<SessionConfigOptionsResult, crate::ws::RpcError>| {
                if let Some(cur) = &mut this.config_options {
                    // 响应到达时选中会话已切换则丢弃陈旧结果
                    if cur.machine == machine && cur.session_id == session_id {
                        cur.loading = false;
                        if let Ok(result) = result {
                            cur.options = result.options;
                        }
                    }
                }
            },
        );
    }

    /// 查询选中会话的斜杠命令（`session.slash_commands`；docs/DESIGN.md
    /// 「普通会话斜杠命令」：存储在 Server 内存，以 Agent 侧数据为权威）。
    /// 尚无 agent 侧会话或 agent 未下发时为空；agent 侧会话在首条 prompt 时
    /// 才惰性创建，命令集合由 turn 结束后的刷新补齐。
    pub(crate) fn refresh_slash_commands(
        &mut self,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
    ) {
        if self.machines.get(machine).is_none() {
            return;
        }
        self.slash_commands = Some(SelectedSlashCommands {
            machine,
            session_id: session_id.clone(),
            commands: Vec::new(),
        });
        self.request_session_data(
            cx,
            machine,
            protocol::method::SESSION_SLASH_COMMANDS,
            SessionIdParams {
                session_id: session_id.clone(),
            },
            move |this, result: Result<SessionSlashCommandsResult, crate::ws::RpcError>| {
                if let Some(cur) = &mut this.slash_commands {
                    // 响应到达时选中会话已切换则丢弃陈旧结果
                    if cur.machine == machine && cur.session_id == session_id {
                        if let Ok(result) = result {
                            cur.commands = result.commands;
                        }
                    }
                }
            },
        );
    }

    /// 斜杠命令上拉框（docs/PRD.md「会话交互视图」：输入 `/` 时根据前缀匹配
    /// 斜杠命令，弹出上拉框供用户选择）。可见性由当前输入文本派生：仅当选中
    /// 普通会话、命令集合非空且输入正处于命令名输入中（`/` 开头、无空白）时
    /// 展示前缀匹配项；点击项回填 `/name ` 后随前缀消失自动收起。
    pub fn render_slash_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let selected = self.slash_commands.as_ref()?;
        let (machine, id) = self.open_session_target()?;
        if selected.machine != machine || selected.session_id != id {
            return None;
        }
        let text = self.input_state.read(cx).value().to_string();
        let prefix = slash_command_prefix(&text)?;
        let matched = filter_slash_commands(&selected.commands, prefix);
        if matched.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let hover_bg = cx.theme().accent;
        let rows = matched
            .into_iter()
            .map(|c| {
                let name = c.name.clone();
                let fill = format!("/{} ", c.name);
                let hint = c.hint.clone();
                div()
                    .id(format!("slash-cmd-{}", c.name))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |d| d.bg(hover_bg))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        // 回填完整命令并留一个空格，供用户继续输入参数
                        this.input_state
                            .update(cx, |s, cx| s.set_value(&fill, window, cx));
                        cx.notify();
                    }))
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
                                Label::new(hint.unwrap_or_else(|| c.description.clone()))
                                    .text_sm()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(muted),
                            ),
                    )
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .id("slash-menu")
                .absolute()
                // 锚在输入行容器上沿之上，向上展开（上拉）
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
                .shadow_lg()
                .children(rows)
                .into_any(),
        )
    }

    /// 设置会话配置选项（`session.configure` 携带 config 设置；Server 向 ACP
    /// Server 发送 `session/set_config_option`）。成功后重新查询选项，
    /// 以 Agent 侧返回的全量集合刷新。
    /// 不依赖 window 上下文：供浮层菜单回调等窗口 update stack 内的场景调用。
    pub(crate) fn set_session_config_option(
        &mut self,
        cx: &mut Context<Self>,
        machine: usize,
        session_id: String,
        config_id: String,
        value: SessionConfigOptionValue,
    ) {
        let Some(m) = self.machines.get(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let params = SessionConfigureParams {
            session_id: session_id.clone(),
            title: None,
            config: Some(SessionConfigSetting {
                config_id: config_id.clone(),
                value,
            }),
        };
        log::info!(
            "发送会话选项设置：machine={} session={session_id} config_id={config_id} params={:?}",
            m.config.name,
            params.config
        );
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let res = client
                .request::<_, OpResult>(protocol::method::SESSION_CONFIGURE, Some(params))
                .await;
            let _ = this.update_in(cx, |this, w, cx| {
                let current_connection = this.machines.get(machine).is_some_and(|m| {
                    m.config.name == machine_name && m.connection_generation == generation
                });
                if !current_connection || !this.is_selected_session(machine, &session_id) {
                    return;
                }
                match res {
                    Ok(_) => {
                        log::info!(
                        "会话选项设置成功，刷新选项：session={session_id} config_id={config_id}"
                    );
                        this.refresh_config_options(cx, machine, session_id);
                    }
                    Err(error) => {
                        log::error!("会话选项设置失败：{error}");
                        w.push_notification(
                            UiNotification::error(format!("会话选项设置失败：{error}"))
                                .title("会话选项"),
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{swap_draft, Draft};
    use std::collections::HashMap;

    fn session_key(machine: &str, id: &str) -> DraftKey {
        DraftKey::Session {
            machine: machine.into(),
            id: id.into(),
        }
    }

    fn draft(text: &str) -> Draft {
        Draft {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    #[::core::prelude::v1::test]
    fn draft_isolated_per_session() {
        let mut drafts = HashMap::new();

        // 在 A 输入后切到 B：A 的草稿留存，B 拿到空草稿
        swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-a")),
            draft("a"),
            Some(session_key("m1", "s-b")),
        );
        assert_eq!(drafts[&session_key("m1", "s-a")].text, "a");

        // 在 B 输入后切回 A：两边各自看到自己的内容
        let for_a = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-b")),
            draft("b"),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(drafts[&session_key("m1", "s-b")].text, "b");
        assert_eq!(for_a.text, "a");
        assert!(!drafts.contains_key(&session_key("m1", "s-a")));
    }

    #[::core::prelude::v1::test]
    fn empty_input_on_leaving_clears_draft() {
        // 曾在 A 留过草稿，之后清空输入再离开，不应残留旧草稿
        let mut drafts = HashMap::new();
        swap_draft(&mut drafts, None, draft(""), Some(session_key("m1", "s-a")));
        swap_draft(&mut drafts, None, draft(""), Some(session_key("m1", "s-b")));

        // 回到 A 带出旧草稿
        let for_a = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-b")),
            draft(""),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(for_a.text, "");

        // 带着空输入再次离开 A
        let for_b = swap_draft(
            &mut drafts,
            Some(session_key("m1", "s-a")),
            draft(""),
            Some(session_key("m1", "s-b")),
        );
        assert_eq!(for_b.text, "");
        assert!(drafts.is_empty());
    }

    #[::core::prelude::v1::test]
    fn unowned_input_is_dropped_without_selection() {
        // 未选中会话时输入区无主，切换不应把内容挂到新会话头上
        let mut drafts = HashMap::new();
        let for_a = swap_draft(
            &mut drafts,
            None,
            draft("x"),
            Some(session_key("m1", "s-a")),
        );
        assert_eq!(for_a.text, "");
        assert!(drafts.is_empty());
    }
}
