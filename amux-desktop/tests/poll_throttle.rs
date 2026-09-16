//! 轮询节流回归：会话选项、斜杠命令与上下文信息必须按各自周期拉取。
//!
//! 曾共用计划面板的节拍（`last.plan` 仅在计划面板打开时推进），计划面板未打开时
//! 每个 250ms 节拍都会重发这些请求。断言在固定次数节拍内的请求次数，即锁定节流。
//!
//! 放在 `tests/`：库内联测试会触发组件宏的深度展开，编译期爆栈。

use std::sync::{Arc, Mutex};

use amux_desktop::config::Connection;
use amux_desktop::poll;
use amux_desktop::state::{ConnectionStatus, Core, OpenTarget, SharedCore, SidePanel};

/// 端点桩的响应体：只覆盖会话视图用到的端点，其余端点返回空对象（调用方按错误忽略）。
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
        "/config/workflows/" => {
            r#"[{"name":"plan","plan":"做点什么"}]"#
        }
        "/config/quick_commands/" => r#"[{"name":"qc","prompt":"快点做"}]"#,
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

/// 打开会话 s1 的 Core：状态置为在线（跳过连接检查），所有节拍自零开始。
fn opened_core(server: String) -> SharedCore {
    let connection = Connection {
        server,
        token: "tk".into(),
    };
    let mut core = Core::new(connection);
    core.status = ConnectionStatus::Online;
    core.open = Some(OpenTarget::Session("s1".to_string()));
    Arc::new(parking_lot::Mutex::new(core))
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

/// 详情面板打开后上下文信息同样按自身周期节流。
#[tokio::test]
async fn context_is_throttled_when_detail_panel_open() {
    let (server, hits) = start_stub().await;
    let core = opened_core(server);
    core.lock().side_panel = Some(SidePanel::Detail);
    for _ in 0..3 {
        poll::tick(Arc::clone(&core)).await;
    }

    assert_eq!(hit_count(&hits, "/sessions/s1/context"), 1);
    assert_eq!(core.lock().view.detail.context_size, 1);
}

/// 列表类配置（计划、快捷指令）常驻拉取：不依赖打开设置页对应分类。
#[tokio::test]
async fn plans_and_quick_commands_are_polled_resident() {
    let (server, hits) = start_stub().await;
    let core = opened_core(server);
    poll::tick(Arc::clone(&core)).await;

    assert_eq!(hit_count(&hits, "/config/workflows/"), 1);
    assert_eq!(hit_count(&hits, "/config/quick_commands/"), 1);
    let core = core.lock();
    assert_eq!(core.settings.plans.len(), 1);
    assert_eq!(core.settings.quick_commands.len(), 1);
}
