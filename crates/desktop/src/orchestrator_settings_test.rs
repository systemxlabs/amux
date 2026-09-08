//! 编排智能体设置保存按钮的置灰契约：表单与最近落盘配置一致时不可保存，
//! 任意一项（含 API 格式单选）修改后可保存，保存成功后回到不可保存。

use std::rc::Rc;
use std::sync::Arc;

use gpui::AppContext;

use crate::app::{AmuxApp, SettingsCategory};
use crate::config::{ApiFormat, ConfigStore, OrchestratorConfig};
use gpui_component::Root;

#[gpui::test]
fn orchestrator_save_gates_on_modification(cx: &mut gpui::TestAppContext) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    cx.update(gpui_component::init);

    let app = Rc::new(std::cell::RefCell::new(None));
    let app_for_window = app.clone();
    // Root 必须是窗口根视图（保存结果弹窗等组件层依赖 Root）
    let (app_entity, cx) = cx.add_window_view(|window, cx| {
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        let _ = store.save_orchestrator(&OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            effort: "high".into(),
        });
        let app = cx.new(|cx| AmuxApp::new(store, window, cx));
        *app_for_window.borrow_mut() = Some(app.clone());
        Root::new(app, window, cx)
    });
    let app = app.borrow().as_ref().expect("app 已构建").clone();
    let _ = app_entity;

    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_settings(window, cx, Some(SettingsCategory::Orchestrator));
            assert!(
                !app.orchestrator_dirty(cx),
                "表单与已保存配置一致时保存按钮应置灰"
            );

            // 修改任一输入后即可保存
            app.settings.orch_base_input.update(cx, |s, cx| {
                s.set_value("http://127.0.0.1:10/v1", window, cx)
            });
            assert!(app.orchestrator_dirty(cx), "修改配置后保存按钮应可点击");

            // 保存成功后回到置灰，且新配置落盘
            app.save_orchestrator(window, cx);
            assert!(!app.orchestrator_dirty(cx), "保存成功后保存按钮应回到置灰");
            let saved = app.store.orchestrator().unwrap();
            assert_eq!(saved.base_url, "http://127.0.0.1:10/v1");

            // 仅切换 API 格式单选同样视为修改
            app.settings.orch_api_format = ApiFormat::Responses;
            assert!(
                app.orchestrator_dirty(cx),
                "切换 API 格式后保存按钮应可点击"
            );
        });
    });
}
