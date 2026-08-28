//! 对话区用户消息图片附件渲染冒烟测试：用真实 AmuxApp 渲染整条对话链。
//!
//! 历史 bug：用户消息中的图片附件（Resource 块）在对话气泡中被完全丢弃
//! （block_text 只取文本）。渲染链引入 gpui img 元素后，锁定其不 panic、
//! 布局可完成。

use std::sync::Arc;

use gpui::{div, point, px, size, AppContext, IntoElement, ParentElement, Render, Styled, Window};

use protocol::ContentBlock;

use crate::app::{AmuxApp, Selected};
use crate::config::{ConfigStore, MachineConfig};
use crate::machine::{MachineStatus, MachineView};

struct DialogHostView {
    app: gpui::Entity<AmuxApp>,
}

impl Render for DialogHostView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let dialog = self
            .app
            .update(cx, |app, cx| app.render_dialog(window, cx).into_any_element());
        div().size_full().child(dialog)
    }
}

/// 1×1 PNG（常见测试用最小有效 PNG）。
const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[gpui::test]
fn dialog_renders_user_message_with_image(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = std::env::temp_dir().join(format!("amux-dialog-img-{}", std::process::id()));

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_dir));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

    use base64::Engine;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(TINY_PNG_BASE64)
        .unwrap();

    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = MachineView::new(
                MachineConfig {
                    name: "test".into(),
                    url: "ws://127.0.0.1:9/".into(),
                    token: "t".into(),
                },
                cx,
            );
            machine.status = MachineStatus::Online;
            machine.sessions.push(protocol::SessionMeta {
                id: "session-1".into(),
                agent: "test-agent".into(),
                cwd: "/tmp".into(),
                state: protocol::SessionState::Idle,
                title: String::new(),
                created_at: 0,
                last_active_at: 0,
                worktree_dir: String::new(),
                context_size: 0,
                context_window_size: 0,
            });
            let view = machine.views.entry("session-1".into()).or_default();
            view.dialog.push(crate::logic::DialogMsg::UserMessage {
                content: vec![
                    ContentBlock::Text {
                        text: "看这张图".into(),
                    },
                    ContentBlock::Resource {
                        mime_type: "image/png".into(),
                        uri: None,
                        text: None,
                        blob: Some(base64::engine::general_purpose::STANDARD.encode(&blob)),
                    },
                ],
                timestamp: 1_000,
            });
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: 0,
                id: "session-1".into(),
            });
            cx.notify();
        });
    });

    // 渲染真实对话区（根 → dialog 气泡 → TextView + img 元素）；img 在布局期
    // 解码 base64 blob，渲染失败/元素树非法会在此暴露。
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| DialogHostView { app: app.clone() })
            .into_any_element()
    });
}
