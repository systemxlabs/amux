//! amux GUI 桌面应用（docs/DESIGN.md §7）。
//! 基于 GPUI + gpui-component（三面板 Dock 布局、对话流 Markdown、会话活动页、
//! diff 编辑器 + Tree Sitter、设置页）。WS 连接经 tokio 与 server 通信。
//!
//! 当前：最小可运行窗口（HelloWorld + Root + Button），验证 GPUI 链路；
//! 完整 UI（三面板 / 对话流 / 活动页 / diff / 设置页）逐步接入。

use gpui::*;
use gpui_component::{button::*, *};

struct AmuxApp;

impl Render for AmuxApp {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_2()
            .size_full()
            .items_center()
            .justify_center()
            .child("amux — agent 控制平面")
            .child(
                Button::new("ok")
                    .primary()
                    .label("连接 server")
                    .on_click(|_, _, _| println!("点击：连接 server（待实现 WS 客户端）")),
            )
    }
}

fn main() {
    gpui_platform::application().run(move |cx| {
        // 必须先初始化 gpui-component
        gpui_component::init(cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|_| AmuxApp);
                // 窗口第一层必须是 Root
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        });
    });
}
