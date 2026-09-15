//! 模拟 ACP v2 agent（官方 SDK `agent-client-protocol` 的 **Agent 侧**实现）。
//! server 侧全部测试（连接对接、会话管理、端到端）都经此子进程走真实 ACP v2
//! stdio 路径——与 `codex-acp-v2` 等真实 agent 的交互方式一致，测试不使用
//! 进程内连接替身。
//!
//! 行为要点（与 v2 语义对齐）：
//! - `session/new` 返回自增的唯一 sessionId（mock_s_1、mock_s_2、…），响应携带
//!   model select 会话选项；`session/resume` 为空响应（server 不带 replayFrom）
//! - `session/prompt` 立即响应（受理），随后在后台任务里发
//!   `state_update` running → 思考/工具调用/输出 update → `state_update` idle（stopReason）
//! - 权限请求用 allow-once 选项，期望 server 自动批准；`AMUX_MOCK_WAIT_FOR_CANCEL=1`
//!   时等待收到 `session/cancel` 通知后再以 cancelled 结束，为忙时 prompt 测试提供
//!   确定性协调点
//! - 把收到的**方法名**追加到 `<state_file>.calls`（含 cancel 通知；供测试断言
//!   server 的 ACP 调用面，如 close/delete、resume 幂等只调一次）
//! - 把收到的权限批准记录追加到状态文件（第二个参数，或 `AMUX_MOCK_STATE`）
//!
//! 场景机制（全部经环境变量开启，供跨进程时序协调）：
//! - `AMUX_MOCK_TURN_GATES=f1,f2,…`：prompt 序号 n 等待文件 f_n 出现才继续
//!   （文件不删除，并发 turn 可共用）；无对应序号的 prompt 不受限
//! - `AMUX_MOCK_STEPS=<json 文件>`：`{"steps":[{kind,text,title,id,tool,gate}…],
//!   "end_gate":…}`，prompt 序号 n 执行 steps 全部步骤（每个步骤先等 gate 文件；
//!   kind ∈ thinking / tool_call / output），全部发完等 end_gate 再收尾
//! - `AMUX_MOCK_BLOCK_NEW_SESSION=<entered>:<gate>`：session/new 先落 entered
//!   文件再等 gate 放行（模拟创建卡住，供删除/取消与惰性创建的并发测试）
//! - `AMUX_MOCK_NO_DELETE=1`：initialize 不声明 `capabilities.session.delete`

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_client_protocol::schema::v2::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelSessionNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, DeleteSessionRequest, DeleteSessionResponse, IdleStateUpdate, InitializeRequest,
    InitializeResponse, MessageId, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PlanEntry, PlanEntryPriority, PlanEntryStatus, PlanItems, PlanUpdate,
    PlanUpdateContent, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption,
    SessionConfigOptionValue, SessionDeleteCapabilities, SessionId, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StateUpdate, StopReason,
    TextCommandInput, ToolCallStatus, ToolCallUpdate, ToolKind, UpdateSessionNotification,
    UsageUpdate,
};
use agent_client_protocol::{Agent, Client, Result, Stdio, V2ConnectionTo};
use serde_json::Value;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 本进程收到的 session/prompt 序号（1 起始）。
static PROMPT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 等待闸门文件出现（测试写入以放行；轮询间隔 5ms，确定性协调）。
async fn wait_for_file(path: &str) {
    while !std::path::Path::new(path).exists() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
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
            agent_client_protocol::schema::v2::SessionConfigSelectOption::new("gpt-4o", "GPT-4o"),
            agent_client_protocol::schema::v2::SessionConfigSelectOption::new("gpt-5", "GPT-5"),
        ],
    )]
}

/// 每会话在途 turn 数：仅当最后一个 turn 结束时才上报 `idle`。
fn foreground_work() -> &'static Mutex<HashMap<String, usize>> {
    use std::sync::OnceLock;
    static F: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

fn begin_work(session_id: &str) {
    *foreground_work()
        .lock()
        .entry(session_id.to_string())
        .or_insert(0) += 1;
}

/// 结束一个 turn；返回是否已无前台工作。
fn end_work(session_id: &str) -> bool {
    let mut map = foreground_work().lock();
    let entry = map.entry(session_id.to_string()).or_insert(1);
    *entry = entry.saturating_sub(1);
    let idle = *entry == 0;
    if idle {
        map.remove(session_id);
    }
    idle
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

/// 发送 `session/update` 通知。
fn send_update(cx: &V2ConnectionTo<Client>, sid: &SessionId, update: SessionUpdate) -> Result<()> {
    cx.send_notification(UpdateSessionNotification::new(sid.clone(), update))
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
    let calls_resume = calls_file.clone();
    let calls_prompt = calls_file.clone();
    let calls_close = calls_file.clone();
    let calls_delete = calls_file.clone();
    let calls_cfg = calls_file.clone();
    let calls_cancel = calls_file.clone();
    let state_prompt = state_file.clone();
    Agent
        .v2()
        .name("mock_acp")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                // AMUX_MOCK_NO_DELETE=1 时不声明 capabilities.session.delete，
                // 模拟不支持会话删除的 agent。
                let session_caps = if std::env::var_os("AMUX_MOCK_NO_DELETE").is_some() {
                    agent_client_protocol::schema::v2::SessionCapabilities::new()
                } else {
                    agent_client_protocol::schema::v2::SessionCapabilities::new()
                        .delete(SessionDeleteCapabilities::new())
                };
                responder.respond(
                    InitializeResponse::new(
                        initialize.protocol_version,
                        agent_client_protocol::schema::v2::Implementation::new("mock_acp", "0.1.0"),
                    )
                    .capabilities(AgentCapabilities::new().session(session_caps)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx| {
                record_call(&calls_new, "session/new");
                // AMUX_MOCK_BLOCK_NEW_SESSION="<entered_file>:<gate_file>"：
                // 先落 entered 文件（测试确认已进入阻塞），再等 gate 文件放行。
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
                let cwd = request.cwd.0.display().to_string();
                sessions().lock().insert(sid.clone(), cwd);
                session_models().lock().insert(sid.clone(), "gpt-4o".into());
                let opts = config_options_for(&sid);
                let session_id = SessionId::new(sid);
                responder
                    .respond(NewSessionResponse::new(session_id.clone()).config_options(opts))?;
                // 新会话的前台状态是空闲；客户端不得把这条 idle 当作后续
                // prompt 的完成信号（须先 running 再 idle）。
                send_update(
                    &cx,
                    &session_id,
                    SessionUpdate::StateUpdate(StateUpdate::Idle(IdleStateUpdate::new())),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                record_call(&calls_cfg, "session/set_config_option");
                let sid = request.session_id.to_string();
                let value = match &request.value {
                    SessionConfigOptionValue::Id { value } => value.0.to_string(),
                    SessionConfigOptionValue::Boolean { value } => value.to_string(),
                    _ => String::new(),
                };
                session_models().lock().insert(sid.clone(), value);
                responder.respond(SetSessionConfigOptionResponse::new(config_options_for(
                    &sid,
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, _cx| {
                record_call(&calls_resume, "session/resume");
                let sid = request.session_id.to_string();
                session_models()
                    .lock()
                    .entry(sid.clone())
                    .or_insert_with(|| "gpt-4o".into());
                responder
                    .respond(ResumeSessionResponse::new().config_options(config_options_for(&sid)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CloseSessionRequest, responder, _cx| {
                record_call(&calls_close, "session/close");
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: DeleteSessionRequest, responder, _cx| {
                record_call(&calls_delete, "session/delete");
                responder.respond(DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: agent_client_protocol::Responder<PromptResponse>,
                        cx: V2ConnectionTo<Client>| {
                record_call(&calls_prompt, "session/prompt");
                let sid = request.session_id.clone();
                let user_text = request
                    .prompt
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                // v2：立即受理，前台工作随后经 session/update 报告
                responder.respond(PromptResponse::new())?;
                let state_file = state_prompt.clone();
                let turn_cx = cx.clone();
                cx.spawn(async move {
                    if let Err(error) = run_turn(turn_cx, sid, user_text, state_file).await {
                        eprintln!("[mock_acp] turn 失败: {error:?}");
                    }
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelSessionNotification, _cx| {
                record_call(&calls_cancel, "session/cancel");
                cancelled_sessions()
                    .lock()
                    .insert(notification.session_id.to_string());
                cancel_notify().notify_waiters();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// 单个 turn 的前台工作：权限 → 闸门 → 事件流 → idle。
async fn run_turn(
    cx: V2ConnectionTo<Client>,
    sid: SessionId,
    user_text: String,
    state_file: String,
) -> Result<()> {
    eprintln!("[mock] turn start {sid}");
    begin_work(&sid.to_string());
    // 受理即进入前台工作中。
    send_update(
        &cx,
        &sid,
        SessionUpdate::StateUpdate(StateUpdate::Running(
            agent_client_protocol::schema::v2::RunningStateUpdate::new(),
        )),
    )?;
    // 场景闸门（AMUX_MOCK_TURN_GATES=f1,f2,…）：prompt 序号 n 等 f_n 出现才继续。
    let prompt_no = PROMPT_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(gates) = std::env::var("AMUX_MOCK_TURN_GATES") {
        let gates: Vec<&str> = gates.split(',').collect();
        if let Some(gate) = gates.get(prompt_no as usize - 1) {
            wait_for_file(gate).await;
        }
    }

    // 权限请求：期望 server 自动批准（记录批准事实供测试断言）。
    let request = RequestPermissionRequest::new(
        sid.clone(),
        "运行命令？",
        vec![PermissionOption::new(
            "allow-once",
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )],
    );
    let response = cx.send_request(request).block_task().await?;
    if matches!(response.outcome, RequestPermissionOutcome::Selected(_)) {
        append_approved(&state_file);
    }

    if std::env::var_os("AMUX_MOCK_WAIT_FOR_CANCEL").is_some()
        && waited_sessions().lock().insert(sid.to_string())
    {
        wait_for_cancel(&sid.to_string()).await;
        return finish(&cx, &sid, StopReason::Cancelled);
    }

    if let Ok(path) = std::env::var("AMUX_MOCK_STEPS") {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
        let scenario: Value = serde_json::from_str(&raw)
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
        // 每轮 turn 独立编号，多轮场景按 prompt 序号取用同一份 steps
        let steps = scenario["steps"].as_array().cloned().unwrap_or_default();
        let mut thought_no = 0usize;
        // 连续 thinking 步骤属于同一思考块（复用 messageId）；工具调用结束该块，
        // 之后的 thinking 开启新块——与 ACP v2 的 messageId 语义一致。
        let mut thought_id = String::new();
        for step in steps {
            if let Some(gate) = step["gate"].as_str() {
                wait_for_file(gate).await;
            }
            match step["kind"].as_str().unwrap_or_default() {
                "thinking" => {
                    if thought_id.is_empty() {
                        thought_no += 1;
                        thought_id = format!("t{thought_no}");
                    }
                    send_update(
                        &cx,
                        &sid,
                        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                            ContentBlock::Text(
                                agent_client_protocol::schema::v2::TextContent::new(
                                    step["text"].as_str().unwrap_or_default(),
                                ),
                            ),
                            MessageId::new(thought_id.clone()),
                        )),
                    )?;
                }
                "tool_call" => {
                    thought_id.clear();
                    send_update(
                        &cx,
                        &sid,
                        SessionUpdate::ToolCallUpdate(
                            ToolCallUpdate::new(step["id"].as_str().unwrap_or("tc1"))
                                .title(step["title"].as_str().unwrap_or("工具调用"))
                                .kind(tool_kind(step["tool"].as_str().unwrap_or("read")))
                                .status(ToolCallStatus::Completed),
                        ),
                    )?;
                }
                "output" => {
                    send_update(
                        &cx,
                        &sid,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(
                                agent_client_protocol::schema::v2::TextContent::new(
                                    step["text"].as_str().unwrap_or_default(),
                                ),
                            ),
                            MessageId::new("a1"),
                        )),
                    )?;
                }
                _ => {}
            }
        }
        if let Some(gate) = scenario["end_gate"].as_str() {
            wait_for_file(gate).await;
        }
        send_profile_updates(&cx, &sid)?;
        return finish(&cx, &sid, StopReason::EndTurn);
    }

    // 默认事件流：思考 → 工具调用（同 id 两次 update 应合并为一条活动）→ 输出
    send_update(
        &cx,
        &sid,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(agent_client_protocol::schema::v2::TextContent::new(
                "让我想想…",
            )),
            MessageId::new("t1"),
        )),
    )?;
    let tool = ToolCallUpdate::new("tc1")
        .title("运行 cargo test")
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::InProgress);
    send_update(&cx, &sid, SessionUpdate::ToolCallUpdate(tool))?;
    let tool = ToolCallUpdate::new("tc1")
        .title("运行 cargo test 完成")
        .status(ToolCallStatus::Completed);
    send_update(&cx, &sid, SessionUpdate::ToolCallUpdate(tool))?;
    send_update(
        &cx,
        &sid,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(agent_client_protocol::schema::v2::TextContent::new(
                format!("完成：{user_text}"),
            )),
            MessageId::new("a1"),
        )),
    )?;
    send_update(
        &cx,
        &sid,
        SessionUpdate::UsageUpdate(UsageUpdate::new(53_000, 200_000)),
    )?;
    send_profile_updates(&cx, &sid)?;
    finish(&cx, &sid, StopReason::EndTurn)
}

/// 下发斜杠命令与计划（全量覆盖），供查询验证。
fn send_profile_updates(cx: &V2ConnectionTo<Client>, sid: &SessionId) -> Result<()> {
    send_update(
        cx,
        sid,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("goal", "设置或查看本会话目标"),
            AvailableCommand::new("review", "审查当前改动").input(AvailableCommandInput::Text(
                TextCommandInput::new("审查重点"),
            )),
        ])),
    )?;
    send_update(
        cx,
        sid,
        SessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::Items(PlanItems::new(
            "plan-1",
            vec![
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
                PlanEntry::new("可选优化", PlanEntryPriority::Low, PlanEntryStatus::Pending),
            ],
        )))),
    )
}

/// 结束本 turn：仅当该会话已无前台工作时上报 `idle` + stopReason
/// （idle 的含义是「已准备好接收新 prompt」，仍有在途 turn 时不得上报）。
fn finish(cx: &V2ConnectionTo<Client>, sid: &SessionId, reason: StopReason) -> Result<()> {
    if !end_work(&sid.to_string()) {
        return Ok(());
    }
    send_update(
        cx,
        sid,
        SessionUpdate::StateUpdate(StateUpdate::Idle(
            IdleStateUpdate::new().stop_reason(reason),
        )),
    )
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "execute" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}
