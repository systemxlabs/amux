//! 斜杠命令上拉框冒烟测试：菜单可见性与匹配项由「输入前缀 + 选中会话 + 命令
//! 集合」派生（docs/PRD.md「会话交互视图」），锁定派生规则与输入区渲染不 panic。

use std::sync::Arc;

use gpui::{div, point, px, size, AppContext, IntoElement, ParentElement, Render, Styled, Window};

use protocol::SlashCommand;

use amux_desktop::app::{AmuxApp, Selected, SelectedSlashCommands};
use amux_desktop::config::{ConfigStore, MachineConfig};
use amux_desktop::machine::{MachineStatus, MachineView};

fn command(name: &str) -> SlashCommand {
    SlashCommand {
        name: name.into(),
        description: format!("{name} 描述"),
        hint: None,
    }
}

struct InputHostView {
    app: gpui::Entity<AmuxApp>,
}

impl Render for InputHostView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let input = self.app.update(cx, |app, cx| {
            app.render_input(window, cx).into_any_element()
        });
        div().size_full().child(input)
    }
}

#[gpui::test]
fn slash_menu_visibility_follows_input_prefix(cx: &mut gpui::TestAppContext) {
    let cx = cx.add_empty_window();
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();

    let app = cx.update(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        cx.new(|cx| AmuxApp::new(store, window, cx))
    });

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
            app.machines.push(machine);
            app.selected = Some(Selected::Session {
                machine: 0,
                id: "session-1".into(),
            });
            app.slash_commands = Some(SelectedSlashCommands {
                machine: 0,
                session_id: "session-1".into(),
                commands: vec![command("goal"), command("review")],
            });

            // 输入 "/" → 展示全量菜单
            app.input_state
                .update(cx, |s, cx| s.set_value("/", window, cx));
            assert!(app.render_slash_menu(cx).is_some(), "输入 / 应弹出菜单");

            // 前缀匹配中 → 菜单保持
            app.input_state
                .update(cx, |s, cx| s.set_value("/go", window, cx));
            assert!(app.render_slash_menu(cx).is_some());

            // 前缀无匹配 → 收起
            app.input_state
                .update(cx, |s, cx| s.set_value("/x", window, cx));
            assert!(app.render_slash_menu(cx).is_none());

            // 命令名输入完成（出现空白，开始输入参数）→ 收起
            app.input_state
                .update(cx, |s, cx| s.set_value("/goal ", window, cx));
            assert!(app.render_slash_menu(cx).is_none());

            // 非斜杠输入 → 收起
            app.input_state
                .update(cx, |s, cx| s.set_value("你好", window, cx));
            assert!(app.render_slash_menu(cx).is_none());

            // 命令集合归属其他会话 → 收起
            app.input_state
                .update(cx, |s, cx| s.set_value("/", window, cx));
            app.slash_commands = Some(SelectedSlashCommands {
                machine: 0,
                session_id: "session-other".into(),
                commands: vec![command("goal")],
            });
            assert!(
                app.render_slash_menu(cx).is_none(),
                "陈旧会话的命令集合不应展示"
            );
        });
    });

    // 渲染冒烟：菜单可见时整条输入区（含上拉框）可完成布局
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.slash_commands = Some(SelectedSlashCommands {
                machine: 0,
                session_id: "session-1".into(),
                commands: vec![command("goal"), command("review")],
            });
            app.input_state
                .update(cx, |s, cx| s.set_value("/", window, cx));
        });
    });
    cx.draw(point(px(0.), px(0.)), size(px(1024.), px(768.)), |_, cx| {
        cx.new(|_| InputHostView { app: app.clone() })
            .into_any_element()
    });
}
