//! 端到端集成测试：启动真实 server 二进制（test-server 固定用 mock_acp），
//! 经 WebSocket + JSON-RPC 验证协议。
//! 覆盖：会话生命周期、非流式交付（通知序列）、标题生成、忙时 steer 报错（-32006）、
//! 多客户端共存、git status/diff/revert、agent 发现与默认模型、skills 列表。

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
    async fn connect(port: u16) -> Self {
        let url = format!("ws://127.0.0.1:{port}/?token=test-token");
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

    /// 只发送请求不等待响应（用于并发/忙时场景）。
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
        loop {
            // 先查已缓冲的
            if let Some((m, p)) = self
                .notifications
                .iter()
                .find(|(m, p)| m == method && pred(p))
            {
                let _ = (m, p);
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            if let Ok(Some(Ok(Message::Text(t)))) =
                tokio::time::timeout(std::time::Duration::from_millis(100), self.read.next()).await
            {
                let v: Value = serde_json::from_str(&t).unwrap();
                if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                    self.notifications.push((
                        m.to_string(),
                        v.get("params").cloned().unwrap_or(Value::Null),
                    ));
                    if m == method && pred(v.get("params").unwrap_or(&Value::Null)) {
                        return true;
                    }
                }
            }
        }
    }
}

async fn wait_port(port: u16) -> u16 {
    // 等待 server 监听就绪（并行测试 + 全量编译负载下偶发启动慢，放宽到 10s）
    for _ in 0..100 {
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

/// 测试结束自动杀掉 server 子进程（避免并行测试端口残留）。
struct ServerGuard {
    child: tokio::process::Child,
}
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn spawn_server() -> (u16, ServerGuard) {
    let bin = env!("CARGO_BIN_EXE_test-server");
    // 每测试唯一端口（并行测试不冲突）
    let port = 36000 + (std::process::id() % 500) as u16 + NEXT_PORT.fetch_add(1, Ordering::SeqCst);
    let child = tokio::process::Command::new(bin)
        .args(["--token", "test-token", "--port", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test-server");
    wait_port(port).await;
    (port, ServerGuard { child })
}

/// 首个可用 harness（get_info 的 available agent）。
fn first_harness(info: &Value) -> String {
    info["result"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["available"].as_bool().unwrap_or(false))
        .and_then(|h| h["name"].as_str())
        .unwrap_or("mock_acp")
        .to_string()
}

#[tokio::test]
async fn session_lifecycle_and_activities() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;

    let info = c.call("get_info", json!({})).await;
    assert_eq!(info["result"]["serverVersion"], "0.1.0");
    let harness = first_harness(&info);
    assert!(!harness.is_empty(), "get_info 应返回可用 agent");

    let created = c
        .call(
            "create_session",
            json!({"harness": harness, "cwd": "/tmp/work"}),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(created["result"]["session"]["state"], "idle");
    assert_eq!(created["result"]["session"]["harness"], harness);

    c.fire(
        "prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "实现登录功能"}]}),
    )
    .await;
    // 等待 turn 结束（透传边界，docs/DESIGN.md §5.1）
    let got = c
        .wait_notification(
            "passthrough",
            |p| p["session_id"] == json!(sid) && p["event"]["kind"] == "turn_ended",
            5000,
        )
        .await;
    assert!(got, "应收到 turn_ended 透传边界");

    // 通知序列是透传事件（turn 边界 + chunk + 活动事件），无聚合交付
    let seq: Vec<&str> = c.notifications.iter().map(|(m, _)| m.as_str()).collect();
    let joined = seq.join(",");
    assert!(
        seq.iter()
            .all(|m| *m == "passthrough" || *m == "session_created" || *m == "session_updated"),
        "通知应全部为透传/会话元数据: {joined}"
    );
    let kinds: Vec<&str> = c
        .notifications
        .iter()
        .filter(|(m, _)| m == "passthrough")
        .filter_map(|(_, p)| p["event"]["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"turn_started"),
        "缺少 turn_started: {joined}"
    );
    assert!(
        kinds.contains(&"output_chunk"),
        "缺少 output_chunk（事件透传而非聚合完整输出）: {kinds:?}"
    );
    assert!(
        kinds.contains(&"thinking_chunk") && kinds.contains(&"tool_call"),
        "应含 thinking/tool_call 透传事件: {kinds:?}"
    );
    // 不再有聚合交付通知（turn_completed / session_state / activity）
    assert!(
        !seq.iter().any(|m| matches!(
            *m,
            "turn_completed" | "session_state" | "activity" | "user_message"
        )),
        "不应有聚合交付通知: {joined}"
    );

    // 首条 prompt 后标题非空（按首条指令生成）
    let list = c.call("list_sessions", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    let meta = sessions.iter().find(|s| s["id"] == json!(sid)).unwrap();
    assert!(
        !meta["title"].as_str().unwrap_or("").is_empty(),
        "首条 prompt 后标题应非空: {meta}"
    );

    // 用户可修改标题
    let r = c
        .call(
            "set_session_title",
            json!({"sessionId": sid, "title": "我的标题"}),
        )
        .await;
    assert!(r.get("error").is_none());
    let list = c.call("list_sessions", json!({})).await;
    let meta = list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!(sid))
        .unwrap();
    assert_eq!(meta["title"], "我的标题");

    // open_session：返回透传事件（GUI 聚合，docs/DESIGN.md §5.2）
    let open = c.call("open_session", json!({"sessionId": sid})).await;
    assert!(open["result"]["events"].is_array());
    let kinds: Vec<&str> = open["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"output_chunk"),
        "open_session 应含 output_chunk 事件: {kinds:?}"
    );

    // 列表状态回到空闲（重连经 meta.state 补齐）
    let list = c.call("list_sessions", json!({})).await;
    let meta = list["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!(sid))
        .unwrap();
    assert_eq!(meta["state"], "idle");
}

#[tokio::test]
async fn busy_prompt_returns_steer_unsupported() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;
    let harness = {
        let info = c.call("get_info", json!({})).await;
        first_harness(&info)
    };

    let created = c
        .call(
            "create_session",
            json!({"harness": harness, "cwd": "/tmp/work"}),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 发第一个 prompt 不等待响应；mock_acp 有 300ms busy 窗口
    c.fire(
        "prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "一"}]}),
    )
    .await;
    // 等待 turn 开始（透传边界；server 列表状态随之 busy）
    let busy = c
        .wait_notification(
            "passthrough",
            |p| p["session_id"] == json!(sid) && p["event"]["kind"] == "turn_started",
            3000,
        )
        .await;
    assert!(busy, "应观察到 turn 开始边界");

    // 忙时 prompt → STEER_UNSUPPORTED (-32006)
    let second = c
        .call(
            "prompt",
            json!({"sessionId": sid, "input": [{"type": "text", "text": "二"}]}),
        )
        .await;
    assert_eq!(
        second["error"]["code"], -32006,
        "忙时 prompt 应报 steer 不支持: {second}"
    );

    // 等待第一个 turn 结束（透传边界），会话恢复 idle 后可继续 prompt
    let idle = c
        .wait_notification(
            "passthrough",
            |p| p["session_id"] == json!(sid) && p["event"]["kind"] == "turn_ended",
            5000,
        )
        .await;
    assert!(idle, "turn 结束后应收到 turn_ended 边界");
    let third = c
        .call(
            "prompt",
            json!({"sessionId": sid, "input": [{"type": "text", "text": "三"}]}),
        )
        .await;
    assert!(
        third.get("error").is_none(),
        "idle 后 prompt 应正常: {third}"
    );
}

#[tokio::test]
async fn close_resume_delete_lifecycle() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;
    let harness = {
        let info = c.call("get_info", json!({})).await;
        first_harness(&info)
    };

    let created = c
        .call(
            "create_session",
            json!({"harness": harness, "cwd": "/tmp/work"}),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 直接 prompt 可正常执行
    let prompt = c
        .call(
            "prompt",
            json!({"sessionId": sid, "input": [{"type": "text", "text": "直接开始"}]}),
        )
        .await;
    assert!(prompt.get("error").is_none(), "prompt 应正常");

    // delete 后列表为空
    let r = c.call("delete_session", json!({"sessionId": sid})).await;
    assert!(r.get("error").is_none());
    let list = c.call("list_sessions", json!({})).await;
    assert!(
        list["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != json!(sid)),
        "delete 后会话应消失"
    );
}

#[tokio::test]
async fn missing_session_returns_not_found() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;

    let open = c.call("open_session", json!({"sessionId": "nope"})).await;
    assert_eq!(open["error"]["code"], -32001);
}

#[tokio::test]
async fn multi_client_coexist() {
    let (port, _guard) = spawn_server().await;
    let mut c1 = Client::connect(port).await;
    let mut c2 = Client::connect(port).await;

    let harness = {
        let info = c1.call("get_info", json!({})).await;
        first_harness(&info)
    };
    let created = c1
        .call(
            "create_session",
            json!({"harness": harness, "cwd": "/tmp/work"}),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // c1 prompt；两个客户端都应收到同一透传事件流
    c1.fire(
        "prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "多客户端"}]}),
    )
    .await;
    let got1 = c1
        .wait_notification(
            "passthrough",
            |p| p["session_id"] == json!(sid) && p["event"]["kind"] == "turn_ended",
            5000,
        )
        .await;
    let got2 = c2
        .wait_notification(
            "passthrough",
            |p| p["session_id"] == json!(sid) && p["event"]["kind"] == "turn_ended",
            5000,
        )
        .await;
    assert!(got1 && got2, "两个客户端都应收到 turn_ended 透传边界");
    // c2 也能对同一会话 prompt（未被踢出）
    let prompt = c2
        .call(
            "prompt",
            json!({"sessionId": sid, "input": [{"type": "text", "text": "c2 发消息"}]}),
        )
        .await;
    assert!(prompt.get("error").is_none(), "c2 prompt 应正常: {prompt}");

    // 两个客户端都未被断开（还能正常请求）
    let list = c1.call("list_sessions", json!({})).await;
    assert_eq!(list["result"]["sessions"].as_array().unwrap().len(), 1);
    let list2 = c2.call("list_sessions", json!({})).await;
    assert_eq!(list2["result"]["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn default_model_and_skills() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;
    let info = c.call("get_info", json!({})).await;
    let harness = first_harness(&info);

    // 配置默认模型后 get_info 返回它
    let r = c
        .call(
            "set_default_model",
            json!({"harness": harness, "model": "gpt-4o"}),
        )
        .await;
    assert!(r.get("error").is_none());
    let info2 = c.call("get_info", json!({})).await;
    let h = info2["result"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == json!(harness))
        .unwrap();
    assert_eq!(h["defaultModel"], "gpt-4o");

    // skills 列表（mock_acp 返回固定列表）
    let skills = c
        .call("list_agent_skills", json!({"harness": harness}))
        .await;
    let names: Vec<&str> = skills["result"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(
        names.contains(&"web-browser"),
        "mock_acp 应返回 skills: {names:?}"
    );
}

// ---- git e2e：对真实临时 git 仓库 ----

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

#[tokio::test]
async fn git_status_diff_revert_e2e() {
    let (port, _guard) = spawn_server().await;
    let mut c = Client::connect(port).await;
    let dir = init_repo();
    let cwd = dir.to_str().unwrap().to_string();

    // 修改 tracked 文件 + 新增 untracked 文件
    std::fs::write(dir.join("a.txt"), "line1\nCHANGED\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();

    // git_status：Modified/Untracked 与增减行数
    let st = c.call("git_status", json!({"cwd": cwd})).await;
    assert!(st.get("error").is_none(), "git_status 失败: {st}");
    let changes = st["result"]["changes"].as_array().unwrap();
    assert_eq!(st["result"]["branch"], "main");
    let a = changes.iter().find(|ch| ch["path"] == "a.txt").unwrap();
    assert_eq!(a["status"], "modified");
    assert_eq!(a["additions"], 1);
    assert_eq!(a["deletions"], 1);
    let n = changes.iter().find(|ch| ch["path"] == "new.txt").unwrap();
    assert_eq!(n["status"], "untracked");

    // git_diff：含 patch 与 hunk
    let d = c.call("git_diff", json!({"cwd": cwd})).await;
    let files = d["result"]["files"].as_array().unwrap();
    let a_diff = files.iter().find(|f| f["path"] == "a.txt").unwrap();
    assert!(a_diff["patch"].as_str().unwrap().contains("diff --git"));
    assert_eq!(a_diff["additions"], 1);
    assert_eq!(a_diff["deletions"], 1);
    assert_eq!(a_diff["hunks"].as_array().unwrap().len(), 1);

    // git_revert 单文件后工作区恢复
    let r = c
        .call("git_revert", json!({"cwd": cwd, "path": "a.txt"}))
        .await;
    assert_eq!(r["result"]["ok"], true, "revert 失败: {r}");
    let content = std::fs::read_to_string(dir.join("a.txt")).unwrap();
    assert_eq!(content, "line1\nline2\n", "单文件 revert 后应恢复到 HEAD");

    // 非 git 目录：git_status 返回 not_repo
    let plain = std::env::temp_dir().join(format!("amux-e2e-plain-{}", std::process::id()));
    std::fs::create_dir_all(&plain).unwrap();
    let st = c
        .call("git_status", json!({"cwd": plain.to_str().unwrap()}))
        .await;
    assert_eq!(
        st["result"]["notRepo"], true,
        "非 git 目录应 not_repo: {st}"
    );
    let d = c
        .call("git_diff", json!({"cwd": plain.to_str().unwrap()}))
        .await;
    assert_eq!(d["result"]["notRepo"], true);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&plain);
}
