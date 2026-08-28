//! amux GUI 桌面应用入口。
//! 三栏（侧边栏/对话/上下文面板）多机器客户端；本地数据拆到 `~/.amux/app/` 多个文件。

#![recursion_limit = "512"]

mod aggregate;
mod app;
mod config;
mod diff;
mod diff_review;
mod display;
mod logic;
mod machine;
mod machines;
mod panels;
mod sessions;
mod settings;
mod text;
mod theme;
mod wfstore;
mod workflow;
mod workflow_view;
mod ws;

#[cfg(test)]
mod diff_scroll_layout_test;
#[cfg(test)]
mod dialog_image_render_test;

use std::path::PathBuf;
use std::sync::Arc;

use app::AmuxApp;
use config::ConfigStore;
use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir = std::env::var("AMUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".amux").join("app"))
                .unwrap_or_else(|_| PathBuf::from(".amux/app"))
        });
    let mut i = 0;
    while i < args.len() {
        if args[i].as_str() == "--data-dir" {
            i += 1;
            if let Some(v) = args.get(i) {
                data_dir = PathBuf::from(v);
            }
        }
        i += 1;
    }

    let log_path = data_dir
        .parent()
        .map(|p| p.join("logs").join("desktop.log"))
        .unwrap_or_else(|| data_dir.join("desktop.log"));
    amux_common::log::init_file_output(&log_path);
    let store = Arc::new(ConfigStore::new(data_dir));

    let app = gpui_platform::application().with_assets(Assets);
    app.run(move |cx| {
        gpui_component::init(cx);
        // 应用退出时确定性关闭全部 WS 连接；
        // 进程退出兜底之外的显式关闭，避免 in-flight 请求被硬掐）
        // Subscription 需保活：drop 即注销，故显式绑定
        let _quit_subscription = cx.on_app_quit(|_cx| async {
            crate::ws::close_all();
        });
        theme::sync_appearance(None, cx);
        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                // 组件默认选项（透明标题栏 + macOS 红绿灯定位）之上保留 WM 标题
                titlebar: Some(TitlebarOptions {
                    title: Some("amux".into()),
                    ..TitleBar::title_bar_options()
                }),
                ..Default::default()
            };
            cx.open_window(window_options, |window, cx| {
                theme::sync_appearance(Some(window), cx);
                let view = cx.new(|cx| AmuxApp::new(store.clone(), window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
