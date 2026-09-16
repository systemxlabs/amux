//! amux 桌面应用入口（crate 结构见 `lib.rs`）。

use std::borrow::Cow;

use amux_desktop::{app, config, theme};
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
    amux_common::log::init_file_output(&amux_common::log::log_path("desktop"));
    let connection = config::load(&config::connection_path());

    let app = gpui_platform::application().with_assets(AmuxAssets);
    app.run(move |cx| {
        gpui_component::init(cx);
        theme::sync_appearance(None, cx);
        let connection = connection.clone();
        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                // 组件默认选项（透明标题栏 + macOS 红绿灯定位）之上保留 WM 标题
                titlebar: Some(TitlebarOptions {
                    title: Some("amux".into()),
                    ..TitleBar::title_bar_options()
                }),
                ..Default::default()
            };
            cx.open_window(window_options, move |window, cx| {
                theme::sync_appearance(Some(window), cx);
                let view = cx.new(|cx| app::AmuxApp::new(connection, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
