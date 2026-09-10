//! 模拟 ACP v1 agent（官方 SDK `agent-client-protocol` 的 **Agent 侧**实现）。
//! server 侧全部测试（连接对接、会话管理、端到端）都经此子进程走真实 ACP v1
//! stdio 路径——与 codex / kimi 等真实 agent 的交互方式一致，测试不使用
//! 进程内连接替身。
//!
//! 行为要点（与协议语义对齐）：
//! - `session/new` 返回自增的唯一 sessionId（mock_s_1、mock_s_2、…），支持多会话
//! - `session/prompt` 记录该会话的用户指令与 agent 输出（内存），`session/load`
//!   全量重放记录的历史；`session/resume` 恢复会话（no-op 响应）
//! - `session/prompt` 先请求权限（期望 server yolo 自动批准）；设置
//!   `AMUX_MOCK_WAIT_FOR_CANCEL=1` 时，等待收到 cancel 通知再结束 turn，
//!   为忙时 prompt 测试提供确定性的协调点
//! - `session/prompt` 指令为 `/terminal` 时，经 `terminal/create`、
//!   `terminal/wait_for_exit`、`terminal/output`、`terminal/release` 全链路在
//!   客户端执行 shell 并把结果作为 agent 输出回传（模拟 kimi acp 的行为）。
//! - 把收到的**方法名**追加到 `<state_file>.calls`（含 cancel 通知；供测试断言
//!   server 的 ACP 调用面，包括 open_session 不触发 `session/load`、resume
//!   幂等只调一次及 close/delete）
//! - 把收到的权限批准记录追加到状态文件（第二个参数，或 `AMUX_MOCK_STATE`）
//!
//! 场景机制（全部经环境变量开启，供跨进程时序协调）：
//! - `AMUX_MOCK_TURN_GATES=f1,f2,…`：prompt 序号 n 等待文件 f_n 出现才继续
//!   （文件不删除，并发 turn 可共用）；无对应序号的 prompt 不受限
//! - `AMUX_MOCK_STEPS=<json 文件>`：`{"steps":[{kind,text,title,id,tool,gate}…],
//!   "end_gate":…}`，prompt 序号 n 执行 steps[n-1..]（每个步骤先等 gate 文件；
//!   越界走默认事件流），全部发完等 end_gate 再收尾
//! - `AMUX_MOCK_BLOCK_NEW_SESSION=<entered>:<gate>`：session/new 先落 entered
//!   文件再等 gate 放行（模拟创建卡住，供删除/取消与惰性创建的并发测试）
//! - `AMUX_MOCK_NO_DELETE=1`：initialize 不声明 sessionCapabilities.delete

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
    CreateTerminalRequest, DeleteSessionRequest, DeleteSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, MessageId, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
    PromptResponse, ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption, SessionConfigOptionValue,
    SessionInfo, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TerminalOutputRequest, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
    UsageUpdate, WaitForTerminalExitRequest,
};
use agent_client_protocol::{Agent, Result, Stdio};
use serde_json::{json, Value};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 本进程收到的 session/prompt 序号（1 起始）。供 `AMUX_MOCK_TURN_GATES` /
/// `AMUX_MOCK_STEPS` 按序号选闸门/步骤。
static PROMPT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 等待闸门文件出现（测试写入以放行；轮询间隔 5ms，确定性协调）。
async fn wait_for_file(path: &str) {
    while !std::path::Path::new(path).exists() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn history() -> &'static Mutex<HashMap<String, Vec<Value>>> {
    use std::sync::OnceLock;
    static H: OnceLock<Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sessions() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 每个会话当前选中的 model 选项值（mock 的 config_options 状态）。
fn session_models() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// mock 的会话配置选项：model select（gpt-4o / gpt-5）。
fn config_options_for(sid: &str) -> Vec<SessionConfigOption> {
    let current = session_models()
        .lock()
        .get(sid)
        .cloned()
        .unwrap_or_else(|| "gpt-4o".into());
    vec![SessionConfigOption::select(
        "model",
        "模型",
        current,
        vec![
            agent_client_protocol::schema::v1::SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
            agent_client_protocol::schema::v1::SessionConfigSelectOption::new("gpt-5", "GPT-5"),
        ],
    )]
}

fn history_len(sid: &str) -> usize {
    history().lock().get(sid).map(|h| h.len()).unwrap_or(0)
}

fn cancelled_sessions() -> &'static Mutex<HashSet<String>> {
    use std::sync::OnceLock;
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashSet::new()))
}

fn waited_sessions() -> &'static Mutex<HashSet<String>> {
    use std::sync::OnceLock;
    static W: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cancel_notify() -> &'static tokio::sync::Notify {
    use std::sync::OnceLock;
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

async fn wait_for_cancel(session_id: &str) {
    loop {
        if cancelled_sessions().lock().remove(session_id) {
            return;
        }
        cancel_notify().notified().await;
    }
}

fn append_line(path: &str, line: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|mut f| {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        });
}

fn record_call(calls_file: &str, method: &str) {
    append_line(calls_file, method);
}

fn append_approved(state_file: &str) {
    append_line(state_file, "approved");
}

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
    let calls_cfg = calls_file.clone();
    let calls_cancel = calls_file.clone();
    let state_prompt = state_file.clone();
    Agent
        .builder()
        .name("mock_acp")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                // AMUX_MOCK_NO_DELETE=1 时不声明 sessionCapabilities.delete，
                // 模拟不支持会话删除的 agent（codex 这类）。
                let caps = if std::env::var_os("AMUX_MOCK_NO_DELETE").is_some() {
                    AgentCapabilities::new()
                } else {
                    AgentCapabilities::new().session_capabilities(
                        agent_client_protocol::schema::v1::SessionCapabilities::new().delete(
                            agent_client_protocol::schema::v1::SessionDeleteCapabilities::new(),
                        ),
                    )
                };
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(caps),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                record_call(&calls_new, "session/new");
                // AMUX_MOCK_BLOCK_NEW_SESSION="<entered_file>:<gate_file>"：
                // 先落 entered 文件（测试确认已进入阻塞），再等 gate 文件放行。
                // 模拟 session/new 慢/卡住，供删除与惰性创建的并发时序测试。
                if let Some(spec) = std::env::var_os("AMUX_MOCK_BLOCK_NEW_SESSION") {
                    let spec = spec.to_string_lossy().into_owned();
                    let (entered, gate) = spec
                        .split_once(':')
                        .expect("BLOCK_NEW_SESSION 格式应为 <entered>:<gate>");
                    std::fs::write(entered, "1").ok();
                    wait_for_file(gate).await;
                }
                let n = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                let sid = format!("mock_s_{n}");
                let cwd = request.cwd.to_string_lossy().into_owned();
                sessions().lock().insert(sid.clone(), cwd);
                session_models().lock().insert(sid.clone(), "gpt-4o".into());
                let opts = config_options_for(&sid);
                responder.respond(
                    NewSessionResponse::new(sid).config_options(opts),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                record_call(&calls_cfg, "session/set_config_option");
                let sid = request.session_id.to_string();
                let value = match &request.value {
                    SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                    SessionConfigOptionValue::Boolean { value } => value.to_string(),
                    // SDK 1.4.0 仅含上述两变体；non_exhaustive 要求通配
                    _ => String::new(),
                };
                session_models()
                    .lock()
                    .insert(sid.clone(), value);
                responder.respond(SetSessionConfigOptionResponse::new(
                    config_options_for(&sid),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx| {
                // 重放 mock 保存的历史后再响应 load。
                record_call(&calls_load, "session/load");
                let sid = request.session_id.to_string();
                let hist = history().lock().get(&sid).cloned().unwrap_or_else(|| {
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
                    // 场景闸门（AMUX_MOCK_TURN_GATES=f1,f2,…）：prompt 序号 n 等
                    // f_n 出现才继续（文件不删除，多个并发 turn 可共用同一闸门）；
                    // 无对应序号的 prompt 不受闸门。模拟 turn 长时间不结束。
                    let prompt_no = PROMPT_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                    if let Ok(gates) = std::env::var("AMUX_MOCK_TURN_GATES") {
                        let gates: Vec<&str> = gates.split(',').collect();
                        if let Some(gate) = gates.get(prompt_no as usize - 1) {
                            wait_for_file(gate).await;
                        }
                    }
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

                    if std::env::var_os("AMUX_MOCK_WAIT_FOR_CANCEL").is_some()
                        && waited_sessions().lock().insert(request.session_id.to_string())
                    {
                        wait_for_cancel(&request.session_id.to_string()).await;
                    }

                    // 模拟 kimi acp 等把 shell 执行委托给客户端的 agent：指令为
                    // /terminal 时经 terminal/create、wait_for_exit、output 全链路执行
                    if user_text == "/terminal" {
                        record_call(&format!("{state_file}.calls"), "terminal/create");
                        let created = cx_task
                            .send_request(
                                CreateTerminalRequest::new(
                                    request.session_id.clone(),
                                    "/bin/sh",
                                )
                                .args(vec!["-c".into(), "echo amux-terminal-ok".into()]),
                            )
                            .block_task()
                            .await?;
                        let exit = cx_task
                            .send_request(WaitForTerminalExitRequest::new(
                                request.session_id.clone(),
                                created.terminal_id.clone(),
                            ))
                            .block_task()
                            .await?;
                        let out = cx_task
                            .send_request(TerminalOutputRequest::new(
                                request.session_id.clone(),
                                created.terminal_id.clone(),
                            ))
                            .block_task()
                            .await?;
                        let _ = cx_task.send_request(ReleaseTerminalRequest::new(
                            request.session_id.clone(),
                            created.terminal_id,
                        ));
                        cx_task.send_notification(SessionNotification::new(
                            request.session_id.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                                TextContent::new(format!(
                                    "exit={} out={}",
                                    exit.exit_status.exit_code.unwrap_or_default(),
                                    out.output.trim()
                                )),
                            ))),
                        ))?;
                    }

                    // 场景步骤（AMUX_MOCK_STEPS=<json 文件>）：
                    // {"steps":[{"kind":"thinking|tool_call|output","text":…,
                    //   "title":…,"id":…,"tool":"read|execute","gate":…}…],"end_gate":…}
                    // prompt 序号 n 执行 steps[n-1]（越界走默认事件流）；每个步骤先等
                    // gate 文件出现再发送，全部发完等 end_gate。测试以 server 侧
                    // 可观察状态（ongoing/活动落盘）为同步点后写 gate 放行。
                    let mut steps_output = String::new();
                    let steps_applied = match std::env::var("AMUX_MOCK_STEPS") {
                        Ok(path) => {
                            let spec: serde_json::Value = serde_json::from_str(
                                &std::fs::read_to_string(&path).unwrap_or_default(),
                            )
                            .unwrap_or(serde_json::Value::Null);
                            let arr = spec["steps"].as_array().cloned().unwrap_or_default();
                            let idx = prompt_no as usize;
                            if idx >= 1 && arr.len() >= idx {
                                for step in &arr[idx - 1..] {
                                    if let Some(gate) = step["gate"].as_str() {
                                        wait_for_file(gate).await;
                                    }
                                    let text =
                                        step["text"].as_str().unwrap_or_default().to_string();
                                    match step["kind"].as_str().unwrap_or_default() {
                                        "thinking" => cx_task.send_notification(
                                            SessionNotification::new(
                                                request.session_id.clone(),
                                                SessionUpdate::AgentThoughtChunk(
                                                    ContentChunk::new(ContentBlock::Text(
                                                        TextContent::new(text),
                                                    )),
                                                ),
                                            ),
                                        )?,
                                        "tool_call" => {
                                            let kind = match step["tool"].as_str() {
                                                Some("read") => ToolKind::Read,
                                                _ => ToolKind::Execute,
                                            };
                                            cx_task.send_notification(
                                                SessionNotification::new(
                                                    request.session_id.clone(),
                                                    SessionUpdate::ToolCall(
                                                        ToolCall::new(
                                                            step["id"]
                                                                .as_str()
                                                                .unwrap_or("tc1")
                                                                .to_owned(),
                                                            step["title"]
                                                                .as_str()
                                                                .unwrap_or_default()
                                                                .to_owned(),
                                                        )
                                                        .kind(kind)
                                                        .status(ToolCallStatus::Pending),
                                                    ),
                                                ),
                                            )?;
                                        }
                                        "output" => {
                                            steps_output.push_str(&text);
                                            cx_task.send_notification(
                                                SessionNotification::new(
                                                    request.session_id.clone(),
                                                    SessionUpdate::AgentMessageChunk(
                                                        ContentChunk::new(ContentBlock::Text(
                                                            TextContent::new(text),
                                                        )),
                                                    ),
                                                ),
                                            )?;
                                        }
                                        _ => {}
                                    }
                                }
                                if let Some(end) = spec["end_gate"].as_str() {
                                    wait_for_file(end).await;
                                }
                                true
                            } else {
                                false
                            }
                        }
                        Err(_) => false,
                    };
                    if !steps_applied {
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
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            "tc1",
                            ToolCallUpdateFields::new()
                                .title("运行 cargo test 完成")
                                .status(ToolCallStatus::Completed),
                        )),
                    ))?;
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new("完成！")),
                        )),
                    ))?;
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::UsageUpdate(UsageUpdate::new(53_000, 200_000)),
                    ))?;
                    // turn 内全量下发斜杠命令集合，供查询验证。
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
                            AvailableCommand::new("goal", "设置或查看本会话目标"),
                            AvailableCommand::new("review", "审查当前改动").input(
                                AvailableCommandInput::Unstructured(
                                    UnstructuredCommandInput::new("审查重点"),
                                ),
                            ),
                        ])),
                    ))?;
                    // turn 内全量下发 agent 计划，供查询验证。
                    cx_task.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::Plan(Plan::new(vec![
                            PlanEntry::new(
                                "梳理需求",
                                PlanEntryPriority::High,
                                PlanEntryStatus::Completed,
                            ),
                            PlanEntry::new(
                                "实现功能",
                                PlanEntryPriority::High,
                                PlanEntryStatus::InProgress,
                            ),
                            PlanEntry::new(
                                "可选优化",
                                PlanEntryPriority::Low,
                                PlanEntryStatus::Pending,
                            ),
                        ])),
                    ))?;

                    }

                    let agent_mid = format!("a{}", history_len(&request.session_id.to_string()));
                    let agent_text = if steps_applied && !steps_output.is_empty() {
                        steps_output
                    } else {
                        "完成！".to_string()
                    };
                    history()
                        .lock()
                        .entry(sid)
                        .or_default()
                        .push(json!({
                            "messageId": agent_mid,
                            "kind": "agent",
                            "content": { "type": "text", "text": agent_text }
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
                sessions().lock().remove(&sid);
                history().lock().remove(&sid);
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DeleteSessionRequest, responder, _cx| {
                record_call(&calls_delete, "session/delete");
                let sid = request.session_id.to_string();
                sessions().lock().remove(&sid);
                history().lock().remove(&sid);
                responder.respond(DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ListSessionsRequest, responder, _cx| {
                record_call(&calls_list, "session/list");
                let s = sessions().lock();
                let infos = s
                    .iter()
                    .map(|(id, cwd)| SessionInfo::new(id.clone(), cwd.clone()))
                    .collect();
                responder.respond(ListSessionsResponse::new(infos))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |cancel: CancelNotification, _cx| {
                record_call(&calls_cancel, "session/cancel");
                cancelled_sessions()
                    .lock()
                    .insert(cancel.session_id.to_string());
                cancel_notify().notify_waiters();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
