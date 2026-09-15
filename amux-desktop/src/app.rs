//! 根视图：三面板布局、设置浮窗与轮询节拍。

use std::sync::Arc;
use std::time::Duration;

use amux_common::api::{
    CreateSessionRequest, OrchestratorConfig, QuickCommand, Skill, WorkflowPlanItem,
};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::*;
use parking_lot::Mutex;

use crate::config::{self, Connection};
use crate::panels;
use crate::poll;
use crate::settings;
use crate::state::{Core, ListEntry, OpenTarget, SettingsTab, SharedCore, SidePanel};
use crate::ui;

/// UI 轮询节拍：驱动后台刷新与重绘。
const TICK: Duration = Duration::from_millis(250);

pub struct AmuxApp {
    pub core: SharedCore,
    pub runtime: tokio::runtime::Handle,
    /// 会话输入框
    pub input: Entity<InputState>,
    /// 工作流计划输入框（新建工作流会话）
    pub plan_input: Entity<InputState>,
    /// 工作目录输入框（新建普通会话）
    pub workspace_input: Entity<InputState>,
    /// 连接设置输入框
    pub server_input: Entity<InputState>,
    pub token_input: Entity<InputState>,
    /// 设置面板的「保存」是否可点（任一配置修改过）
    pub settings_dirty: bool,
    /// 侧栏面板
    pub side_panel: Option<SidePanel>,
    /// 设置表单编辑缓冲（技能/快捷指令/工作流计划/编排智能体）
    pub skill_form: (String, String),
    pub quick_form: (String, String),
    pub plan_form: (String, String),
    pub orchestrator_form: Option<OrchestratorConfig>,
    /// 行内重命名的会话 id 与输入框
    pub renaming_id: Option<String>,
    pub rename_input: Entity<InputState>,
    /// 设置表单输入框
    pub orch_base_url: Entity<InputState>,
    pub orch_api_key: Entity<InputState>,
    pub orch_model: Entity<InputState>,
    pub orch_effort: Entity<InputState>,
    pub quick_name: Entity<InputState>,
    pub quick_prompt: Entity<InputState>,
    pub skill_name: Entity<InputState>,
    pub skill_desc: Entity<InputState>,
    pub plan_name: Entity<InputState>,
    pub plan_plan: Entity<InputState>,
    /// 终端命令行输入
    pub terminal_input: Entity<InputState>,
}

impl AmuxApp {
    pub fn new(connection: Connection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let core: SharedCore = Arc::new(Mutex::new(Core::new(connection.clone())));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio runtime 失败")
            .handle()
            .clone();

        let server = connection.server.clone();
        let token = connection.token.clone();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("输入指令，Enter 发送"));
        let plan_input = cx.new(|cx| InputState::new(window, cx).placeholder("工作流计划"));
        let workspace_input = cx.new(|cx| InputState::new(window, cx).placeholder("工作目录"));
        let server_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://amux.example.com:34567"));
        let token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("认证 token")
                .masked(true)
        });
        if !server.is_empty() {
            server_input.update(cx, |state, cx| state.set_value(server.clone(), window, cx));
        }
        if !token.is_empty() {
            token_input.update(cx, |state, cx| state.set_value(token.clone(), window, cx));
        }

        // 输入变更即标记「保存」可用
        for state in [&server_input, &token_input] {
            cx.subscribe(state, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.settings_dirty = true;
                    cx.notify();
                }
            })
            .detach();
        }

        // 轮询节拍：后台刷新 + 重绘
        let tick_core = Arc::clone(&core);
        let tick_runtime = runtime.clone();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            tick_runtime.spawn(poll::tick(tick_core.clone()));
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();

        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));
        let orch_base_url = cx.new(|cx| InputState::new(window, cx).placeholder("Base URL"));
        let orch_api_key = cx.new(|cx| InputState::new(window, cx).placeholder("API Key"));
        let orch_model = cx.new(|cx| InputState::new(window, cx).placeholder("模型名称"));
        let orch_effort = cx.new(|cx| InputState::new(window, cx).placeholder("推理级别"));
        let quick_name = cx.new(|cx| InputState::new(window, cx).placeholder("指令名称"));
        let quick_prompt = cx.new(|cx| InputState::new(window, cx).placeholder("指令内容"));
        let skill_name = cx.new(|cx| InputState::new(window, cx).placeholder("技能名称"));
        let skill_desc = cx.new(|cx| InputState::new(window, cx).placeholder("技能描述"));
        let plan_name = cx.new(|cx| InputState::new(window, cx).placeholder("计划名称"));
        let plan_plan = cx.new(|cx| InputState::new(window, cx).placeholder("计划内容"));
        let terminal_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("终端命令，回车发送"));

        Self {
            core,
            runtime,
            input,
            plan_input,
            workspace_input,
            server_input,
            token_input,
            settings_dirty: false,
            side_panel: None,
            skill_form: (String::new(), String::new()),
            quick_form: (String::new(), String::new()),
            plan_form: (String::new(), String::new()),
            orchestrator_form: None,
            renaming_id: None,
            rename_input,
            orch_base_url,
            orch_api_key,
            orch_model,
            orch_effort,
            quick_name,
            quick_prompt,
            skill_name,
            skill_desc,
            plan_name,
            plan_plan,
            terminal_input,
        }
    }

    /// 重新发现机器上的 agents。
    pub fn rediscover(&mut self, machine: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.rediscover(&machine).await {
                Ok(agents) => {
                    let mut core = core.lock();
                    core.settings.agents.retain(|(name, _)| name != &machine);
                    core.settings.agents.push((machine, agents));
                }
                Err(error) => core.lock().note(format!("重新发现失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 重启（或启动）指定 agent。
    pub fn restart_agent(&mut self, machine: String, agent: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.restart_agent(&machine, &agent).await {
                Ok(()) => core.lock().note(format!("已重启 {agent}@{machine}")),
                Err(error) => core.lock().note(format!("重启失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 终端命令行：发送命令文本（补换行）。
    pub fn send_terminal_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.terminal_input.read(cx).value().trim_end().to_string();
        if text.is_empty() {
            return;
        }
        self.terminal_input
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        let mut data = text.into_bytes();
        data.push(b'\n');
        self.terminal_input(data, cx);
    }

    /// 查看文件内容（工作目录面板）。
    pub fn read_file(&mut self, machine: String, path: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.read_file(&machine, &path, 400, 0).await {
                Ok(result) => core.lock().view.detail.file_content = Some(result.content),
                Err(error) => core.lock().note(format!("读取文件失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 关闭终端。
    pub fn close_terminal(&mut self, terminal: String, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Err(error) = client.close_terminal(&id, &terminal).await {
                core.lock().note(format!("关闭终端失败：{error}"));
            }
            let mut core = core.lock();
            if core.view.detail.active_terminal.as_deref() == Some(terminal.as_str()) {
                core.view.detail.active_terminal = None;
                core.view.detail.terminal_output.clear();
            }
        });
        cx.notify();
    }

    /// 调整终端行列。
    pub fn resize_terminal(
        &mut self,
        terminal: String,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Err(error) = client.resize_terminal(&id, &terminal, cols, rows).await {
                core.lock().note(format!("调整终端尺寸失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 终端控制键（Ctrl-C / Esc / Tab / 方向键等）。
    pub fn send_terminal_key(&mut self, bytes: &'static [u8], cx: &mut Context<Self>) {
        self.terminal_input(bytes.to_vec(), cx);
    }

    /// 打开列表条目（普通会话或工作流会话）。
    pub fn open_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_workflow = self.with_core(|core| {
            core.entries
                .iter()
                .any(|entry| matches!(entry, ListEntry::Workflow(workflow) if workflow.id == id))
        });
        {
            let mut core = self.core.lock();
            if is_workflow {
                poll::open_workflow(&mut core, id);
            } else {
                poll::open_session(&mut core, id);
            }
        }
        cx.notify();
    }

    /// 开始行内重命名。
    pub fn begin_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.with_core(|core| {
            core.entries
                .iter()
                .find(|entry| entry.id() == id)
                .map(|entry| entry.title())
                .unwrap_or_default()
        });
        let _ = window;
        self.renaming_id = Some(id.to_string());
        let input = self.rename_input.clone();
        input.update(cx, |state, cx| state.set_value(current, window, cx));
        cx.notify();
    }

    /// 提交重命名（Enter）。
    pub fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming_id.clone() else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_string();
        self.renaming_id = None;
        if title.is_empty() {
            cx.notify();
            return;
        }
        let target = self.with_core(|core| {
            core.entries
                .iter()
                .find(|entry| entry.id() == id)
                .map(|entry| match entry {
                    ListEntry::Session(_) => OpenTarget::Session(id.clone()),
                    ListEntry::Workflow(_) => OpenTarget::Workflow(id.clone()),
                })
        });
        if let Some(target) = target {
            self.rename(target, title, cx);
        } else {
            cx.notify();
        }
    }

    /// 删除确认弹窗。
    pub fn confirm_delete(
        &mut self,
        entry: ListEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = entry.title();
        let label = if title.trim().is_empty() {
            "未命名会话".to_string()
        } else {
            title
        };
        let is_workflow = matches!(entry, ListEntry::Workflow(_));
        let app = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let mut alert = alert
                .title(format!("删除「{label}」？"))
                .description(if is_workflow {
                    "工作流会话及其关联的普通会话都会被删除。"
                } else {
                    "会话记录与其 worktree 会被清理。"
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text("删除")
                        .show_cancel(true),
                );
            let app = app.clone();
            let entry = entry.clone();
            alert = alert.on_ok(move |_ev, _window, cx| {
                let entry = entry.clone();
                app.update(cx, |this, cx| {
                    this.delete_entry(entry, cx);
                });
                true
            });
            alert
        });
    }

    /// 切换当前终端（重置游标与输出缓冲）。
    pub fn select_terminal(&mut self, terminal: String, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.view.detail.active_terminal = Some(terminal);
            core.view.detail.terminal_output.clear();
            core.last.terminal_cursor = 0;
            core.last.terminal = None;
        });
        cx.notify();
    }

    /// 加载工作目录一页。
    pub fn load_workspace(&mut self, machine: String, cx: &mut Context<Self>) {
        let (client, path) = self.with_core(|core| {
            let path = core
                .view
                .session
                .as_ref()
                .map(|session| {
                    if session.worktree_dir.is_empty() {
                        session.workspace.clone()
                    } else {
                        session.worktree_dir.clone()
                    }
                })
                .unwrap_or_default();
            (core.client.clone(), path)
        });
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.list_dir(&machine, Some(&path), 200, 0).await {
                Ok(result) => core.lock().view.detail.workspace_entries = result.entries,
                Err(error) => core.lock().note(format!("读取工作目录失败：{error}")),
            }
        });
        cx.notify();
    }

    pub fn with_core<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        let mut core = self.core.lock();
        f(&mut core)
    }

    /// 连接状态（设置面板展示）。
    pub fn status_label(&self) -> String {
        self.with_core(|core| core.status.label())
    }

    /// 保存连接设置：写入 `~/.amux/app/server.json` 并重建客户端。
    pub fn save_connection(&mut self, cx: &mut Context<Self>) {
        let server = self.server_input.read(cx).value().to_string();
        let token = self.token_input.read(cx).value().to_string();
        let connection = Connection { server, token };
        let message = match config::save(&config::connection_path(), &connection) {
            Ok(()) => {
                self.with_core(|core| core.apply_connection(connection));
                "连接设置已保存".to_string()
            }
            Err(error) => error,
        };
        self.settings_dirty = false;
        self.with_core(|core| core.note(message));
        cx.notify();
    }

    /// 发送输入框内容。
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let target = self.with_core(|core| core.open.clone());
        let Some(target) = target else { return };
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        self.input
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match poll::send_prompt(&client, &target, &text).await {
                Ok(()) => {}
                Err(error) => core.lock().note(format!("发送失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 取消进行中的工作。
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        let target = self.with_core(|core| core.open.clone());
        let Some(target) = target else { return };
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Err(error) = poll::cancel(&client, &target).await {
                core.lock().note(format!("取消失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 新建会话（普通或工作流模式）。
    pub fn create_session(&mut self, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let form = self.with_core(|core| core.new_session.clone());
        let workspace = self.workspace_input.read(cx).value().to_string();
        let plan = self.plan_input.read(cx).value().to_string();
        let core = Arc::clone(&self.core);
        if form.workflow_mode {
            if plan.trim().is_empty() {
                core.lock().note("请填写工作流计划");
                cx.notify();
                return;
            }
            self.runtime.spawn(async move {
                match client.create_workflow(&plan, None).await {
                    Ok(workflow) => {
                        poll::open_workflow(&mut core.lock(), &workflow.id);
                    }
                    Err(error) => core.lock().note(format!("创建失败：{error}")),
                }
            });
        } else {
            let (Some(machine), Some(agent)) = (form.machine.clone(), form.agent.clone()) else {
                core.lock().note("请选择机器与 agent");
                cx.notify();
                return;
            };
            let request = CreateSessionRequest {
                machine,
                agent,
                workspace: workspace.trim().to_string(),
                use_worktree: form.use_worktree,
            };
            if request.workspace.is_empty() {
                core.lock().note("请填写工作目录");
                cx.notify();
                return;
            }
            self.runtime.spawn(async move {
                match client.create_session(&request).await {
                    Ok(session) => {
                        let mut core = core.lock();
                        core.note("会话已创建");
                        poll::open_session(&mut core, &session.id);
                    }
                    Err(error) => core.lock().note(format!("创建失败：{error}")),
                }
            });
        }
        cx.notify();
    }

    /// 删除会话条目（工作流会话连同关联普通会话）。
    pub fn delete_entry(&mut self, entry: ListEntry, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let id = entry.id().to_string();
            match poll::delete(&client, &entry).await {
                Ok(()) => {
                    let mut core = core.lock();
                    if core.open.as_ref().map(|target| match target {
                        OpenTarget::Session(open) | OpenTarget::Workflow(open) => open == &id,
                    }) == Some(true)
                    {
                        core.open = None;
                    }
                    core.note("已删除");
                }
                Err(error) => core.lock().note(format!("删除失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 重命名会话（普通会话走 configure，工作流会话走 configure）。
    pub fn rename(&mut self, target: OpenTarget, title: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match &target {
                OpenTarget::Session(id) => client.configure_session(id, Some(title), None).await,
                OpenTarget::Workflow(id) => client.configure_workflow(id, Some(title)).await,
            };
            if let Err(error) = result {
                core.lock().note(format!("重命名失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 打开终端视图（首次打开时创建终端）。
    pub fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let (client, open, existing) = self.with_core(|core| {
            (
                core.client.clone(),
                core.open.clone(),
                core.view.detail.active_terminal.clone(),
            )
        });
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        self.side_panel = Some(SidePanel::Terminal);
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match existing {
                Some(terminal) => {
                    let mut core = core.lock();
                    core.view.detail.active_terminal = Some(terminal);
                    core.view.detail.terminal_output.clear();
                    core.last.terminal_cursor = 0;
                }
                None => match client.open_terminal(&id, None, 100, 30).await {
                    Ok(terminal) => {
                        let mut core = core.lock();
                        core.view.detail.terminal_output.clear();
                        core.view.detail.active_terminal = Some(terminal);
                        core.last.terminal_cursor = 0;
                    }
                    Err(error) => core.lock().note(format!("打开终端失败：{error}")),
                },
            }
        });
        cx.notify();
    }

    /// 终端输入（按键字节）。
    pub fn terminal_input(&mut self, data: Vec<u8>, cx: &mut Context<Self>) {
        let (client, open, terminal) = self.with_core(|core| {
            (
                core.client.clone(),
                core.open.clone(),
                core.view.detail.active_terminal.clone(),
            )
        });
        let (Some(client), Some(OpenTarget::Session(id)), Some(terminal)) =
            (client, open, terminal)
        else {
            return;
        };
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Err(error) = client.terminal_input(&id, &terminal, data).await {
                core.lock().note(format!("终端输入失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 刷新改动 diff。
    pub fn refresh_diff(&mut self, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.diff(&id).await {
                Ok(diff) => core.lock().view.detail.diff = Some(diff),
                Err(error) => core.lock().note(format!("读取改动失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 撤销指定文件或代码块改动。
    pub fn restore(&mut self, path: Option<String>, patch: Option<String>, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.restore(&id, path, patch).await {
                Ok(result) => {
                    if !result.ok {
                        core.lock()
                            .note(format!("撤销失败：{}", result.message.unwrap_or_default()));
                    }
                    if let Ok(diff) = client.diff(&id).await {
                        core.lock().view.detail.diff = Some(diff);
                    }
                }
                Err(error) => core.lock().note(format!("撤销失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 安装/更新/卸载技能：由应用侧发起临时目录会话并发送指令（docs/DESIGN.md「技能操作」）。
    pub fn apply_skill(&mut self, skill: Skill, action: &str, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        crate::settings::install_skill(
            client,
            self.runtime.clone(),
            Arc::clone(&self.core),
            skill,
            action.to_string(),
        );
        cx.notify();
    }

    /// 保存设置里的列表类配置。
    pub fn save_settings(&mut self, cx: &mut Context<Self>) {
        let (client, tab, skills, plans, quick, orchestrator) = self.with_core(|core| {
            (
                core.client.clone(),
                core.settings_tab,
                core.settings.skills.clone(),
                core.settings.plans.clone(),
                core.settings.quick_commands.clone(),
                core.settings
                    .orchestrator
                    .clone()
                    .or_else(|| self.orchestrator_form.clone()),
            )
        });
        let Some(client) = client else { return };
        let skill_form = self.skill_form.clone();
        let quick_form = self.quick_form.clone();
        let plan_form = self.plan_form.clone();
        let orchestrator = self.orchestrator_form.clone().or(orchestrator);
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match tab {
                SettingsTab::Skills => {
                    let mut next = skills;
                    if !skill_form.0.trim().is_empty() {
                        next.push(Skill {
                            name: skill_form.0.trim().to_string(),
                            description: skill_form.1.clone(),
                        });
                    }
                    client.set_skills(&next).await
                }
                SettingsTab::WorkflowPlans => {
                    let mut next: Vec<WorkflowPlanItem> = plans;
                    if !plan_form.0.trim().is_empty() {
                        next.push(WorkflowPlanItem {
                            name: plan_form.0.trim().to_string(),
                            plan: plan_form.1.clone(),
                        });
                    }
                    client.set_workflow_plans(&next).await
                }
                SettingsTab::QuickCommands => {
                    let mut next: Vec<QuickCommand> = quick;
                    if !quick_form.0.trim().is_empty() {
                        next.push(QuickCommand {
                            name: quick_form.0.trim().to_string(),
                            prompt: quick_form.1.clone(),
                        });
                    }
                    client.set_quick_commands(&next).await
                }
                SettingsTab::Orchestrator => match orchestrator {
                    Some(config) => client.set_orchestrator(&config).await,
                    None => Err("请填写编排智能体配置".to_string()),
                },
                SettingsTab::Connection | SettingsTab::Machines => Ok(()),
            };
            match result {
                Ok(()) => core.lock().note("设置已保存"),
                Err(error) => core.lock().note(format!("保存失败：{error}")),
            }
        });
        cx.notify();
    }
}

impl Render for AmuxApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.lock().clone();
        let _ = ui::timestamp(0);

        let left = panels::render_left(&core, self, cx);
        let middle = panels::render_middle(&core, self, cx);
        let right = self
            .side_panel
            .map(|panel| panels::render_right(&core, panel, self, cx));

        let mut root = div()
            .size_full()
            .flex()
            .flex_row()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(left)
            .child(div().flex_1().h_full().child(middle));
        if let Some(right) = right {
            root = root.child(right);
        }
        if core.settings_open {
            root = root.child(settings::render_overlay(&core, self, cx));
        }
        if let Some(toast) = core.toast.clone() {
            root = root.child(
                div()
                    .absolute()
                    .bottom_3()
                    .left_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .child(toast),
            );
        }
        root
    }
}

/// 输入框 → 便于在面板中复用。
pub fn text_input(state: &Entity<InputState>) -> Input {
    Input::new(state)
}
