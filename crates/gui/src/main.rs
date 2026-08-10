//! amux GUI 桌面应用入口（docs/DESIGN.md §7）。
//! 连接 amux server（默认 ws://127.0.0.1:34567），显示会话列表与对话流。

mod app;
mod ws;

use app::AmuxApp;
use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

fn main() {
    // 连接参数：--token <值>（必填，与 server 一致）；可选 --host/--port/--cwd
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut token: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port = "34567".to_string();
    let mut cwd = "/tmp/work".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" => {
                i += 1;
                token = args.get(i).cloned();
            }
            "--host" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    host = v.clone();
                }
            }
            "--port" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    port = v.clone();
                }
            }
            "--cwd" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    cwd = v.clone();
                }
            }
            _ => {}
        }
        i += 1;
    }
    let token = token.filter(|t| !t.is_empty()).unwrap_or_else(|| {
        eprintln!("未指定 token：请用 --token <值>（与 server 启动时一致）");
        std::process::exit(1);
    });
    let url = format!("ws://{host}:{port}/?token={token}");

    let app = gpui_platform::application().with_assets(Assets);
    app.run(move |cx| {
        // 必须先初始化 gpui-component
        gpui_component::init(cx);
        cx.spawn(async move |cx| {
            let window_options = WindowOptions::default();
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| AmuxApp::new(url.clone(), cwd.clone(), window, cx));
                // 窗口第一层必须是 Root
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
