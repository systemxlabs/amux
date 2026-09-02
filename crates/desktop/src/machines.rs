use gpui::*;

use protocol::{
    AgentListResult, AgentParams, ContentBlock, SessionNewParams, SessionPromptParams,
    SessionResult,
};

use crate::config::{machine_ws_url, SkillEntry};
use crate::machine::{next_connection_generation, MachineView};
use crate::ws::WsClient;

use crate::app::{AmuxApp, DraftKey, Selected, SkillAction};

impl AmuxApp {
    /// 按机器名（稳定域身份）解析当前下标。
    pub(crate) fn machine_idx_by_name(&self, name: &str) -> Option<usize> {
        self.machines.iter().position(|m| m.config.name == name)
    }

    pub(crate) fn fetch_agents(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| match client
            .request::<_, AgentListResult>(protocol::method::AGENT_LIST, None::<serde_json::Value>)
            .await
        {
            Ok(result) => {
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
                    m.agents = result.agents;
                    // agent 列表异步到达，晚于 auth_ok 时的 hub 快照；工作流
                    // 编排的 list_agents 读 hub，必须在此重同步
                    this.sync_machine_hub();
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update_in(cx, |this, _w, cx| {
                    let Some(idx) = this.machine_idx_by_name(&machine_name) else {
                        return;
                    };
                    if let Some(m) = this.machines.get_mut(idx) {
                        if m.connection_generation != generation {
                            return;
                        }
                        // 连接状态机之外的操作级提示
                        m.notice = Some(format!("agent 列表获取失败：{e}"));
                    }
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
        let Some(machine) = self.machine_idx_by_name(machine_name) else {
            return;
        };
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
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
                if !this.is_current_machine_connection(machine, &machine_name, generation) {
                    return;
                }
                match result {
                    Ok(session_id) => {
                        this.refresh_sessions(machine, window, cx);
                        this.open_session(window, cx, machine, session_id);
                    }
                    Err(error) => {
                        if let Some(m) = this.machines.get_mut(machine) {
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
        machine: usize,
        method: &'static str,
        params: Option<P>,
        error_message: F,
    ) where
        P: serde::Serialize + 'static,
        F: FnOnce(String) -> String + 'static,
    {
        let Some(m) = self.machine(machine) else {
            return;
        };
        let client = m.client.clone();
        let machine_name = m.config.name.clone();
        let generation = m.connection_generation;
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = client.request_ok(method, params).await;
            let notice = result.err().map(|error| error_message(error.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.is_current_machine_connection(machine, &machine_name, generation) {
                    return;
                }
                if let Some(notice) = notice {
                    if let Some(m) = this.machines.get_mut(machine) {
                        m.notice = Some(notice);
                    }
                }
                this.fetch_agents(machine, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn restart_agent(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        let notice_agent = agent.clone();
        self.run_agent_action(
            window,
            cx,
            machine,
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
        machine: usize,
    ) {
        self.run_agent_action(
            window,
            cx,
            machine,
            protocol::method::AGENT_REDISCOVER,
            None::<()>,
            |error| format!("重新发现 agents 失败：{}", error),
        );
    }

    pub(crate) fn confirm_rediscover_agents(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "重新发现",
            false,
            "重新发现 agents",
            "确定重新扫描本机 agents 吗？".to_string(),
            move |this, window, cx| {
                this.rediscover_agents(window, cx, machine);
            },
        );
    }

    pub(crate) fn confirm_restart_agent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        machine: usize,
        agent: String,
    ) {
        self.confirm_dialog(
            window,
            cx,
            "重启",
            false,
            "重启 agent",
            format!("确定重启 agent「{agent}」吗？"),
            move |this, window, cx| {
                let agent = agent.clone();
                this.restart_agent(window, cx, machine, agent);
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
            false,
            "重连机器",
            format!("确定重连机器「{name}」吗？"),
            move |this, window, cx| {
                this.reconnect_machine(window, cx, &name);
            },
        );
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
        self.machines[idx].connection_generation = next_connection_generation();
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
        // 初始 Connecting：状态由 ws 认证通知驱动，不伪造「已连接」
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        let generation = self.machines[idx].connection_generation;
        let t = self.spawn_machine_tasks(window, cx, name, client, generation);
        self._tasks.push(t);
        self.sync_machine_hub();
        // 不在此处立即拉取：连接任务在 auth 握手完成前会拒绝一切请求，
        // 提前发的 agent.list 必然失败并把 notice 染成「agent 列表获取失败」。
        // 初始数据由 on_notify 的 auth_ok 分支统一拉取（同重连流程）。
        self.settings.machine_form_error = None;
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
        if self.workflows.iter().any(|workflow| {
            workflow
                .session
                .read()
                .children
                .iter()
                .any(|child| child.machine_idx == idx)
        }) {
            self.settings.machine_form_error =
                Some("请先删除关联工作流会话，再移除该机器。".into());
            cx.notify();
            return;
        }
        let name = self.machines[idx].config.name.clone();
        // 草稿键以机器名定位，须在下标重排/机器移除前完成旧草稿保存与新草稿换入
        let next = match self.selected.clone() {
            Some(Selected::Session { machine, .. }) if machine == idx => None,
            Some(Selected::Session { machine, id }) if machine > idx => Some(Selected::Session {
                machine: machine - 1,
                id,
            }),
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
        for wf in self.workflows.iter_mut() {
            let mut children_guard = wf.session.write();
            for c in children_guard.children.iter_mut() {
                if c.machine_idx == idx {
                    // 保留机器名和远端会话关联，但标记为未绑定，避免下标
                    // 左移后误操作另一台机器。
                    c.machine_idx = usize::MAX;
                } else if c.machine_idx > idx {
                    c.machine_idx -= 1;
                }
            }
        }
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
            true,
            "移除机器",
            format!("确定移除机器「{name}」吗？其本地注册信息将被删除。"),
            move |this, window, cx| {
                this.remove_machine(window, cx, &name);
            },
        );
    }
}
