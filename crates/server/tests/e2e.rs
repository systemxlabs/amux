//! 端到端集成测试：启动真实 server 二进制（test-server 固定用 mock_acp），
//! 经 WebSocket + JSON-RPC 验证协议。
//! 覆盖：认证（未认证 AUTH_FAILED / 成功）、agent.list、会话惰性创建、
//! prompt（含 state_change busy→idle 推送）、session.history/activities/ongoing_activity 分页、
//! session.configure、session.slash_commands、删除触发 ACP session/close + session/delete、
//! workspace.diff/restore。

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

    async fn connect(port: u16, token: &str) -> Self {
        let mut c = Self::connect_raw(port).await;
        let r = c.call("auth", json!({"token": token})).await;
        assert!(r.get("error").is_none(), "认证应成功: {r}");
        c
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
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

    async fn fire(&mut self, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.write
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }

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
    _data_dir: tempfile::TempDir,
}
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn spawn_server_with_delay(
    port: u16,
    data_dir: tempfile::TempDir,
    delay_ms: u32,
) -> ServerGuard {
    let data_path = data_dir.path();
    let bin = env!("CARGO_BIN_EXE_test-server");
    let child = tokio::process::Command::new(bin)
        .args([
            "--token",
            "test-token",
            "--port",
            &port.to_string(),
            "--data-dir",
            data_path.to_str().unwrap(),
        ])
        .env("AMUX_MOCK_STATE", data_path.join("mock.state"))
        .env("AMUX_NO_DISCOVERY", "1")
        .env("AMUX_MOCK_DELAY_MS", delay_ms.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test-server");
    wait_port(port).await;
    ServerGuard {
        child,
        _data_dir: data_dir,
    }
}

async fn start_server() -> (u16, ServerGuard) {
    let (port, _data_dir, guard) = start_server_with_dir().await;
    (port, guard)
}

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

#[tokio::test]
async fn auth_required_and_enforced() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect_raw(port).await;
    let r = c.call("session.list", json!({})).await;
    assert_eq!(r["error"]["code"], -32000, "未认证应 AUTH_FAILED: {r}");

    let mut c2 = Client::connect_raw(port).await;
    let r = c2.call("auth", json!({"token": "wrong"})).await;
    assert_eq!(r["error"]["code"], -32000, "错误 token 应 AUTH_FAILED: {r}");

    let r = c2.call("auth", json!({"token": "test-token"})).await;
    assert!(r.get("error").is_none(), "认证应成功: {r}");
    let list = c2.call("agent.list", json!({})).await;
    assert!(list.get("error").is_none(), "认证后可正常调用: {list}");
}

#[tokio::test]
async fn auth_required_requests_are_not_released_after_later_auth() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect_raw(port).await;

    c.write
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session.list",
                "params": {}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    c.write
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "auth",
                "params": {"token": "test-token"}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let mut responses = std::collections::HashMap::new();
    while responses.len() < 2 {
        let msg = c.read.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else { continue };
        let response: Value = serde_json::from_str(&text).unwrap();
        if let Some(id) = response["id"].as_u64() {
            responses.insert(id, response);
        }
    }
    assert_eq!(responses[&1]["error"]["code"], -32000);
    assert!(responses[&2].get("error").is_none());
}

#[tokio::test]
async fn agent_list() {
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
}

#[tokio::test]
async fn agent_rediscover_succeeds_and_keeps_agents_available() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    // 重新发现成功（测试环境为显式 AMUX_AGENT_BIN + no_discovery，重扫为 no-op，
    // 已配置 agent 不受影响）
    let r = c.call("agent.rediscover", json!({})).await;
    assert!(r.get("error").is_none(), "rediscover 应成功: {r}");
    let list = c.call("agent.list", json!({})).await;
    assert!(
        list["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "mock_acp"),
        "重新发现后 mock_acp 仍应在列: {list}"
    );
}

#[tokio::test]
async fn session_lifecycle_state_change_and_delete() {
    let (port, data_dir, _guard) = start_server_with_dir().await;
    let mut c = Client::connect(port, "test-token").await;

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

    let list = c.call("session.list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], json!(sid));

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

    let list = c.call("session.list", json!({})).await;
    let meta = list["result"]["sessions"].as_array().unwrap()[0].clone();
    assert!(
        !meta["title"].as_str().unwrap_or("").is_empty(),
        "首条 prompt 后标题非空: {meta}"
    );
    // 回归：会话选项不在元数据里，改经
    // session.config_options 查询（以 Agent 侧数据为权威，存于 Server 内存）
    let cfg = c
        .call("session.config_options", json!({"sessionId": sid}))
        .await;
    let options = cfg["result"]["options"].as_array().unwrap();
    assert!(
        options.iter().any(|o| o["id"] == "model"),
        "session.config_options 应返回 Agent 侧选项: {cfg}"
    );
    assert!(
        meta.get("configOptions").is_none(),
        "元数据不应再携带会话选项: {meta}"
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

    let h = c.call("session.history", json!({"sessionId": sid})).await;
    let items = h["result"]["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(
        items[0]["kind"], "user_message",
        "历史首条为用户消息: {items:?}"
    );
    assert!(items.iter().any(|i| i["kind"] == "agent_message"));

    let a = c
        .call("session.activities", json!({"sessionId": sid}))
        .await;
    let acts = a["result"]["activities"].as_array().unwrap();
    assert!(acts.iter().any(|x| x["kind"] == "thinking"), "{acts:?}");
    assert!(acts.iter().any(|x| x["kind"] == "tool_call"), "{acts:?}");
    let tool_calls = acts
        .iter()
        .filter(|x| x["kind"] == "tool_call")
        .collect::<Vec<_>>();
    assert_eq!(
        tool_calls.len(),
        1,
        "同 tool_call_id 的 tool_call + tool_call_update 应合并为一条活动: {acts:?}"
    );
    assert_eq!(
        tool_calls[0]["title"], "运行 cargo test 完成",
        "update 的 title 应覆盖初始 title: {acts:?}"
    );

    let oa = c
        .call("session.ongoing_activity", json!({"sessionId": sid}))
        .await;
    assert!(
        oa["result"]["activity"].is_null(),
        "空闲 ongoing_activity 为 null: {oa}"
    );

    let r = c.call("session.delete", json!({"sessionId": sid})).await;
    assert!(r.get("error").is_none(), "删除失败: {r}");
    // 资源清理异步化：close/delete 帧在删除 RPC 返回后于后台完成，轮询等待
    let mut closed = false;
    for _ in 0..50 {
        if mock_calls(&data_dir).contains("session/close") {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(closed, "后台清理应触发 ACP session/close");
    let list = c.call("session.list", json!({})).await;
    assert!(list["result"]["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn config_options_query_lazy_and_configure() {
    let (port, data_dir, _guard) = start_server_with_dir().await;
    let mut c = Client::connect(port, "test-token").await;

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
    assert!(
        !mock_calls(&data_dir).contains("session/new"),
        "未查询选项/未发指令前不应触发 ACP"
    );

    // 查询会话选项触发惰性创建
    let cfg = c
        .call("session.config_options", json!({"sessionId": sid}))
        .await;
    assert!(
        mock_calls(&data_dir).contains("session/new"),
        "查询会话选项应触发惰性 session/new: {}",
        mock_calls(&data_dir)
    );
    let options = cfg["result"]["options"].as_array().unwrap();
    assert_eq!(options.len(), 1, "{cfg}");
    assert_eq!(options[0]["id"], "model");
    assert_eq!(options[0]["current_value"], "gpt-4o", "{cfg}");

    // session.configure 携带会话选项设置：Server 转发 ACP
    // session/set_config_option，响应全量覆盖内存存储
    let r = c
        .call(
            "session.configure",
            json!({
                "sessionId": sid,
                "config": {"configId": "model", "type": "value_id", "value": "gpt-5"}
            }),
        )
        .await;
    assert!(r.get("error").is_none(), "configure 应成功: {r}");
    assert!(
        mock_calls(&data_dir).contains("session/set_config_option"),
        "configure(config) 应转发 ACP session/set_config_option"
    );
    let cfg = c
        .call("session.config_options", json!({"sessionId": sid}))
        .await;
    assert_eq!(
        cfg["result"]["options"][0]["current_value"], "gpt-5",
        "设置后的选项应以 Agent 侧数据为权威: {cfg}"
    );
}

#[tokio::test]
async fn slash_commands_notification_driven() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;

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

    // 查询不触发惰性创建：仅由 available_commands_update 通知驱动，尚无 agent 侧会话时返回空
    let r = c
        .call("session.slash_commands", json!({"sessionId": sid}))
        .await;
    assert!(
        r["result"]["commands"].as_array().unwrap().is_empty(),
        "无 agent 侧会话时斜杠命令为空: {r}"
    );

    // prompt 期间 agent 下发 available_commands_update，全量覆盖内存存储
    c.fire(
        "session.prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "开始"}]}),
    )
    .await;
    let got = c
        .wait_notification(
            "session.state_change",
            |p| p["sessionId"] == json!(sid) && p["newState"] == "idle",
            8000,
        )
        .await;
    assert!(got, "prompt 应结束");

    let r = c
        .call("session.slash_commands", json!({"sessionId": sid}))
        .await;
    let commands = r["result"]["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 2, "{r}");
    assert_eq!(commands[0]["name"], "goal");
    assert_eq!(commands[0]["description"], "设置或查看本会话目标");
    assert!(
        commands[0].get("hint").is_none(),
        "无输入提示的命令不应携带 hint: {r}"
    );
    assert_eq!(commands[1]["name"], "review");
    assert_eq!(commands[1]["hint"], "审查重点", "{r}");

    // 不存在的会话报 SESSION_NOT_FOUND
    let r = c
        .call("session.slash_commands", json!({"sessionId": "nope"}))
        .await;
    assert_eq!(r["error"]["code"], -32001, "{r}");
}

#[tokio::test]
async fn plan_notification_driven() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;

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

    // 查询不触发惰性创建：仅由 plan 通知驱动，尚无 agent 侧会话时返回空
    let r = c.call("session.plan", json!({"sessionId": sid})).await;
    assert!(
        r["result"]["entries"].as_array().unwrap().is_empty(),
        "无 agent 侧会话时计划为空: {r}"
    );

    // prompt 期间 agent 下发 plan 通知，全量覆盖内存存储
    c.fire(
        "session.prompt",
        json!({"sessionId": sid, "input": [{"type": "text", "text": "开始"}]}),
    )
    .await;
    let got = c
        .wait_notification(
            "session.state_change",
            |p| p["sessionId"] == json!(sid) && p["newState"] == "idle",
            8000,
        )
        .await;
    assert!(got, "prompt 应结束");

    let r = c.call("session.plan", json!({"sessionId": sid})).await;
    let entries = r["result"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3, "{r}");
    assert_eq!(entries[0]["content"], "梳理需求");
    assert_eq!(entries[0]["priority"], "high");
    assert_eq!(entries[0]["status"], "completed");
    assert_eq!(entries[1]["content"], "实现功能");
    assert_eq!(entries[1]["status"], "in_progress");
    assert_eq!(entries[2]["content"], "可选优化");
    assert_eq!(entries[2]["priority"], "low");
    assert_eq!(entries[2]["status"], "pending");

    // 不存在的会话报 SESSION_NOT_FOUND
    let r = c.call("session.plan", json!({"sessionId": "nope"})).await;
    assert_eq!(r["error"]["code"], -32001, "{r}");
}

#[tokio::test]
async fn session_list_pagination() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let agent = mock_acp_name(&mut c).await;

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

    let first = c.call("session.list", json!({"limit": 2})).await;
    let sessions = first["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "limit=2 应返回前 2 条: {first}");
    assert_eq!(first["result"]["hasMore"], true);
    assert_eq!(sessions[0]["id"], json!(sids[2]), "最晚 prompt 排最前");
    assert_eq!(sessions[1]["id"], json!(sids[1]));

    // 按数量查询：数量增大是更长前缀，不重不漏
    let second = c.call("session.list", json!({"limit": 3})).await;
    let s2 = second["result"]["sessions"].as_array().unwrap();
    assert_eq!(s2.len(), 3);
    assert_eq!(s2[0]["id"], json!(sids[2]));
    assert_eq!(s2[2]["id"], json!(sids[0]));
    assert_eq!(second["result"]["hasMore"], false);
}

#[tokio::test]
async fn workspace_diff_reflects_changes() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::write(dir.path().join("a.txt"), "line1\nCHANGED\n").unwrap();
    let session = c
        .call(
            "session.new",
            json!({"agent": "mock_acp", "cwd": dir.path().to_str().unwrap()}),
        )
        .await["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let d = c
        .call("workspace.diff", json!({"sessionId": session}))
        .await;
    assert!(d.get("error").is_none(), "diff 失败: {d}");
    let files = d["result"]["files"].as_array().unwrap();
    assert_eq!(files[0]["path"], "a.txt");
    assert!(files[0]["patch"].as_str().unwrap().contains("diff --git"));

    let r = c
        .call(
            "workspace.restore",
            json!({"sessionId": session, "path": "a.txt"}),
        )
        .await;
    assert_eq!(r["result"]["ok"], true, "restore 失败: {r}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "line1\nline2\n"
    );
}

#[tokio::test]
async fn workspace_list_and_read_browse_session_directory() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "one\ntwo\nthree\n").unwrap();
    let session = c
        .call(
            "session.new",
            json!({"agent": "mock_acp", "cwd": dir.path().to_str().unwrap()}),
        )
        .await["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let root = c
        .call("workspace.list", json!({"sessionId": session, "limit": 10}))
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
            json!({"sessionId": session, "path": "src"}),
        )
        .await;
    assert_eq!(nested["result"]["entries"][0]["path"], "src/main.rs");

    let first = c
        .call(
            "workspace.read",
            json!({"sessionId": session, "path": "README.md", "limit": 2}),
        )
        .await;
    assert_eq!(first["result"]["content"], "one\ntwo\n");
    assert_eq!(first["result"]["hasMore"], true);
    let second = c
        .call(
            "workspace.read",
            json!({
                "sessionId": session,
                "path": "README.md",
                "offset": first["result"]["nextOffset"],
                "limit": 2
            }),
        )
        .await;
    assert_eq!(second["result"]["content"], "three\n");
    assert_eq!(second["result"]["hasMore"], false);
}

#[tokio::test]
async fn workspace_read_rejects_invalid_and_binary_paths() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let dir = init_repo();
    std::fs::write(dir.path().join("binary.dat"), [0xff, 0xfe, 0xfd]).unwrap();
    let session = c
        .call(
            "session.new",
            json!({"agent": "mock_acp", "cwd": dir.path().to_str().unwrap()}),
        )
        .await["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let traversal = c
        .call(
            "workspace.read",
            json!({"sessionId": session, "path": "../outside.txt"}),
        )
        .await;
    assert!(
        traversal.get("error").is_some(),
        "应拒绝越界路径: {traversal}"
    );

    let binary = c
        .call(
            "workspace.read",
            json!({"sessionId": session, "path": "binary.dat"}),
        )
        .await;
    assert!(
        binary.get("error").is_some(),
        "应拒绝非 UTF-8 文件: {binary}"
    );

    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let symlink = c
            .call(
                "workspace.read",
                json!({"sessionId": session, "path": "link/secret.txt"}),
            )
            .await;
        assert!(
            symlink.get("error").is_some(),
            "应拒绝越界 symlink: {symlink}"
        );
    }
}

async fn mock_acp_name(c: &mut Client) -> String {
    let list = c.call("agent.list", json!({})).await;
    first_agent(&list)
}

async fn start_server_with_dir() -> (u16, std::path::PathBuf, ServerGuard) {
    let port = 36000 + (std::process::id() % 500) as u16 + NEXT_PORT.fetch_add(1, Ordering::SeqCst);
    let data_dir = tempfile::Builder::new()
        .prefix("amux-e2e-")
        .tempdir()
        .unwrap();
    let data_path = data_dir.path().to_path_buf();
    let guard = spawn_server_with_delay(port, data_dir, 150).await;
    (port, data_path, guard)
}

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
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("amux-e2e-git-")
        .tempdir()
        .unwrap();
    git(dir.path(), &["init", "-b", "main", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("a.txt"), "line1\nline2\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init", "-q"]);
    dir
}

#[tokio::test]
async fn agent_restart_keeps_agent_available() {
    let (port, _guard) = start_server().await;
    let mut c = Client::connect(port, "test-token").await;
    let agent = mock_acp_name(&mut c).await;

    let restarted = c.call("agent.restart", json!({ "agent": agent })).await;
    assert_eq!(
        restarted["result"]["ok"], true,
        "agent.restart 应成功: {restarted}"
    );

    let list = c.call("agent.list", json!({})).await;
    let agents = list["result"]["agents"].as_array().unwrap();
    let entry = agents
        .iter()
        .find(|a| a["name"] == json!(agent))
        .expect("重启后 agent 仍在列表");
    assert_eq!(entry["available"], true, "重启后应可用: {list}");

    let created = c
        .call(
            "session.new",
            json!({ "agent": agent, "cwd": "/tmp/restart" }),
        )
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let prompted = c
        .call(
            "session.prompt",
            json!({"sessionId": sid, "input": [{"type":"text","text":"重启后指令"}]}),
        )
        .await;
    assert_eq!(
        prompted["result"]["ok"], true,
        "重启后 prompt 应成功: {prompted}"
    );
}

#[tokio::test]
async fn busy_prompt_rejected_and_cancel_works() {
    let port = 36000 + (std::process::id() % 500) as u16 + NEXT_PORT.fetch_add(1, Ordering::SeqCst);
    let data_dir = tempfile::Builder::new()
        .prefix("amux-e2e-busy-")
        .tempdir()
        .unwrap();
    let _guard = spawn_server_with_delay(port, data_dir, 3000).await;
    let mut c = Client::connect(port, "test-token").await;
    let agent = mock_acp_name(&mut c).await;

    let created = c
        .call("session.new", json!({ "agent": agent, "cwd": "/tmp/busy" }))
        .await;
    let sid = created["result"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    c.fire(
        "session.prompt",
        json!({"sessionId": sid, "input": [{"type":"text","text":"长任务"}]}),
    )
    .await;
    let got_busy = c
        .wait_notification(
            "session.state_change",
            |p| p["sessionId"] == json!(sid) && p["oldState"] == "idle" && p["newState"] == "busy",
            8000,
        )
        .await;
    assert!(got_busy, "应推送 idle→busy");

    let second = c
        .call(
            "session.prompt",
            json!({"sessionId": sid, "input": [{"type":"text","text":"插队"}]}),
        )
        .await;
    assert_eq!(
        second["error"]["code"],
        json!(-32003),
        "忙时 prompt 应返回 -32003: {second}"
    );

    let cancelled = c.call("session.cancel", json!({"sessionId": sid})).await;
    assert_eq!(
        cancelled["result"]["ok"], true,
        "cancel 应成功: {cancelled}"
    );
    let got_idle = c
        .wait_notification(
            "session.state_change",
            |p| p["sessionId"] == json!(sid) && p["newState"] == "idle",
            10000,
        )
        .await;
    assert!(got_idle, "cancel 后应回 idle");

    let again = c
        .call(
            "session.prompt",
            json!({"sessionId": sid, "input": [{"type":"text","text":"再来"}]}),
        )
        .await;
    assert_ne!(
        again["error"]["code"],
        json!(-32003),
        "空闲后不应再报 busy: {again}"
    );
}

use base64::Engine as _;

/// 终端端到端：真实 WS 连接上走 terminal.open/input/resize/close，
/// 验证输出仅发所属连接（不走全局广播）与关闭的连接归属校验。
#[tokio::test(flavor = "multi_thread")]
async fn terminal_open_input_output_and_connection_binding() {
    let (port, data_dir, _guard) = start_server_with_dir().await;
    let mut c1 = Client::connect(port, "test-token").await;
    let mut c2 = Client::connect(port, "test-token").await;

    // 打开终端（cwd 用测试临时目录，shell 可正常落位）
    let cwd = data_dir.to_string_lossy().into_owned();
    let opened = c1
        .call("terminal.open", json!({"cwd": cwd, "cols": 80, "rows": 24}))
        .await;
    assert!(
        opened.get("error").is_none(),
        "terminal.open 应成功: {opened}"
    );
    let terminal_id = opened["result"]["terminalId"]
        .as_str()
        .expect("open 应返回 terminalId")
        .to_string();

    // shell 启动输出（提示符）到达所属连接
    let got = c1
        .wait_notification(
            "terminal.output",
            |p| p["terminalId"] == json!(terminal_id),
            5000,
        )
        .await;
    assert!(got, "c1 应收到终端输出");

    // 其他连接不收到终端帧：c2 的下一个响应前不得出现 terminal.output 通知
    let listed = c2.call("agent.list", json!({})).await;
    assert!(listed.get("error").is_none());
    assert!(
        !c2.notifications.iter().any(|(m, _)| m == "terminal.output"),
        "终端输出不应广播到其他连接"
    );

    // 输入 echo：回显内容经输出帧返回（真实 PTY 字节流路径）
    let mark = format!("amux-e2e-{}", uuid::Uuid::new_v4().simple());
    let input = base64::engine::general_purpose::STANDARD.encode(format!("echo {mark}\n"));
    let w = c1
        .call(
            "terminal.input",
            json!({"terminalId": terminal_id, "data": input}),
        )
        .await;
    assert!(w.get("error").is_none(), "terminal.input 应成功: {w}");
    let echoed = c1
        .wait_notification(
            "terminal.output",
            |p| {
                p["data"]
                    .as_str()
                    .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                    .map(|b| String::from_utf8_lossy(&b).contains(&mark))
                    .unwrap_or(false)
            },
            5000,
        )
        .await;
    assert!(echoed, "回显输出应包含 {mark}");

    // resize 正常
    let resized = c1
        .call(
            "terminal.resize",
            json!({"terminalId": terminal_id, "cols": 100, "rows": 30}),
        )
        .await;
    assert!(resized.get("error").is_none(), "resize 应成功: {resized}");

    // 非所属连接越权关闭被拒（TERMINAL_NOT_FOUND）
    let rejected = c2
        .call("terminal.close", json!({"terminalId": terminal_id}))
        .await;
    assert_eq!(rejected["error"]["code"], json!(-32006));

    // 所属连接显式关闭 → 后续操作报终端不存在
    let closed = c1
        .call("terminal.close", json!({"terminalId": terminal_id}))
        .await;
    assert!(
        closed.get("error").is_none(),
        "所属连接关闭应成功: {closed}"
    );
    let after = c1
        .call(
            "terminal.input",
            json!({"terminalId": terminal_id, "data": ""}),
        )
        .await;
    assert_eq!(after["error"]["code"], json!(-32006));
}
