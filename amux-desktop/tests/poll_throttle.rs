//! 刷新机制回归：会话视图数据按各自周期拉取，设置类数据不做定时刷新。
//!
//! 断言在固定次数节拍内的请求次数，即锁定节流；设置类数据只在视图打开时拉取一次
//! （docs/DESIGN.md「应用」各视图小节）。
//!
//! 放在 `tests/`：库内联测试会触发组件宏的深度展开，编译期爆栈。

use std::sync::{Arc, Mutex};

use amux_desktop::client::Client;
use amux_desktop::config::Connection;
use amux_desktop::poll;
use amux_desktop::state::{ConnectionStatus, Core, OpenTarget, SettingsTab, SharedCore, SidePanel};

/// 端点桩的响应体：只覆盖视图用到的端点，其余端点返回空对象（调用方按错误忽略）。
fn stub(path: &str) -> &'static str {
    match path {
        "/sessions" => r#"{"sessions":[],"hasMore":false}"#,
        "/workflows" => r#"{"workflows":[],"hasMore":false}"#,
        "/sessions/s1" => {
            r#"{"id":"s1","machine":"pc","agent":"codex","title":"t","state":"idle","workspace":"/tmp","worktreeDir":"","createdAt":1,"updatedAt":1}"#
        }
        "/sessions/s1/history" => r#"{"items":[],"hasMore":false}"#,
        "/sessions/s1/plan" => r#"{"entries":[]}"#,
        "/sessions/s1/context" => r#"{"contextSize":1,"contextWindowSize":2}"#,
        "/sessions/s1/config_options" => {
            r#"{"options":[{"id":"model","name":"模型","type":"select","current_value":"a","options":[{"value":"a","name":"A"}]}]}"#
        }
        "/sessions/s1/slash_commands" => r#"{"commands":[{"name":"goal","description":"目标"}]}"#,
        "/machines" => {
            r#"[{"name":"pc","os":"linux","arch":"x86_64","hostname":"pc","tempDir":"/tmp","version":"0.1.0"}]"#
        }
        "/machines/pc/agents" => r#"[{"name":"codex","available":true}]"#,
        "/config/workflows/" => r#"[{"name":"plan","plan":"做点什么"}]"#,
        "/config/quick_commands/" => r#"[{"name":"qc","prompt":"快点做"}]"#,
        "/config/skills/" => r#"[{"name":"skill","description":"描述"}]"#,
        "/config/recent_workspaces/" => {
            r#"[{"machine":"pc","workspace":"/tmp/proj","lastUsed":1}]"#
        }
        "/config/agent/" => {
            r#"{"apiFormat":"chat_completions","baseUrl":"https://api.example.com/v1","apiKey":"sk","model":"m","effort":"high"}"#
        }
        _ => "{}",
    }
}

/// 启动记录请求路径的端点桩，返回 Server 地址与请求记录。
async fn start_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::http::StatusCode;
    use axum::Router;

    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = Arc::clone(&hits);
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let hits = Arc::clone(&recorded);
        async move {
            let path = request.uri().path().to_string();
            let body = stub(&path);
            hits.lock().unwrap().push(path);
            (StatusCode::OK, body)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

/// 未打开任何会话的 Core：状态置为在线（跳过连接检查），所有节拍自零开始。
fn core_with(server: String) -> SharedCore {
    let connection = Connection {
        server,
        token: "tk".into(),
    };
    let mut core = Core::new(connection);
    core.status = ConnectionStatus::Online;
    Arc::new(parking_lot::Mutex::new(core))
}

/// 打开会话 s1 的 Core。
fn opened_core(server: String) -> SharedCore {
    let core = core_with(server);
    core.lock().open = Some(OpenTarget::Session("s1".to_string()));
    core
}

fn client_of(core: &SharedCore) -> Client {
    core.lock().client.clone().expect("已配置连接")
}

fn hit_count(hits: &Arc<Mutex<Vec<String>>>, path: &str) -> usize {
    hits.lock()
        .unwrap()
        .iter()
        .filter(|hit| *hit == path)
        .count()
}

/// 计划面板未打开时会话选项与斜杠命令也必须按自身周期节流。
#[tokio::test]
async fn options_and_slash_commands_are_throttled() {
    let (server, hits) = start_stub().await;
    let core = opened_core(server);
    for _ in 0..3 {
        poll::tick(Arc::clone(&core)).await;
    }

    assert_eq!(hit_count(&hits, "/sessions/s1/config_options"), 1);
    assert_eq!(hit_count(&hits, "/sessions/s1/slash_commands"), 1);
    assert_eq!(hit_count(&hits, "/sessions/s1/plan"), 0, "计划面板未打开");

    let core = core.lock();
    assert_eq!(core.view.detail.config_options.len(), 1);
    assert_eq!(core.view.detail.slash_commands.len(), 1);
}

/// 设置类数据（机器/agents、编排智能体、计划、快捷指令、技能）不参与定时刷新：
/// 即使设置浮窗与会话打开，节拍也不得请求这些端点。
#[tokio::test]
async fn tick_does_not_poll_settings_data() {
    let (server, hits) = start_stub().await;
    let core = opened_core(server);
    {
        let mut core = core.lock();
        core.settings_open = true;
        core.settings_tab = SettingsTab::Machines;
        core.side_panel = Some(SidePanel::Detail);
    }
    for _ in 0..3 {
        poll::tick(Arc::clone(&core)).await;
    }

    for path in [
        "/machines",
        "/machines/pc/agents",
        "/config/agent/",
        "/config/workflows/",
        "/config/quick_commands/",
        "/config/skills/",
        "/config/recent_workspaces/",
        "/sessions/s1/context",
    ] {
        assert_eq!(hit_count(&hits, path), 0, "{path} 不应定时拉取");
    }
}

/// 设置浮窗打开/切换分类时只拉取当前分类的配置。
#[tokio::test]
async fn settings_load_fetches_only_the_selected_tab() {
    let (server, hits) = start_stub().await;
    let core = core_with(server);
    let client = client_of(&core);

    poll::refresh_settings(&client, &core, SettingsTab::Skills).await;
    assert_eq!(hit_count(&hits, "/config/skills/"), 1);
    assert_eq!(hit_count(&hits, "/config/quick_commands/"), 0);
    assert_eq!(core.lock().settings.skills.len(), 1);

    poll::refresh_settings(&client, &core, SettingsTab::Machines).await;
    assert_eq!(hit_count(&hits, "/machines"), 1);
    assert_eq!(hit_count(&hits, "/machines/pc/agents"), 1);
    let core = core.lock();
    assert_eq!(core.settings.machines.len(), 1);
    assert_eq!(core.settings.agents[0].1.len(), 1);
}

/// 新建会话视图打开时实时拉取机器、agents 与常用工作目录。
#[tokio::test]
async fn new_session_load_fetches_machines_agents_and_recent_workspaces() {
    let (server, hits) = start_stub().await;
    let core = core_with(server);
    let client = client_of(&core);

    poll::refresh_new_session(&client, &core).await;

    assert_eq!(hit_count(&hits, "/machines"), 1);
    assert_eq!(hit_count(&hits, "/machines/pc/agents"), 1);
    assert_eq!(hit_count(&hits, "/config/recent_workspaces/"), 1);
    // 新建会话视图只拉这三项：编排智能体配置与计划在进入工作流模式时才拉
    assert_eq!(hit_count(&hits, "/config/agent/"), 0);
    assert_eq!(hit_count(&hits, "/config/workflows/"), 0);
    let core = core.lock();
    assert_eq!(core.settings.machines.len(), 1);
    assert_eq!(core.recent_workspaces.len(), 1);
}

/// 工作流模式所需数据（编排智能体配置与计划）在进入该模式时拉取
/// （docs/PRD.md「新建会话视图」工作流模式）。
#[tokio::test]
async fn workflow_setup_load_fetches_orchestrator_and_plans() {
    let (server, hits) = start_stub().await;
    let core = core_with(server);
    let client = client_of(&core);

    poll::refresh_workflow_setup(&client, &core).await;

    assert_eq!(hit_count(&hits, "/config/agent/"), 1);
    assert_eq!(hit_count(&hits, "/config/workflows/"), 1);
    let core = core.lock();
    assert!(core.settings.orchestrator_loaded);
    assert_eq!(core.settings.plans.len(), 1);
}

/// 会话交互视图打开时实时拉取可用性与快捷指令。
#[tokio::test]
async fn interaction_load_fetches_agents_and_quick_commands() {
    let (server, hits) = start_stub().await;
    let core = core_with(server);
    let client = client_of(&core);

    poll::refresh_interaction(&client, &core).await;

    assert_eq!(hit_count(&hits, "/machines/pc/agents"), 1);
    assert_eq!(hit_count(&hits, "/config/quick_commands/"), 1);
    assert_eq!(hit_count(&hits, "/config/agent/"), 1);
    let core = core.lock();
    assert_eq!(core.settings.quick_commands.len(), 1);
    assert!(core.settings.agents[0].1[0].available);
}
