//! amux GUI 桌面应用入口（crate 结构见 `lib.rs`）。

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use amux_desktop::app::AmuxApp;
use amux_desktop::config::ConfigStore;
use amux_desktop::theme;
use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

/// 应用自有图标（gpui-component 默认图标集之外），优先于组件默认资产加载。
const FILE_DIFF_SVG: &str = include_str!("../assets/icons/file-diff.svg");

/// 资产源：应用自有图标优先，其余委托 gpui-component 默认资产。
struct AmuxAssets;

impl AssetSource for AmuxAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == "icons/file-diff.svg" {
            return Ok(Some(Cow::Borrowed(FILE_DIFF_SVG.as_bytes())));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Assets.list(path)
    }
}

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

    let app = gpui_platform::application().with_assets(AmuxAssets);
    app.run(move |cx| {
        gpui_component::init(cx);
        // 应用退出时确定性关闭全部 WS 连接；
        // 进程退出兜底之外的显式关闭，避免 in-flight 请求被硬掐）
        // Subscription drop 即注销，而本闭包体在事件循环开始前就会结束，
        // 局部绑定保不住它——注册一次后有意泄漏，生命周期覆盖整个进程。
        let quit_subscription = cx.on_app_quit(|_cx| async {
            amux_desktop::ws::close_all();
        });
        std::mem::forget(quit_subscription);
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
