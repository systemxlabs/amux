//! amux 主视图（docs/DESIGN.md §7 / PRD §3.1）。
//! 三面板布局 + 多机器 + 设置页（机器管理）：
//! - 左侧：机器与会话列表（按机器分组）+ 底部设置入口
//! - 中间：对话流 / 会话活动页切换 + 快捷按钮栏 + 输入区 + 对话流右侧竖排悬浮按钮
//! - 右侧：上下文面板（默认折叠，悬浮按钮展开 diff / 会话详情）
//! - 设置页：机器接入 / 移除 / 连接配置 / 在线状态（本地注册表持久化）

use std::sync::Arc;

use gpui::*;
use gpui_component::{
    button::*,
    input::{Input, InputState},
    label::Label,
    *,
};
use serde_json::json;

use protocol::{Activity, ContentBlock, DialogItem, SessionMeta};

use crate::config::{machine_ws_url, ConfigStore, MachineConfig, NotifyPrefs};
use crate::ws::{Notification, WsClient};

/// 中间面板视图：对话流 / 会话活动页。
#[derive(Clone, Copy, PartialEq)]
enum CenterView {
    Dialog,
    Activities,
}

/// 右侧面板（默认折叠，悬浮按钮展开）。
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Diff,
    Detail,
}

/// 设置页分类（PRD §3.4：机器管理 / 通知 / 快捷按钮 / Skills）。
#[derive(Clone, Copy, PartialEq)]
enum SettingsCategory {
    Machines,
    Notify,
    QuickButtons,
    Skills,
}

/// 单机器视图：独立连接 + 会话列表 + 选中会话的对话/活动。
struct MachineView {
    config: MachineConfig,
    client: WsClient,
    status: String,
    sessions: Vec<SessionMeta>,
    selected: Option<String>,
    dialog: Vec<DialogItem>,
    activities: Vec<Activity>,
}

impl MachineView {
    fn new(config: MachineConfig) -> Self {
        let url = machine_ws_url(&config);
        MachineView {
            config,
            client: WsClient::connect(url),
            status: "连接中…".into(),
            sessions: Vec::new(),
            selected: None,
            dialog: Vec::new(),
            activities: Vec::new(),
        }
    }
}

pub struct AmuxApp {
    store: Arc<ConfigStore>,
    machines: Vec<MachineView>,
    /// 当前查看的机器下标
    active: Option<usize>,
    center_view: CenterView,
    panel: Option<Panel>,
    diff: String,
    show_settings: bool,
    /// 设置页当前分类（导航侧边栏选中项）
    settings_category: SettingsCategory,
    /// 通知偏好缓存（设置页读写，保存时落盘）
    notify_prefs: NotifyPrefs,
    input_state: Entity<InputState>,
    /// 新建会话的工作目录（PRD §4.1：新建会话时指定工作目录）
    session_cwd_input: Entity<InputState>,
    settings_input: Entity<InputState>,
    _tasks: Vec<Task<()>>,
}

fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl AmuxApp {
    pub fn new(store: Arc<ConfigStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input_state =
            cx.new(|cx| InputState::new(window, cx).placeholder("输入消息，Ctrl+Enter 发送"));
        // 工作目录输入框：新建会话时由用户填写（PRD §4.1）
        let session_cwd_input = cx.new(|cx| InputState::new(window, cx).placeholder("工作目录"));
        let settings_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("名称 ws://地址 token（空格分隔）"));
        let machines = store
            .list_machines()
            .into_iter()
            .map(|config| {
                let mut m = MachineView::new(config.clone());
                m.status = "已连接".into();
                m
            })
            .collect::<Vec<_>>();

        // 通知偏好缓存（结构体字面量前读取，store 随后被移入）
        let notify_prefs = store.load().notify;

        let mut app = Self {
            store,
            machines,
            active: None,
            center_view: CenterView::Dialog,
            panel: None,
            diff: String::new(),
            show_settings: false,
            settings_category: SettingsCategory::Machines,
            notify_prefs,
            input_state,
            session_cwd_input,
            settings_input,
            _tasks: Vec::new(),
        };
        app.spawn_notify_tasks(window, cx);
        // 启动即拉取各机器会话列表（否则左侧为空，直到收到会话通知）
        for i in 0..app.machines.len() {
            app.refresh_sessions(i, window, cx);
        }
        app
    }

    fn active_mut(&mut self) -> Option<&mut MachineView> {
        self.active.and_then(|i| self.machines.get_mut(i))
    }

    fn active_view(&self) -> Option<&MachineView> {
        self.active.and_then(|i| self.machines.get(i))
    }

    /// 每机器通知 task（在 new 后调用，用 active 机器建立通知路由）。
    fn spawn_notify_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.machines.len() {
            let mut notify_rx = self.machines[i].client.subscribe();
            let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
                while let Ok(n) = notify_rx.recv().await {
                    let _ = this.update_in(cx, |this, window, cx| {
                        Self::on_notify(this, window, cx, i, &n);
                    });
                }
            });
            self._tasks.push(t);
        }
    }

    fn on_notify(
        this: &mut Self,
        window: &mut Window,
        cx: &mut Context<Self>,
        idx: usize,
        n: &Notification,
    ) {
        let Some(m) = this.machines.get_mut(idx) else {
            return;
        };
        let changed = match n.method.as_str() {
            "turn_completed" => {
                let output = n.params.get("output").cloned().unwrap_or_default();
                let ts = n
                    .params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                m.dialog.push(DialogItem::AgentOutput {
                    content: serde_json::from_value(output).unwrap_or_default(),
                    timestamp: ts,
                });
                true
            }
            "user_message" => {
                let content = n.params.get("content").cloned().unwrap_or_default();
                let ts = n
                    .params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                m.dialog.push(DialogItem::UserMessage {
                    content: serde_json::from_value(content).unwrap_or_default(),
                    timestamp: ts,
                });
                true
            }
            "session_created" | "session_deleted" => {
                this.refresh_sessions(idx, window, cx);
                true
            }
            _ => false,
        };
        if changed {
            cx.notify();
        }
    }

    // ---- 动作（按机器下标）----

    fn refresh_sessions(&self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.sessions = serde_json::from_value(sessions).unwrap_or_default();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn create_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.active else { return };
        // 工作目录取自输入框（PRD §4.1：新建会话时指定工作目录，agent 即在该目录启动）
        let cwd = self.session_cwd_input.read(cx).value().trim().to_string();
        if cwd.is_empty() {
            if let Some(m) = self.machines.get_mut(idx) {
                m.status = "请填写工作目录".into();
            }
            cx.notify();
            return;
        }
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(
                    protocol::method::CREATE_SESSION,
                    Some(json!({ "harness": "codex", "cwd": cwd })),
                )
                .await
            {
                let sid = res
                    .get("session")
                    .and_then(|s| s.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        if !sid.is_empty() {
                            m.selected = Some(sid.clone());
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_session(&self, window: &mut Window, cx: &mut Context<Self>, session_id: String) {
        let Some(idx) = self.active else { return };
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(
                    protocol::method::OPEN_SESSION,
                    Some(json!({ "sessionId": session_id })),
                )
                .await
            {
                let items = res.get("items").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.selected = Some(session_id.clone());
                        m.dialog = serde_json::from_value(items).unwrap_or_default();
                        m.activities.clear();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_activities(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(idx), Some(sid)) = (
            self.active,
            self.active_view().and_then(|m| m.selected.clone()),
        ) else {
            return;
        };
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(
                    protocol::method::GET_ACTIVITIES,
                    Some(json!({ "sessionId": sid })),
                )
                .await
            {
                let acts = res.get("activities").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.activities = serde_json::from_value(acts).unwrap_or_default();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_diff(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.active else { return };
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        // 工作区：优先选中会话的工作目录；未选会话时用工作目录输入框的值
        let cwd = m
            .selected
            .as_ref()
            .and_then(|sid| m.sessions.iter().find(|s| s.id == *sid))
            .map(|s| s.cwd.clone())
            .or_else(|| {
                let v = self.session_cwd_input.read(cx).value().trim().to_string();
                (!v.is_empty()).then_some(v)
            });
        let Some(cwd) = cwd else { return };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client
                .request(protocol::method::GIT_DIFF, Some(json!({ "cwd": cwd })))
                .await
            {
                let diff = res.as_str().unwrap_or("").to_string();
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.diff = diff;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.active else { return };
        let Some(session_id) = self.active_mut().and_then(|m| m.selected.clone()) else {
            if let Some(m) = self.active_mut() {
                m.status = "请先选择会话".into();
            }
            cx.notify();
            return;
        };
        let text = self.input_state.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Err(e) = client
                .request(
                    protocol::method::PROMPT,
                    Some(json!({ "sessionId": session_id, "input": [{ "type": "text", "text": text }] })),
                )
                .await
            {
                let msg = format!("prompt 失败: {e}");
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.status = msg.clone();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn quick_button(&mut self, window: &mut Window, cx: &mut Context<Self>, id: &str) {
        let Some(idx) = self.active else { return };
        let Some(session_id) = self.active_mut().and_then(|m| m.selected.clone()) else {
            if let Some(m) = self.active_mut() {
                m.status = "请先选择会话".into();
            }
            cx.notify();
            return;
        };
        let template = match id {
            "commit-push" => {
                "提交并推送当前工作区的更改：为改动写一条简洁的 commit message，commit 后 push。"
            }
            "submit-pr" => "提交一个 Pull Request：stage → commit → push → 创建 PR。",
            _ => return,
        };
        let Some(m) = self.machines.get(idx) else {
            return;
        };
        let client = m.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let _ = client
                .request(
                    protocol::method::PROMPT,
                    Some(json!({ "sessionId": session_id, "input": [{ "type": "text", "text": template }] })),
                )
                .await;
            let _ = this.update_in(cx, |this, _window, _cx| {
                let _ = this;
            });
        })
        .detach();
    }

    // ---- 设置页 ----

    fn add_machine(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        name: String,
        url: String,
        token: String,
    ) {
        if name.trim().is_empty() || !url.starts_with("ws://") || token.trim().is_empty() {
            return;
        }
        let machine = self
            .store
            .add_machine(name.trim(), url.trim(), token.trim());
        let mut view = MachineView::new(machine);
        view.status = "已连接".into();
        let idx = self.machines.len();
        self.machines.push(view);
        let client = self.machines[idx].client.clone();
        // 新机器拉会话
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(m) = this.machines.get_mut(idx) {
                        m.sessions = serde_json::from_value(sessions).unwrap_or_default();
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        self.active = Some(idx);
        cx.notify();
    }

    fn remove_machine(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.machines.len() {
            let id = self.machines[idx].config.id.clone();
            self.store.remove_machine(&id);
            self.machines.remove(idx);
            if self.active == Some(idx)
                || self
                    .active
                    .map(|a| a >= self.machines.len())
                    .unwrap_or(false)
            {
                self.active = None;
            }
            cx.notify();
        }
    }

    // ---- 渲染 ----

    fn render_sidebar(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(230.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(rgb(0xf0f1f4))
            .child(h_flex().gap_1().child(Label::new("机器与会话")))
            .child(
                div()
                    .id("sidebar-machines")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_1()
                    .children(self.machines.iter().enumerate().map(|(mi, m)| {
                        let machine_active = self.active == Some(mi);
                        let machine_view = v_flex()
                            .gap_1()
                            .p_1()
                            .bg(rgb(0xe4e6ea))
                            .rounded_md()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(format!("{} ({})", m.config.name, m.status)))
                                    .child(
                                        Button::new(format!("use-{mi}"))
                                            .small()
                                            .label("使用")
                                            .on_click(cx.listener(
                                                move |this, _ev, _window, cx| {
                                                    this.active = Some(mi);
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                            )
                            .children(m.sessions.iter().map(|s| {
                                let sid = s.id.clone();
                                let selected =
                                    machine_active && m.selected.as_deref() == Some(s.id.as_str());
                                let label: SharedString =
                                    format!("{} · {}", s.harness, short_cwd(&s.cwd)).into();
                                let btn = Button::new(format!("sess-{mi}-{sid}"))
                                    .small()
                                    .label(label)
                                    .on_click(cx.listener(move |this, _ev, window, cx| {
                                        this.active = Some(mi);
                                        this.open_session(window, cx, sid.clone());
                                    }));
                                if selected {
                                    btn.primary()
                                } else {
                                    btn
                                }
                            }));
                        if machine_active {
                            machine_view.child(
                                v_flex()
                                    .gap_1()
                                    .child(Input::new(&self.session_cwd_input))
                                    .child(
                                        Button::new(format!("new-{mi}"))
                                            .small()
                                            .label("＋ 新建会话")
                                            .on_click(cx.listener(move |this, _ev, window, cx| {
                                                this.active = Some(mi);
                                                this.create_session(window, cx);
                                            })),
                                    ),
                            )
                        } else {
                            machine_view
                        }
                    })),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("settings")
                        .small()
                        .label("⚙ 设置")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.show_settings = true;
                            cx.notify();
                        })),
                ),
            )
    }

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .p_2()
            .child(self.render_view_switch(_window_placeholder(window), cx))
            .child(self.render_center(window, cx))
            .child(self.render_quick_buttons(window, cx))
            .child(self.render_input(_window_placeholder(window), cx))
    }

    fn render_view_switch(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(
                Button::new("view-dialog")
                    .small()
                    .label("对话流")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.center_view = CenterView::Dialog;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("view-activities")
                    .small()
                    .label("会话活动")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.center_view = CenterView::Activities;
                        this.load_activities(window, cx);
                        cx.notify();
                    })),
            )
    }

    fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_h_0()
            // 默认 items_center 会让对话区不撑满、长对话溢出；改为 stretch
            .items_stretch()
            .child(match self.center_view {
                CenterView::Dialog => self.render_dialog(_window_placeholder(window), cx),
                CenterView::Activities => self.render_activities(_window_placeholder(window), cx),
            })
            .child(self.render_floating_buttons(window, cx))
    }

    fn render_dialog(&self, _window: &mut Window, _cx: &mut Context<Self>) -> gpui::AnyElement {
        let dialog = self
            .active_view()
            .map(|m| m.dialog.clone())
            .unwrap_or_default();
        let rows = dialog
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DialogItem::UserMessage { content, .. } => div()
                    .id(("row", i))
                    .w_full()
                    .p_2()
                    .bg(rgb(0xe6ecf4))
                    .rounded_md()
                    .child("我：")
                    .child(block_text(content)),
                DialogItem::AgentOutput { content, .. } => div()
                    .id(("row", i))
                    .w_full()
                    .p_2()
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .child("Agent：")
                    .child(block_text(content)),
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            div()
                .id("dialog-empty")
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new("选择左侧会话查看对话，或输入消息开始"))
                .into_any()
        } else {
            div()
                .id("dialog")
                .flex_1()
                .gap_1()
                .overflow_y_scroll()
                .children(rows)
                .into_any()
        }
    }

    fn render_activities(&self, _window: &mut Window, _cx: &mut Context<Self>) -> gpui::AnyElement {
        let activities = self
            .active_view()
            .map(|m| m.activities.clone())
            .unwrap_or_default();
        let rows = activities
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let (kind, detail) = match a {
                    Activity::Thinking { content, .. } => ("思考", content.clone()),
                    Activity::ToolCall {
                        name,
                        title,
                        content,
                        ..
                    } => (
                        "工具调用",
                        format!(
                            "{} {} {}",
                            name,
                            title.clone().unwrap_or_default(),
                            content.clone().unwrap_or_default()
                        ),
                    ),
                    Activity::Compaction { detail, .. } => ("压缩", detail.clone()),
                };
                div()
                    .id(("act", i))
                    .w_full()
                    .p_1()
                    .bg(rgb(0xf5f6f8))
                    .rounded_md()
                    .child(format!("[{kind}] {detail}"))
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            div()
                .id("activities-empty")
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new(
                    "暂无活动（打开会话后产生 thinking / tool_call 等）",
                ))
                .into_any()
        } else {
            div()
                .id("activities")
                .flex_1()
                .gap_1()
                .overflow_y_scroll()
                .children(rows)
                .into_any()
        }
    }

    fn render_floating_buttons(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .justify_center()
            .child(
                Button::new("float-diff")
                    .small()
                    .label("Diff")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        if this.panel == Some(Panel::Diff) {
                            this.panel = None;
                        } else {
                            this.panel = Some(Panel::Diff);
                            this.load_diff(window, cx);
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("float-detail")
                    .small()
                    .label("详情")
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.panel = if this.panel == Some(Panel::Detail) {
                            None
                        } else {
                            Some(Panel::Detail)
                        };
                        cx.notify();
                    })),
            )
    }

    fn render_quick_buttons(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_1()
            .p_1()
            .child(
                Button::new("commit-push")
                    .small()
                    .label("Commit & Push")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.quick_button(window, cx, "commit-push");
                    })),
            )
            .child(
                Button::new("submit-pr")
                    .small()
                    .label("Submit PR")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.quick_button(window, cx, "submit-pr");
                    })),
            )
    }

    fn render_input(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().gap_2().child(Input::new(&self.input_state)).child(
            Button::new("send")
                .primary()
                .label("发送")
                .on_click(cx.listener(|this, _ev, window, cx| {
                    this.send_prompt(window, cx);
                })),
        )
    }

    fn render_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        match self.panel {
            Some(Panel::Diff) => Some(self.render_diff_panel(window, cx)),
            Some(Panel::Detail) => Some(self.render_detail_panel(window, cx)),
            None => None,
        }
    }

    fn render_diff_panel(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let text: SharedString = if self.diff.is_empty() {
            "（无变更）".into()
        } else {
            self.diff.clone().into()
        };
        v_flex()
            .w(px(340.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(rgb(0xf7f8fa))
            .child(
                h_flex().child(Label::new("工作区 Diff")).child(
                    Button::new("close-panel")
                        .small()
                        .label("✕")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.panel = None;
                            cx.notify();
                        })),
                ),
            )
            .child(
                div()
                    .id("diff-text")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(text),
            )
            .into_any()
    }

    fn render_detail_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let meta = self.active_view().and_then(|m| {
            m.selected
                .as_ref()
                .and_then(|sid| m.sessions.iter().find(|s| s.id == *sid))
                .cloned()
        });
        let Some(meta) = meta else {
            return div().w(px(340.)).child(Label::new("未选择会话")).into_any();
        };
        v_flex()
            .w(px(340.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(rgb(0xf7f8fa))
            .child(
                h_flex().child(Label::new("会话详情")).child(
                    Button::new("close-panel2")
                        .small()
                        .label("✕")
                        .on_click(cx.listener(|this, _ev, _window, cx| {
                            this.panel = None;
                            cx.notify();
                        })),
                ),
            )
            .child(Label::new(format!("ID: {}", meta.id)))
            .child(Label::new(format!("Harness: {}", meta.harness)))
            .child(Label::new(format!("工作目录: {}", meta.cwd)))
            .child(Label::new(format!(
                "状态: {}",
                if meta.closed {
                    "已关闭"
                } else if meta.interrupted {
                    "已中断"
                } else {
                    "正常"
                }
            )))
            .into_any()
    }

    /// 设置浮窗：半透明遮罩 + 居中卡片（PRD §3.4）。
    /// 卡片内部为分类导航侧边栏 + 右侧设置内容；点遮罩或"关闭"收起。
    /// 遮罩与卡片是兄弟元素，且卡片拦截鼠标按下，面板内点击不会误触遮罩关闭。
    fn render_settings_overlay(
        &self,
        _window: &mut Window,
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
                    .bg(hsla(0., 0., 0., 0.45))
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.show_settings = false;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .id("settings-card")
                    // 拦截卡片内的鼠标按下，阻止事件继续分发到全屏遮罩（兄弟元素），
                    // 否则点输入框/按钮时遮罩的 click 也触发、浮窗被关闭
                    .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                        cx.stop_propagation();
                    })
                    .w(px(720.))
                    .h(px(560.))
                    .bg(rgb(0xffffff))
                    .rounded_md()
                    .shadow_lg()
                    .child(self.render_settings_nav(cx))
                    .child(self.render_settings_content(cx)),
            )
    }

    /// 设置分类导航侧边栏（选中项高亮）+ 底部关闭。
    fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-nav")
            .w(px(180.))
            .h_full()
            .gap_1()
            .p_2()
            .bg(rgb(0xf0f1f4))
            .child(Label::new("设置"))
            .child(self.settings_nav_item(SettingsCategory::Machines, cx))
            .child(self.settings_nav_item(SettingsCategory::Notify, cx))
            .child(self.settings_nav_item(SettingsCategory::QuickButtons, cx))
            .child(self.settings_nav_item(SettingsCategory::Skills, cx))
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

    /// 单个分类导航项：选中时 primary 高亮。
    fn settings_nav_item(
        &self,
        target: SettingsCategory,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (id, label) = match target {
            SettingsCategory::Machines => ("cat-machines", "机器管理"),
            SettingsCategory::Notify => ("cat-notify", "通知"),
            SettingsCategory::QuickButtons => ("cat-quickbuttons", "快捷按钮"),
            SettingsCategory::Skills => ("cat-skills", "Skills"),
        };
        let btn = Button::new(id).small().label(label).on_click(cx.listener(
            move |this, _ev, _window, cx| {
                this.settings_category = target;
                cx.notify();
            },
        ));
        if self.settings_category == target {
            btn.primary()
        } else {
            btn
        }
    }

    /// 右侧设置内容（按当前分类渲染）。
    fn render_settings_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("settings-content")
            .flex_1()
            .h_full()
            .gap_2()
            .p_4()
            .overflow_y_scroll()
            .child(match self.settings_category {
                SettingsCategory::Machines => self.render_machines_settings(cx),
                SettingsCategory::Notify => self.render_notify_settings(cx),
                SettingsCategory::QuickButtons => settings_placeholder(
                    "快捷按钮",
                    "PRD §4.6：预设 Commit & Push / Submit PR，可增删、改提示词",
                ),
                SettingsCategory::Skills => {
                    settings_placeholder("Skills", "PRD §4.9：Skills 注册表（仓库 URL + 本地目录）")
                }
            })
    }

    /// 机器管理：列表 + 移除 + 添加。
    fn render_machines_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let machines = self
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| {
                h_flex()
                    .gap_1()
                    .child(Label::new(format!(
                        "{} · {} · {}",
                        m.config.name, m.config.url, m.status
                    )))
                    .child(
                        Button::new(format!("remove-{i}"))
                            .small()
                            .label("移除")
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.remove_machine(i, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_2()
            .child(Label::new("机器管理"))
            .children(machines)
            .child(
                h_flex()
                    .gap_1()
                    .child(Input::new(&self.settings_input))
                    .child(Button::new("settings-add").small().label("添加").on_click(
                        cx.listener(|this, _ev, window, cx| {
                            let text = this.settings_input.read(cx).value().to_string();
                            let parts: Vec<&str> = text.split_whitespace().collect();
                            if parts.len() >= 3 {
                                this.add_machine(
                                    window,
                                    cx,
                                    parts[0].to_string(),
                                    parts[1].to_string(),
                                    parts[2].to_string(),
                                );
                            }
                        }),
                    )),
            )
            .child(Label::new("添加格式：名称 ws://地址 token（空格分隔）"))
            .into_any()
    }

    /// 通知偏好：工作结束 / 出错开关（保存到本地配置）。
    fn render_notify_settings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let prefs = self.notify_prefs.clone();
        let view = cx.entity();
        v_flex()
            .gap_2()
            .child(Label::new("通知"))
            .child(
                switch::Switch::new("notify-work-ended")
                    .checked(prefs.work_ended)
                    .label("工作结束时发送桌面通知")
                    .on_click({
                        let view = view.clone();
                        move |checked, _window, cx| {
                            view.update(cx, |this, cx| {
                                this.notify_prefs.work_ended = *checked;
                                this.store.save_notify(&this.notify_prefs);
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(
                switch::Switch::new("notify-on-error")
                    .checked(prefs.on_error)
                    .label("出错时发送桌面通知")
                    .on_click({
                        let view = view.clone();
                        move |checked, _window, cx| {
                            view.update(cx, |this, cx| {
                                this.notify_prefs.on_error = *checked;
                                this.store.save_notify(&this.notify_prefs);
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(Label::new(format!(
                "长时间无响应阈值：{} 秒（配置项，暂不可调）",
                prefs.long_idle_seconds
            )))
            .into_any()
    }
}

/// 待实现分类的占位内容。
fn settings_placeholder(title: &str, note: &str) -> gpui::AnyElement {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_1()
        .child(Label::new(title))
        .child(Label::new(format!("待实现（{note}）")))
        .into_any()
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.render_panel(window, cx);
        let mut root = h_flex()
            .size_full()
            .relative()
            // h_flex 默认 items_center：改为 stretch，让 main 撑满窗口高度
            .items_stretch()
            .child(self.render_sidebar(window, cx))
            .child(self.render_main(window, cx));
        if let Some(p) = panel {
            root = root.child(p);
        }
        // 设置浮窗：作为最后一个子元素盖在主界面之上
        if self.show_settings {
            root = root.child(self.render_settings_overlay(window, cx));
        }
        root.into_any()
    }
}

fn short_cwd(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}

fn _window_placeholder(w: &mut Window) -> &mut Window {
    w
}
