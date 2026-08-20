//! amux GUI 桌面应用入口（docs/DESIGN.md §7「桌面应用」「应用」）。
//! 三栏（侧边栏/对话/上下文面板）多机器客户端；本地数据拆到 `~/.amux/app/` 多个文件。

#![recursion_limit = "512"]

mod aggregate;
mod app;
mod config;
mod display;
mod logic;
mod text;
mod theme;
mod wfstore;
mod workflow;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use app::AmuxApp;
use config::ConfigStore;
use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

fn main() {
    // 参数：可选 --data-dir（应用数据目录，默认 ~/.amux/app）
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

    // 应用本地数据拆分到 data_dir 下多个文件；
    // 工作流会话状态持久化于 data_dir/sessions/（docs/DESIGN.md「工作流会话存储」）
    let log_path = data_dir
        .parent()
        .map(|p| p.join("logs").join("desktop.log"))
        .unwrap_or_else(|| data_dir.join("desktop.log"));
    protocol::log::init_file_output(&log_path);
    let store = Arc::new(ConfigStore::new(data_dir));

    let app = gpui_platform::application().with_assets(Assets);
    app.run(move |cx| {
        gpui_component::init(cx);
        theme::sync_appearance(None, cx);
        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("amux".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(Point {
                        x: px(12.0),
                        y: px(10.0),
                    }),
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
