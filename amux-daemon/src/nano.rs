use std::{collections::HashMap, io, path::PathBuf, sync::Arc};

use agent_client_protocol::schema::v2::ToolCallContent;
use agent_client_protocol::{
    schema::v2::*, Agent, Client, ConnectTo, Error, Lines, V2ConnectionTo,
};
use amux_common::{
    api::{OrchestratorConfig, AMUX_AUTH_METHOD, NANO_AGENT},
    daemon::{notify, AcpForward},
    model::{self, ToolFuture, Tools},
};
use futures_util::{sink, stream};
use parking_lot::Mutex;
use rig_core::completion::{message::ToolCall, Message, ToolDefinition};
use tokio::sync::{mpsc, watch};

use crate::{frames, outbox::Outbox};

#[derive(Default)]
struct Nano {
    config: Mutex<Option<OrchestratorConfig>>,
    sessions: Mutex<HashMap<SessionId, Session>>,
}

struct Session {
    cwd: PathBuf,
    history: Vec<Message>,
    cancel: Option<watch::Sender<bool>>,
}

impl Session {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            history: Vec::new(),
            cancel: None,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            let _ = cancel.send(true);
        }
    }
}

impl Nano {
    fn config(&self) -> Result<OrchestratorConfig, Error> {
        self.config.lock().clone().ok_or_else(Error::auth_required)
    }

    fn cancel(&self, id: &SessionId) {
        if let Some(cancel) = self.sessions.lock().get(id).and_then(|s| s.cancel.as_ref()) {
            let _ = cancel.send(true);
        }
    }
}

pub async fn run(incoming: mpsc::Receiver<String>, outbox: Arc<Outbox>) -> Result<(), Error> {
    let nano = Arc::new(Nano::default());
    let outgoing = sink::unfold(outbox, |outbox, raw: String| async move {
        outbox.push(frames::notification(
            notify::ACP,
            &AcpForward {
                agent: NANO_AGENT.into(),
                raw,
            },
        ));
        Ok::<_, io::Error>(outbox)
    });
    let incoming = stream::unfold(incoming, |mut rx| async move {
        rx.recv().await.map(|line| (Ok::<_, io::Error>(line), rx))
    });
    serve(nano, Lines::new(outgoing, incoming)).await
}

async fn serve(nano: Arc<Nano>, transport: impl ConnectTo<Agent>) -> Result<(), Error> {
    Agent.v2().name(NANO_AGENT)
        .on_receive_request(async |request: InitializeRequest, responder, _cx| {
            responder.respond(InitializeResponse::new(request.protocol_version, Implementation::new(NANO_AGENT, env!("CARGO_PKG_VERSION")))
                .auth_methods(vec![AuthMethod::Other(OtherAuthMethod::new("_amux_config", AMUX_AUTH_METHOD, "Amux 模型配置", Default::default()))])
                .capabilities(AgentCapabilities::new().session(SessionCapabilities::new()
                    .prompt(PromptCapabilities::new()).delete(SessionDeleteCapabilities::new()))))
        }, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |request: LoginAuthRequest, responder, _cx| {
            if request.method_id.to_string() != AMUX_AUTH_METHOD { return responder.respond_with_error(Error::invalid_params()); }
            match OrchestratorConfig::from_auth_meta(request.meta.unwrap_or_default()) {
                Ok(config) => { *nano.config.lock() = Some(config); responder.respond(LoginAuthResponse::new()) }
                Err(error) => responder.respond_with_error(Error::invalid_params().data(error)),
            }
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |_request: LogoutAuthRequest, responder, _cx| {
            *nano.config.lock() = None;
            nano.sessions.lock().clear();
            responder.respond(LogoutAuthResponse::new())
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |request: NewSessionRequest, responder, _cx| {
            if let Err(error) = nano.config() { return responder.respond_with_error(error); }
            let id = SessionId::new(uuid::Uuid::new_v4().to_string());
            nano.sessions.lock().insert(id.clone(), Session::new(request.cwd.0));
            responder.respond(NewSessionResponse::new(id))
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |request: ResumeSessionRequest, responder, _cx| {
            if let Err(error) = nano.config() { return responder.respond_with_error(error); }
            nano.sessions.lock().entry(request.session_id).or_insert_with(|| Session::new(request.cwd.0));
            responder.respond(ResumeSessionResponse::new())
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |request: CloseSessionRequest, responder, _cx| {
            nano.sessions.lock().remove(&request.session_id);
            responder.respond(CloseSessionResponse::new())
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_request({ let nano = nano.clone(); async move |request: DeleteSessionRequest, responder, _cx| {
            nano.sessions.lock().remove(&request.session_id);
            responder.respond(DeleteSessionResponse::new())
        }}, agent_client_protocol::on_receive_request!())
        .on_receive_notification({ let nano = nano.clone(); async move |request: CancelSessionNotification, _cx| {
            nano.cancel(&request.session_id); Ok(())
        }}, agent_client_protocol::on_receive_notification!())
        .on_receive_request({ let nano = nano.clone(); async move |request: PromptRequest, responder, cx: V2ConnectionTo<Client>| {
            let config = match nano.config() { Ok(config) => config, Err(error) => return responder.respond_with_error(error) };
            let text = request.prompt.iter().map(|block| match block {
                ContentBlock::Text(text) => Ok(text.text.as_str()),
                _ => Err(Error::invalid_params().data("Nano 仅支持文本输入")),
            }).collect::<Result<Vec<_>, _>>();
            let text = match text { Ok(text) => text.join("\n"), Err(error) => return responder.respond_with_error(error) };
            let (cwd, mut history, mut cancel) = {
                let mut sessions = nano.sessions.lock();
                let Some(session) = sessions.get_mut(&request.session_id) else { return responder.respond_with_error(Error::invalid_params()); };
                if session.cancel.is_some() { return responder.respond_with_error(Error::invalid_request().data("会话正在工作中")); }
                let (tx, rx) = watch::channel(false);
                session.cancel = Some(tx);
                (session.cwd.clone(), std::mem::take(&mut session.history), rx)
            };
            history.push(Message::user(text));
            responder.respond(PromptResponse::new())?;
            let tools = Shell { cwd, cx: cx.clone(), id: request.session_id.clone(), last_call: Mutex::new(String::new()) };
            tools.update(SessionUpdate::StateUpdate(StateUpdate::Running(RunningStateUpdate::new())));
            let nano = nano.clone();
            cx.spawn(async move {
                let reason = tokio::select! {
                    biased;
                    _ = cancel.changed() => StopReason::Cancelled,
                    result = model::run(&config, "你是 Nano，使用 shell 工具在指定工作目录中完成用户任务。", &mut history, &tools) => {
                        match result {
                            Ok(_) => StopReason::EndTurn,
                            Err(error) => { tools.record_text(&error); StopReason::Other("_error".into()) }
                        }
                    }
                };
                let mut sessions = nano.sessions.lock();
                if let Some(session) = sessions.get_mut(&request.session_id) {
                    // 删除后恢复的同名会话不得被旧任务覆盖。
                    if session.cancel.as_ref().is_some_and(|tx| tx.subscribe().same_channel(&cancel)) {
                        session.history = history;
                        session.cancel = None;
                        tools.update(SessionUpdate::StateUpdate(StateUpdate::Idle(IdleStateUpdate::new().stop_reason(reason))));
                    }
                }
                Ok(())
            })
        }}, agent_client_protocol::on_receive_request!())
        .connect_to(transport).await
}

struct Shell {
    cwd: PathBuf,
    cx: V2ConnectionTo<Client>,
    id: SessionId,
    /// 最近一次记录的工具调用 ID：dispatch 结束后据此补发 Completed 状态
    last_call: Mutex<String>,
}

impl Shell {
    fn update(&self, update: SessionUpdate) {
        if let Err(error) = self
            .cx
            .send_notification(UpdateSessionNotification::new(self.id.clone(), update))
        {
            log::warn!("Nano 通知发送失败: {error}");
        }
    }
}

impl Tools for Shell {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "shell".into(),
            description: "在会话工作目录执行 shell 命令".into(),
            parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        }]
    }
    fn dispatch<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            if name != "shell" {
                return Err(format!("未知工具: {name}"));
            }
            let command = arguments["command"].as_str().ok_or("缺少 command")?;
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.cwd)
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            let result = format!(
                "exit: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            self.update(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(self.last_call.lock().clone())
                    .title("shell")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Content(Box::new(Content::new(
                        ContentBlock::Text(TextContent::new(&result)),
                    )))]),
            ));
            Ok(result)
        })
    }
    fn record_text(&self, text: &str) {
        self.update(SessionUpdate::AgentMessage(
            AgentMessage::new(uuid::Uuid::new_v4().to_string())
                .content(vec![text.to_string().into()]),
        ));
    }
    fn record_thinking(&self, text: &str) {
        self.update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text.to_string().into(),
            MessageId::new(uuid::Uuid::new_v4().to_string()),
        )));
    }
    fn record_tool_call(&self, call: &ToolCall) {
        *self.last_call.lock() = call.id.to_string();
        self.update(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(call.id.to_string())
                .title("shell")
                .kind(ToolKind::Execute)
                .status(ToolCallStatus::InProgress)
                .raw_input(call.function.arguments.clone()),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_common::jsonrpc::JsonRpcRequest;
    use std::time::Duration;

    /// 合法的最小模型配置（baseUrl 指向必然拒绝连接的本地端口）。
    fn config_meta() -> serde_json::Value {
        serde_json::json!({
            "amuxApiFormat": "responses",
            "amuxBaseUrl": "http://127.0.0.1:1",
            "amuxApiKey": "sk-test",
            "amuxModel": "test-model",
            "amuxEffort": ""
        })
    }

    async fn send(tx: &mpsc::Sender<String>, id: u64, method: &str, params: serde_json::Value) {
        tx.send(serde_json::to_string(&JsonRpcRequest::new(id, method, params)).unwrap())
            .await
            .unwrap();
    }

    /// 取下一条 nano 上行的 ACP 消息（等待出站帧，确定性协调）。
    async fn next_raw(outbox: &Outbox) -> serde_json::Value {
        let (seq, frame) = tokio::time::timeout(Duration::from_secs(5), outbox.peek())
            .await
            .expect("等待 nano 上行消息超时");
        outbox.ack(seq);
        let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["method"], notify::ACP);
        serde_json::from_str(frame["params"]["raw"].as_str().unwrap()).unwrap()
    }

    /// 启动 nano 并完成 initialize，返回 (入站通道, 出站缓存, 任务)。
    async fn started() -> (
        mpsc::Sender<String>,
        Arc<Outbox>,
        tokio::task::JoinHandle<Result<(), Error>>,
    ) {
        let outbox = Arc::new(Outbox::new());
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(run(rx, Arc::clone(&outbox)));
        send(
            &tx,
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": 2,
                "info": { "name": "t", "version": "0" }
            }),
        )
        .await;
        (tx, outbox, task)
    }

    /// 未登录时 session/new 必须返回标准的 auth_required 错误（docs/DESIGN.md「ACP 认证」）。
    #[tokio::test]
    async fn session_new_before_login_is_auth_required() {
        let (tx, outbox, task) = started().await;
        let init = next_raw(&outbox).await;
        assert!(init["result"]["authMethods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["methodId"] == AMUX_AUTH_METHOD));
        send(&tx, 2, "session/new", serde_json::json!({ "cwd": "/tmp" })).await;
        let response = next_raw(&outbox).await;
        assert_eq!(response["id"], 2);
        assert_eq!(response["error"]["code"], -32000);
        drop(tx);
        let _ = task.await;
    }

    /// 登录后 session/new 成功；close/delete 后 resume 新建会话（内存中恢复）。
    #[tokio::test]
    async fn auth_login_then_session_lifecycle() {
        let (tx, outbox, task) = started().await;
        let _ = next_raw(&outbox).await; // initialize 应答
        send(
            &tx,
            2,
            "auth/login",
            serde_json::json!({ "methodId": AMUX_AUTH_METHOD, "_meta": config_meta() }),
        )
        .await;
        let login = next_raw(&outbox).await;
        assert!(login.get("error").is_none(), "{login}");

        send(&tx, 3, "session/new", serde_json::json!({ "cwd": "/tmp" })).await;
        let created = next_raw(&outbox).await;
        let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();

        send(
            &tx,
            4,
            "session/delete",
            serde_json::json!({ "sessionId": session_id }),
        )
        .await;
        let deleted = next_raw(&outbox).await;
        assert!(deleted.get("error").is_none(), "{deleted}");

        send(
            &tx,
            5,
            "session/resume",
            serde_json::json!({ "sessionId": session_id, "cwd": "/tmp" }),
        )
        .await;
        let resumed = next_raw(&outbox).await;
        assert!(resumed.get("error").is_none(), "{resumed}");

        drop(tx);
        let _ = task.await;
    }
}
