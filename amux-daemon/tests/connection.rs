//! Daemon ↔ Server 连接集成测试：起真实 daemon 二进制，用假 Server 走真实 WebSocket。
//!
//! 覆盖握手认证、JSON-RPC 方法分发、未知方法错误、无效 `acp` 通知的健壮性，
//! 以及握手被拒后 daemon 保持存活（每分钟重连，不退出）。

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const TOKEN: &str = "test-token";
const MACHINE: &str = "testpc";

/// 被测试的 daemon 子进程：退出测试时回收。
struct DaemonProcess {
    child: Child,
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl DaemonProcess {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

fn spawn_daemon(server: String, home: &std::path::Path) -> DaemonProcess {
    spawn_daemon_named(MACHINE, server, home)
}

fn spawn_daemon_named(machine: &str, server: String, home: &std::path::Path) -> DaemonProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_amux-daemon"))
        .args(["--machine", machine, "--server", &server, "--token", TOKEN])
        .env("AMUX_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 amux-daemon 失败");
    DaemonProcess { child }
}

/// 假 Server 侧的一条连接（accept 得到的是裸 TCP 上的 WebSocket）。
struct FakeServer {
    sink: SplitSink<WebSocketStream<TcpStream>, Message>,
    stream: SplitStream<WebSocketStream<TcpStream>>,
}

impl FakeServer {
    // accept_hdr_async 回调的 Result<Response, ErrorResponse> 签名由 tungstenite 固定
    #[allow(clippy::result_large_err)]
    async fn accept(listener: &TcpListener) -> (Self, Option<String>, Option<String>) {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
            .await
            .expect("等待 daemon 连接超时")
            .expect("accept 失败");
        let headers: Arc<Mutex<(Option<String>, Option<String>)>> =
            Arc::new(Mutex::new((None, None)));
        let captured = Arc::clone(&headers);
        let ws = tokio_tungstenite::accept_hdr_async(
            stream,
            move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                let mut captured = captured.lock().expect("头信息锁");
                captured.0 = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                captured.1 = request
                    .headers()
                    .get("amux-machine")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                Ok(response)
            },
        )
        .await
        .expect("完成 WebSocket 握手失败");
        let (sink, stream) = ws.split();
        let (authorization, machine) = headers.lock().expect("头信息锁").clone();
        (Self { sink, stream }, authorization, machine)
    }

    async fn send(&mut self, frame: String) {
        self.sink
            .send(Message::text(frame))
            .await
            .expect("发送失败");
    }

    /// 发起一次请求并等待同 id 的响应（跳过通知）。
    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        self.send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        )
        .await;
        loop {
            let message = tokio::time::timeout(Duration::from_secs(15), self.stream.next())
                .await
                .expect("等待响应超时")
                .expect("连接已关闭")
                .expect("接收失败");
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).expect("响应不是合法 JSON");
            if value.get("id").and_then(|id| id.as_u64()) == Some(id) {
                return value;
            }
        }
    }
}

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录失败")
}

#[tokio::test]
async fn daemon_authenticates_and_serves_requests() {
    let home = temp_home();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut daemon = spawn_daemon(format!("ws://{addr}"), home.path());

    let (mut server, authorization, machine) = FakeServer::accept(&listener).await;
    assert_eq!(authorization.as_deref(), Some("Bearer test-token"));
    assert_eq!(machine.as_deref(), Some(MACHINE));

    // machine.info：机器名即启动参数
    let info = server
        .request(1, "machine.info", serde_json::json!({}))
        .await;
    assert_eq!(info["result"]["name"], MACHINE);
    assert!(
        info["result"]["os"]
            .as_str()
            .is_some_and(|os| !os.is_empty()),
        "machine.info 应带上操作系统: {info}"
    );

    // agent.list：返回数组（本机可能未安装 codex）
    let agents = server.request(2, "agent.list", serde_json::json!({})).await;
    assert!(
        agents["result"]["agents"].is_array(),
        "agent.list 应返回 agents 数组: {agents}"
    );

    // fs.list：浏览临时目录
    let fs_list = server
        .request(
            3,
            "fs.list",
            serde_json::json!({ "path": home.path().to_string_lossy(), "limit": 10, "offset": 0 }),
        )
        .await;
    assert!(
        fs_list["result"]["entries"].is_array(),
        "fs.list 应返回 entries: {fs_list}"
    );

    // 未知方法：METHOD_NOT_FOUND
    let unknown = server
        .request(4, "nope.method", serde_json::json!({}))
        .await;
    assert_eq!(unknown["error"]["code"], -32601, "{unknown}");

    // 非法参数：INVALID_PARAMS
    let bad = server
        .request(
            5,
            "terminal.open",
            serde_json::json!({ "cols": 0, "rows": 0 }),
        )
        .await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    assert!(daemon.alive(), "daemon 应保持运行");
}

/// 机器名以 URL 编码放入 `amux-machine` 头（docs/DESIGN.md「认证」）：头值只能是可见
/// ASCII，非 ASCII 机器名此前会被 `HeaderValue::from_str` 直接拒绝。
#[tokio::test]
async fn machine_header_is_url_encoded() {
    let home = temp_home();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut daemon = spawn_daemon_named("开发机 A", format!("ws://{addr}"), home.path());

    let (mut server, authorization, machine) = FakeServer::accept(&listener).await;
    assert_eq!(authorization.as_deref(), Some("Bearer test-token"));
    assert_eq!(machine.as_deref(), Some("%E5%BC%80%E5%8F%91%E6%9C%BA%20A"));

    // 机器名本身不编码：machine.info 返回原文
    let info = server.request(1, "machine.info", serde_json::json!({})).await;
    assert_eq!(info["result"]["name"], "开发机 A");
    assert!(daemon.alive(), "daemon 应保持运行");
}

#[tokio::test]
async fn invalid_acp_notification_does_not_break_connection() {
    let home = temp_home();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut daemon = spawn_daemon(format!("ws://{addr}"), home.path());

    let (mut server, _, _) = FakeServer::accept(&listener).await;

    // agent 未启动：下行 acp 消息被拒绝，但连接与进程都必须存活
    server
        .send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "acp",
                "params": { "agent": "codex", "raw": "{\"jsonrpc\":\"2.0\",\"id\":1}" },
            })
            .to_string(),
        )
        .await;

    let info = server
        .request(1, "machine.info", serde_json::json!({}))
        .await;
    assert_eq!(info["result"]["name"], MACHINE);
    assert!(daemon.alive(), "daemon 应保持运行");
}

#[tokio::test]
// accept_hdr_async 回调的 Result<Response, ErrorResponse> 签名由 tungstenite 固定
#[allow(clippy::result_large_err)]
async fn rejected_handshake_keeps_daemon_alive() {
    let home = temp_home();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut daemon = spawn_daemon(format!("ws://{addr}"), home.path());

    let (stream, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
        .await
        .expect("等待 daemon 连接超时")
        .expect("accept 失败");
    let rejected = tokio_tungstenite::accept_hdr_async(
        stream,
        move |_request: &Request, _response: Response| -> Result<Response, ErrorResponse> {
            let mut denied = ErrorResponse::new(Some("token 不正确".to_string()));
            *denied.status_mut() = StatusCode::UNAUTHORIZED;
            Err(denied)
        },
    )
    .await;
    assert!(rejected.is_err(), "握手应被拒绝");

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(daemon.alive(), "握手被拒后 daemon 不应退出");
}
