//! amux 桌面应用入口。

mod app;
mod client;
mod config;
mod panels;
mod poll;
mod settings;
mod state;
mod terminal_view;
mod ui;

use gpui::*;
use gpui_component::*;
use gpui_component_assets::Assets;

/// 资产源：委托 gpui-component 默认资产。
struct AmuxAssets;

impl AssetSource for AmuxAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
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
        let connection = connection.clone();
        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("amux".into()),
                    ..TitleBar::title_bar_options()
                }),
                ..Default::default()
            };
            cx.open_window(window_options, move |window, cx| {
                let view = cx.new(|cx| app::AmuxApp::new(connection, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
