//! 模拟 ACP v1 agent（官方 SDK `agent-client-protocol` 的 **Agent 侧**实现）。
//! 用于驱动 `AcpAgentDriver` 的对接测试（crates/server/tests/acp.rs）与
//! test-server 的端到端测试。
//!
//! 行为要点（与协议语义对齐）：
//! - `session/new` 返回自增的唯一 sessionId（mock_s_1、mock_s_2、…），支持多会话
//! - `session/prompt` 记录该会话的用户指令与 agent 输出（内存），`session/load`
//!   全量重放记录的历史；`session/resume` 恢复会话（no-op 响应）
//! - `session/prompt` 先请求权限（期望 server yolo 自动批准），随后 sleep
//!   `AMUX_MOCK_DELAY_MS`（默认 300ms）再发事件流与响应——保证忙时 prompt
//!   （-32006）测试有确定性的 busy 窗口
//! - `skill/list` 返回固定的 skills 列表。
//! - 把收到的**方法名**追加到 `<state_file>.calls`（供测试断言 server 的 ACP 调用面，
//!   包括 open_session 不触发 `session/load`、resume 幂等只调一次及 close/delete）。
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

fn history() -> &'static std::sync::Mutex<HashMap<String, Vec<Value>>> {
    use std::sync::OnceLock;
    static H: OnceLock<std::sync::Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn sessions() -> &'static std::sync::Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static S: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
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

fn record_call(calls_file: &str, method: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(calls_file)
        .map(|mut f| {
            use std::io::Write;
            let _ = writeln!(f, "{method}");
        });
}

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

/// 自定义请求：ACP `skill/list`（SDK schema v1 未收录该方法）。
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
    let calls_file = format!("{state_file}.calls");
    let calls_new = calls_file.clone();
    let calls_load = calls_file.clone();
    let calls_resume = calls_file.clone();
    let calls_prompt = calls_file.clone();
    let calls_delete = calls_file.clone();
    let calls_close = calls_file.clone();
    let calls_list = calls_file.clone();
    let calls_skill = calls_file.clone();
    let state_prompt = state_file.clone();
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
                record_call(&calls_new, "session/new");
                let n = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                let sid = format!("mock_s_{n}");
                let cwd = request.cwd.to_string_lossy().into_owned();
                sessions().lock().unwrap().insert(sid.clone(), cwd);
                responder.respond(NewSessionResponse::new(sid))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                // 重放 mock 保存的历史后再响应 load。
                record_call(&calls_load, "session/load");
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
                record_call(&calls_resume, "session/resume");
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                record_call(&calls_prompt, "session/prompt");
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

                // 后台任务承载整个 turn（权限 → busy 窗口 → 事件流 → 响应），
                // 不阻塞 SDK 事件循环（handler 内 await 会卡住连接）。
                let state_file = state_prompt.clone();
                let cx_task = cx.clone();
                cx.spawn(async move {
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

                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _cx| {
                record_call(&calls_close, "session/close");
                let sid = request.session_id.to_string();
                sessions().lock().unwrap().remove(&sid);
                history().lock().unwrap().remove(&sid);
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DeleteSessionRequest, responder, _cx| {
                record_call(&calls_delete, "session/delete");
                let sid = request.session_id.to_string();
                sessions().lock().unwrap().remove(&sid);
                history().lock().unwrap().remove(&sid);
                responder.respond(DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ListSessionsRequest, responder, _cx| {
                record_call(&calls_list, "session/list");
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
                record_call(&calls_skill, "skill/list");
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
