//! 根视图：三面板布局、设置浮窗与轮询节拍。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use amux_common::api::{
    ApiFormat, CreateSessionRequest, OrchestratorConfig, QuickCommand, SessionConfigSetting, Skill,
    WorkflowPlanItem,
};
use amux_common::domain::{
    ContentBlock, SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SlashCommand,
};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState, Paste};
use gpui_component::resizable::{h_resizable, resizable_panel};
use gpui_component::select::{SelectEvent, SelectState};
use gpui_component::*;
use parking_lot::Mutex;

use crate::config::{self, Connection};
use crate::dialog::{self, FormTarget};
use crate::panels;
use crate::poll;
use crate::settings;
use crate::state::{
    Attachment, Core, DirectoryCache, ListEntry, OpenTarget, SharedCore, SidePanel, WorkspaceNode,
};
use crate::ui;

/// UI 轮询节拍：驱动后台刷新与重绘。
const TICK: Duration = Duration::from_millis(250);

/// 会话选项下拉框的条目类型（选项值为字符串）。
pub type ConfigSelect = Entity<SelectState<Vec<SharedString>>>;

pub struct AmuxApp {
    pub core: SharedCore,
    /// 后台任务运行时（须持有 Runtime，仅保留 Handle 会让任务无法被调度）
    pub runtime: tokio::runtime::Runtime,
    /// 会话输入框
    pub input: Entity<InputState>,
    /// 工作流计划输入框（新建工作流会话）
    pub plan_input: Entity<InputState>,
    /// 工作目录输入框（新建普通会话）
    pub workspace_input: Entity<InputState>,
    /// 连接设置输入框
    pub server_input: Entity<InputState>,
    pub token_input: Entity<InputState>,
    /// 连接设置的「保存」是否可点（server/token 输入变更后置位）
    pub settings_dirty: bool,
    /// 编排智能体 API 格式的当前选择（文本项直接取输入框）
    pub orchestrator_format: Option<ApiFormat>,
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
    /// 待发送附件（拖拽/粘贴产生）
    pub attachments: Vec<Attachment>,
    /// 斜杠命令上拉框中高亮项
    pub slash_selected: usize,
    /// 斜杠命令上拉框是否被 Esc 收起
    pub slash_dismissed: bool,
    /// 会话选项下拉框：按选项 id 保留实体
    config_selects: HashMap<String, ConfigSelect>,
    /// 选项实体对应的会话 id
    config_selects_session: Option<String>,
    /// 选项实体已同步的（选项 id, 当前值）快照
    config_selects_snapshot: Vec<(String, String)>,
    /// 选项下拉框的事件订阅
    config_subscriptions: Vec<Subscription>,
    /// 改动审查：已折叠的文件
    pub diff_collapsed_files: HashSet<String>,
    /// 改动审查：文件树中已折叠的目录
    pub diff_collapsed_dirs: HashSet<String>,
    /// 改动审查：整个文件树区域是否展开
    pub diff_tree_visible: bool,
    /// 改动审查：已选中的文件
    pub diff_selected_files: HashSet<String>,
    /// 改动审查：已选中的代码块（文件路径, hunk 头）
    pub diff_selected_hunks: HashSet<(String, String)>,
    /// 改动审查：给 agent 的指令输入
    pub diff_instruction: Entity<InputState>,
    /// 改动审查：改动区域滚动句柄（点击文件时定位）
    pub diff_scroll: ScrollHandle,
}

impl AmuxApp {
    pub fn new(connection: Connection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let core: SharedCore = Arc::new(Mutex::new(Core::new(connection.clone())));
        // Runtime 必须由 AmuxApp 持有：drop 掉 Runtime 会终止其上所有后台任务
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio runtime 失败");

        let server = connection.server.clone();
        let token = connection.token.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入指令，Enter 发送，Shift+Enter 换行")
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
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

        // 手动输入工作目录时刷新前缀匹配的目录项
        cx.subscribe(
            &workspace_input,
            |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh_workspace_suggestions(cx);
                }
            },
        )
        .detach();

        // 轮询节拍：后台刷新 + 同步视图状态 + 重绘
        let tick_core = Arc::clone(&core);
        let tick_runtime = runtime.handle().clone();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            tick_runtime.spawn(poll::tick(tick_core.clone()));
            // 同步会话选项控件（下拉框需要 Window）后重绘
            if this
                .update_in(cx, |this, window, cx| {
                    this.sync_config_options(window, cx);
                    cx.notify();
                })
                .is_err()
            {
                // 视图已销毁或窗口已关闭：结束刷新任务
                break;
            }
        })
        .detach();

        // 输入框：回车发送、内容变化时重置斜杠命令上拉框
        cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.send(window, cx),
                InputEvent::Change => {
                    this.slash_selected = 0;
                    this.slash_dismissed = false;
                    cx.notify();
                }
                _ => {}
            },
        )
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
        let diff_instruction = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("对选中的改动说明指令，发送给 agent")
                .auto_grow(1, 4)
                .submit_on_enter(true)
        });
        let diff_instruction_entity = diff_instruction.clone();
        cx.subscribe_in(
            &diff_instruction_entity,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.send_diff_review(window, cx);
                }
            },
        )
        .detach();

        Self {
            core,
            runtime,
            input,
            plan_input,
            workspace_input,
            server_input,
            token_input,
            settings_dirty: false,
            orchestrator_format: None,
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
            attachments: Vec::new(),
            slash_selected: 0,
            slash_dismissed: false,
            config_selects: HashMap::new(),
            config_selects_session: None,
            config_selects_snapshot: Vec::new(),
            config_subscriptions: Vec::new(),
            diff_collapsed_files: HashSet::new(),
            diff_collapsed_dirs: HashSet::new(),
            diff_tree_visible: true,
            diff_selected_files: HashSet::new(),
            diff_selected_hunks: HashSet::new(),
            diff_instruction,
            diff_scroll: ScrollHandle::new(),
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
        // 视图按会话隔离：切换会话时收起不适用或有残留内容的面板与状态
        self.with_core(|core| {
            if core
                .side_panel
                .is_some_and(|panel| !panel.is_available(is_workflow))
            {
                core.side_panel = None;
            }
        });
        self.attachments.clear();
        self.diff_collapsed_files.clear();
        self.diff_collapsed_dirs.clear();
        self.diff_selected_files.clear();
        self.diff_selected_hunks.clear();
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
        dialog::confirm(
            window,
            cx,
            format!("删除「{label}」？"),
            if matches!(entry, ListEntry::Workflow(_)) {
                "工作流会话及其关联的普通会话都会被删除。"
            } else {
                "会话记录与其 worktree 会被清理。"
            }
            .to_string(),
            "删除",
            true,
            move |this, cx| this.delete_entry(entry.clone(), cx),
        );
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

    /// 加载工作目录树根节点。
    pub fn load_workspace(&mut self, cx: &mut Context<Self>) {
        let (client, target) = self.with_core(|core| {
            let target = core
                .view
                .session
                .as_ref()
                .map(|session| (session.machine.clone(), session.root_dir().to_string()));
            (core.client.clone(), target)
        });
        let (Some(client), Some((machine, path))) = (client, target) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.list_dir(&machine, Some(&path), 500, 0).await {
                Ok(result) => {
                    let mut core = core.lock();
                    core.view.detail.workspace_tree =
                        result.entries.into_iter().map(WorkspaceNode::new).collect();
                }
                Err(error) => core.lock().note(format!("读取工作目录失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 展开/折叠工作目录树节点；子目录首次展开时拉取其内容。
    pub fn toggle_workspace_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let (client, machine, loaded) = self.with_core(|core| {
            let loaded = WorkspaceNode::find_mut(&mut core.view.detail.workspace_tree, &path)
                .map(|node| {
                    node.expanded = !node.expanded;
                    node.children.is_some()
                })
                .unwrap_or(false);
            let machine = core.view.session.as_ref().map(|s| s.machine.clone());
            (core.client.clone(), machine, loaded)
        });
        if loaded {
            cx.notify();
            return;
        }
        let (Some(client), Some(machine)) = (client, machine) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.list_dir(&machine, Some(&path), 500, 0).await {
                Ok(result) => {
                    let mut core = core.lock();
                    if let Some(node) =
                        WorkspaceNode::find_mut(&mut core.view.detail.workspace_tree, &path)
                    {
                        node.children =
                            Some(result.entries.into_iter().map(WorkspaceNode::new).collect());
                    }
                }
                Err(error) => core.lock().note(format!("读取目录失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 填入工作目录（最近目录或前缀联想项）。
    pub fn set_workspace(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_input
            .update(cx, |state, cx| state.set_value(path, window, cx));
        self.with_core(|core| core.new_session.suggestions.clear());
        cx.notify();
    }

    /// 填入工作流计划（已保存计划）。
    pub fn set_plan(&mut self, plan: String, window: &mut Window, cx: &mut Context<Self>) {
        self.plan_input
            .update(cx, |state, cx| state.set_value(plan, window, cx));
        cx.notify();
    }

    /// 刷新前缀匹配的目录项：取输入最后一段为前缀，列其所在目录的子目录。
    fn refresh_workspace_suggestions(&mut self, cx: &mut Context<Self>) {
        let text = self.workspace_input.read(cx).value().to_string();
        let machine = self.with_core(|core| core.new_session.machine.clone());
        // 无目录分隔符或前缀为空时不联想（避免每次选中目录项都重新展开整目录）
        let parsed = text
            .rsplit_once('/')
            .map(|(base, prefix)| (format!("{base}/"), prefix.to_string()))
            .filter(|(_, prefix)| !prefix.is_empty());
        let (Some((dir, prefix)), Some(machine)) = (parsed, machine) else {
            self.with_core(|core| core.new_session.suggestions.clear());
            cx.notify();
            return;
        };
        let cached = self.with_core(|core| {
            let cache = core.new_session.suggestion_cache.as_ref()?;
            (cache.machine == machine && cache.dir == dir).then(|| cache.matching(&prefix))
        });
        if let Some(suggestions) = cached {
            self.with_core(|core| core.new_session.suggestions = suggestions);
            cx.notify();
            return;
        }
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let entries = match client.list_dir(&machine, Some(&dir), 500, 0).await {
                Ok(result) => result.entries,
                Err(_) => Vec::new(),
            };
            let cache = DirectoryCache {
                machine,
                dir,
                entries,
            };
            let mut core = core.lock();
            core.new_session.suggestions = cache.matching(&prefix);
            core.new_session.suggestion_cache = Some(cache);
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

    /// 发送输入框内容：文本与附件一并作为用户输入发出。
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text });
        }
        blocks.extend(
            std::mem::take(&mut self.attachments)
                .into_iter()
                .map(|attachment| attachment.block),
        );
        self.input
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.slash_selected = 0;
        self.send_blocks(blocks, cx);
    }

    /// 快捷指令：把预设提示词直接作为用户输入发送（docs/PRD.md「快捷指令」）。
    pub fn send_quick_command(&mut self, prompt: String, cx: &mut Context<Self>) {
        self.send_blocks(vec![ContentBlock::Text { text: prompt }], cx);
    }

    /// 内容块作为用户输入发往当前会话。
    fn send_blocks(&mut self, blocks: Vec<ContentBlock>, cx: &mut Context<Self>) {
        let target = self.with_core(|core| core.open.clone());
        let Some(target) = target else { return };
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match poll::send_prompt(&client, &target, blocks).await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.last.list = None;
                    core.last.history = None;
                }
                Err(error) => core.lock().note(format!("发送失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 拖入的文件作为资源链接附件。
    pub fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            self.attachments.push(Attachment {
                block: ContentBlock::ResourceLink {
                    uri: format!("file://{}", path.display()),
                    name: name.clone(),
                    mime_type: None,
                    title: None,
                    description: None,
                },
                label: name,
            });
        }
        cx.notify();
    }

    pub fn remove_attachment(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.attachments.len() {
            self.attachments.remove(ix);
        }
        cx.notify();
    }

    /// 粘贴：图片作为内容块附件、文件路径作为资源链接，其余交给输入框处理文本。
    pub fn paste_into_composer(&mut self, _: &Paste, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        let mut attached = false;
        for entry in clipboard.entries() {
            match entry {
                ClipboardEntry::Image(image) if !image.bytes.is_empty() => {
                    self.attachments.push(Attachment {
                        block: ContentBlock::Resource {
                            mime_type: image.format.mime_type().to_string(),
                            uri: None,
                            text: None,
                            blob: Some(base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                &image.bytes,
                            )),
                        },
                        label: "粘贴的图片".to_string(),
                    });
                    attached = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths(), cx);
                    attached = true;
                }
                _ => {}
            }
        }
        if attached {
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// 斜杠命令上拉框候选：输入 `/前缀` 时按前缀匹配（docs/PRD.md 输入区）。
    pub fn slash_candidates(&self, cx: &App) -> Vec<SlashCommand> {
        if self.slash_dismissed {
            return Vec::new();
        }
        let value = self.input.read(cx).value();
        let Some(query) = value.strip_prefix('/') else {
            return Vec::new();
        };
        if query.contains(char::is_whitespace) {
            return Vec::new();
        }
        let query = query.to_lowercase();
        self.with_core(|core| core.view.detail.slash_commands.clone())
            .into_iter()
            .filter(|command| command.name.to_lowercase().starts_with(&query))
            .collect()
    }

    /// 采纳斜杠命令：输入替换为 `/命令名 `。
    pub fn apply_slash_command(
        &mut self,
        command: &SlashCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = format!("/{} ", command.name);
        self.input
            .update(cx, |state, cx| state.set_value(value, window, cx));
        self.slash_selected = 0;
        cx.notify();
    }

    /// 输入区按键：上拉框打开时上下选择、回车采纳、Esc 收起。
    pub fn composer_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let candidates = self.slash_candidates(cx);
        if candidates.is_empty() {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => {
                self.slash_selected = self.slash_selected.saturating_sub(1);
                cx.stop_propagation();
            }
            "down" => {
                self.slash_selected = (self.slash_selected + 1).min(candidates.len() - 1);
                cx.stop_propagation();
            }
            "enter" => {
                let selected = self.slash_selected.min(candidates.len() - 1);
                self.apply_slash_command(&candidates[selected], window, cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.slash_dismissed = true;
                cx.stop_propagation();
            }
            _ => {}
        }
        cx.notify();
    }

    /// 会话选项：调用 configure 端点设置选项值。
    pub fn apply_config_option(
        &mut self,
        config_id: String,
        value: SessionConfigOptionValue,
        cx: &mut Context<Self>,
    ) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        // 立即反映到本地选项（下一次轮询确认；失败时由轮询回滚）
        self.with_core(|core| {
            let Some(option) = core
                .view
                .detail
                .config_options
                .iter_mut()
                .find(|option| option.id == config_id)
            else {
                return;
            };
            match (&mut option.kind, &value) {
                (
                    SessionConfigKind::Boolean { current_value },
                    SessionConfigOptionValue::Boolean { value },
                ) => *current_value = *value,
                (
                    SessionConfigKind::Select { current_value, .. },
                    SessionConfigOptionValue::ValueId { value },
                ) => *current_value = value.clone(),
                _ => {}
            }
        });
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let setting = SessionConfigSetting { config_id, value };
            if let Err(error) = client.configure_session(&id, None, Some(setting)).await {
                core.lock().note(format!("设置会话选项失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 同步会话选项控件：选项集合变化时重建下拉框，仅在当前值变化时就地更新。
    fn sync_config_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (session, options) = self.with_core(|core| match core.open.clone() {
            Some(OpenTarget::Session(id)) => (Some(id), core.view.detail.config_options.clone()),
            _ => (None, Vec::new()),
        });
        let snapshot: Vec<(String, String)> = options
            .iter()
            .map(|option| (option.id.clone(), ui::config_value(option)))
            .collect();
        if session == self.config_selects_session && snapshot == self.config_selects_snapshot {
            return;
        }
        let same_options = session == self.config_selects_session
            && snapshot
                .iter()
                .map(|(id, _)| id)
                .eq(self.config_selects_snapshot.iter().map(|(id, _)| id));
        if same_options {
            for option in &options {
                let SessionConfigKind::Select {
                    options: values,
                    current_value,
                } = &option.kind
                else {
                    continue;
                };
                let Some(select) = self.config_selects.get(&option.id).cloned() else {
                    continue;
                };
                let items: Vec<SharedString> = values
                    .iter()
                    .map(|entry| SharedString::from(entry.name.clone()))
                    .collect();
                let selected = values
                    .iter()
                    .position(|entry| &entry.value == current_value);
                select.update(cx, |state, cx| {
                    state.set_items(items, window, cx);
                    state.set_selected_index(selected.map(IndexPath::new), window, cx);
                });
            }
        } else {
            self.rebuild_config_selects(&options, window, cx);
            self.config_selects_session = session;
        }
        self.config_selects_snapshot = snapshot;
    }

    /// 重建会话选项下拉框：每个 select 选项一个实体，确认后写回 Server。
    fn rebuild_config_selects(
        &mut self,
        options: &[SessionConfigOption],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.config_selects.clear();
        self.config_subscriptions.clear();
        for option in options {
            let SessionConfigKind::Select {
                options: values,
                current_value,
            } = &option.kind
            else {
                continue;
            };
            let items: Vec<SharedString> = values
                .iter()
                .map(|entry| SharedString::from(entry.name.clone()))
                .collect();
            let selected = values
                .iter()
                .position(|entry| &entry.value == current_value);
            let entity = cx.new(|cx| {
                SelectState::new(
                    items,
                    selected.map(|row| IndexPath::default().row(row)),
                    window,
                    cx,
                )
            });
            let config_id = option.id.clone();
            let subscription = cx.subscribe_in(
                &entity,
                window,
                move |this: &mut Self, _, event: &SelectEvent<Vec<SharedString>>, _window, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        this.apply_config_option(
                            config_id.clone(),
                            SessionConfigOptionValue::ValueId {
                                value: value.to_string(),
                            },
                            cx,
                        );
                    }
                },
            );
            self.config_subscriptions.push(subscription);
            self.config_selects.insert(option.id.clone(), entity);
        }
    }

    /// 当前会话选项下拉框（无对应选项时为空）。
    pub fn config_select(&self, id: &str) -> Option<&ConfigSelect> {
        self.config_selects.get(id)
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

    /// 新建会话（普通或工作流模式）。按钮仅在表单完备时可点（docs/PRD.md「新建会话视图」）。
    pub fn create_session(&mut self, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let form = self.with_core(|core| core.new_session.clone());
        let workspace = self.workspace_input.read(cx).value().trim().to_string();
        let plan = self.plan_input.read(cx).value().trim().to_string();
        let core = Arc::clone(&self.core);
        if form.workflow_mode {
            if plan.is_empty() {
                return;
            }
            self.runtime.spawn(async move {
                match client.create_workflow(&plan, None).await {
                    Ok(workflow) => {
                        let mut core = core.lock();
                        core.last.list = None;
                        poll::open_workflow(&mut core, &workflow.id);
                    }
                    Err(error) => core.lock().note(format!("创建失败：{error}")),
                }
            });
        } else {
            let (Some(machine), Some(agent)) = (form.machine.clone(), form.agent.clone()) else {
                return;
            };
            if workspace.is_empty() {
                return;
            }
            let request = CreateSessionRequest {
                machine,
                agent,
                workspace,
                use_worktree: form.use_worktree,
            };
            self.runtime.spawn(async move {
                match client.create_session(&request).await {
                    Ok(session) => {
                        let mut core = core.lock();
                        core.last.list = None;
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
                    core.last.list = None;
                    core.note("已删除");
                }
                Err(error) => core.lock().note(format!("删除失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 重命名会话（经 configure 接口更新标题）。
    pub fn rename(&mut self, target: OpenTarget, title: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match &target {
                OpenTarget::Session(id) => client.configure_session(id, Some(title), None).await,
                OpenTarget::Workflow(id) => client.configure_workflow(id, Some(title)).await,
            };
            match result {
                Ok(()) => core.lock().last.list = None,
                Err(error) => core.lock().note(format!("重命名失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 打开右侧面板：面板数据立即刷新，工作目录与终端在首次打开时加载。
    pub fn open_side_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.side_panel = Some(panel);
            match panel {
                SidePanel::Activities => core.last.activities = None,
                SidePanel::Plan | SidePanel::Detail => core.last.plan = None,
                _ => {}
            }
        });
        match panel {
            SidePanel::Workspace => {
                let loaded = self.with_core(|core| !core.view.detail.workspace_tree.is_empty());
                if !loaded {
                    self.load_workspace(cx);
                }
            }
            SidePanel::Terminal => self.open_terminal(cx),
            SidePanel::Diff => self.refresh_diff(cx),
            _ => {}
        }
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
        self.with_core(|core| core.side_panel = Some(SidePanel::Terminal));
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match existing {
                Some(terminal) => {
                    let mut core = core.lock();
                    core.view.detail.active_terminal = Some(terminal);
                    core.view.detail.terminal_output.clear();
                    core.last.terminal_cursor = 0;
                    core.last.terminal = None;
                }
                None => match client.open_terminal(&id, None, 100, 30).await {
                    Ok(terminal) => {
                        let mut core = core.lock();
                        core.view.detail.terminal_output.clear();
                        core.view.detail.active_terminal = Some(terminal);
                        core.last.terminal_cursor = 0;
                        core.last.terminal = None;
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

    /// 折叠/展开单个文件的 diff。
    pub fn toggle_diff_file(&mut self, path: String, cx: &mut Context<Self>) {
        toggle_set(&mut self.diff_collapsed_files, path);
        cx.notify();
    }

    /// 折叠/展开文件树中的目录。
    pub fn toggle_diff_dir(&mut self, key: String, cx: &mut Context<Self>) {
        toggle_set(&mut self.diff_collapsed_dirs, key);
        cx.notify();
    }

    /// 折叠/展开整个文件树区域。
    pub fn toggle_diff_tree(&mut self, cx: &mut Context<Self>) {
        self.diff_tree_visible = !self.diff_tree_visible;
        cx.notify();
    }

    /// 折叠/展开全部文件改动（折叠后仅显示文件名）。
    pub fn toggle_all_diffs(&mut self, cx: &mut Context<Self>) {
        let paths = self.diff_paths();
        if paths.is_empty() {
            return;
        }
        let collapse = !paths
            .iter()
            .all(|path| self.diff_collapsed_files.contains(path));
        for path in paths {
            if collapse {
                self.diff_collapsed_files.insert(path);
            } else {
                self.diff_collapsed_files.remove(&path);
            }
        }
        cx.notify();
    }

    /// 改动审查：选中/取消选中整个文件。
    pub fn toggle_diff_file_selected(&mut self, path: String, cx: &mut Context<Self>) {
        toggle_set(&mut self.diff_selected_files, path);
        cx.notify();
    }

    /// 改动审查：选中/取消选中代码块。
    pub fn toggle_diff_hunk_selected(
        &mut self,
        path: String,
        header: String,
        cx: &mut Context<Self>,
    ) {
        toggle_set(&mut self.diff_selected_hunks, (path, header));
        cx.notify();
    }

    /// 点击文件：右侧改动区域滚动到该文件。
    pub fn scroll_to_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.diff_scroll.scroll_to_top_of_item(ix);
        cx.notify();
    }

    /// 把选中的文件与代码块连同指令发送给 agent（docs/PRD.md「改动审查」）。
    pub fn send_diff_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let instruction = self.diff_instruction.read(cx).value().trim().to_string();
        if instruction.is_empty()
            || (self.diff_selected_files.is_empty() && self.diff_selected_hunks.is_empty())
        {
            return;
        }
        let files = self.with_core(|core| {
            core.view
                .detail
                .diff
                .as_ref()
                .map(|diff| diff.files.clone())
                .unwrap_or_default()
        });
        let mut sections = Vec::new();
        for file in &files {
            if self.diff_selected_files.contains(&file.path) {
                sections.push(format!("{}（整个文件）\n{}", file.path, file.patch));
                continue;
            }
            for hunk in &file.hunks {
                if self
                    .diff_selected_hunks
                    .contains(&(file.path.clone(), hunk.header.clone()))
                {
                    sections.push(format!("{}（{}）\n{}", file.path, hunk.header, hunk.patch));
                }
            }
        }
        if sections.is_empty() {
            return;
        }
        let text = format!("{instruction}\n\n改动内容：\n{}", sections.join("\n"));
        self.diff_instruction
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.diff_selected_files.clear();
        self.diff_selected_hunks.clear();
        self.send_blocks(vec![ContentBlock::Text { text }], cx);
    }

    /// 当前改动列表中的文件路径。
    fn diff_paths(&self) -> Vec<String> {
        self.with_core(|core| {
            core.view
                .detail
                .diff
                .as_ref()
                .map(|diff| diff.files.iter().map(|file| file.path.clone()).collect())
                .unwrap_or_default()
        })
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
            self.runtime.handle().clone(),
            Arc::clone(&self.core),
            skill,
            action.to_string(),
        );
        cx.notify();
    }

    /// 全量保存列表类配置（技能/快捷指令/工作流计划），成功后更新本地缓存。
    pub fn save_list(&mut self, request: SettingsList, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match &request {
                SettingsList::Skills(list) => client.set_skills(list).await,
                SettingsList::QuickCommands(list) => client.set_quick_commands(list).await,
                SettingsList::Plans(list) => client.set_workflow_plans(list).await,
            };
            match result {
                Ok(()) => {
                    let mut core = core.lock();
                    match request {
                        SettingsList::Skills(list) => core.settings.skills = list,
                        SettingsList::QuickCommands(list) => core.settings.quick_commands = list,
                        SettingsList::Plans(list) => core.settings.plans = list,
                    }
                    core.note("设置已保存");
                }
                Err(error) => core.lock().note(format!("保存失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 打开快捷指令表单弹窗（新增或编辑）。
    pub fn open_quick_command_form(
        &mut self,
        target: FormTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = match &target {
            FormTarget::New => None,
            FormTarget::Edit(name) => self.with_core(|core| {
                core.settings
                    .quick_commands
                    .iter()
                    .find(|item| &item.name == name)
                    .cloned()
            }),
        };
        if let FormTarget::Edit(_) = target {
            let Some(command) = editing else { return };
            self.quick_name
                .update(cx, |state, cx| state.set_value(command.name, window, cx));
            self.quick_prompt
                .update(cx, |state, cx| state.set_value(command.prompt, window, cx));
        } else {
            self.quick_name
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
            self.quick_prompt
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        let title = match &target {
            FormTarget::New => "新增快捷指令",
            FormTarget::Edit(_) => "编辑快捷指令",
        };
        dialog::form(
            window,
            cx,
            title.to_string(),
            vec![
                ("指令名称", self.quick_name.clone()),
                ("指令内容", self.quick_prompt.clone()),
            ],
            move |this, cx| this.save_quick_command(target.clone(), cx),
        );
    }

    /// 保存快捷指令（新增或编辑）；名称为空时保留弹窗。
    pub fn save_quick_command(&mut self, target: FormTarget, cx: &mut Context<Self>) -> bool {
        let name = self.quick_name.read(cx).value().trim().to_string();
        let prompt = self.quick_prompt.read(cx).value().to_string();
        if name.is_empty() {
            self.with_core(|core| core.note("请填写指令名称"));
            cx.notify();
            return false;
        }
        let mut list = self.with_core(|core| core.settings.quick_commands.clone());
        let command = QuickCommand { name, prompt };
        match &target {
            FormTarget::New => list.push(command),
            FormTarget::Edit(old) => {
                if let Some(item) = list.iter_mut().find(|item| &item.name == old) {
                    *item = command;
                }
            }
        }
        self.save_list(SettingsList::QuickCommands(list), cx);
        true
    }

    /// 删除快捷指令（弹窗确认）。
    pub fn delete_quick_command(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        dialog::confirm(
            window,
            cx,
            format!("删除快捷指令「{name}」？"),
            "删除后无法恢复。".to_string(),
            "删除",
            true,
            move |this, cx| {
                let list = this.with_core(|core| {
                    core.settings
                        .quick_commands
                        .iter()
                        .filter(|item| item.name != name)
                        .cloned()
                        .collect()
                });
                this.save_list(SettingsList::QuickCommands(list), cx);
            },
        );
    }

    /// 打开技能表单弹窗（新增或编辑）。
    pub fn open_skill_form(
        &mut self,
        target: FormTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = match &target {
            FormTarget::New => None,
            FormTarget::Edit(name) => self.with_core(|core| {
                core.settings
                    .skills
                    .iter()
                    .find(|item| &item.name == name)
                    .cloned()
            }),
        };
        if let FormTarget::Edit(_) = target {
            let Some(skill) = editing else { return };
            self.skill_name
                .update(cx, |state, cx| state.set_value(skill.name, window, cx));
            self.skill_desc.update(cx, |state, cx| {
                state.set_value(skill.description, window, cx)
            });
        } else {
            self.skill_name
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
            self.skill_desc
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        let title = match &target {
            FormTarget::New => "新增技能",
            FormTarget::Edit(_) => "编辑技能",
        };
        dialog::form(
            window,
            cx,
            title.to_string(),
            vec![
                ("技能名称", self.skill_name.clone()),
                ("技能描述", self.skill_desc.clone()),
            ],
            move |this, cx| this.save_skill(target.clone(), cx),
        );
    }

    /// 保存技能（新增或编辑）；名称为空时保留弹窗。
    pub fn save_skill(&mut self, target: FormTarget, cx: &mut Context<Self>) -> bool {
        let name = self.skill_name.read(cx).value().trim().to_string();
        let description = self.skill_desc.read(cx).value().to_string();
        if name.is_empty() {
            self.with_core(|core| core.note("请填写技能名称"));
            cx.notify();
            return false;
        }
        let mut list = self.with_core(|core| core.settings.skills.clone());
        let skill = Skill { name, description };
        match &target {
            FormTarget::New => list.push(skill),
            FormTarget::Edit(old) => {
                if let Some(item) = list.iter_mut().find(|item| &item.name == old) {
                    *item = skill;
                }
            }
        }
        self.save_list(SettingsList::Skills(list), cx);
        true
    }

    /// 删除技能（弹窗确认）。
    pub fn delete_skill(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        dialog::confirm(
            window,
            cx,
            format!("删除技能「{name}」？"),
            "删除后无法恢复。".to_string(),
            "删除",
            true,
            move |this, cx| {
                let list = this.with_core(|core| {
                    core.settings
                        .skills
                        .iter()
                        .filter(|item| item.name != name)
                        .cloned()
                        .collect()
                });
                this.save_list(SettingsList::Skills(list), cx);
            },
        );
    }

    /// 打开工作流计划表单弹窗（新增或编辑）。
    pub fn open_plan_form(
        &mut self,
        target: FormTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = match &target {
            FormTarget::New => None,
            FormTarget::Edit(name) => self.with_core(|core| {
                core.settings
                    .plans
                    .iter()
                    .find(|item| &item.name == name)
                    .cloned()
            }),
        };
        if let FormTarget::Edit(_) = target {
            let Some(plan) = editing else { return };
            self.plan_name
                .update(cx, |state, cx| state.set_value(plan.name, window, cx));
            self.plan_plan
                .update(cx, |state, cx| state.set_value(plan.plan, window, cx));
        } else {
            self.plan_name
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
            self.plan_plan
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        let title = match &target {
            FormTarget::New => "新增工作流计划",
            FormTarget::Edit(_) => "编辑工作流计划",
        };
        dialog::form(
            window,
            cx,
            title.to_string(),
            vec![
                ("计划名称", self.plan_name.clone()),
                ("计划内容", self.plan_plan.clone()),
            ],
            move |this, cx| this.save_plan(target.clone(), cx),
        );
    }

    /// 保存工作流计划（新增或编辑）；名称为空时保留弹窗。
    pub fn save_plan(&mut self, target: FormTarget, cx: &mut Context<Self>) -> bool {
        let name = self.plan_name.read(cx).value().trim().to_string();
        let plan = self.plan_plan.read(cx).value().to_string();
        if name.is_empty() {
            self.with_core(|core| core.note("请填写计划名称"));
            cx.notify();
            return false;
        }
        let mut list = self.with_core(|core| core.settings.plans.clone());
        let item = WorkflowPlanItem { name, plan };
        match &target {
            FormTarget::New => list.push(item),
            FormTarget::Edit(old) => {
                if let Some(existing) = list.iter_mut().find(|existing| &existing.name == old) {
                    *existing = item;
                }
            }
        }
        self.save_list(SettingsList::Plans(list), cx);
        true
    }

    /// 删除工作流计划（弹窗确认）。
    pub fn delete_plan(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        dialog::confirm(
            window,
            cx,
            format!("删除工作流计划「{name}」？"),
            "删除后无法恢复。".to_string(),
            "删除",
            true,
            move |this, cx| {
                let list = this.with_core(|core| {
                    core.settings
                        .plans
                        .iter()
                        .filter(|item| item.name != name)
                        .cloned()
                        .collect()
                });
                this.save_list(SettingsList::Plans(list), cx);
            },
        );
    }

    /// 保存编排智能体配置。
    pub fn save_orchestrator(&mut self, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let config = OrchestratorConfig {
            api_format: self.current_orchestrator_format(),
            base_url: self.orch_base_url.read(cx).value().trim().to_string(),
            api_key: self.orch_api_key.read(cx).value().trim().to_string(),
            model: self.orch_model.read(cx).value().trim().to_string(),
            effort: self.orch_effort.read(cx).value().trim().to_string(),
        };
        if config.base_url.is_empty() || config.api_key.is_empty() || config.model.is_empty() {
            self.with_core(|core| core.note("请填写 Base URL、API Key 与模型名称"));
            cx.notify();
            return;
        }
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.set_orchestrator(&config).await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.settings.orchestrator = Some(config);
                    core.note("设置已保存");
                }
                Err(error) => core.lock().note(format!("保存失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 编排智能体 API 格式：用户已选择则用其选择，否则用已保存配置的格式。
    pub fn current_orchestrator_format(&self) -> ApiFormat {
        self.orchestrator_format
            .or_else(|| {
                self.with_core(|core| core.settings.orchestrator.as_ref().map(|c| c.api_format))
            })
            .unwrap_or(ApiFormat::ChatCompletions)
    }

    /// 打开编排智能体设置页时预填已保存的配置。
    pub fn load_orchestrator_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.with_core(|core| core.settings.orchestrator.clone());
        let config = stored.unwrap_or(OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            effort: String::new(),
        });
        self.orchestrator_format = Some(config.api_format);
        self.orch_base_url
            .update(cx, |state, cx| state.set_value(config.base_url, window, cx));
        self.orch_api_key
            .update(cx, |state, cx| state.set_value(config.api_key, window, cx));
        self.orch_model
            .update(cx, |state, cx| state.set_value(config.model, window, cx));
        self.orch_effort
            .update(cx, |state, cx| state.set_value(config.effort, window, cx));
    }
}

/// 设置页可全量保存的列表类配置。
pub enum SettingsList {
    Skills(Vec<Skill>),
    QuickCommands(Vec<QuickCommand>),
    Plans(Vec<WorkflowPlanItem>),
}

impl Render for AmuxApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.lock().clone();

        let left = panels::render_left(&core, self, cx);
        let middle = panels::render_middle(&core, self, cx);
        let right = core
            .side_panel
            .map(|panel| panels::render_right(&core, panel, self, cx));

        // 三栏可拖拽调整宽度（docs/PRD.md 右侧面板）
        let mut shell = h_resizable("shell")
            .child(
                resizable_panel()
                    .size(px(panels::LEFT_WIDTH))
                    .size_range(px(panels::LEFT_MIN_WIDTH)..px(panels::LEFT_MAX_WIDTH))
                    .flex_none()
                    .child(left),
            )
            .child(resizable_panel().child(middle));
        if let Some(right) = right {
            shell = shell.child(
                resizable_panel()
                    .size(px(panels::RIGHT_WIDTH))
                    .size_range(px(panels::RIGHT_MIN_WIDTH)..px(panels::RIGHT_MAX_WIDTH))
                    .flex_none()
                    .child(right),
            );
        }

        let mut root = div()
            .size_full()
            .flex()
            .flex_row()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(shell);
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

/// 集合中已存在则移除，否则插入（折叠/选中状态切换）。
fn toggle_set<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}
