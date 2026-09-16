//! `--web` 静态托管（docs/DESIGN.md「Server 启动」）。
//!
//! 覆盖三条对外可见行为：传 `--web` 时 `GET /` 返回应用页面且静态资源可取、静态资源免鉴权
//! （浏览器无法携带 `Authorization` 头）、未传 `--web` 时静态资源请求返回 404。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TOKEN: &str = "web-token";
const TIMEOUT: Duration = Duration::from_secs(30);

struct Process {
    child: Child,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    listener.local_addr().unwrap().port()
}

fn spawn_server(port: u16, home: &Path, web: Option<&Path>) -> Process {
    let mut command = Command::new(env!("CARGO_BIN_EXE_amux-server"));
    command.args([
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--token",
        TOKEN,
    ]);
    if let Some(dir) = web {
        command.arg("--web").arg(dir);
    }
    let child = command
        .env("AMUX_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 amux-server 失败");
    Process { child }
}

/// 等 Server 监听就绪（`GET /machines` 有应答即可，未鉴权时为 401）。
async fn wait_ready(base: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if reqwest::get(format!("{base}/machines")).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待 Server 监听超时"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 构建产物目录：页面外壳 + 被页面引用的脚本。
fn dist(dir: &Path) {
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        r#"<!doctype html><html><body><div id="root"></div><script type="module" src="/assets/app.js"></script></body></html>"#,
    )
    .unwrap();
    std::fs::write(dir.join("assets/app.js"), "console.log(\"amux\");\n").unwrap();
}

#[tokio::test]
async fn serves_page_and_assets_without_authentication() {
    let home = tempfile::tempdir().unwrap();
    let web = tempfile::tempdir().unwrap();
    dist(web.path());
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let _server = spawn_server(port, home.path(), Some(web.path()));
    wait_ready(&base).await;

    // 页面：无 Authorization 头也返回应用外壳
    let page = reqwest::get(format!("{base}/")).await.expect("GET / 失败");
    assert_eq!(page.status(), reqwest::StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(html.contains(r#"id="root""#), "缺少挂载点: {html}");
    assert!(html.contains("/assets/app.js"), "缺少构建脚本引用: {html}");

    // 页面引用的构建产物可取
    let asset = reqwest::get(format!("{base}/assets/app.js"))
        .await
        .expect("GET /assets/app.js 失败");
    assert_eq!(asset.status(), reqwest::StatusCode::OK);
    assert_eq!(asset.text().await.unwrap(), "console.log(\"amux\");\n");

    // API 仍强制鉴权
    let anonymous = reqwest::get(format!("{base}/machines")).await.unwrap();
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
    let authorized = reqwest::Client::new()
        .get(format!("{base}/machines"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert!(authorized.status().is_success());
}

#[tokio::test]
async fn without_web_directory_static_requests_are_not_found() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let _server = spawn_server(port, home.path(), None);
    wait_ready(&base).await;

    for path in ["/", "/assets/app.js", "/index.html"] {
        let response = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path} 应返回 404"
        );
    }
}
