//! 新建会话视图回归测试：机器/agent 选择器只展示在线机器与可用 agent，
//! 创建按钮在机器、agent（须显式选择）、工作目录等前置条件就绪前置灰不可点击。

use std::sync::Arc;

use gpui::{point, px, size, AppContext, IntoElement};

use protocol::AgentInfo;

use crate::app::{AmuxApp, NewSessionMode, Selected};
use crate::config::{ApiFormat, ConfigStore, MachineConfig, OrchestratorConfig};
use crate::machine::{MachineStatus, MachineView};

fn new_app(
    cx: &mut gpui::TestAppContext,
) -> (
    gpui::Entity<AmuxApp>,
    gpui::Entity<gpui_component::Root>,
    tempfile::TempDir,
    &mut gpui::VisualTestContext,
) {
    let data_dir = tempfile::tempdir().unwrap();
    let data_path = data_dir.path().to_path_buf();
    // 与 main 一致：根视图须为 gpui_component::Root 包裹（点击命中按钮后的
    // 焦点处理依赖 Root，否则 simulate_click 报 Root 缺失）
    let slot = std::cell::OnceCell::new();
    let (root_view, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let store = Arc::new(ConfigStore::new(data_path.clone()));
        // 工作流模式按钮测试需要编排 agent 已配置
        let _ = store.save_orchestrator(&OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            effort: "high".into(),
        });
        let app = cx.new(|cx| AmuxApp::new(store, window, cx));
        let root = gpui_component::Root::new(app.clone(), window, cx);
        slot.set(app).ok().unwrap();
        root
    });
    let app = slot.into_inner().unwrap();
    // TempDir 须由调用方持有：提前释放会连带删掉配置目录
    (app, root_view, data_dir, cx)
}

/// 强制一帧绘制（显式重绘根视图，保证最新状态进入渲染帧）。
fn redraw(cx: &mut gpui::VisualTestContext, root: &gpui::Entity<gpui_component::Root>) {
    cx.draw(
        point(px(0.), px(0.)),
        size(px(1200.), px(800.)),
        |_, _cx| root.clone().into_any_element(),
    );
}

/// 点击包装层（debug_selector 定位）内的创建按钮：按钮在卡片中左对齐、
/// 纵向占满包装层，点击左缘偏移处必落在按钮上。
fn click_button(
    cx: &mut gpui::VisualTestContext,
    root: &gpui::Entity<gpui_component::Root>,
    selector: &'static str,
) {
    redraw(cx, root);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} 未渲染"));
    cx.simulate_click(
        point(bounds.left() + px(10.), (bounds.top() + bounds.bottom()) / 2.),
        gpui::Modifiers::default(),
    );
}

fn online_machine(name: &str, cx: &mut gpui::App) -> MachineView {
    let mut machine = MachineView::new(
        MachineConfig {
            name: name.into(),
            url: "ws://127.0.0.1:9/".into(),
            token: "t".into(),
        },
        cx,
    );
    machine.status = MachineStatus::Online;
    machine
}

#[gpui::test]
fn create_button_disabled_until_machine_agent_cwd_ready(cx: &mut gpui::TestAppContext) {
    let (app, root, _data_dir, cx) = new_app(cx);
    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = online_machine("m1", cx);
            machine.agents = vec![AgentInfo {
                name: "codex".into(),
                available: true,
            }];
            app.machines.push(machine);
            // 预置错误：按钮置灰时点击不应触发 create_session_only（不会清除它）
            app.new_session_error = Some("预设错误".into());
            cx.notify();
        });
    });

    // 工作目录为空：按钮置灰，点击无效（错误保持不变）
    click_button(cx, &root, "ns-create-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            assert_eq!(
                app.new_session_error.as_deref(),
                Some("预设错误"),
                "创建按钮置灰时点击不应触发创建逻辑"
            );
        });
    });

    // 未显式选择 agent：即便工作目录已填，按钮仍置灰
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.session_cwd_input
                .update(cx, |s, cx| s.set_value("/tmp/proj", window, cx));
        });
    });
    click_button(cx, &root, "ns-create-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            assert_eq!(
                app.new_session_error.as_deref(),
                Some("预设错误"),
                "未显式选择 agent 时创建按钮应置灰"
            );
        });
    });

    // 显式选择可用 agent 后按钮可用：点击触发创建逻辑（同步清除预置错误）
    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            app.new_session_agent = Some("codex".into());
            cx.notify();
        });
    });
    click_button(cx, &root, "ns-create-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            assert!(
                app.new_session_error.is_none(),
                "前置条件就绪后点击创建按钮应触发创建逻辑（清除预置错误）"
            );
        });
    });
}

#[gpui::test]
fn offline_machine_shows_disabled_and_blocks_create(cx: &mut gpui::TestAppContext) {
    let (app, root, _data_dir, cx) = new_app(cx);
    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            // 仅一台离线机器：机器按钮置灰不可点击，创建按钮置灰
            let mut machine = online_machine("offline", cx);
            machine.status = MachineStatus::Connecting;
            machine.agents = vec![AgentInfo {
                name: "codex".into(),
                available: true,
            }];
            app.machines.push(machine);
            app.new_session_error = Some("预设错误".into());
            cx.notify();
        });
    });
    click_button(cx, &root, "ns-create-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            // 默认选中首台机器（离线），但创建条件要求在线 → 按钮置灰
            assert!(
                !app.can_create_session(cx),
                "机器离线时创建会话按钮不应可点击"
            );
            assert_eq!(
                app.new_session_error.as_deref(),
                Some("预设错误"),
                "机器离线时点击创建不应触发创建逻辑"
            );
        });
    });
}

#[gpui::test]
fn effective_agent_requires_explicit_available_selection(cx: &mut gpui::TestAppContext) {
    let (app, _root, _data_dir, cx) = new_app(cx);
    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            let mut machine = online_machine("m1", cx);
            machine.agents = vec![
                AgentInfo {
                    name: "busy".into(),
                    available: false,
                },
                AgentInfo {
                    name: "first".into(),
                    available: true,
                },
            ];
            app.machines.push(machine);
        });
    });
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            // 无显式选择：无隐式默认（创建按钮置灰）
            assert_eq!(app.effective_new_session_agent(), None);

            // 显式选择不可用 agent：不生效
            app.new_session_agent = Some("busy".into());
            assert_eq!(app.effective_new_session_agent(), None);

            // 显式选择可用：生效
            let m = app.machine_mut_by_name("m1").unwrap();
            m.agents[0].available = true;
            assert_eq!(app.effective_new_session_agent().as_deref(), Some("busy"));

            // 显式选择的 agent 失去可用性：不生效（按钮随之置灰）
            let m = app.machine_mut_by_name("m1").unwrap();
            m.agents[0].available = false;
            assert_eq!(app.effective_new_session_agent(), None);
        });
    });
}

#[gpui::test]
fn workflow_create_button_disabled_until_plan_ready(cx: &mut gpui::TestAppContext) {
    let (app, root, _data_dir, cx) = new_app(cx);
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.new_session_mode = NewSessionMode::Workflow;
            // 预置错误：按钮置灰时点击不应触发 create_workflow（不会覆盖它）
            app.workflow_error = Some("预设错误".into());
            cx.notify();
        });
    });

    // 计划为空：按钮置灰，点击无效
    click_button(cx, &root, "ns-create-workflow-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            assert_eq!(
                app.workflow_error.as_deref(),
                Some("预设错误"),
                "工作计划为空时点击创建不应触发创建逻辑"
            );
            assert!(app.selected.is_none(), "不应创建工作流会话");
        });
    });

    // 输入计划后按钮可用：点击创建工作流会话
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.workflow_input
                .update(cx, |s, cx| s.set_value("测试计划", window, cx));
        });
    });
    click_button(cx, &root, "ns-create-workflow-wrap");
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            assert!(
                matches!(app.selected, Some(Selected::Workflow { .. })),
                "工作计划就绪后点击创建应创建工作流会话"
            );
        });
    });
}
