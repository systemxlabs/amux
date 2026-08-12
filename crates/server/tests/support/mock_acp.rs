//! 模拟 ACP v1 agent（官方 SDK `agent-client-protocol` 的 **Agent 侧**实现）。
//! 用于驱动 `AcpAgentDriver` 的对接测试（crates/server/tests/acp.rs）与
//! test-server 的端到端测试。
//!
//! 行为要点（与协议语义对齐）：
//! - `session/new` 返回自增的唯一 sessionId（mock_s_1、mock_s_2、…），支持多会话；
//!   会话注册表（id → cwd）与每会话历史**持久化**到 `<state_file>.json`
//!   （启动时加载、变更时落盘）——模拟真实 agent 的历史在磁盘、server 重启后经
//!   `session/list` 恢复的语义（docs/DESIGN.md §4.1）；计数器从已恢复会话继续递增
//! - `session/prompt` 记录该会话的用户指令与 agent 输出（内存 + 落盘），`session/load`
//!   全量重放记录的历史（更接近真实 agent 的持久化语义）
//! - `session/prompt` 先请求权限（期望 server yolo 自动批准），随后 sleep
//!   `AMUX_MOCK_DELAY_MS`（默认 300ms）再发事件流与响应——保证忙时 prompt
//!   （-32006）测试有确定性的 busy 窗口
//! - `skill/list` 返回固定的 skills 列表（PRD §3.3）
//! - 把收到的权限批准记录追加到状态文件（第二个参数，或 `AMUX_MOCK_STATE`）

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, DeleteSessionRequest, DeleteSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, MessageId, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionInfo,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, JsonRpcRequest, Result, Stdio};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 每会话的历史（用户指令 + agent 输出 content block 列表）。
fn history() -> &'static std::sync::Mutex<HashMap<String, Vec<Value>>> {
    use std::sync::OnceLock;
    static H: OnceLock<std::sync::Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 会话注册表：session_id → cwd（与 history 一起持久化）。
fn sessions() -> &'static std::sync::Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static S: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 持久化状态（会话注册表 + 每会话历史），落盘到 `<state_file>.json`。
#[derive(Serialize, Deserialize)]
struct MockState {
    sessions: HashMap<String, String>,
    history: HashMap<String, Vec<Value>>,
}

fn history_file(state_file: &str) -> std::path::PathBuf {
    std::path::Path::new(state_file).with_extension("json")
}

/// 启动时从磁盘加载会话注册表与历史（模拟 agent 历史在磁盘，server 重启后仍可恢复）。
fn load_state(state_file: &str) {
    let Ok(raw) = std::fs::read_to_string(history_file(state_file)) else {
        return;
    };
    let Ok(st) = serde_json::from_str::<MockState>(&raw) else {
        return;
    };
    *sessions().lock().unwrap() = st.sessions;
    *history().lock().unwrap() = st.history;
    // 计数器从已恢复会话的最大序号继续（避免重启后 session/new 撞 id）
    let max_n = sessions()
        .lock()
        .unwrap()
        .keys()
        .filter_map(|k| k.strip_prefix("mock_s_").and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);
    if max_n > 0 {
        SESSION_COUNTER.store(max_n, Ordering::SeqCst);
    }
}

/// 每次会话注册表/历史变更后落盘。
fn save_state(state_file: &str) {
    let st = MockState {
        sessions: sessions().lock().unwrap().clone(),
        history: history().lock().unwrap().clone(),
    };
    let Ok(json) = serde_json::to_string_pretty(&st) else {
        return;
    };
    let _ = std::fs::write(history_file(state_file), json);
}

fn history_len(sid: &str) -> usize {
    history()
        .lock()
        .unwrap()
        .get(sid)
        .map(|h| h.len())
        .unwrap_or(0)
}

fn delay_ms() -> u64 {
    std::env::var("AMUX_MOCK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// 把权限批准（outcome.selected）记录到状态文件（供测试断言 yolo 生效）。
fn append_approved(state_file: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_file)
        .map(|mut f| {
            use std::io::Write;
            let _ = writeln!(f, "approved");
        });
}

/// 自定义请求：ACP `skill/list`（PRD §3.3；SDK schema v1 未收录该方法）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "skill/list", response = serde_json::Value)]
struct SkillListRequest {}

fn main() -> Result<()> {
    let state_file = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AMUX_MOCK_STATE").ok())
        .unwrap_or_else(|| "/tmp/mock_acp_state".into());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio runtime 失败");
    rt.block_on(run(&state_file))
}

async fn run(state_file: &str) -> Result<()> {
    let state_file = state_file.to_string();
    load_state(&state_file);
    let state_new = state_file.clone();
    let state_prompt = state_file.clone();
    let state_delete = state_file.clone();
    Agent
        .builder()
        .name("mock_acp")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                let n = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                let sid = format!("mock_s_{n}");
                let cwd = request.cwd.to_string_lossy().into_owned();
                sessions().lock().unwrap().insert(sid.clone(), cwd);
                save_state(&state_new);
                responder.respond(NewSessionResponse::new(sid))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                // 全量重放：该会话记录的历史（用户指令 + agent 输出），重放完才响应
                let sid = request.session_id.to_string();
                let hist = history().lock().unwrap().get(&sid).cloned().unwrap_or_else(|| {
                    vec![
                        json!({ "messageId": "u1", "kind": "user", "content": { "type": "text", "text": "你好" } }),
                        json!({ "messageId": "a1", "kind": "agent", "content": { "type": "text", "text": "历史回复" } }),
                    ]
                });
                for (i, item) in hist.iter().enumerate() {
                    let (kind, mid) = if item["kind"] == "user" {
                        ("user_message_chunk", format!("u{i}"))
                    } else {
                        ("agent_message_chunk", format!("a{i}"))
                    };
                    let text = item["content"]["text"].as_str().unwrap_or("").to_string();
                    let update = if kind == "user_message_chunk" {
                        SessionUpdate::UserMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                                .message_id(MessageId::new(mid)),
                        )
                    } else {
                        SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                                .message_id(MessageId::new(mid)),
                        )
                    };
                    cx.send_notification(SessionNotification::new(request.session_id.clone(), update))?;
                }
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ResumeSessionRequest, responder, _cx| {
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                // 记录用户指令（先取 id 再锁，避免 json! 内再次锁同一 mutex 死锁）
                let sid = request.session_id.to_string();
                let user_text: String = request
                    .prompt
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let user_mid = format!("u{}", history_len(&sid));
                history()
                    .lock()
                    .unwrap()
                    .entry(sid.clone())
                    .or_default()
                    .push(json!({
                        "messageId": user_mid,
                        "kind": "user",
                        "content": { "type": "text", "text": user_text }
                    }));
                save_state(&state_prompt);

                // 后台任务承载整个 turn（权限 → busy 窗口 → 事件流 → 响应），
                // 不阻塞 SDK 事件循环（handler 内 await 会卡住连接）。
                let state_file = state_prompt.clone();
                let cx_task = cx.clone();
                cx.spawn(async move {
                    // 1) 先请求权限（期望 client yolo 自动批准）→ 记录批准
                    let perm = ToolCallUpdate::new(
                        "tc1",
                        ToolCallUpdateFields::new()
                            .kind(ToolKind::Execute)
                            .title("运行命令？")
                            .status(ToolCallStatus::Pending),
                    );
                    let req = RequestPermissionRequest::new(
                        request.session_id.clone(),
                        perm,
                        vec![PermissionOption::new(
                            "allow-once",
                            "Allow once",
                            PermissionOptionKind::AllowOnce,
                        )],
                    );
                    let resp = cx_task.send_request(req).block_task().await?;
                    if matches!(
                        resp.outcome,
                        RequestPermissionOutcome::Selected(_)
                    ) {
                        append_approved(&state_file);
                    }

                    // 2) busy 窗口：让 server 有确定的时间观察 Busy 状态（-32006 测试）
                    let ms = delay_ms();
                    if ms > 0 {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }

                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new("思考中")),
                        )),
                    ))?;
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::ToolCall(
                            ToolCall::new("tc1", "运行 cargo test")
                                .kind(ToolKind::Execute)
                                .status(ToolCallStatus::Pending),
                        ),
                    ))?;
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new("完成！")),
                        )),
                    ))?;

                    // 记录 agent 输出（先取 id 再锁，避免 json! 内再次锁同一 mutex 死锁）
                    let agent_mid = format!("a{}", history_len(&request.session_id.to_string()));
                    history()
                        .lock()
                        .unwrap()
                        .entry(sid)
                        .or_default()
                        .push(json!({
                            "messageId": agent_mid,
                            "kind": "agent",
                            "content": { "type": "text", "text": "完成！" }
                        }));
                    // turn 落盘：server 观察到 turn 结束（响应返回）前历史已持久化
                    save_state(&state_file);

                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CloseSessionRequest, responder, _cx| {
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DeleteSessionRequest, responder, _cx| {
                let sid = request.session_id.to_string();
                sessions().lock().unwrap().remove(&sid);
                history().lock().unwrap().remove(&sid);
                save_state(&state_delete);
                responder.respond(DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ListSessionsRequest, responder, _cx| {
                // 返回真实会话注册表（含重启前持久化的会话，docs/DESIGN.md §4.1）
                let s = sessions().lock().unwrap();
                let infos = s
                    .iter()
                    .map(|(id, cwd)| SessionInfo::new(id.clone(), cwd.clone()))
                    .collect();
                responder.respond(ListSessionsResponse::new(infos))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: SkillListRequest, responder, _cx| {
                responder.respond(json!({
                    "skills": [
                        { "name": "web-browser" },
                        { "name": "docs-search" },
                        { "name": "code-analysis" }
                    ]
                }))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: CancelNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
