//! 端到端集成测试：启动真实 server 二进制（test-server 固定用 mock_acp），
//! 经 WebSocket + JSON-RPC 验证协议。
//! 覆盖：认证（未认证 AUTH_FAILED / 成功）、agent.list/skills、会话惰性创建、
//! prompt（含 state_change busy→idle 推送）、session.history/activities/ongoing_activity 分页、
//! session.configure、删除触发 ACP session/close、workspace.diff/restore。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

struct Client {
    write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    next_id: u64,
    notifications: Vec<(String, Value)>,
}

impl Client {
    /// 连接不发送认证。
    async fn connect_raw(port: u16) -> Self {
        let url = format!("ws://127.0.0.1:{port}");
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("连接 server");
        let (write, read) = ws.split();
        Client {
            write,
            read,
            next_id: 1,
            notifications: Vec::new(),
        }
    }

    /// 连接并完成 `auth` 认证。
    async fn connect(port: u16, token: &str) -> Self {
        let mut c = Self::connect_raw(port).await;
        let r = c.call("auth", json!({"token": token})).await;
        assert!(r.get("error").is_none(), "认证应成功: {r}");
        c
    }

    /// 发送请求并等待匹配 id 的响应；期间到达的通知缓冲到 notifications。
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
            ))
            .await
            .unwrap();
        loop {
            let msg = self.read.next().await.unwrap().unwrap();
            let Message::Text(t) = msg else { continue };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                self.notifications.push((
                    m.to_string(),
                    v.get("params").cloned().unwrap_or(Value::Null),
                ));
            }
        }
    }

    /// 只发送请求不等待响应（用于忙时/后台场景）。
    async fn fire(&mut self, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.write
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
            ))
            .await
            .unwrap();
    }

    /// 在超时内等待某个通知（可带谓词）；返回是否等到。
    async fn wait_notification(
        &mut self,
        method: &str,
        pred: impl Fn(&Value) -> bool,
        timeout_ms: u64,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            let msg = tokio::time::timeout(remaining, self.read.next())
                .await
                .ok()
                .flatten()
                .unwrap()
                .unwrap();
            let Message::Text(t) = msg else { continue };
            let v: Value = match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("method").and_then(|m| m.as_str()) == Some(method)
                && pred(v.get("params").unwrap_or(&Value::Null))
            {
                return true;
            }
        }
        false
    }
}

async fn wait_port(port: u16) -> u16 {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return port;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("server 未就绪");
}

use std::sync::atomic::{AtomicU16, Ordering};

static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

struct ServerGuard {
    child: tokio::process::Child,
}
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn spawn_server_with_delay(
    port: u16,
    data_dir: std::path::PathBuf,
    delay_ms: u32,
) -> ServerGuard {
    let bin = env!("CARGO_BIN_EXE_test-server");
    let child = tokio::process::Command::new(bin)
        .args([
            "--token",
            "test-token",
            "--port",
            &port.to_string(),
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .env("AMUX_MOCK_STATE", data_dir.join("mock.state"))
        .env("AMUX_NO_DISCOVERY", "1")
        .env("AMUX_MOCK_DELAY_MS", delay_ms.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test-server");
    wait_port(port).await;
    ServerGuard { child }
}

async fn start_server() -> (u16, ServerGuard) {
    let (port, _data_dir, guard) = start_server_with_dir().await;
    (port, guard)
}

/// 第一个可用 agent 名（agent.list）。
fn first_agent(list: &Value) -> String {
    list["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["available"].as_bool().unwrap_or(false))
        .and_then(|a| a["name"].as_str())
        .unwrap_or("mock_acp")
        .to_string()
}

fn mock_calls(data_dir: &std::path::Path) -> String {
    std::fs::read_to_string(data_dir.join("mock.state.calls")).unwrap_or_default()
}

// ---- 认证（docs/DESIGN.md「认证」）----

#[tokio::test]
async fn auth_required_and_enforced() {
    let (port, _guard) = start_server().await;
    // 未认证连接请求其它方法 → AUTH_FAILED
    let mut c = Client::connect_raw(port).await;
    let r = c.call("session.list", json!({})).await;
    assert_eq!(r["error"]["code"], -32000, "未认证应 AUTH_FAILED: {r}");

    // 错误 token → AUTH_FAILED
    let mut c2 = Client::connect_raw(port).await;
    let r = c2.call("auth", json!({"token": "wrong"})).await;
    assert_eq!(r["error"]["code"], -32000, "错误 token 应 AUTH_FAILED: {r}");

    // 正确 token → 认证通过，可正常调用
    let r = c2.call("auth", json!({"token": "test-token"})).await;
    assert!(r.get("error").is_none(), "认证应成功: {r}");
    let list = c2.call("agent.list", json!({})).await;
    assert!(list.get("error").is_none(), "认证后可正常调用: {list}");
}

// ---- agent.list / agent.skills ----

#[tokio::test]
async fn agent_list_and_skills() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let list = c.call("agent.list", json!({})).await;
    let agents: Vec<&str> = list["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert!(
        agents.contains(&"mock_acp"),
        "agent.list 应含 mock_acp: {list}"
    );

    let s = c.call("agent.skills", json!({"agent": "mock_acp"})).await;
    let skills: Vec<&str> = s["result"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert!(
        skills.contains(&"web-browser"),
        "mock_acp 应返回 skills: {skills:?}"
    );
}

// ---- 会话生命周期 + 惰性 + state_change + 删除触发 session/close ----

#[tokio::test]
async fn session_lifecycle_state_change_and_delete() {
    let (port, data_dir, _guard) = start_server_with_dir().await;
    let mut c = Client::connect(port, "test-token").await;

    // session.new 惰性：只写注册表，不触发 ACP session/new
    let created = c
        .call(
            "session.new",
            json!({"agent": "mock_acp", "cwd": "/tmp/work"}),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(created["result"]["session"]["state"], "idle");
    assert!(
        !mock_calls(&data_dir).contains("session/new"),
        "惰性：session.new 不应触发 ACP session/new"
    );

    // session.list 惰性分页
    let list = c.call("session.list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], json!(sid));

    // prompt：触发 ACP session/new（懒创建）+ state_change busy→idle 推送
    c.fire(
        "session.prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "实现登录功能"}]}),
    )
    .await;
    let got = c
        .wait_notification(
            "session.state_change",
            |p| p["sessionId"] == json!(sid) && p["oldState"] == "busy" && p["newState"] == "idle",
            8000,
        )
        .await;
    assert!(got, "prompt 结束应推送 state_change busy→idle");
    assert!(
        mock_calls(&data_dir).contains("session/new"),
        "首条 prompt 才懒创建 ACP session/new"
    );

    // 标题按首条指令生成 + configure 可改标题
    let list = c.call("session.list", json!({})).await;
    let meta = list["result"]["sessions"].as_array().unwrap()[0].clone();
    assert!(
        !meta["title"].as_str().unwrap_or("").is_empty(),
        "首条 prompt 后标题非空: {meta}"
    );
    let r = c
        .call(
            "session.configure",
            json!({"sessionId": sid, "title": "我的标题"}),
        )
        .await;
    assert!(r.get("error").is_none());
    let list = c.call("session.list", json!({})).await;
    assert_eq!(
        list["result"]["sessions"].as_array().unwrap()[0]["title"],
        "我的标题"
    );

    // session.history：用户消息 + 一条合并输出
    let h = c.call("session.history", json!({"sessionId": sid})).await;
    let items = h["result"]["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(
        items[0]["kind"], "user_message",
        "历史首条为用户消息: {items:?}"
    );
    assert!(items.iter().any(|i| i["kind"] == "agent_message"));

    // session.activities：thinking + tool_call
    let a = c
        .call("session.activities", json!({"sessionId": sid}))
        .await;
    let acts = a["result"]["activities"].as_array().unwrap();
    assert!(acts.iter().any(|x| x["kind"] == "thinking"), "{acts:?}");
    assert!(acts.iter().any(|x| x["kind"] == "tool_call"), "{acts:?}");

    // ongoing_activity：空闲时为 null
    let oa = c
        .call("session.ongoing_activity", json!({"sessionId": sid}))
        .await;
    assert!(
        oa["result"]["activity"].is_null(),
        "空闲 ongoing_activity 为 null: {oa}"
    );

    // 删除触发 session/close
    let r = c.call("session.delete", json!({"sessionId": sid})).await;
    assert!(r.get("error").is_none(), "删除失败: {r}");
    assert!(
        mock_calls(&data_dir).contains("session/close"),
        "删除应触发 ACP session/close"
    );
    let list = c.call("session.list", json!({})).await;
    assert!(list["result"]["sessions"].as_array().unwrap().is_empty());
}

// ---- session.list 分页 ----

#[tokio::test]
async fn session_list_pagination() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let agent = mock_acp_name(&mut c).await;

    // 创建 3 个会话并分别 prompt（mock 延迟 → lastActiveAt 严格递增）
    let mut sids = Vec::new();
    for i in 0..3 {
        let created = c
            .call(
                "session.new",
                json!({"agent": agent, "cwd": format!("/tmp/w{i}")}),
            )
            .await;
        let sid = created["result"]["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        c.call(
            "session.prompt",
            json!({"sessionId": sid, "input": [{"type":"text","text":format!("指令{i}")}]}),
        )
        .await;
        sids.push(sid);
    }

    // limit=2：最近活跃一窗，hasMore=true，nextBefore=窗口最后一条
    let first = c.call("session.list", json!({"limit": 2})).await;
    let sessions = first["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "limit=2 应返回一窗: {first}");
    assert_eq!(first["result"]["hasMore"], true);
    let next_before = first["result"]["nextBefore"].as_u64().unwrap();
    assert_eq!(sessions[0]["id"], json!(sids[2]), "最晚 prompt 排最前");
    assert_eq!(sessions[1]["id"], json!(sids[1]));

    // before=next_before：更早一窗只剩最旧
    let second = c
        .call("session.list", json!({"limit": 2, "before": next_before}))
        .await;
    let s2 = second["result"]["sessions"].as_array().unwrap();
    assert_eq!(s2.len(), 1);
    assert_eq!(s2[0]["id"], json!(sids[0]));
    assert_eq!(second["result"]["hasMore"], false);
}

// ---- workspace.diff / workspace.restore ----

#[tokio::test]
async fn workspace_diff_reflects_changes() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();

    let d = c
        .call("workspace.diff", json!({"cwd": dir.to_str().unwrap()}))
        .await;
    assert!(d.get("error").is_none(), "diff 失败: {d}");
    let files = d["result"]["files"].as_array().unwrap();
    assert_eq!(files[0]["path"], "a.txt");
    assert!(files[0]["patch"].as_str().unwrap().contains("diff --git"));

    let r = c
        .call(
            "workspace.restore",
            json!({"cwd": dir.to_str().unwrap(), "path": "a.txt"}),
        )
        .await;
    assert_eq!(r["result"]["ok"], true, "restore 失败: {r}");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "line1\nline2\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workspace_list_and_read_browse_session_directory() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("README.md"), "one\ntwo\nthree\n").unwrap();

    let root = c
        .call(
            "workspace.list",
            json!({"cwd": dir.to_str().unwrap(), "limit": 10}),
        )
        .await;
    assert!(root.get("error").is_none(), "list 失败: {root}");
    assert_eq!(root["result"]["path"], "");
    assert!(root["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "src" && entry["isDir"] == true));

    let nested = c
        .call(
            "workspace.list",
            json!({"cwd": dir.to_str().unwrap(), "path": "src"}),
        )
        .await;
    assert_eq!(nested["result"]["entries"][0]["path"], "src/main.rs");

    let first = c
        .call(
            "workspace.read",
            json!({"cwd": dir.to_str().unwrap(), "path": "README.md", "limit": 2}),
        )
        .await;
    assert_eq!(first["result"]["content"], "one\ntwo\n");
    assert_eq!(first["result"]["hasMore"], true);
    let second = c
        .call(
            "workspace.read",
            json!({
                "cwd": dir.to_str().unwrap(),
                "path": "README.md",
                "offset": first["result"]["nextOffset"],
                "limit": 2
            }),
        )
        .await;
    assert_eq!(second["result"]["content"], "three\n");
    assert_eq!(second["result"]["hasMore"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn workspace_read_rejects_invalid_and_binary_paths() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::write(dir.join("binary.dat"), [0xff, 0xfe, 0xfd]).unwrap();

    let traversal = c
        .call(
            "workspace.read",
            json!({"cwd": dir.to_str().unwrap(), "path": "../outside.txt"}),
        )
        .await;
    assert!(
        traversal.get("error").is_some(),
        "应拒绝越界路径: {traversal}"
    );

    let binary = c
        .call(
            "workspace.read",
            json!({"cwd": dir.to_str().unwrap(), "path": "binary.dat"}),
        )
        .await;
    assert!(
        binary.get("error").is_some(),
        "应拒绝非 UTF-8 文件: {binary}"
    );

    #[cfg(unix)]
    {
        let outside =
            std::env::temp_dir().join(format!("amux-e2e-workspace-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let symlink = c
            .call(
                "workspace.read",
                json!({"cwd": dir.to_str().unwrap(), "path": "link/secret.txt"}),
            )
            .await;
        assert!(
            symlink.get("error").is_some(),
            "应拒绝越界 symlink: {symlink}"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- helpers ----

async fn mock_acp_name(c: &mut Client) -> String {
    let list = c.call("agent.list", json!({})).await;
    first_agent(&list)
}

async fn start_server_with_dir() -> (u16, std::path::PathBuf, ServerGuard) {
    let port = 36000 + (std::process::id() % 500) as u16 + NEXT_PORT.fetch_add(1, Ordering::SeqCst);
    let data_dir = std::env::temp_dir().join(format!(
        "amux-e2e-{}",
        std::process::id() as u64 * 1000 + NEXT_PORT.fetch_add(1, Ordering::SeqCst) as u64
    ));
    std::fs::create_dir_all(&data_dir).unwrap();
    let guard = spawn_server_with_delay(port, data_dir.clone(), 150).await;
    (port, data_dir, guard)
}

// git helpers
use std::path::Path;
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn init_repo() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("amux-e2e-git-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-b", "main", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "init", "-q"]);
    dir
}
