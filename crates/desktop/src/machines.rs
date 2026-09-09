use gpui::*;

use gpui_component::button::ButtonVariant;

use protocol::{
    AgentListResult, AgentParams, ContentBlock, SessionNewParams, SessionPromptParams,
    SessionResult,
};

use crate::config::{machine_ws_url, SkillEntry};
use crate::machine::{next_connection_generation, MachineView};
use crate::workflow::WorkflowEngine;
use crate::ws::WsClient;

use crate::app::{AmuxApp, DraftKey, Selected, SkillAction};

impl AmuxApp {
    /// 按机器名（稳定域身份）解析当前下标。
    pub(crate) fn machine_idx_by_name(&self, name: &str) -> Option<usize> {
        self.machines.iter().position(|m| m.config.name == name)
    }

    pub(crate) fn fetch_agents(
        &self,
        machine_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(m) = self.machine_by_name(machine_name) else {
            return;
        };
        let machine_name = machine_name.to_string();
        let generation = m.connection_generation;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| match client
            .request::<_, AgentListResult>(protocol::method::AGENT_LIST, None::<serde_json::Value>)
            .await
        {
            Ok(result) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(m) = this.machine_mut_by_name(&machine_name) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    m.agents = result.agents;
                    // agent 列表异步到达，晚于 auth_ok 时的 hub 快照；工作流
                    // 编排的 list_agents 读 hub，必须在此重同步
                    this.sync_machine_hub();
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(m) = this.machine_mut_by_name(&machine_name) else {
                        return;
                    };
                    if m.connection_generation != generation {
                        return;
                    }
                    // 连接状态机之外的操作级提示
                    m.notice = Some(format!("agent 列表获取失败：{e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 通过普通会话执行技能操作，保留完整会话供用户继续干预。
    pub(crate) fn manage_skill_on_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
        agent: String,
        skill: SkillEntry,
        action: SkillAction,
    ) {
        let Some(m) = self.machine_by_name(machine_name) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = machine_name.to_string();
        let generation = m.connection_generation;
        // 技能操作的临时会话固定在系统临时目录中执行。
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let operation_prompt = action.prompt(&skill);
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = async {
                let session = client
                    .request::<_, SessionResult>(
                        protocol::method::SESSION_NEW,
                        Some(SessionNewParams {
                            agent: agent.clone(),
                            cwd: cwd.clone(),
                            // 技能操作是临时会话：在系统临时目录执行
                            use_worktree: false,
                        }),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let session_id = session.session.id;
                if session_id.is_empty() {
                    return Err("创建技能操作会话响应缺少 session.id".to_string());
                }
                let input = SessionPromptParams {
                    session_id: session_id.clone(),
                    input: vec![ContentBlock::Text {
                        text: operation_prompt,
                    }],
                };
                client
                    .request_ok(protocol::method::SESSION_PROMPT, Some(input))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<String, String>(session_id)
            }
            .await;

            let _ = this.update_in(cx, |this, window, cx| {
                if !this.is_current_machine_connection(&machine_name, generation) {
                    return;
                }
                match result {
                    Ok(session_id) => {
                        this.refresh_sessions(&machine_name, window, cx);
                        this.open_session(window, cx, &machine_name, session_id);
                    }
                    Err(error) => {
                        if let Some(m) = this.machine_mut_by_name(&machine_name) {
                            m.notice = Some(format!("技能{}失败：{error}", action.label()));
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn run_agent_action<P, F>(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
        method: &'static str,
        params: Option<P>,
        error_message: F,
    ) where
        P: serde::Serialize + 'static,
        F: FnOnce(String) -> String + 'static,
    {
        let Some(m) = self.machine_by_name(machine_name) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = machine_name.to_string();
        let generation = m.connection_generation;
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = client.request_ok(method, params).await;
            let notice = result.err().map(|error| error_message(error.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.is_current_machine_connection(&machine_name, generation) {
                    return;
                }
                if let Some(notice) = notice {
                    if let Some(m) = this.machine_mut_by_name(&machine_name) {
                        m.notice = Some(notice);
                    }
                }
                this.fetch_agents(&machine_name, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn restart_agent(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
        agent: String,
    ) {
        let notice_agent = agent.clone();
        self.run_agent_action(
            window,
            cx,
            machine_name,
            protocol::method::AGENT_RESTART,
            Some(AgentParams { agent }),
            move |error| format!("agent「{}」重启失败：{}", notice_agent, error),
        );
    }

    /// 重新发现机器上的 agents（`agent.rediscover`）：server 重扫本机并拉起
    /// 未运行的 agent，成功后刷新 agent 列表。
    pub(crate) fn rediscover_agents(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
    ) {
        self.run_agent_action(
            window,
            cx,
            machine_name,
            protocol::method::AGENT_REDISCOVER,
            None::<()>,
            |error| format!("重新发现 agents 失败：{}", error),
        );
    }

    pub(crate) fn confirm_rediscover_agents(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
    ) {
        let machine_name = machine_name.to_string();
        self.confirm_dialog(
            window,
            cx,
            "重新发现",
            ButtonVariant::Primary,
            "重新发现 agents",
            "确定重新扫描本机 agents 吗？".to_string(),
            move |this, window, cx| {
                this.rediscover_agents(window, cx, &machine_name);
            },
        );
    }

    pub(crate) fn confirm_restart_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine_name: &str,
        agent: String,
    ) {
        let machine_name = machine_name.to_string();
        self.confirm_dialog(
            window,
            cx,
            "重启",
            ButtonVariant::Primary,
            "重启 agent",
            format!("确定重启 agent「{agent}」吗？"),
            move |this, window, cx| {
                let agent = agent.clone();
                this.restart_agent(window, cx, &machine_name, agent);
            },
        );
    }

    pub(crate) fn confirm_reconnect_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "重连",
            ButtonVariant::Primary,
            "重连机器",
            format!("确定重连机器「{name}」吗？"),
            move |this, window, cx| {
                this.reconnect_machine(window, cx, &name);
            },
        );
    }

    /// 清理绑定当前 WS 连接的本地资源和请求状态。
    /// 连接断开后旧回调必须失效，不能继续写回 workspace/diff 缓存。
    pub(crate) fn clear_connection_state(
        &mut self,
        idx: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(machine) = self.machines.get_mut(idx) else {
            return;
        };
        let machine_name = machine.config.name.clone();
        machine.connection_generation = next_connection_generation();
        machine.terminals.clear();
        machine.active_terminal = None;
        let rem_size = window.rem_size();
        machine.diff.update(cx, |st, _| {
            st.request_id = st.request_id.saturating_add(1);
            st.files.clear();
            st.not_repo = false;
            st.selection.clear();
            st.loading = false;
            st.error = None;
            st.rebuild_rows(rem_size);
        });
        machine.workspace_directories.clear();
        machine.workspace_expanded.clear();
        machine.workspace_loading.clear();
        machine.fs_list_request_id = machine.fs_list_request_id.saturating_add(1);
        machine.fs_read_request_id = machine.fs_read_request_id.saturating_add(1);
        machine.workspace_file = None;
        machine.workspace_content.clear();
        machine.workspace_error = None;
        machine.fs_read_loading = false;
        machine.fs_read_has_more = false;
        machine.fs_read_next_offset = 0;
        if self
            .config_options
            .as_ref()
            .is_some_and(|options| options.machine == machine_name)
        {
            self.config_options = None;
        }
        if self
            .slash_commands
            .as_ref()
            .is_some_and(|commands| commands.machine == machine_name)
        {
            self.slash_commands = None;
        }
    }

    /// 重连机器：重建其 WS 连接视图（按稳定机器名定位，删机重排不影响身份）。
    pub(crate) fn reconnect_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: &str,
    ) {
        let Some(idx) = self.machine_idx_by_name(name) else {
            return;
        };
        let old_client = self.machines[idx].client.clone();
        let cfg = self.machines[idx].config.clone();
        let client = WsClient::connect_with_token(machine_ws_url(&cfg), cfg.token.clone());
        old_client.close();
        self.machines[idx].status = crate::machine::MachineStatus::Connecting;
        // 终端、工作目录和 diff 都绑定旧 WS 连接；旧连接的通知会因 generation
        // 不匹配被丢弃，因此必须在切换连接时主动释放本地连接态。
        self.clear_connection_state(idx, window, cx);
        let generation = self.machines[idx].connection_generation;
        self.machines[idx].client = client.clone();
        self.sync_machine_hub();
        let t = self.spawn_machine_tasks(window, cx, name.to_string(), client, generation);
        self._tasks.push(t);
        // 不在此处立即拉取：连接任务在 auth 握手完成前会拒绝一切请求，
        // 提前发的 agent.list 必然失败并把 notice 染成「agent 列表获取失败」。
        // 初始数据由 on_notify 的 auth_ok 分支统一拉取（同启动流程）。
        cx.notify();
    }

    pub(crate) fn add_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
        url: String,
        token: String,
    ) -> bool {
        let validation_error = if name.trim().is_empty() {
            Some("请输入机器名称。")
        } else if !url.trim().starts_with("ws://") {
            Some("连接地址必须以 ws:// 开头。")
        } else if token.trim().is_empty() {
            Some("请输入连接 Token。")
        } else {
            None
        };
        if let Some(error) = validation_error {
            self.settings.machine_form_error = Some(error.into());
            cx.notify();
            return false;
        }
        let machine = self
            .store
            .add_machine(name.trim(), url.trim(), token.trim());
        let view = MachineView::new(machine, cx);
        // 初始 Connecting：状态由 ws 认证通知驱动，不伪造「在线」
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        let generation = self.machines[idx].connection_generation;
        let machine_name = self.machines[idx].config.name.clone();
        let t = self.spawn_machine_tasks(window, cx, machine_name, client, generation);
        self._tasks.push(t);
        self.sync_machine_hub();
        // 不在此处立即拉取：连接任务在 auth 握手完成前会拒绝一切请求，
        // 提前发的 agent.list 必然失败并把 notice 染成「agent 列表获取失败」。
        // 初始数据由 on_notify 的 auth_ok 分支统一拉取（同重连流程）。
        self.settings.machine_form_error = None;
        cx.notify();
        true
    }

    /// 编辑机器连接信息：机器名不可编辑，仅更新地址/token 后按新连接重连。
    /// 校验失败返回 false 保持编辑对话框打开。
    pub(crate) fn update_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: &str,
        url: String,
        token: String,
    ) -> bool {
        let validation_error = if !url.trim().starts_with("ws://") {
            Some("连接地址必须以 ws:// 开头。")
        } else if token.trim().is_empty() {
            Some("请输入连接 Token。")
        } else {
            None
        };
        if let Some(error) = validation_error {
            self.settings.machine_form_error = Some(error.into());
            cx.notify();
            return false;
        }
        let Some(idx) = self.machine_idx_by_name(name) else {
            return false;
        };
        let url = url.trim().to_string();
        let token = token.trim().to_string();
        self.store.update_machine(name, &url, &token);
        self.machines[idx].config.url = url;
        self.machines[idx].config.token = token;
        self.settings.machine_form_error = None;
        self.reconnect_machine(window, cx, name);
        cx.notify();
        true
    }

    pub(crate) fn remove_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: &str,
    ) {
        let Some(idx) = self.machine_idx_by_name(name) else {
            return;
        };
        let linked_in_memory = self.workflows.iter().any(|workflow| {
            workflow
                .session
                .read()
                .linked_sessions
                .iter()
                .any(|linked| linked.machine_name == name)
        });
        let linked_in_storage =
            match WorkflowEngine::has_linked_sessions_on_machine(&self.data_dir, name) {
                Ok(linked) => linked,
                Err(error) => {
                    self.settings.machine_form_error =
                        Some(format!("无法确认机器是否有关联工作流，请稍后重试：{error}"));
                    cx.notify();
                    return;
                }
            };
        if linked_in_memory || linked_in_storage {
            self.settings.machine_form_error =
                Some("请先删除关联工作流会话，再移除该机器。".into());
            cx.notify();
            return;
        }
        let name = self.machines[idx].config.name.clone();
        // 选中态以机器名（稳定身份）定位：被移除机器上的选中会话直接取消选中，
        // 其余机器的选中会话不受机器列表变化影响。
        let next = match self.selected.clone() {
            Some(Selected::Session { machine, .. }) if machine == name => None,
            other => other,
        };
        self.set_selected(next, window, cx);
        self.store.remove_machine(&name);
        self.machines[idx].client.close();
        self.drafts.retain(|key, _| match key {
            DraftKey::Session { machine, .. } => machine != &name,
            DraftKey::Workflow { .. } => true,
        });
        self.machines.remove(idx);
        self.sync_machine_hub();
        cx.notify();
    }

    pub(crate) fn confirm_remove_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "确认移除",
            ButtonVariant::Danger,
            "移除机器",
            format!("确定移除机器「{name}」吗？其本地注册信息将被删除。"),
            move |this, window, cx| {
                this.remove_machine(window, cx, &name);
            },
        );
    }
}
