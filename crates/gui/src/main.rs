//! amux GUI 桌面应用入口（docs/DESIGN.md §7）。
//! 三面板（机器/会话 + 对话流 + 上下文面板）多机器客户端；本地配置（机器注册表）持久化。

mod app;
mod config;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use app::AmuxApp;
use config::{ConfigStore, FileBackend};
use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

fn main() {
    // 参数：可选 --data-dir（GUI 配置目录，默认 ~/.amux/gui）
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir = std::env::var("AMUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".amux").join("gui"))
                .unwrap_or_else(|_| PathBuf::from(".amux/gui"))
        });
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    data_dir = PathBuf::from(v);
                }
            }
            _ => {}
        }
        i += 1;
    }

    // 本地配置（机器注册表 / 通知偏好）持久化
    let config_path = data_dir.join("config.json");
    let store = Arc::new(ConfigStore::new(Box::new(FileBackend::new(config_path))));

    let app = gpui_platform::application().with_assets(Assets);
    app.run(move |cx| {
        // 必须先初始化 gpui-component
        gpui_component::init(cx);
        cx.spawn(async move |cx| {
            let window_options = WindowOptions::default();
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| AmuxApp::new(store.clone(), window, cx));
                // 窗口第一层必须是 Root
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
