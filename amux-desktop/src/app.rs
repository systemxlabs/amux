//! 根视图：窗口外壳（标题栏、三栏与拖拽调宽）、设置浮窗与轮询节拍。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use amux_common::api::{
    Agent, ApiFormat, CreateSessionRequest, OrchestratorConfig, Project, QuickCommand,
    RecentWorkspace, SessionConfigSetting, Skill, WorkflowPlanItem,
};
use amux_common::domain::{
    ContentBlock, GitDiffHunk, SessionConfigKind, SessionConfigOption, SessionConfigOptionValue,
    SlashCommand,
};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::*;
use gpui_component::input::{InputEvent, InputState, Paste};
use gpui_component::label::Label;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    h_flex, v_flex, ActiveTheme, Disableable as _, GlobalState, Root, Sizable, TitleBar,
    WindowExt as _,
};
use parking_lot::Mutex;

use crate::config::{self, Connection};
use crate::dialog::{self, FormTarget};
use crate::diff;
use crate::login;
use crate::panels;
use crate::poll;
use crate::sessions;
use crate::settings;
use crate::state::{
    matching_prefix, Attachment, ConnectionStatus, Core, DirectoryListing, ListEntry, OpenTarget,
    Paging, SharedCore, SidePanel, WorkspaceNode, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE,
};
use crate::terminal_view;
use crate::theme::SIDEBAR_WIDTH;
use crate::ui;

/// UI 轮询节拍：驱动后台刷新、通知投递与重绘。
const TICK: Duration = Duration::from_millis(250);

/// 面板拖拽手柄宽度。
pub const PANEL_RESIZE_HANDLE_WIDTH: f32 = 5.0;
/// 输入框高度拖拽手柄厚度。
pub const INPUT_RESIZE_HANDLE_HEIGHT: f32 = 12.0;
/// 输入框默认高度（约 3 行，与 Web 输入区一致，docs/PRD.md「会话交互视图」：多行输入框，可拖拽高度）。
pub const INPUT_DEFAULT_HEIGHT: f32 = 96.0;
/// 输入框高度拖拽下限（与 Web 输入区的 `min-h-16` 一致）。
pub const INPUT_MIN_HEIGHT: f32 = 64.0;
/// 会话选项值上限宽度：值文本超过它就省略（docs/PRD.md「会话交互视图」：宽度自适应 + 超长省略）。
const CONFIG_VALUE_MAX_WIDTH: f32 = 224.0;

actions!(amux, [CloseSettingsOverlay, TerminalTab, TerminalBackTab]);

/// 侧栏调宽拖拽载荷（与面板载荷分开，避免全局拖拽事件互相触发）。
struct SidebarResizeDrag;
/// 右侧面板调宽拖拽载荷。
struct PanelResizeDrag;
/// 输入框高度拖拽载荷。
pub(crate) struct InputResizeDrag;
/// 当前终端 SSE 的身份；会话、终端或面板变化时重建连接。
#[derive(Clone, PartialEq, Eq)]
struct TerminalStreamKey {
    session: String,
    terminal: String,
}

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
    /// 内置智能体 API 格式的当前选择（文本项直接取输入框）
    pub orchestrator_format: Option<ApiFormat>,
    /// 内置智能体表单已预填的配置（分类打开时的拉取是异步的，落地后据此重填一次）
    pub orchestrator_prefilled: Option<OrchestratorConfig>,
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
    pub project_name: Entity<InputState>,
    pub project_desc: Entity<InputState>,
    /// 终端 VT 网格（本地维护屏幕内容与光标；输入经 `send_terminal_input` 上行）
    pub terminal: terminal_view::TerminalScreen,
    /// 终端焦点：按键经它派发（Tab/Shift+Tab 走 action，其余走 key_down）
    pub terminal_focus: FocusHandle,
    /// 当前终端 SSE 连接与任务；视图关闭或终端切换时 abort。
    terminal_stream: Option<(TerminalStreamKey, tokio::task::JoinHandle<()>)>,
    /// 待发送附件（拖拽/粘贴产生）
    pub attachments: Vec<Attachment>,
    /// 斜杠命令上拉框中高亮项
    pub slash_selected: usize,
    /// 斜杠命令上拉框是否被 Esc 收起
    pub slash_dismissed: bool,
    /// 改动审查：已折叠的文件
    pub diff_collapsed_files: HashSet<String>,
    /// 改动审查：文件树中已折叠的目录
    pub diff_collapsed_dirs: HashSet<String>,
    /// 改动审查：整个文件树区域是否展开
    pub diff_tree_visible: bool,
    /// 改动审查：当前评论目标
    pub diff_comment_target: Option<diff::CommentTarget>,
    /// 改动审查：评论输入框
    pub diff_comment_input: Entity<InputState>,
    /// 改动审查：代码行拖动选择
    pub diff_line_selection: Option<diff::LineSelection>,
    /// 改动审查：改动区域滚动句柄（点击文件时定位）
    pub diff_scroll: ScrollHandle,
    /// 改动审查：文件树宽度
    pub diff_tree_width: f32,
    /// 改动审查：文件树宽度拖拽起点（指针 x, 起始宽度）
    diff_tree_drag: Option<(f32, f32)>,
    /// 会话列表滚动句柄（滚动分页）
    pub list_scroll: ScrollHandle,
    /// 对话历史滚动句柄（贴底判断与滚动分页）
    pub dialog_scroll: ScrollHandle,
    pub dialog_scroll_on_entry: bool,
    /// 活动历史滚动句柄
    pub activities_scroll: ScrollHandle,
    /// 活动视图打开时是否尚需贴到最新一条（进入时默认滚动到底部）
    pub activities_scroll_on_entry: bool,
    /// 计划面板滚动句柄
    pub plan_scroll: ScrollHandle,
    /// 已展开的活动条目（key = 时间戳 + 文案，跨帧稳定）
    pub expanded_activities: HashSet<String>,
    /// 工作目录最近目录下拉是否展开
    pub workspace_recent_open: bool,
    /// 新建会话工作目录输入框的窗口坐标（浮层按输入框边缘锚定）
    pub workspace_input_bounds: Option<Bounds<Pixels>>,
    /// 工作目录面板当前查看的文件路径
    pub workspace_file: Option<String>,
    /// 工作目录面板中文件树区域是否展开
    pub workspace_tree_visible: bool,
    /// 工作目录面板中文件内容区域是否展开
    pub workspace_content_visible: bool,
    /// 工作目录面板：文件树宽度
    pub workspace_tree_width: f32,
    /// 工作目录面板：文件树宽度拖拽起点（指针 x, 起始宽度）
    workspace_tree_drag: Option<(f32, f32)>,
    /// 输入框高度（拖拽调整，docs/PRD.md「会话交互视图」：多行输入框，可拖拽高度）
    pub composer_height: f32,
    /// 输入框高度拖拽起点（指针 y, 起始高度）
    composer_drag: Option<(f32, f32)>,
    /// 左侧面板宽度（逻辑像素）
    sidebar_width: f32,
    /// 侧栏拖拽起点（指针 x, 起始宽度）
    sidebar_drag: Option<(f32, f32)>,
    /// 右侧面板宽度（逻辑像素，不含手柄）
    panel_width: f32,
    /// 面板拖拽起点（指针 x, 起始宽度）
    panel_drag: Option<(f32, f32)>,
    /// 设置浮窗的焦点锚点（Esc 关闭依赖焦点在浮窗内）
    pub settings_focus: FocusHandle,
}

impl AmuxApp {
    pub fn new(connection: Connection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let core: SharedCore = Arc::new(Mutex::new(Core::new(connection.clone())));
        // Runtime 必须由 AmuxApp 持有：drop 掉 Runtime 会终止其上所有后台任务
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio runtime 失败");

        // Esc 关闭设置浮窗：仅在浮窗持有焦点（SettingsOverlay 上下文）时生效。
        // Tab/Shift+Tab 进终端输入：action 绑定先于 key_down 监听器派发，而 gpui-component
        // 的 Root 全局绑定了 tab（焦点循环），终端不持更具体的绑定就收不到这两个键。
        cx.bind_keys([
            KeyBinding::new("escape", CloseSettingsOverlay, Some("SettingsOverlay")),
            KeyBinding::new("tab", TerminalTab, Some("Terminal")),
            KeyBinding::new("shift-tab", TerminalBackTab, Some("Terminal")),
        ]);

        let server = connection.server.clone();
        let token = connection.token.clone();
        let input = cx.new(|cx| {
            // 高度由拖拽手柄决定（不再按内容自动增高）：超出高度的内容在框内滚动，
            // 与 Web 输入区（可竖向拖拽的 textarea）一致（docs/PRD.md「会话交互视图」）
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("输入消息，Enter 发送；Shift+Enter 换行")
                .submit_on_enter(true)
        });
        let plan_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 10)
        });
        let workspace_input = cx.new(|cx| InputState::new(window, cx));
        let server_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://amux.example.com:34567"));
        let token_input = cx.new(|cx| InputState::new(window, cx).masked(true));
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

        // 工作目录输入框：手动输入时上拉框换成前缀匹配下拉框；失焦时两个浮层都收起
        // （docs/PRD.md「新建会话视图」）
        cx.subscribe(
            &workspace_input,
            |this: &mut Self, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.workspace_recent_open = false;
                    this.refresh_workspace_suggestions(cx);
                }
                InputEvent::Blur => this.dismiss_workspace_popups(cx),
                _ => {}
            },
        )
        .detach();

        // 轮询节拍：后台刷新 + 投递通知 + 同步视图状态 + 重绘
        let tick_core = Arc::clone(&core);
        let tick_runtime = runtime.handle().clone();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            tick_runtime.spawn(poll::tick(tick_core.clone()));
            if this
                .update_in(cx, |this, window, cx| {
                    this.sync_view_data();
                    this.sync_terminal_stream();
                    this.sync_paging();
                    this.sync_orchestrator_form(window, cx);
                    this.flush_notes(window, cx);
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

        // 输入框的提示只补充 label 之外的信息：label 已说明字段含义时不再重复
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));
        cx.subscribe(
            &rename_input,
            |this: &mut Self, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(cx),
                InputEvent::Blur => this.cancel_rename(cx),
                _ => {}
            },
        )
        .detach();
        let orch_base_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.openai.com/v1"));
        let orch_api_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let orch_model = cx.new(|cx| InputState::new(window, cx));
        let orch_effort = cx.new(|cx| InputState::new(window, cx));
        let quick_name = cx.new(|cx| InputState::new(window, cx));
        let quick_prompt = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("点击后作为用户输入发送的一段提示词")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let skill_name = cx.new(|cx| InputState::new(window, cx));
        let skill_desc = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("仓库 / 资源 URL 或安装方法说明")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let plan_name = cx.new(|cx| InputState::new(window, cx));
        let plan_plan = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("自然语言描述")
                .multi_line(true)
                .auto_grow(3, 8)
        });
        let project_name = cx.new(|cx| InputState::new(window, cx));
        let project_desc = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("项目描述")
                .multi_line(true)
                .auto_grow(2, 6)
        });
        let diff_comment_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入评论")
                .multi_line(true)
                .auto_grow(2, 6)
        });

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
            orchestrator_prefilled: None,
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
            project_name,
            project_desc,
            diff_comment_input,
            terminal: terminal_view::TerminalScreen::default(),
            terminal_focus: cx.focus_handle(),
            terminal_stream: None,
            attachments: Vec::new(),
            slash_selected: 0,
            slash_dismissed: false,
            diff_collapsed_files: HashSet::new(),
            diff_collapsed_dirs: HashSet::new(),
            diff_tree_visible: true,
            diff_comment_target: None,
            diff_line_selection: None,
            diff_scroll: ScrollHandle::new(),
            diff_tree_width: panels::DIFF_TREE_WIDTH,
            diff_tree_drag: None,
            list_scroll: ScrollHandle::new(),
            dialog_scroll: ScrollHandle::new(),
            dialog_scroll_on_entry: true,
            activities_scroll: ScrollHandle::new(),
            activities_scroll_on_entry: false,
            plan_scroll: ScrollHandle::new(),
            expanded_activities: HashSet::new(),
            workspace_recent_open: false,
            workspace_input_bounds: None,
            workspace_file: None,
            workspace_tree_visible: true,
            workspace_content_visible: true,
            workspace_tree_width: panels::WORKSPACE_TREE_WIDTH,
            workspace_tree_drag: None,
            composer_height: INPUT_DEFAULT_HEIGHT,
            composer_drag: None,
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_drag: None,
            panel_width: 0.0,
            panel_drag: None,
            settings_focus: cx.focus_handle(),
        }
    }

    pub fn with_core<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        let mut core = self.core.lock();
        f(&mut core)
    }

    /// 把后台排队的提示投递为通知（后台任务无窗口，只能在有窗口的节拍里投递）。
    fn flush_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(note) = self.with_core(|core| core.notes.pop_front()) {
            window.push_notification(dialog::note_notification(note), cx);
        }
    }

    /// 记录输入框高度拖拽起点（docs/PRD.md「会话交互视图」：多行输入框，可拖拽高度）。
    pub fn begin_composer_resize(&mut self, pointer_y: f32) {
        self.composer_drag = Some((pointer_y, self.composer_height));
    }

    /// 拖拽调整输入框高度：向上拖（y 变小）变高；上限取窗口高度，避免把输入区顶出窗口。
    pub fn resize_composer(&mut self, pointer_y: f32, window: &Window, cx: &mut Context<Self>) {
        let Some((origin, initial)) = self.composer_drag else {
            return;
        };
        self.composer_height = (initial + (origin - pointer_y))
            .clamp(INPUT_MIN_HEIGHT, window.bounds().size.height.as_f32());
        cx.notify();
    }

    /// 打开设置浮窗；`tab` 为 `None` 时保持当前分类。
    pub fn open_settings(
        &mut self,
        tab: Option<crate::state::SettingsTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let switched = self.with_core(|core| {
            let switched = tab.is_some() && core.settings_tab != tab.unwrap();
            if let Some(tab) = tab {
                core.settings_tab = tab;
            }
            core.settings_open = true;
            switched
        });
        if switched
            && self.with_core(|core| core.settings_tab) == crate::state::SettingsTab::Orchestrator
        {
            self.load_orchestrator_form(window, cx);
        }
        // 设置项在打开时实时获取，不做定时刷新（docs/DESIGN.md「设置页面」）
        self.load_view_data();
        // 焦点落在浮窗上，Esc（SettingsOverlay 上下文）才能被浮窗接收
        window.focus(&self.settings_focus, cx);
        cx.notify();
    }

    /// 关闭设置浮窗。
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.with_core(|core| core.settings_open = false);
        cx.notify();
    }

    /// 设置分类切换：切到内置智能体分类时预填已保存配置。
    pub fn select_settings_tab(
        &mut self,
        tab: crate::state::SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let switched = self.with_core(|core| {
            let switched = core.settings_tab != tab;
            core.settings_tab = tab;
            switched
        });
        if switched {
            if tab == crate::state::SettingsTab::Orchestrator {
                self.load_orchestrator_form(window, cx);
            }
            self.load_view_data();
        }
        cx.notify();
    }

    /// 拉取当前视图的打开时数据（设置浮窗 / 会话交互 / 新建会话），并记为已拉取。
    ///
    /// 设置类数据不做定时刷新（docs/DESIGN.md「设置页面」），只在视图打开时拉取一次；
    /// 视图切换与连接建立后重取由 [`Self::sync_view_data`] 补上。
    fn load_view_data(&mut self) {
        let (settings_open, tab, open) =
            self.with_core(|core| (core.settings_open, core.settings_tab, core.open.clone()));
        self.with_core(|core| core.loaded_view = Some(core.view_key()));
        if settings_open {
            self.load_settings(tab);
        } else if open.is_some() {
            self.load_interaction();
        } else {
            self.load_new_session();
        }
    }

    /// 视图切换（或连接建立）后按当前视图重新拉取一次：视图打开时的拉取此时才可能成功
    /// （应用启动即显示新建会话视图、设置浮窗关闭后回到下面的视图等）。
    fn sync_view_data(&mut self) {
        let key = self.with_core(|core| core.view_key());
        let loaded = self.with_core(|core| core.loaded_view.clone());
        if loaded.as_ref() == Some(&key) {
            return;
        }
        self.load_view_data();
    }

    /// 滚动分页：按各列表的滚动位置写入页大小并预取相邻一页，插入更早一页后锚定滚动位置
    /// （docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」「活动列表滚动机制」）。
    fn sync_paging(&mut self) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };

        let load_unassigned = {
            let core = self.core.lock();
            core.project_groups.get("").is_some_and(|group| {
                !core.collapsed_project_groups.contains("")
                    && group.has_more
                    && !group.loading
                    && self.list_scroll.max_offset().y.as_f32()
                        + self.list_scroll.offset().y.as_f32()
                        <= 48.0
            })
        };
        if load_unassigned {
            let core = Arc::clone(&self.core);
            let client = client.clone();
            self.runtime.spawn(async move {
                poll::load_more_project_group(&client, &core, String::new(), None).await
            });
        }

        let Some(target) = self.with_core(|core| core.open.clone()) else {
            return;
        };
        let handle = self.dialog_scroll.clone();
        let loaded = self.with_core(|core| core.view.detail.history.len());
        if !self.dialog_scroll_on_entry
            && self.sync_list_paging(&handle, loaded, true, |core| {
                &mut core.view.detail.history_paging
            })
        {
            let core = Arc::clone(&self.core);
            let client = client.clone();
            let target = target.clone();
            self.runtime
                .spawn(async move { poll::load_older_history(&client, &core, &target).await });
        }

        if self.with_core(|core| core.side_panel == Some(SidePanel::Activities)) {
            let handle = self.activities_scroll.clone();
            let loaded = self.with_core(|core| core.view.detail.activities.len());
            if !self.activities_scroll_on_entry
                && self.sync_list_paging(&handle, loaded, true, |core| {
                    &mut core.view.detail.activities_paging
                })
            {
                let core = Arc::clone(&self.core);
                self.runtime.spawn(async move {
                    poll::load_older_activities(&client, &core, &target).await
                });
            }
        }
    }

    /// 单个列表的分页推进：写入页大小，并在视口贴近已加载窗口的「更早」一侧时返回 true
    /// 让调用方拉取更早一页；`older_at_top` 表示更早的条目显示在顶部。
    fn sync_list_paging(
        &mut self,
        handle: &ScrollHandle,
        loaded: usize,
        older_at_top: bool,
        pick: fn(&mut Core) -> &mut Paging,
    ) -> bool {
        let top = handle.top_item();
        let bottom = handle.bottom_item();
        let page_size = page_size(handle, loaded);
        self.with_core(|core| {
            let paging = pick(core);
            paging.page_size = page_size;
            // 更早一页已插入：按位移量把原首条目滚回视口顶部，保持阅读位置不动
            if !paging.loading_older {
                if let Some(shift) = paging.shift.take() {
                    handle.scroll_to_top_of_item(top + shift);
                }
            }
            // 预取相邻一页：视口进入窗口「更早」一侧的一页之内即拉取
            let near_older_edge = loaded > 0
                && if older_at_top {
                    top <= page_size
                } else {
                    bottom + page_size >= loaded
                };
            paging.has_older && !paging.loading_older && near_older_edge
        })
    }

    /// 拉取设置分类的数据（打开浮窗或切换分类时触发）。
    fn load_settings(&mut self, tab: crate::state::SettingsTab) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime
            .spawn(async move { poll::refresh_settings(&client, &core, tab).await });
    }

    /// 表单保留工作流模式时，同时重新检查内置智能体配置。
    fn load_new_session(&mut self) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        let workflow_mode = self.with_core(|core| core.new_session.workflow_mode);
        self.runtime.spawn(async move {
            poll::refresh_new_session(&client, &core).await;
            if workflow_mode {
                poll::refresh_workflow_setup(&client, &core).await;
            }
        });
    }

    /// 拉取工作流模式所需数据（切换到工作流模式时）。
    pub fn load_workflow_setup(&mut self) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime
            .spawn(async move { poll::refresh_workflow_setup(&client, &core).await });
    }

    /// 拉取会话交互视图的常驻数据：机器/agents、内置智能体配置与快捷指令。
    fn load_interaction(&mut self) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime
            .spawn(async move { poll::refresh_interaction(&client, &core).await });
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
                Err(error) => core.lock().error(format!("重新发现失败：{error}")),
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
                Ok(()) => core.lock().success(format!("已重启 {agent}@{machine}")),
                Err(error) => core.lock().error(format!("重启失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 终端按键：粘贴、控制键与可打印字符转字节序列上行。
    pub fn terminal_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        // 剪贴板是 IME 之外输入中文的主要途径（macOS 用 Cmd+V）
        let paste_modifier = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control
        };
        if paste_modifier && event.keystroke.key == "v" {
            cx.stop_propagation();
            self.paste_into_terminal(cx);
            return;
        }
        let Some(bytes) = terminal_view::keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        self.send_terminal_input(bytes, cx);
    }

    /// 粘贴剪贴板文本：终端处于 bracketed paste 模式（应用开启 DECSET 2004）时原样
    /// 包裹发送，多行粘贴不会被逐行当作回车执行；否则换行归一为 `\r`。
    fn paste_into_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let bytes = if self.terminal.bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        };
        self.send_terminal_input(bytes, cx);
    }

    /// 终端滚轮：向上（delta.y 为负）进入回滚历史。
    pub fn terminal_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let line_height = px(terminal_view::CELL_HEIGHT);
        let lines = (-event.delta.pixel_delta(line_height).y / line_height).round() as i32;
        if lines == 0 {
            return;
        }
        self.terminal.scroll(lines);
        cx.notify();
    }

    /// 查看文件内容（工作目录面板）。
    pub fn read_file(&mut self, machine: String, path: String, cx: &mut Context<Self>) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        self.workspace_file = Some(path.clone());
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.read_file(&machine, &path, 400, 0).await {
                Ok(result) => core.lock().view.detail.file_content = Some(result.content),
                Err(error) => core.lock().error(format!("读取文件失败：{error}")),
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
                core.lock().error(format!("关闭终端失败：{error}"));
            }
            let mut core = core.lock();
            if core.view.detail.active_terminal.as_deref() == Some(terminal.as_str()) {
                core.view.detail.active_terminal = None;
                core.view.detail.terminal_output.reset();
            }
        });
        cx.notify();
    }

    /// 终端可视区尺寸变化时同步 VT 网格与 PTY 行列（PTY 尺寸未变则不发请求）。
    ///
    /// 布局未就绪时画布 bounds 可能退化为 0：此时直接忽略，绝不能把 0 除字宽后
    /// clamp 成最小行列——那会把 PTY 缩到不可用尺寸并丢掉输出。
    pub fn sync_terminal_size(&mut self, width: Pixels, height: Pixels, cx: &mut Context<Self>) {
        let cell_width = px(terminal_view::CELL_WIDTH);
        let cell_height = px(terminal_view::CELL_HEIGHT);
        if width < cell_width || height < cell_height {
            return;
        }
        let cols = ((width.as_f32() / terminal_view::CELL_WIDTH).floor() as u16)
            .clamp(terminal_view::MIN_COLS, terminal_view::MAX_COLS);
        let rows = ((height.as_f32() / terminal_view::CELL_HEIGHT).floor() as u16)
            .clamp(terminal_view::MIN_ROWS, terminal_view::MAX_ROWS);
        self.terminal.resize(cols, rows);
        let (active, current) = self.with_core(|core| {
            let active = core.view.detail.active_terminal.clone();
            let current = active.as_ref().and_then(|id| {
                core.view
                    .detail
                    .terminals
                    .iter()
                    .find(|terminal| &terminal.id == id)
                    .map(|terminal| (terminal.cols, terminal.rows))
            });
            (active, current)
        });
        let (Some(terminal), Some(current)) = (active, current) else {
            return;
        };
        if current != (cols, rows) {
            self.resize_terminal(terminal, cols, rows, cx);
        }
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
                core.lock().error(format!("调整终端尺寸失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 打开新建会话视图：实时拉取机器、agents 与最近工作目录
    /// （docs/DESIGN.md「新建会话视图」）。
    pub fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.dialog_scroll_on_entry = true;
        self.with_core(|core| {
            core.open = None;
            core.side_panel = None;
            core.view = Default::default();
        });
        self.load_view_data();
        cx.notify();
    }

    /// 打开列表条目（普通会话或工作流会话）。
    pub fn open_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_workflow =
            self.with_core(|core| matches!(core.entry(id), Some(ListEntry::Workflow(_))));
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
        self.diff_comment_target = None;
        self.diff_line_selection = None;
        self.expanded_activities.clear();
        self.workspace_file = None;
        self.workspace_tree_visible = true;
        self.dialog_scroll_on_entry = true;
        self.activities_scroll_on_entry = true;
        self.load_view_data();
        cx.notify();
    }

    /// 开始行内重命名。
    pub fn begin_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.with_core(|core| core.entry(id).map_or(String::new(), |e| e.title()));
        self.renaming_id = Some(id.to_string());
        let input = self.rename_input.clone();
        input.update(cx, |state, cx| {
            state.set_value(current, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// 取消行内重命名。
    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming_id = None;
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
            core.entry(&id).map(|entry| match entry {
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
            if matches!(entry, ListEntry::Workflow(_)) {
                "删除工作流会话"
            } else {
                "删除会话"
            },
            format!(
                "「{label}」将被删除，{}",
                if matches!(entry, ListEntry::Workflow(_)) {
                    "其关联的普通会话也会一并删除，此操作不可撤销。"
                } else {
                    "会话记录与其 worktree 会被清理，此操作不可撤销。"
                }
            ),
            "删除",
            ButtonVariant::Danger,
            move |this, cx| this.delete_entry(entry.clone(), cx),
        );
    }

    /// 切换当前终端并重建本地输出缓冲。
    pub fn select_terminal(&mut self, terminal: String, cx: &mut Context<Self>) {
        self.with_core(|core| {
            core.view.detail.active_terminal = Some(terminal);
            core.view.detail.terminal_output.reset();
        });
        cx.notify();
    }

    /// 终端 SSE 只在普通会话的终端面板打开且已选择终端时存在；连接身份变化即重建。
    fn sync_terminal_stream(&mut self) {
        let next = self.with_core(|core| {
            if core.status != ConnectionStatus::Online
                || core.side_panel != Some(SidePanel::Terminal)
            {
                return None;
            }
            match (&core.open, &core.view.detail.active_terminal) {
                (Some(OpenTarget::Session(session)), Some(terminal)) => Some(TerminalStreamKey {
                    session: session.clone(),
                    terminal: terminal.clone(),
                }),
                _ => None,
            }
        });
        if self
            .terminal_stream
            .as_ref()
            .is_some_and(|(key, _)| Some(key) == next.as_ref())
        {
            return;
        }
        if let Some((_, task)) = self.terminal_stream.take() {
            task.abort();
        }
        let Some(key) = next else {
            return;
        };
        let Some(client) = self.with_core(|core| core.client.clone()) else {
            return;
        };
        let core = Arc::clone(&self.core);
        let task = self.runtime.spawn({
            let key = key.clone();
            async move { stream_terminal_output(client, core, key).await }
        });
        self.terminal_stream = Some((key, task));
    }

    /// 加载工作目录树根节点；每次打开面板都实时拉取，不缓存
    /// （docs/DESIGN.md「工作目录视图」）。
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
            match client.list_dir(&machine, Some(&path), 500, 0, false).await {
                Ok(result) => {
                    let mut core = core.lock();
                    core.view.detail.workspace_tree =
                        result.entries.into_iter().map(WorkspaceNode::new).collect();
                }
                Err(error) => core.lock().error(format!("读取工作目录失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 展开/折叠工作目录树节点；展开时实时拉取子目录项，不缓存
    /// （docs/DESIGN.md「工作目录视图」）。
    pub fn toggle_workspace_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let (client, machine, expanded) = self.with_core(|core| {
            let expanded = WorkspaceNode::find_mut(&mut core.view.detail.workspace_tree, &path)
                .map(|node| {
                    node.expanded = !node.expanded;
                    if !node.expanded {
                        // 折叠即丢弃已拉取内容：目录项不缓存
                        node.children = None;
                    }
                    node.expanded
                })
                .unwrap_or(false);
            let machine = core.view.session.as_ref().map(|s| s.machine.clone());
            (core.client.clone(), machine, expanded)
        });
        let (Some(client), Some(machine)) = (client, machine) else {
            cx.notify();
            return;
        };
        if !expanded {
            cx.notify();
            return;
        }
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.list_dir(&machine, Some(&path), 500, 0, false).await {
                Ok(result) => {
                    let mut core = core.lock();
                    if let Some(node) =
                        WorkspaceNode::find_mut(&mut core.view.detail.workspace_tree, &path)
                    {
                        node.children =
                            Some(result.entries.into_iter().map(WorkspaceNode::new).collect());
                    }
                }
                Err(error) => core.lock().error(format!("读取目录失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 删除最近工作目录项：移除后全量保存（docs/PRD.md「新建会话视图」删除按钮「x」）。
    pub fn delete_recent_workspace(&mut self, target: RecentWorkspace, cx: &mut Context<Self>) {
        let list = self.with_core(|core| {
            core.recent_workspaces
                .iter()
                .filter(|item| {
                    !(item.machine == target.machine && item.workspace == target.workspace)
                })
                .cloned()
                .collect()
        });
        self.save_list(SettingsList::RecentWorkspaces(list), cx);
    }

    /// 选定最近工作目录：填入并收起上下拉框（最近目录是选定项，不再联想）。
    pub fn select_recent_workspace(
        &mut self,
        target: RecentWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_input.update(cx, |state, cx| {
            state.set_value(target.workspace.clone(), window, cx)
        });
        self.with_core(|core| {
            core.new_session.project = target.last_used_project.filter(|project| {
                core.settings
                    .projects
                    .iter()
                    .any(|candidate| &candidate.name == project)
            });
        });
        self.dismiss_workspace_popups(cx);
    }

    /// 点选前缀联想项：填入该目录并继续列出其下的条目，用户可一路点选下钻
    /// （docs/DESIGN.md「新建会话视图」：输入以 `/` 结尾时列出该目录全部条目）。
    pub fn set_workspace(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_input
            .update(cx, |state, cx| state.set_value(path, window, cx));
        self.refresh_workspace_suggestions(cx);
        cx.notify();
    }

    /// 打开发起新增表单（设置各分类右上角「+」）。
    pub fn open_new_form(
        &mut self,
        tab: crate::state::SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tab {
            crate::state::SettingsTab::QuickCommands => {
                self.open_quick_command_form(FormTarget::New, window, cx)
            }
            crate::state::SettingsTab::Skills => self.open_skill_form(FormTarget::New, window, cx),
            crate::state::SettingsTab::WorkflowPlans => {
                self.open_plan_form(FormTarget::New, window, cx)
            }
            crate::state::SettingsTab::Projects => {
                self.open_project_form(FormTarget::New, window, cx)
            }
            _ => {}
        }
    }

    /// 填入工作流计划（已保存计划）。
    pub fn set_plan(
        &mut self,
        plan: WorkflowPlanItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.plan_input
            .update(cx, |state, cx| state.set_value(plan.plan, window, cx));
        self.with_core(|core| {
            core.new_session.project = plan.last_used_project.filter(|project| {
                core.settings
                    .projects
                    .iter()
                    .any(|candidate| &candidate.name == project)
            });
        });
        cx.notify();
    }

    /// 展开工作目录最近目录上拉框：收起前缀匹配项，两者不叠加（进行中的联想应答也会因
    /// 已拉取目录被清空而丢弃）。已有输入时保持手动输入流程，不重新展示最近目录。
    pub fn show_recent_workspaces(&mut self, cx: &mut Context<Self>) {
        if !self.workspace_input.read(cx).value().trim().is_empty() {
            return;
        }
        self.workspace_recent_open = true;
        self.with_core(|core| {
            core.new_session.suggestions.clear();
            core.new_session.suggestion = None;
        });
        cx.notify();
    }

    /// 收起工作目录的上下拉框（输入框失焦、点击浮层之外）。
    pub fn dismiss_workspace_popups(&mut self, cx: &mut Context<Self>) {
        self.workspace_recent_open = false;
        self.with_core(|core| {
            core.new_session.suggestions.clear();
            core.new_session.suggestion = None;
        });
        cx.notify();
    }

    /// 刷新前缀匹配的目录项（docs/DESIGN.md「新建会话视图」）：输入 `…/tom/` 列出该目录下
    /// 全部条目；输入 `…/tom` 列出其父目录下与 `tom` 前缀匹配的条目。
    ///
    /// 目录变了才重新拉取（边界处触发，且一次拉全该目录的条目），目录没变时直接用已拉取的
    /// 条目按最新前缀过滤，避免同一个目录在输入过程中被反复拉取。应答回来时若目录或机器已变
    /// 则丢弃。联想优先于最近目录下拉，避免两层浮层叠加。
    fn refresh_workspace_suggestions(&mut self, cx: &mut Context<Self>) {
        let text = self.workspace_input.read(cx).value().to_string();
        let machine = self.with_core(|core| core.new_session.machine.clone());
        let parsed = ui::split_dir_query(&text);
        let (Some((dir, prefix)), Some(machine)) = (parsed, machine) else {
            self.with_core(|core| {
                core.new_session.suggestions.clear();
                core.new_session.suggestion = None;
            });
            cx.notify();
            return;
        };
        // 还在同一个目录里输入：用已拉取的条目做前缀匹配，不再拉取
        let listed = self.with_core(|core| {
            core.new_session
                .suggestion
                .as_ref()
                .is_some_and(|listing| listing.machine == machine && listing.dir == dir)
        });
        if listed {
            self.with_core(|core| {
                core.new_session.suggestion_prefix = prefix.to_string();
                let entries = core
                    .new_session
                    .suggestion
                    .as_ref()
                    .map(|listing| listing.entries.clone())
                    .unwrap_or_default();
                core.new_session.suggestions = matching_prefix(entries, prefix);
            });
            cx.notify();
            return;
        }
        self.workspace_recent_open = false;
        let dir = dir.to_string();
        let prefix = prefix.to_string();
        self.with_core(|core| {
            // 先占位：新目录的条目到达前不展示上一个目录的项，也不为它重复拉取
            core.new_session.suggestion = Some(DirectoryListing {
                machine: machine.clone(),
                dir: dir.clone(),
                entries: Vec::new(),
            });
            core.new_session.suggestion_prefix = prefix;
            core.new_session.suggestions.clear();
        });
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = client.list_all_dirs(&machine, &dir).await;
            let mut core = core.lock();
            // 用户可能已经继续输入、换了目录或换了机器：过期应答不落地
            let stale = core
                .new_session
                .suggestion
                .as_ref()
                .is_none_or(|listing| listing.machine != machine || listing.dir != dir);
            if stale {
                return;
            }
            match result {
                Ok(entries) => {
                    let prefix = core.new_session.suggestion_prefix.clone();
                    core.new_session.suggestions = matching_prefix(entries.clone(), &prefix);
                    core.new_session.suggestion = Some(DirectoryListing {
                        machine,
                        dir,
                        entries,
                    });
                }
                // 拉取失败：撤掉占位，下次输入时重试
                Err(_) => {
                    core.new_session.suggestion = None;
                    core.new_session.suggestions.clear();
                }
            }
        });
        cx.notify();
    }

    /// 保存连接设置：写入 `~/.amux/app/server.json` 并重建客户端；结果以通知提示
    /// （docs/PRD.md「连接设置」）。
    pub fn save_connection(&mut self, cx: &mut Context<Self>) {
        match self.save_input_connection(cx) {
            Ok(()) => self.with_core(|core| core.success("连接设置已保存")),
            Err(error) => self.with_core(|core| core.error(format!("保存失败：{error}"))),
        }
        cx.notify();
    }

    /// 登录页「进入」：写入连接信息并立即尝试连接；连接成功后由连接状态切到主页面
    /// （docs/PRD.md「登录页面」）。
    pub fn login(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.save_input_connection(cx) {
            self.with_core(|core| core.error(format!("保存失败：{error}")));
            cx.notify();
        }
    }

    /// 落盘输入框中的连接信息并重建客户端；下一次连接检查立即进行（连接检查本身按周期节流）。
    fn save_input_connection(&mut self, cx: &App) -> Result<(), String> {
        let connection = Connection {
            server: self.server_input.read(cx).value().trim().to_string(),
            token: self.token_input.read(cx).value().trim().to_string(),
        };
        config::save(&config::connection_path(), &connection)?;
        self.with_core(|core| {
            core.apply_connection(connection);
            core.last.list = None;
        });
        self.settings_dirty = false;
        Ok(())
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
        self.dialog_scroll.scroll_to_bottom();
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match poll::send_prompt(&client, &target, blocks).await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.last.list = None;
                    core.last.history = None;
                }
                Err(error) => core.lock().error(format!("消息未发送：{error}")),
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

    /// 打开系统文件选择窗口并添加附件。
    pub fn pick_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("选择附件".into()),
        });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let _ =
                    cx.update(|_, cx| this.update(cx, |this, cx| this.attach_paths(&paths, cx)));
            })
            .detach();
    }

    pub fn remove_attachment(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.attachments.len() {
            self.attachments.remove(ix);
        }
        cx.notify();
    }

    pub fn clear_attachments(&mut self, cx: &mut Context<Self>) {
        self.attachments.clear();
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
            if let Err(error) = client
                .configure_session(&id, None, Some(setting), None)
                .await
            {
                core.lock().error(format!("会话选项设置失败：{error}"));
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
                core.lock().error(format!("取消失败：{error}"));
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
        let selected_plan = if form.workflow_mode {
            self.with_core(|core| {
                core.settings
                    .plans
                    .iter()
                    .find(|item| item.plan == plan)
                    .map(|item| item.name.clone())
            })
        } else {
            None
        };
        let plan_project = form.project.clone();
        let task = if form.workflow_mode {
            if plan.is_empty() {
                return;
            }
            self.runtime.spawn(async move {
                client
                    .create_workflow(&plan, None, form.project.clone())
                    .await
                    .map(|workflow| OpenTarget::Workflow(workflow.id))
            })
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
                project: form.project.clone(),
            };
            self.runtime.spawn(async move {
                client
                    .create_session(&request)
                    .await
                    .map(|session| OpenTarget::Session(session.id))
            })
        };
        cx.spawn(async move |this, cx| {
            let Ok(result) = task.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(target) => {
                        if let Some(plan_name) = selected_plan {
                            let mut plans = this.with_core(|core| core.settings.plans.clone());
                            if let Some(item) = plans.iter_mut().find(|item| item.name == plan_name)
                            {
                                item.last_used_project = plan_project;
                                this.save_list(SettingsList::Plans(plans), cx);
                            }
                        }
                        this.with_core(|core| {
                            let workflow_mode = core.new_session.workflow_mode;
                            core.new_session = Default::default();
                            core.new_session.workflow_mode = workflow_mode;
                            core.last.list = None;
                            match target {
                                OpenTarget::Session(id) => poll::open_session(core, &id),
                                OpenTarget::Workflow(id) => poll::open_workflow(core, &id),
                            }
                        });
                        this.workspace_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        this.plan_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    Err(error) => {
                        this.with_core(|core| core.error(format!("创建会话失败：{error}")))
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
                        core.side_panel = None;
                    }
                    core.last.list = None;
                    core.success("已删除");
                }
                Err(error) => core.lock().error(format!("删除失败：{error}")),
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
                OpenTarget::Session(id) => {
                    client.configure_session(id, Some(title), None, None).await
                }
                OpenTarget::Workflow(id) => client.configure_workflow(id, Some(title), None).await,
            };
            match result {
                Ok(()) => core.lock().last.list = None,
                Err(error) => core.lock().error(format!("重命名失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 设置会话所属项目（None = 未归属；docs/PRD.md「会话列表视图」拖拽/菜单切换项目）。
    pub fn set_entry_project(
        &mut self,
        entry: ListEntry,
        project: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            let result = match &entry {
                ListEntry::Session(session) => {
                    client
                        .configure_session(&session.id, None, None, project)
                        .await
                }
                ListEntry::Workflow(workflow) => {
                    client.configure_workflow(&workflow.id, None, project).await
                }
            };
            match result {
                Ok(()) => core.lock().last.list = None,
                Err(error) => core.lock().error(format!("设置所属项目失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 切换右侧面板：已打开则收起，否则打开并加载（docs/PRD.md 右侧面板）。
    pub fn toggle_side_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        let current = self.with_core(|core| core.side_panel);
        if current == Some(panel) {
            self.with_core(|core| core.side_panel = None);
            self.panel_width = 0.0;
            cx.notify();
            return;
        }
        self.open_side_panel(panel, cx);
    }

    /// 打开右侧面板：面板数据立即刷新，工作目录、会话详情与终端在打开时加载。
    pub fn open_side_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        let previous = self.with_core(|core| core.side_panel);
        self.with_core(|core| {
            core.side_panel = Some(panel);
            match panel {
                SidePanel::Activities => core.last.activities = None,
                SidePanel::Plan => core.last.plan = None,
                _ => {}
            }
        });
        if panel == SidePanel::Activities {
            self.activities_scroll_on_entry = true;
        }
        // 在已打开的面板之间切换时保留用户调整后的宽度；首次打开用默认宽度
        if previous.is_none() || previous == Some(panel) {
            self.panel_width = panel.default_width();
        }
        match panel {
            SidePanel::Workspace => self.load_workspace(cx),
            SidePanel::Terminal => self.open_terminal(cx),
            SidePanel::Diff => self.refresh_diff(cx),
            SidePanel::Detail => self.refresh_context(cx),
            _ => {}
        }
        cx.notify();
    }

    /// 打开终端面板：刷新终端列表并选中当前（或首个）终端；没有终端时不自动新建
    /// （新建只由标签栏的 + 触发，docs/PRD.md「终端」只要求「可创建多个终端」）。
    pub fn open_terminal(&mut self, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        self.with_core(|core| core.side_panel = Some(SidePanel::Terminal));
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Ok(terminals) = client.terminals(&id).await {
                crate::state::set_terminals(&mut core.lock(), terminals);
            }
        });
        cx.notify();
    }

    /// 新建终端：无论已有多少终端都再创建一个（docs/PRD.md「可创建多个终端」）。
    pub fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        self.with_core(|core| core.side_panel = Some(SidePanel::Terminal));
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            create_terminal(&client, &core, &id).await;
        });
        cx.notify();
    }

    /// 终端输入（原始字节，base64 上行）。
    pub fn send_terminal_input(&mut self, data: Vec<u8>, cx: &mut Context<Self>) {
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
                core.lock().error(format!("终端输入失败：{error}"));
            }
        });
        cx.notify();
    }

    /// 刷新会话上下文用量；视图打开时刷新一次，不做定时刷新
    /// （docs/DESIGN.md「会话详情视图」）。
    pub fn refresh_context(&mut self, cx: &mut Context<Self>) {
        let (client, open) = self.with_core(|core| (core.client.clone(), core.open.clone()));
        let (Some(client), Some(OpenTarget::Session(id))) = (client, open) else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            if let Ok(info) = client.context(&id).await {
                let mut core = core.lock();
                core.view.detail.context_size = info.context_size;
                core.view.detail.context_window_size = info.context_window_size;
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
                Err(error) => core.lock().error(format!("无法加载改动：{error}")),
            }
        });
        cx.notify();
    }

    /// 折叠/展开文件树中的目录。
    pub fn toggle_diff_dir(&mut self, key: String, cx: &mut Context<Self>) {
        toggle_set(&mut self.diff_collapsed_dirs, key);
        cx.notify();
    }

    /// 折叠/展开工作流下的关联普通会话。
    pub fn toggle_workflow(&mut self, id: &str, cx: &mut Context<Self>) {
        self.with_core(|core| {
            if !core.expanded_workflows.remove(id) {
                core.expanded_workflows.insert(id.to_string());
            }
        });
        cx.notify();
    }

    /// 折叠/展开工作目录面板的文件树区域。
    pub fn toggle_workspace_tree(&mut self, cx: &mut Context<Self>) {
        self.workspace_tree_visible = !self.workspace_tree_visible;
        cx.notify();
    }

    /// 折叠/展开整个文件树区域。
    /// 折叠/展开工作目录视图的文件内容区域（docs/PRD.md「工作目录视图」）。
    pub fn toggle_workspace_content(&mut self, cx: &mut Context<Self>) {
        self.workspace_content_visible = !self.workspace_content_visible;
        cx.notify();
    }

    /// 记录工作目录文件树宽度拖拽起点。
    pub fn begin_workspace_tree_resize(&mut self, pointer_x: f32) {
        self.workspace_tree_drag = Some((pointer_x, self.workspace_tree_width));
    }

    /// 调整工作目录文件树宽度（docs/PRD.md「工作目录视图」）。
    pub fn resize_workspace_tree(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin, initial)) = self.workspace_tree_drag else {
            return;
        };
        self.workspace_tree_width = tree_width(initial + pointer_x - origin, self.panel_width);
        cx.notify();
    }

    pub fn toggle_diff_tree(&mut self, cx: &mut Context<Self>) {
        self.diff_tree_visible = !self.diff_tree_visible;
        cx.notify();
    }

    /// 记录改动文件树宽度拖拽起点。
    pub fn begin_diff_tree_resize(&mut self, pointer_x: f32) {
        self.diff_tree_drag = Some((pointer_x, self.diff_tree_width));
    }

    /// 调整改动文件树宽度（docs/PRD.md「改动审查视图」）。
    pub fn resize_diff_tree(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin, initial)) = self.diff_tree_drag else {
            return;
        };
        self.diff_tree_width = tree_width(initial + pointer_x - origin, self.panel_width);
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

    /// 点击文件：右侧改动区域滚动到该文件。
    pub fn scroll_to_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.diff_scroll.scroll_to_top_of_item(ix);
        cx.notify();
    }

    /// 折叠/展开会话列表项目组。
    pub fn toggle_project_group(&mut self, key: String, cx: &mut Context<Self>) {
        let expanded = {
            let mut core = self.core.lock();
            if core.collapsed_project_groups.remove(&key) {
                true
            } else {
                core.collapsed_project_groups.insert(key.clone());
                false
            }
        };
        if expanded {
            self.load_project_group(key, cx);
        }
        cx.notify();
    }

    /// 项目组末尾「显示更多」：该组多展示一页。
    pub fn show_more_project_group(&mut self, key: String, cx: &mut Context<Self>) {
        let (client, project) = {
            let core = self.core.lock();
            let Some(client) = core.client.clone() else {
                return;
            };
            let project = (!key.is_empty()).then(|| key.clone());
            (client, project)
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            poll::load_more_project_group(&client, &core, key, project).await
        });
        cx.notify();
    }

    /// 展开项目组时立即加载其已保存的分页窗口。
    fn load_project_group(&mut self, key: String, cx: &mut Context<Self>) {
        let (client, project, limit) = {
            let core = self.core.lock();
            let Some(client) = core.client.clone() else {
                return;
            };
            let project = (!key.is_empty()).then(|| key.clone());
            let page = poll::project_group_page_size(project.as_deref());
            let limit = core
                .project_groups
                .get(&key)
                .map(|group| group.loaded.max(page))
                .unwrap_or(page);
            (client, project, limit)
        };
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            poll::refresh_project_group(&client, &core, key, project, limit).await
        });
        cx.notify();
    }

    /// 开始评论整个文件。
    pub fn begin_diff_file_comment(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_diff_comment(diff::CommentTarget::File { path }, window, cx);
    }

    /// 开始拖动选择代码行。
    pub fn begin_diff_line_selection(&mut self, line: diff::LineRef, cx: &mut Context<Self>) {
        self.diff_comment_target = None;
        self.diff_line_selection = Some(diff::LineSelection {
            start: line.clone(),
            end: line,
        });
        cx.notify();
    }

    /// 扩展同一 hunk 内的代码行选择。
    pub fn extend_diff_line_selection(&mut self, line: diff::LineRef, cx: &mut Context<Self>) {
        let Some(selection) = self.diff_line_selection.as_mut() else {
            return;
        };
        if selection.start.path != line.path || selection.start.hunk_header != line.hunk_header {
            return;
        }
        selection.end = line;
        cx.notify();
    }

    /// 松开鼠标：把选中行作为评论目标并展开输入框。
    pub fn finish_diff_line_selection(
        &mut self,
        hunk: GitDiffHunk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.diff_line_selection.take() else {
            return;
        };
        let start = selection.start.line.min(selection.end.line);
        let end = selection.start.line.max(selection.end.line);
        let code = hunk
            .lines
            .iter()
            .skip(start)
            .take(end.saturating_sub(start) + 1)
            .map(|line| format!("{}{}", line.kind.prefix(), line.text))
            .collect::<Vec<_>>()
            .join("\n");
        self.open_diff_comment(
            diff::CommentTarget::Code {
                path: selection.start.path,
                hunk_header: hunk.header,
                end_line: end,
                code,
            },
            window,
            cx,
        );
    }

    /// 取消评论，同时清除拖动选择。
    pub fn cancel_diff_comment(&mut self, cx: &mut Context<Self>) {
        self.diff_comment_target = None;
        self.diff_line_selection = None;
        cx.notify();
    }

    /// 评论作为用户消息发送到会话。
    pub fn submit_diff_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let comment = self.diff_comment_input.read(cx).value().trim().to_string();
        let Some(target) = self.diff_comment_target.take() else {
            return;
        };
        if comment.is_empty() {
            self.diff_comment_target = Some(target);
            return;
        }
        self.diff_comment_input
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        self.diff_line_selection = None;
        self.send_blocks(
            vec![ContentBlock::Text {
                text: target.message(&comment),
            }],
            cx,
        );
    }

    fn open_diff_comment(
        &mut self,
        target: diff::CommentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_comment_target = Some(target);
        self.diff_comment_input.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
            state.focus(window, cx);
        });
        cx.notify();
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

    /// 技能安装/更新/卸载：先选目标机器与 agent（docs/PRD.md「技能管理设置」）。
    ///
    /// 目标列表与设置项一样在打开时实时获取，弹窗内容每帧按最新数据重建。
    pub fn open_skill_action_form(
        &mut self,
        skill: Skill,
        action: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.with_core(|core| core.client.clone());
        if let Some(client) = client {
            let core = Arc::clone(&self.core);
            self.runtime.spawn(async move {
                poll::refresh_machines(&client, &core).await;
            });
        }
        let app = cx.entity();
        let width = rems(32.5).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let skill = skill.clone();
            dialog
                .title(format!("{action}技能"))
                .width(width)
                .footer(
                    Button::new("skill-action-cancel")
                        .small()
                        .label("取消")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .content(move |content, _, cx| {
                    app.update(cx, |this, cx| {
                        let targets = this.skill_targets();
                        content.child(skill_target_card(&targets, &skill, action, cx))
                    })
                })
        });
        cx.notify();
    }

    /// 技能操作的候选目标：机器 · agent（未拉取到机器时为 空）。
    fn skill_targets(&self) -> Vec<(String, Vec<Agent>)> {
        self.with_core(|core| {
            core.settings
                .machines
                .iter()
                .map(|machine| {
                    let agents = core
                        .settings
                        .agents
                        .iter()
                        .find(|(name, _)| name == &machine.name)
                        .map(|(_, agents)| agents.clone())
                        .unwrap_or_default();
                    (machine.name.clone(), agents)
                })
                .collect()
        })
    }

    /// 安装/更新/卸载技能：由应用侧在目标机器与 agent 上发起临时目录会话并发送指令
    /// （docs/DESIGN.md「技能操作」）。
    pub fn apply_skill(
        &mut self,
        skill: Skill,
        action: &str,
        machine: String,
        agent: String,
        cx: &mut Context<Self>,
    ) {
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        settings::manage_skill(
            client,
            self.runtime.handle().clone(),
            Arc::clone(&self.core),
            skill,
            action.to_string(),
            machine,
            agent,
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
                SettingsList::RecentWorkspaces(list) => client.set_recent_workspaces(list).await,
                SettingsList::Projects(_) => Ok(()),
            };
            match result {
                Ok(()) => {
                    // 保存成功不弹提示：列表内容与表单关闭即是反馈（失败才提示）
                    let mut core = core.lock();
                    match request {
                        SettingsList::Skills(list) => core.settings.skills = list,
                        SettingsList::QuickCommands(list) => core.settings.quick_commands = list,
                        SettingsList::Plans(list) => core.settings.plans = list,
                        SettingsList::RecentWorkspaces(list) => core.recent_workspaces = list,
                        SettingsList::Projects(_) => {}
                    }
                }
                Err(error) => core.lock().error(format!("保存失败：{error}")),
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
            title,
            "保存",
            32.5,
            vec![
                ("指令名称", self.quick_name.clone()),
                ("指令内容", self.quick_prompt.clone()),
            ],
            move |this, cx| this.save_quick_command(target.clone(), cx),
        );
    }

    /// 保存快捷指令（新增或编辑）；校验未通过时保留弹窗。
    pub fn save_quick_command(&mut self, target: FormTarget, cx: &mut Context<Self>) -> bool {
        let name = self.quick_name.read(cx).value().trim().to_string();
        let prompt = self.quick_prompt.read(cx).value().to_string();
        if name.is_empty() {
            self.with_core(|core| core.warning("请输入指令名称"));
            cx.notify();
            return false;
        }
        if prompt.trim().is_empty() {
            self.with_core(|core| core.warning("请输入指令内容"));
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
            "删除快捷指令",
            format!("快捷指令「{name}」将被删除，此操作不可撤销。"),
            "删除",
            ButtonVariant::Danger,
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
            title,
            "保存",
            32.5,
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
            self.with_core(|core| core.warning("请输入技能名称"));
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
            "删除技能",
            format!("技能「{name}」将被删除，此操作不可撤销。"),
            "删除",
            ButtonVariant::Danger,
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
            title,
            "保存",
            35.0,
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
            self.with_core(|core| core.warning("请输入计划名称"));
            cx.notify();
            return false;
        }
        if plan.trim().is_empty() {
            self.with_core(|core| core.warning("请输入计划内容"));
            cx.notify();
            return false;
        }
        let mut list = self.with_core(|core| core.settings.plans.clone());
        let last_used_project = match &target {
            FormTarget::New => None,
            FormTarget::Edit(old) => list
                .iter()
                .find(|item| &item.name == old)
                .and_then(|item| item.last_used_project.clone()),
        };
        let item = WorkflowPlanItem {
            name,
            plan,
            last_used_project,
        };
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
            "删除工作流计划",
            format!("工作流计划「{name}」将被删除，此操作不可撤销。"),
            "删除",
            ButtonVariant::Danger,
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

    /// 打开项目表单弹窗（新增 / 编辑）。
    pub fn open_project_form(
        &mut self,
        target: FormTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = match &target {
            FormTarget::New => None,
            FormTarget::Edit(name) => self.with_core(|core| {
                core.settings
                    .projects
                    .iter()
                    .find(|item| &item.name == name)
                    .cloned()
            }),
        };
        if let Some(project) = editing {
            self.project_name
                .update(cx, |state, cx| state.set_value(project.name, window, cx));
            self.project_desc.update(cx, |state, cx| {
                state.set_value(project.description, window, cx)
            });
        } else {
            self.project_name
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
            self.project_desc
                .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        let title = match &target {
            FormTarget::New => "新增项目",
            FormTarget::Edit(_) => "编辑项目",
        };
        // 编辑时项目名称不可修改（docs/PRD.md「项目管理」）
        let fields = match &target {
            FormTarget::New => vec![
                ("项目名称", self.project_name.clone()),
                ("项目描述", self.project_desc.clone()),
            ],
            FormTarget::Edit(_) => vec![("项目描述", self.project_desc.clone())],
        };
        dialog::form(
            window,
            cx,
            title,
            "保存",
            30.0,
            fields,
            move |this, cx| this.save_project(target.clone(), cx),
        );
    }

    /// 保存项目（新增或编辑）。
    pub fn save_project(&mut self, target: FormTarget, cx: &mut Context<Self>) -> bool {
        let name = self.project_name.read(cx).value().trim().to_string();
        let description = self.project_desc.read(cx).value().trim().to_string();
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return false };
        let core = Arc::clone(&self.core);
        match &target {
            FormTarget::New => {
                if name.is_empty() {
                    self.with_core(|core| core.warning("请输入项目名称"));
                    cx.notify();
                    return false;
                }
                let project = Project { name, description };
                self.runtime.spawn(async move {
                    match client.create_project(&project).await {
                        Ok(()) => {
                            core.lock().settings.projects.push(project);
                        }
                        Err(error) => core.lock().error(format!("保存项目失败：{error}")),
                    }
                });
            }
            FormTarget::Edit(old) => {
                let old = old.clone();
                let description = description.clone();
                self.runtime.spawn(async move {
                    match client.update_project(&old, &description).await {
                        Ok(()) => {
                            let mut core = core.lock();
                            if let Some(item) = core
                                .settings
                                .projects
                                .iter_mut()
                                .find(|item| item.name == *old)
                            {
                                item.description = description;
                            }
                        }
                        Err(error) => core.lock().error(format!("保存项目失败：{error}")),
                    }
                });
            }
        }
        true
    }

    /// 删除项目：确认后删除，并让其下会话回到未归属（docs/PRD.md「项目管理」）。
    pub fn delete_project(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        dialog::confirm(
            window,
            cx,
            "删除项目",
            format!("项目「{name}」将被删除，其下会话将回到未归属，此操作不可撤销。"),
            "删除",
            ButtonVariant::Danger,
            move |this, _cx| {
                let name = name.clone();
                let client = this.with_core(|core| core.client.clone());
                let Some(client) = client else { return };
                let core = Arc::clone(&this.core);
                this.runtime.spawn(async move {
                    match client.delete_project(&name).await {
                        Ok(()) => {
                            let mut core = core.lock();
                            core.settings.projects.retain(|item| item.name != name);
                        }
                        Err(error) => core.lock().error(format!("删除项目失败：{error}")),
                    }
                });
            },
        );
    }

    /// 调整项目顺序（上移 / 下移）并落盘。
    pub fn move_project(&mut self, name: String, delta: isize, cx: &mut Context<Self>) {
        let list = self.with_core(|core| core.settings.projects.clone());
        let Some(index) = list.iter().position(|item| item.name == name) else {
            return;
        };
        let target = index as isize + delta;
        if target < 0 || target as usize >= list.len() {
            return;
        }
        let mut next = list;
        next.swap(index, target as usize);
        let client = self.with_core(|core| core.client.clone());
        let Some(client) = client else { return };
        let core = Arc::clone(&self.core);
        let names: Vec<String> = next.iter().map(|item| item.name.clone()).collect();
        self.runtime.spawn(async move {
            match client.set_project_order(&names).await {
                Ok(()) => {
                    core.lock().settings.projects = next;
                }
                Err(error) => core.lock().error(format!("调整项目顺序失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 项目卡片拖拽排序：把源项目移动到目标项目当前位置。
    pub fn reorder_project(&mut self, from: String, target: String, cx: &mut Context<Self>) {
        let list = self.with_core(|core| core.settings.projects.clone());
        let Some(from_ix) = list.iter().position(|item| item.name == from) else {
            return;
        };
        let Some(target_ix) = list.iter().position(|item| item.name == target) else {
            return;
        };
        if from_ix != target_ix {
            self.move_project(from, target_ix as isize - from_ix as isize, cx);
        }
    }

    /// 保存内置智能体配置；保存结果以弹窗反馈（docs/DESIGN.md 连接/内置智能体设置）。
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
            self.with_core(|core| core.error("保存失败：请填写 Base URL、API Key 与模型名称。"));
            cx.notify();
            return;
        }
        let core = Arc::clone(&self.core);
        self.runtime.spawn(async move {
            match client.set_orchestrator(&config).await {
                Ok(()) => {
                    let mut core = core.lock();
                    core.settings.orchestrator = Some(config);
                    core.success("内置智能体设置已保存");
                }
                Err(error) => core.lock().error(format!("保存失败：{error}")),
            }
        });
        cx.notify();
    }

    /// 内置智能体表单是否与已保存配置不同（决定「保存」是否可点）。
    /// 尚未保存过配置时以空配置为基准，表单未填写则不视为改动。
    pub fn orchestrator_dirty(&self, cx: &App) -> bool {
        let saved = self
            .with_core(|core| core.settings.orchestrator.clone())
            .unwrap_or_else(empty_orchestrator_config);
        saved != self.orchestrator_form(cx)
    }

    /// 表单当前值。
    fn orchestrator_form(&self, cx: &App) -> OrchestratorConfig {
        OrchestratorConfig {
            api_format: self.current_orchestrator_format(),
            base_url: self.orch_base_url.read(cx).value().trim().to_string(),
            api_key: self.orch_api_key.read(cx).value().trim().to_string(),
            model: self.orch_model.read(cx).value().trim().to_string(),
            effort: self.orch_effort.read(cx).value().trim().to_string(),
        }
    }

    /// 内置智能体 API 格式：用户已选择则用其选择，否则用已保存配置的格式。
    pub fn current_orchestrator_format(&self) -> ApiFormat {
        self.orchestrator_format
            .or_else(|| {
                self.with_core(|core| core.settings.orchestrator.as_ref().map(|c| c.api_format))
            })
            .unwrap_or(ApiFormat::ChatCompletions)
    }

    /// 打开内置智能体设置页时预填已保存的配置。
    pub fn load_orchestrator_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self
            .with_core(|core| core.settings.orchestrator.clone())
            .unwrap_or_else(empty_orchestrator_config);
        self.orchestrator_format = Some(config.api_format);
        self.orchestrator_prefilled = Some(config.clone());
        self.orch_base_url
            .update(cx, |state, cx| state.set_value(config.base_url, window, cx));
        self.orch_api_key
            .update(cx, |state, cx| state.set_value(config.api_key, window, cx));
        self.orch_model
            .update(cx, |state, cx| state.set_value(config.model, window, cx));
        self.orch_effort
            .update(cx, |state, cx| state.set_value(config.effort, window, cx));
        cx.notify();
    }

    /// 内置智能体分类打开时拉取的配置落地后重填一次表单（打开瞬间可能还是缓存或空）。
    fn sync_orchestrator_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.with_core(|core| {
            core.settings_open && core.settings_tab == crate::state::SettingsTab::Orchestrator
        });
        if !open {
            return;
        }
        let saved = self
            .with_core(|core| core.settings.orchestrator.clone())
            .unwrap_or_else(empty_orchestrator_config);
        if self.orchestrator_prefilled.as_ref() == Some(&saved) {
            return;
        }
        self.load_orchestrator_form(window, cx);
    }

    /// 会话选项下拉框：单个 select 选项。
    pub fn config_option_menu(
        &self,
        option: &SessionConfigOption,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let SessionConfigKind::Select { options, .. } = &option.kind else {
            return div().into_any_element();
        };
        let current = ui::config_value_label(option);
        let app = cx.entity();
        let id = format!("cfg-select-{}", option.id);
        let entries: Vec<(String, String)> = options
            .iter()
            .map(|entry| (entry.value.clone(), entry.name.clone()))
            .collect();
        let selected = match &option.kind {
            SessionConfigKind::Select { current_value, .. } => current_value.clone(),
            SessionConfigKind::Boolean { .. } => String::new(),
        };
        let config_id = option.id.clone();
        Button::new(id)
            .small()
            .outline()
            // 值文本按内容自适应宽度、超过上限则省略：`.label()` 渲染的文本在本版本 gpui-component
            // 里不截断（会溢出按钮边框），因此自带一个可截断的文本子元素，行高对齐 Button 内部的标签
            .child(
                div()
                    .max_w(px(CONFIG_VALUE_MAX_WIDTH))
                    .line_height(relative(1.))
                    .truncate()
                    .child(current),
            )
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                let mut menu = menu;
                for (value, name) in entries.clone() {
                    let config_id = config_id.clone();
                    let app = app.clone();
                    let checked = value == selected;
                    menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                        move |_, _, cx| {
                            app.update(cx, |this, cx| {
                                this.apply_config_option(
                                    config_id.clone(),
                                    SessionConfigOptionValue::ValueId {
                                        value: value.clone(),
                                    },
                                    cx,
                                )
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }
}

/// 设置页可全量保存的列表类配置。
pub enum SettingsList {
    Skills(Vec<Skill>),
    QuickCommands(Vec<QuickCommand>),
    Plans(Vec<WorkflowPlanItem>),
    RecentWorkspaces(Vec<RecentWorkspace>),
    Projects(Vec<Project>),
}

impl AmuxApp {
    /// 主页面：会话列表 + 中间面板 + 悬浮按钮 + 右侧面板。
    fn render_main_page(&mut self, core: &Core, cx: &mut Context<Self>) -> impl IntoElement {
        let has_open = core.open.is_some();
        let panel = self.render_panel(cx);
        let sidebar = self.render_sidebar(cx);
        let main = sessions::render_main(core, self, cx);
        let rail = self.render_rail(cx);

        let mut main_row = h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .child(sidebar)
            // 中间面板：悬浮按钮叠加在其右缘之上（不占布局空间），
            // 面板向左展开时随中列右缘移动，始终可见
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .child(main)
                    .when(has_open, |wrapper| {
                        wrapper.child(div().absolute().top_2().right_1().child(rail))
                    }),
            );
        if let Some(panel) = panel {
            main_row = main_row.child(panel);
        }
        main_row
    }

    /// 标题栏：拖拽/双击最大化与窗口控制按钮由组件负责。
    fn render_title_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;
        div()
            .id("title-bar-wrap")
            .w_full()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                GlobalState::suppress_text_selection(cx);
            })
            .child(
                TitleBar::new().child(
                    Label::new("amux")
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(foreground),
                ),
            )
    }

    /// 左侧面板：内容 + 右侧拖拽手柄（手柄含在面板宽度内）。
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self.sidebar_width;
        let core = self.core.lock().clone();
        let theme = cx.theme();
        let handle_color = theme.sidebar_border;
        let primary = theme.primary;
        let handle = div()
            .id("sidebar-resize-handle")
            .w(px(PANEL_RESIZE_HANDLE_WIDTH))
            .h_full()
            .bg(handle_color.opacity(0.6))
            .hover(move |handle| handle.bg(primary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.sidebar_drag = Some((event.position.x.as_f32(), this.sidebar_width));
                }),
            )
            .on_drag(SidebarResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<SidebarResizeDrag>, window, cx| {
                    let Some((origin, initial)) = this.sidebar_drag else {
                        return;
                    };
                    // docs/PRD.md「主页面」：不限制最大宽度，最小宽度以保证可拖拽回为准。
                    // 手柄含在面板宽度内，因此下限取手柄宽度，否则手柄被拖没了就抓不回来；
                    // 上限取窗口宽度，再宽手柄就会被推到窗口外。
                    this.sidebar_width = (initial + (event.event.position.x.as_f32() - origin))
                        .clamp(
                            PANEL_RESIZE_HANDLE_WIDTH,
                            window.bounds().size.width.as_f32(),
                        );
                    cx.notify();
                },
            ));
        h_flex()
            .w(px(width))
            .h_full()
            .child(panels::render_sidebar(&core, self, cx))
            .child(handle)
    }

    /// 右侧面板：左侧拖拽手柄 + 面板内容（手柄不计入面板宽度）。
    fn render_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let core = self.core.lock().clone();
        let panel = core.side_panel?;
        let theme = cx.theme();
        let border = theme.border;
        let primary = theme.primary;
        let handle = div()
            .id("panel-resize-handle")
            .w(px(PANEL_RESIZE_HANDLE_WIDTH))
            .h_full()
            .bg(border.opacity(0.35))
            .hover(move |handle| handle.bg(primary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.panel_drag = Some((event.position.x.as_f32(), this.panel_width));
                }),
            )
            .on_drag(PanelResizeDrag, |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<PanelResizeDrag>, window, cx| {
                    let Some((origin, initial)) = this.panel_drag else {
                        return;
                    };
                    // 向左拖（x 变小）即面板变宽。docs/PRD.md「主页面」：不限制最大宽度，
                    // 最小宽度以保证可拖拽回为准 —— 手柄在面板之外，面板宽度可以一直拖到 0，
                    // 手柄仍留在原位可再拖回；上限取窗口宽度，再宽手柄会被挤出窗口。
                    this.panel_width = (initial + (origin - event.event.position.x.as_f32()))
                        .clamp(0.0, window.bounds().size.width.as_f32());
                    cx.notify();
                },
            ));
        Some(
            h_flex()
                .h_full()
                .child(handle)
                .child(
                    div()
                        .w(px(self.panel_width))
                        .h_full()
                        .min_w_0()
                        .child(panels::render_panel(&core, panel, self, cx)),
                )
                .into_any_element(),
        )
    }

    /// 中间面板右缘的悬浮按钮栏（展开右侧面板）。
    fn render_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.lock().clone();
        panels::render_rail(&core, self, cx)
    }
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let core = self.core.lock().clone();

        let mut root = v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .child(self.render_title_bar(cx));
        // 连接中整页转圈；连不上 Server 时进入登录页面，连接成功后切到主页面
        // （docs/PRD.md「登录页面」）
        if core.status == ConnectionStatus::Online {
            root = root.child(self.render_main_page(&core, cx));
            if core.settings_open {
                root = root.child(settings::render_overlay(&core, self, cx));
            }
        } else if core.status == ConnectionStatus::Connecting {
            root = root.child(login::connecting(cx));
        } else {
            root = root.child(login::render(&core, self, cx));
        }
        // 组件层（sheet / dialog / 通知）必须由根视图渲染：Root 自身只渲染子视图
        if let Some(layer) = Root::render_sheet_layer(window, cx) {
            root = root.child(layer);
        }
        if let Some(layer) = Root::render_dialog_layer(window, cx) {
            root = root.child(layer);
        }
        if let Some(layer) = Root::render_notification_layer(window, cx) {
            root = root.child(layer);
        }
        root
    }
}

/// 技能操作的目标列表：机器 · agent，不可用的 agent 置灰不可点击。
fn skill_target_card(
    targets: &[(String, Vec<Agent>)],
    skill: &Skill,
    action: &'static str,
    cx: &mut Context<AmuxApp>,
) -> AnyElement {
    let theme = ui::Colors::of(cx.theme());
    let mut buttons = h_flex().flex_wrap().gap_1();
    for (machine, agents) in targets {
        for agent in agents {
            buttons = buttons.child(
                Button::new(SharedString::from(format!(
                    "skill-target-{action}-{machine}-{}",
                    agent.name
                )))
                .small()
                .label(format!("{machine} · {}", agent.name))
                .disabled(!agent.available)
                .on_click(cx.listener({
                    let skill = skill.clone();
                    let machine = machine.clone();
                    let agent = agent.name.clone();
                    move |this, _, window, cx| {
                        this.apply_skill(skill.clone(), action, machine.clone(), agent.clone(), cx);
                        window.close_dialog(cx);
                    }
                })),
            );
        }
    }
    let has_target = targets.iter().any(|(_, agents)| !agents.is_empty());
    v_flex()
        .gap_2()
        .child(
            Label::new(format!("选择执行技能「{}」的机器和 agent", skill.name))
                .text_sm()
                .text_color(theme.muted_foreground),
        )
        .child(if has_target {
            buttons.into_any_element()
        } else {
            Label::new("当前没有已发现的机器 agent。")
                .text_sm()
                .text_color(theme.danger)
                .into_any_element()
        })
        .into_any_element()
}

/// 未保存过内置智能体配置时的空基准。
fn empty_orchestrator_config() -> OrchestratorConfig {
    OrchestratorConfig {
        api_format: ApiFormat::ChatCompletions,
        base_url: String::new(),
        api_key: String::new(),
        model: String::new(),
        effort: String::new(),
    }
}

/// 持续消费终端 SSE；每次连接的首个事件重建缓冲，断线后自动重连。
async fn stream_terminal_output(
    client: crate::client::Client,
    core: SharedCore,
    key: TerminalStreamKey,
) {
    const RETRY_DELAY: Duration = Duration::from_millis(500);
    loop {
        let mut first = true;
        let mut last_cursor = None;
        let result = client
            .terminal_output_stream(&key.session, &key.terminal, |output| {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &output.data,
                )
                .unwrap_or_default();
                let mut core = core.lock();
                if core.status != ConnectionStatus::Online
                    || core.side_panel != Some(SidePanel::Terminal)
                    || !matches!(
                        core.open.as_ref(),
                        Some(OpenTarget::Session(session)) if session == &key.session
                    )
                    || core.view.detail.active_terminal.as_deref() != Some(key.terminal.as_str())
                {
                    return;
                }
                if last_cursor.is_some_and(|cursor| output.next_cursor <= cursor) {
                    return;
                }
                if first || output.truncated {
                    core.view.detail.terminal_output.reset();
                }
                first = false;
                last_cursor = Some(output.next_cursor);
                core.view.detail.terminal_output.append(&bytes);
            })
            .await;

        if !terminal_stream_current(&core, &key) {
            return;
        }
        if let Err(error) = result {
            if error.starts_with("HTTP 401") {
                core.lock().status = ConnectionStatus::Failed(error);
                return;
            }
            if error.starts_with("HTTP 404") {
                let mut core = core.lock();
                if core.view.detail.active_terminal.as_deref() == Some(key.terminal.as_str()) {
                    core.view.detail.active_terminal = None;
                    core.view.detail.terminal_output.reset();
                }
                return;
            }
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

fn terminal_stream_current(core: &SharedCore, key: &TerminalStreamKey) -> bool {
    let core = core.lock();
    core.status == ConnectionStatus::Online
        && core.side_panel == Some(SidePanel::Terminal)
        && matches!(
            core.open.as_ref(),
            Some(OpenTarget::Session(session)) if session == &key.session
        )
        && core.view.detail.active_terminal.as_deref() == Some(key.terminal.as_str())
}

/// 创建一个终端并设为当前终端，随后刷新终端列表。
async fn create_terminal(client: &crate::client::Client, core: &SharedCore, session: &str) {
    match client.open_terminal(session, None, 100, 30).await {
        Ok(terminal) => {
            {
                let mut core = core.lock();
                core.view.detail.terminal_output.reset();
                core.view.detail.active_terminal = Some(terminal);
            }
            if let Ok(terminals) = client.terminals(session).await {
                crate::state::set_terminals(&mut core.lock(), terminals);
            }
        }
        Err(error) => core.lock().error(format!("打开终端失败：{error}")),
    }
}

/// 集合中已存在则移除，否则插入（折叠/选中状态切换）。
fn toggle_set<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

/// 将文件树宽度限制在面板可容纳的左右最小宽度之间。
fn tree_width(width: f32, panel_width: f32) -> f32 {
    let min = panels::TREE_MIN_WIDTH.min(panel_width / 2.0);
    let max = (panel_width - min - panels::TREE_RESIZE_HANDLE_WIDTH).max(min);
    width.clamp(min, max)
}

/// 页大小：面板可视高度大致能容纳的条目数，随可视高度自适应（docs/DESIGN.md 各滚动机制小节）。
fn page_size(handle: &ScrollHandle, loaded: usize) -> usize {
    let viewport = handle.bounds().size.height.as_f32();
    let content = viewport + handle.max_offset().y.as_f32();
    if loaded == 0 || viewport <= 0.0 || content <= 0.0 {
        return DEFAULT_PAGE_SIZE;
    }
    let visible = (viewport / (content / loaded as f32)).round();
    (visible as usize).clamp(1, MAX_PAGE_SIZE)
}
