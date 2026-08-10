//! amux 主视图（docs/DESIGN.md §7 / PRD §3.1）：连接 server、会话列表、对话流、输入。
//! 当前为最小可用版：连 server → 会话列表 → 打开会话显示对话 → 输入发送（turn_completed 追加）。

use std::sync::Arc;

use gpui::*;
use gpui_component::{
    button::*,
    input::{Input, InputState},
    label::Label,
    *,
};
use serde_json::json;

use protocol::{ContentBlock, DialogItem, SessionMeta};

use crate::ws::{Notification, WsClient};

pub struct AmuxApp {
    client: Arc<WsClient>,
    cwd: String,
    status: SharedString,
    sessions: Vec<SessionMeta>,
    selected: Option<String>,
    dialog: Vec<DialogItem>,
    input_state: Entity<InputState>,
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
    pub fn new(url: String, cwd: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let client = Arc::new(WsClient::connect(url));
        let input_state =
            cx.new(|cx| InputState::new(window, cx).placeholder("输入消息，Ctrl+Enter 发送"));
        let mut tasks = Vec::new();

        // 后台通知 task：turn_completed / user_message / session_created 更新 UI
        let mut notify_rx = client.subscribe();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            while let Ok(n) = notify_rx.recv().await {
                let _ =
                    this.update_in(cx, |this, window, cx| Self::on_notify(this, window, cx, &n));
            }
        });
        tasks.push(t);

        // 初始：拉会话列表
        let client2 = client.clone();
        let t = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client2.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.sessions = serde_json::from_value(sessions).unwrap_or_default();
                    this.status = "已连接".into();
                    cx.notify();
                });
            }
        });
        tasks.push(t);

        Self {
            client,
            cwd,
            status: "连接中…".into(),
            sessions: Vec::new(),
            selected: None,
            dialog: Vec::new(),
            input_state,
            _tasks: tasks,
        }
    }

    fn on_notify(this: &mut Self, window: &mut Window, cx: &mut Context<Self>, n: &Notification) {
        let changed = match n.method.as_str() {
            "turn_completed" => {
                let output = n.params.get("output").cloned().unwrap_or_default();
                let ts = n
                    .params
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                this.dialog.push(DialogItem::AgentOutput {
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
                this.dialog.push(DialogItem::UserMessage {
                    content: serde_json::from_value(content).unwrap_or_default(),
                    timestamp: ts,
                });
                true
            }
            "session_created" | "session_deleted" => {
                this.refresh_sessions(window, cx);
                true
            }
            _ => false,
        };
        if changed {
            cx.notify();
        }
    }

    fn refresh_sessions(&self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Ok(res) = client.request(protocol::method::LIST_SESSIONS, None).await {
                let sessions = res.get("sessions").cloned().unwrap_or_default();
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.sessions = serde_json::from_value(sessions).unwrap_or_default();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn create_session(&self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let cwd = self.cwd.clone();
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
                    if !sid.is_empty() {
                        this.selected = Some(sid.clone());
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn open_session(&self, window: &mut Window, cx: &mut Context<Self>, session_id: String) {
        let client = self.client.clone();
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
                    this.selected = Some(session_id.clone());
                    this.dialog = serde_json::from_value(items).unwrap_or_default();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected.clone() else {
            self.status = "请先选择会话".into();
            cx.notify();
            return;
        };
        let text = self.input_state.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            if let Err(e) = client
                .request(
                    protocol::method::PROMPT,
                    Some(json!({
                        "sessionId": session_id,
                        "input": [{ "type": "text", "text": text }],
                    })),
                )
                .await
            {
                let msg = format!("prompt 失败: {e}");
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.status = msg.into();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ---- 渲染 ----

    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .child(Label::new("amux — agent 控制平面"))
            .child(
                Button::new("refresh")
                    .ghost()
                    .label(self.status.clone())
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.refresh_sessions(window, cx);
                    })),
            )
    }

    fn render_sessions(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .p_1()
            .child(Label::new("会话:"))
            .child(
                Button::new("new-session")
                    .small()
                    .label("＋ 新建")
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.create_session(window, cx);
                    })),
            )
            .children(self.sessions.iter().map(|s| {
                let sid = s.id.clone();
                let selected = self.selected.as_deref() == Some(s.id.as_str());
                let label: SharedString = format!("{} · {}", s.harness, short_cwd(&s.cwd)).into();
                let btn = Button::new(format!("sess-{sid}"))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.open_session(window, cx, sid.clone());
                    }));
                if selected {
                    btn.primary()
                } else {
                    btn
                }
            }))
    }

    fn render_dialog(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .dialog
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DialogItem::UserMessage { content, .. } => div()
                    .id(("row", i))
                    .w_full()
                    .p_2()
                    .bg(rgb(0x2a2f38))
                    .rounded_md()
                    .child("我：")
                    .child(block_text(content)),
                DialogItem::AgentOutput { content, .. } => div()
                    .id(("row", i))
                    .w_full()
                    .p_2()
                    .bg(rgb(0x1b1e24))
                    .rounded_md()
                    .child("Agent：")
                    .child(block_text(content)),
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            div()
                .id("dialog-empty")
                .flex_1()
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
}

impl Render for AmuxApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(self.render_header(window, cx))
            .child(self.render_sessions(window, cx))
            .child(self.render_dialog(cx))
            .child(self.render_input(window, cx))
    }
}

fn short_cwd(cwd: &str) -> String {
    cwd.rsplit('/').next().unwrap_or(cwd).to_string()
}
