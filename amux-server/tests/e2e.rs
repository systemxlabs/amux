//! 端到端：真实 `amux-server` + 真实 `amux-daemon` + 模拟 ACP v2 agent（stdio）。
//!
//! 覆盖新架构的主链路：Daemon 握手接入 → Agent 生命周期（发现/启动/ACP 初始化）
//! → Client API 建会话/发指令 → ACP 通知落盘与缓存 → 终端与配置。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt as _;
use serde_json::{json, Value};

const TOKEN: &str = "e2e-token";
const MACHINE: &str = "testpc";
const TIMEOUT: Duration = Duration::from_secs(30);

/// 测试运行目录：默认临时目录，可用 `AMUX_E2E_HOME` 固定以便事后查看日志。
enum TestHome {
    Temp(tempfile::TempDir),
    Fixed(PathBuf),
}

impl TestHome {
    fn path(&self) -> &Path {
        match self {
            TestHome::Temp(dir) => dir.path(),
            TestHome::Fixed(path) => path.as_path(),
        }
    }
}

struct Process {
    child: Child,
}

impl Process {
    #[cfg(unix)]
    fn terminate(&self) {
        // SAFETY: child.id() 返回当前子进程 PID；SIGTERM 仅发送给该进程。
        let result = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        assert_eq!(result, 0, "发送 SIGTERM 失败");
    }

    #[cfg(unix)]
    async fn wait_for_exit(&mut self) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("查询子进程状态失败") {
                assert!(status.success(), "子进程异常退出: {status}");
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "等待子进程退出超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("测试可执行路径");
    let dir = exe.parent().expect("目录");
    if dir.ends_with("deps") {
        dir.parent().unwrap_or(dir).to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// daemon 二进制与测试不同包，无法用 `CARGO_BIN_EXE_`；缺失时现构建一次。
fn daemon_binary() -> PathBuf {
    let path = target_dir().join("amux-daemon");
    if !path.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "amux-daemon", "--bin", "amux-daemon"])
            .status()
            .expect("构建 amux-daemon");
        assert!(status.success(), "构建 amux-daemon 失败");
    }
    assert!(path.exists(), "缺少 amux-daemon 二进制: {path:?}");
    path
}

/// 假 bin 目录：`codex`（发现用）与 `bunx`（启动 agent 时改为运行模拟 agent）。
fn fake_bin(mock_agent: &Path, state_file: &Path) -> PathBuf {
    let dir = state_file.parent().expect("状态文件目录").join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let codex = dir.join("codex");
    std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    let bunx = dir.join("bunx");
    std::fs::write(
        &bunx,
        format!(
            "#!/bin/sh\nexec '{}' '{}'\n",
            mock_agent.display(),
            state_file.display()
        ),
    )
    .unwrap();
    for file in [codex, bunx] {
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(0o755);
        }
        std::fs::set_permissions(&file, permissions).unwrap();
    }
    dir
}

fn spawn_server(port: u16, home: &Path) -> Process {
    let child = Command::new(env!("CARGO_BIN_EXE_amux-server"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--token",
            TOKEN,
        ])
        .env("AMUX_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 amux-server 失败");
    Process { child }
}

fn spawn_daemon(port: u16, home: &Path, path_dirs: &[&Path]) -> Process {
    let mut paths: Vec<PathBuf> = path_dirs.iter().map(|dir| dir.to_path_buf()).collect();
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).expect("PATH");
    let child = Command::new(daemon_binary())
        .args([
            "--machine",
            MACHINE,
            "--server",
            &format!("ws://127.0.0.1:{port}"),
            "--token",
            TOKEN,
        ])
        .env("AMUX_HOME", home)
        .env("PATH", path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 amux-daemon 失败");
    Process { child }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    listener.local_addr().unwrap().port()
}

/// 轮询直到取得结果（超时即失败）。
async fn poll<T, F, Fut>(mut probe: F, what: &str) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "等待超时: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    async fn get(&self, path: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("请求失败");
        assert!(
            response.status().is_success(),
            "GET {path} 返回 {}",
            response.status()
        );
        response.json().await.expect("响应不是 JSON")
    }

    /// 容错读取：连接失败或非 2xx 返回 None（轮询用）。
    async fn try_get(&self, path: &str) -> Option<Value> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .ok()?;
        response.status().is_success().then_some(())?;
        response.json().await.ok()
    }

    async fn get_status(&self, path: &str) -> reqwest::StatusCode {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("请求失败")
            .status()
    }

    /// 不带认证头的请求（用于校验未认证一律被拒）。
    async fn get_status_anonymous(&self, path: &str) -> reqwest::StatusCode {
        self.http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("请求失败")
            .status()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .expect("请求失败")
    }

    async fn post_ok(&self, path: &str, body: Value) -> Value {
        let response = self.post(path, body).await;
        assert!(
            response.status().is_success(),
            "POST {path} 返回 {}",
            response.status()
        );
        response.json().await.unwrap_or(Value::Null)
    }

    async fn delete(&self, path: &str) -> reqwest::StatusCode {
        self.http
            .delete(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("请求失败")
            .status()
    }

    async fn put_ok(&self, path: &str, body: Value) {
        let response = self
            .http
            .put(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .expect("请求失败");
        assert!(response.status().is_success(), "PUT {path} 失败");
    }
}

#[tokio::test]
async fn server_daemon_agent_end_to_end() {
    // 诊断用：AMUX_E2E_HOME 指定时保留运行目录（便于复现失败现场）
    let home = match std::env::var("AMUX_E2E_HOME") {
        Ok(path) if !path.is_empty() => {
            std::fs::create_dir_all(&path).unwrap();
            tempfile::TempDir::new().map(|_| ()).ok();
            TestHome::Fixed(PathBuf::from(path))
        }
        _ => TestHome::Temp(tempfile::tempdir().unwrap()),
    };
    let workspace = tempfile::tempdir().unwrap();
    let state_file = home.path().join("mock_state");
    let bin = fake_bin(Path::new(env!("CARGO_BIN_EXE_mock_acp")), &state_file);
    let port = free_port();

    let _server = spawn_server(port, home.path());

    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };

    // 先等 server 监听再拉起 daemon：daemon 首次连不上会等一个重连周期（1 分钟）
    poll(
        || {
            let client = &client;
            async move {
                client
                    .http
                    .get(format!("{}/machines", client.base))
                    .send()
                    .await
                    .ok()
            }
        },
        "server 监听",
    )
    .await;
    assert_eq!(
        client.get_status_anonymous("/machines").await,
        reqwest::StatusCode::UNAUTHORIZED
    );

    let _daemon = spawn_daemon(port, home.path(), &[&bin]);

    // Daemon 接入：机器信息来自 machine.info
    let machines = poll(
        || {
            let client = &client;
            async move {
                let value = client.try_get("/machines").await?;
                let machines = value.as_array().cloned().unwrap_or_default();
                (!machines.is_empty()).then_some(machines)
            }
        },
        "机器列表非空",
    )
    .await;
    assert!(
        machines.iter().any(|machine| machine["name"] == MACHINE),
        "应包含 {MACHINE}: {machines:?}"
    );

    // Agent 生命周期：发现 → 启动 → ACP 初始化完成 ⇒ 可用
    let agents = poll(
        || async {
            let value = client
                .try_get(&format!("/machines/{MACHINE}/agents"))
                .await?;
            let agents = value.as_array().cloned().unwrap_or_default();
            agents
                .iter()
                .any(|agent| agent["name"] == "codex" && agent["available"] == true)
                .then_some(agents)
        },
        "codex 可用",
    )
    .await;
    // 仅 codex 可用：nano 已内置但未配置模型，连接未就绪
    let available: Vec<_> = agents
        .iter()
        .filter(|agent| agent["available"] == true)
        .map(|agent| agent["name"].clone())
        .collect();
    assert_eq!(available, vec![json!("codex")], "{agents:?}");

    // 建会话（惰性创建 agent 侧会话）
    let session = client
        .post_ok(
            "/sessions",
            json!({
                "machine": MACHINE,
                "agent": "codex",
                "workspace": workspace.path().to_string_lossy(),
            }),
        )
        .await;
    let session_id = session["id"].as_str().unwrap().to_string();
    assert_eq!(session["state"], "idle");

    // 发指令：用户消息立即落盘，agent 输出与活动随后到达
    client
        .post_ok(
            &format!("/sessions/{session_id}"),
            json!({ "input": [{ "type": "text", "text": "你好" }] }),
        )
        .await;

    let history = poll(
        || {
            let client = &client;
            let path = format!("/sessions/{session_id}/history");
            async move {
                let value = client.try_get(&path).await?;
                let items = value["items"].as_array().cloned().unwrap_or_default();
                items
                    .iter()
                    .any(|item| {
                        item["role"] == "agent"
                            && item["content"][0]["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("完成：你好"))
                    })
                    .then_some(items)
            }
        },
        "agent 回复落盘",
    )
    .await;
    assert!(
        history.iter().any(|item| item["role"] == "user"),
        "应有用户消息: {history:?}"
    );

    // 会话状态回到空闲、标题取首条提示词
    let session = poll(
        || {
            let client = &client;
            let path = format!("/sessions/{session_id}");
            async move {
                let value = client.try_get(&path).await?;
                (value["state"] == "idle" && value["title"] == "你好").then_some(value)
            }
        },
        "会话回到空闲且标题生成",
    )
    .await;
    assert_eq!(session["machine"], MACHINE);
    assert_eq!(session["agent"], "codex");

    let agents = client.get(&format!("/machines/{MACHINE}/agents")).await;
    let codex = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["name"] == "codex")
        .expect("应有 codex");
    assert_eq!(
        codex["openedSessions"], 1,
        "打开但可能空闲的 ACP session 应计入打开会话数: {agents:?}"
    );

    // 活动：思考与工具调用按 upsert 合并为两条
    let activities = poll(
        || {
            let client = &client;
            let path = format!("/sessions/{session_id}/activities");
            async move {
                let value = client.try_get(&path).await?;
                let activities = value["activities"].as_array().cloned().unwrap_or_default();
                (activities.len() >= 2).then_some(activities)
            }
        },
        "活动落盘",
    )
    .await;
    assert!(
        activities.iter().any(|item| item["kind"] == "thinking"),
        "{activities:?}"
    );
    let tool_call = activities
        .iter()
        .find(|item| item["kind"] == "tool_call")
        .expect("应有工具调用活动");
    assert_eq!(tool_call["tool_call_id"], "tc1");
    assert_eq!(
        activities
            .iter()
            .filter(|item| item["kind"] == "tool_call")
            .count(),
        1,
        "同一 toolCallId 的多次 update 应合并为一条活动"
    );

    // 斜杠命令 / 计划 / 上下文：来自 ACP 通知的缓存
    let commands = client
        .get(&format!("/sessions/{session_id}/slash_commands"))
        .await;
    assert_eq!(commands["commands"].as_array().unwrap().len(), 2);
    let plan = client.get(&format!("/sessions/{session_id}/plan")).await;
    assert_eq!(plan["entries"].as_array().unwrap().len(), 3);
    let context = client.get(&format!("/sessions/{session_id}/context")).await;
    assert_eq!(context["contextSize"], 53_000);
    assert_eq!(context["contextWindowSize"], 200_000);

    // 会话选项：session/new 返回的 model 选项
    let options = client
        .get(&format!("/sessions/{session_id}/config_options"))
        .await;
    assert!(
        !options["options"].as_array().unwrap().is_empty(),
        "{options}"
    );

    // 终端：PTY 输出经 Daemon 上行、Server 缓存，并通过 SSE 增量读取
    let terminal = client
        .post_ok(
            &format!("/sessions/{session_id}/terminals"),
            json!({
                "cwd": workspace.path().to_string_lossy(),
                "cols": 80,
                "rows": 24,
            }),
        )
        .await;
    let terminal_id = terminal["terminalId"]
        .as_str()
        .expect("terminalId")
        .to_string();
    let response = client
        .http
        .get(format!(
            "{}/sessions/{session_id}/terminals/{terminal_id}",
            client.base
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("连接终端 SSE 失败");
    assert!(response.status().is_success());
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );

    let input = base64::engine::general_purpose::STANDARD.encode(b"echo amux-terminal\n");
    client
        .post_ok(
            &format!("/sessions/{session_id}/terminals/{terminal_id}"),
            json!({ "data": input }),
        )
        .await;
    let stream = response.bytes_stream();
    tokio::pin!(stream);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut wire = String::new();
    let output = loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("等待终端 SSE 输出超时")
            .expect("终端 SSE 提前结束")
            .expect("读取终端 SSE 失败");
        wire.push_str(&String::from_utf8_lossy(&chunk));
        let found = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|value| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value["data"].as_str().unwrap_or_default())
                    .unwrap_or_default();
                String::from_utf8_lossy(&bytes).contains("amux-terminal")
            });
        if let Some(output) = found {
            break output;
        }
    };
    assert!(output["nextCursor"].as_u64().unwrap() > 0);
    let terminals = client
        .get(&format!("/sessions/{session_id}/terminals"))
        .await;
    assert_eq!(terminals.as_array().unwrap().len(), 1);

    // 配置读写
    client
        .put_ok(
            "/config/skills/",
            json!([{ "name": "opencli", "description": "desc" }]),
        )
        .await;
    let skills = client.get("/config/skills/").await;
    assert_eq!(skills[0]["name"], "opencli");
    assert!(
        client
            .get("/config/recent_workspaces/")
            .await
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["machine"] == MACHINE),
        "建会话后应记录最近工作目录"
    );

    // 最近工作目录全量更新
    let recent = client.get("/config/recent_workspaces/").await;
    let mut recent = recent.as_array().unwrap().clone();
    recent.push(json!({
        "machine": "other-pc",
        "workspace": "/tmp/amux",
        "lastUsed": 1729000000000_i64,
    }));
    client
        .put_ok("/config/recent_workspaces/", json!(recent))
        .await;
    let updated = client.get("/config/recent_workspaces/").await;
    assert!(
        updated
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["machine"] == "other-pc"),
        "PUT 全量更新后应包含新机器"
    );

    // 工作流会话：创建与查询（关联会话由编排智能体建立）
    let workflow = client
        .post_ok("/workflows", json!({ "plan": "使用本机 codex 实现功能" }))
        .await;
    let workflow_id = workflow["id"].as_str().unwrap().to_string();
    let workflows = client.get("/workflows").await;
    assert_eq!(workflows["workflows"].as_array().unwrap().len(), 1);
    let fetched = client.get(&format!("/workflows/{workflow_id}")).await;
    assert_eq!(fetched["state"], "idle");
    assert!(fetched["linkedSessions"].as_array().unwrap().is_empty());

    // 删除会话：元数据立即消失
    assert_eq!(
        client.delete(&format!("/sessions/{session_id}")).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client.get_status(&format!("/sessions/{session_id}")).await,
        reqwest::StatusCode::NOT_FOUND
    );
    poll(
        || {
            let client = &client;
            async move {
                let agents = client
                    .try_get(&format!("/machines/{MACHINE}/agents"))
                    .await?;
                agents
                    .as_array()?
                    .iter()
                    .find(|agent| agent["name"] == "codex")
                    .filter(|agent| agent["openedSessions"] == 0)
                    .cloned()
            }
        },
        "关闭 ACP session 后打开会话数归零",
    )
    .await;
    assert_eq!(
        client.delete(&format!("/workflows/{workflow_id}")).await,
        reqwest::StatusCode::OK
    );
}

#[cfg(unix)]
#[tokio::test]
async fn server_shutdown_closes_all_open_agent_sessions() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let state_file = home.path().join("mock_state");
    let bin = fake_bin(Path::new(env!("CARGO_BIN_EXE_mock_acp")), &state_file);
    let port = free_port();

    let mut server = spawn_server(port, home.path());
    let client = Client {
        http: reqwest::Client::new(),
        base: format!("http://127.0.0.1:{port}"),
    };
    poll(
        || {
            let client = &client;
            async move {
                client
                    .http
                    .get(format!("{}/machines", client.base))
                    .send()
                    .await
                    .ok()
            }
        },
        "server 监听",
    )
    .await;

    let _daemon = spawn_daemon(port, home.path(), &[&bin]);
    poll(
        || async {
            let agents = client
                .try_get(&format!("/machines/{MACHINE}/agents"))
                .await?;
            agents
                .as_array()?
                .iter()
                .any(|agent| agent["name"] == "codex" && agent["available"] == true)
                .then_some(())
        },
        "codex 可用",
    )
    .await;

    for _ in 0..2 {
        let session = client
            .post_ok(
                "/sessions",
                json!({
                    "machine": MACHINE,
                    "agent": "codex",
                    "workspace": workspace.path().to_string_lossy(),
                }),
            )
            .await;
        let session_id = session["id"].as_str().unwrap();
        client
            .get(&format!("/sessions/{session_id}/config_options"))
            .await;
    }

    poll(
        || async {
            let agents = client
                .try_get(&format!("/machines/{MACHINE}/agents"))
                .await?;
            agents
                .as_array()?
                .iter()
                .find(|agent| agent["name"] == "codex")
                .filter(|agent| agent["openedSessions"] == 2)
                .cloned()
        },
        "两个 agent 侧会话均已打开",
    )
    .await;

    server.terminate();
    server.wait_for_exit().await;

    let calls = std::fs::read_to_string(format!("{}.calls", state_file.display())).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| *line == "session/close")
            .count(),
        2,
        "Server 关闭时应关闭所有打开的 Agent 侧会话: {calls:?}"
    );
}

#[tokio::test]
async fn prompt_accepts_body_above_axum_default_limit() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let _server = spawn_server(port, home.path());

    // 先等 server 监听，否则请求会连不上。
    poll(
        || {
            let client = reqwest::Client::new();
            let base = base.clone();
            async move {
                client
                    .get(format!("{base}/machines"))
                    .send()
                    .await
                    .ok()
                    .map(|_| ())
            }
        },
        "server 监听",
    )
    .await;

    // 超过 axum 默认 2 MiB 的 base64 图片附件：会话不存在时应解析 body 后返回 404，
    // 而不是在读取 body 时被 413 Payload Too Large 拒绝。
    let blob = "A".repeat(4 * 1024 * 1024);
    let response = reqwest::Client::new()
        .post(format!("{base}/sessions/nonexistent"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "input": [{
                "type": "resource",
                "resource": {
                    "blob": blob,
                    "uri": "large.png",
                    "mimeType": "image/png",
                },
            }],
        }))
        .send()
        .await
        .expect("POST /sessions 失败");
    assert_ne!(
        response.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "图片附件不应触发 413"
    );
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}
