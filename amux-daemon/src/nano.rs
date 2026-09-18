use std::{collections::HashMap, io, path::PathBuf, sync::Arc};

mod runtime;
mod shell;
use agent_client_protocol::{
    schema::v2::*, Agent, Client, ConnectTo, Error, Lines, V2ConnectionTo,
};
use amux_common::{
    api::{OrchestratorConfig, AMUX_AUTH_METHOD, NANO_AGENT},
    daemon::{notify, AcpForward},
};
use futures_util::{sink, stream};
use parking_lot::Mutex;
use rig_core::completion::Message;
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
            let (cwd, history, mut cancel) = {
                let mut sessions = nano.sessions.lock();
                let Some(session) = sessions.get_mut(&request.session_id) else { return responder.respond_with_error(Error::invalid_params()); };
                if session.cancel.is_some() { return responder.respond_with_error(Error::invalid_request().data("会话正在工作中")); }
                let (tx, rx) = watch::channel(false);
                session.cancel = Some(tx);
                (session.cwd.clone(), std::mem::take(&mut session.history), rx)
            };
            let history = Arc::new(Mutex::new(history));
            responder.respond(PromptResponse::new())?;
            let tools = runtime::Events { cx: cx.clone(), id: request.session_id.clone() };
            tools.update(SessionUpdate::StateUpdate(StateUpdate::Running(RunningStateUpdate::new())));
            let nano = nano.clone();
            cx.spawn(async move {
                let reason = tokio::select! {
                    biased;
                    _ = cancel.changed() => StopReason::Cancelled,
                    result = async {
                        runtime::run(runtime::builder(&config)?, cwd, text, history.clone(), tools.clone()).await
                    } => {
                        match result {
                            Ok(_) => StopReason::EndTurn,
                            Err(error) => { tools.text(&error); StopReason::Other("_error".into()) }
                        }
                    }
                };
                let mut sessions = nano.sessions.lock();
                if let Some(session) = sessions.get_mut(&request.session_id) {
                    // 删除后恢复的同名会话不得被旧任务覆盖。
                    if session.cancel.as_ref().is_some_and(|tx| tx.subscribe().same_channel(&cancel)) {
                        session.history = std::mem::take(&mut *history.lock());
                        session.cancel = None;
                        tools.update(SessionUpdate::StateUpdate(StateUpdate::Idle(IdleStateUpdate::new().stop_reason(reason))));
                    }
                }
                Ok(())
            })
        }}, agent_client_protocol::on_receive_request!())
        .connect_to(transport).await
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

    #[tokio::test]
    async fn cancelling_inflight_model_request_returns_idle_and_allows_next_prompt() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, mut entered_rx) = mpsc::channel(2);
        let server = tokio::spawn(async move {
            let mut sockets = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                sockets.push(socket);
                entered_tx.send(()).await.unwrap();
            }
            std::future::pending::<()>().await;
        });
        let (tx, outbox, task) = started().await;
        let _ = next_raw(&outbox).await;
        let mut meta = config_meta();
        meta["amuxBaseUrl"] = format!("http://{address}").into();
        send(
            &tx,
            2,
            "auth/login",
            serde_json::json!({"methodId": AMUX_AUTH_METHOD, "_meta": meta}),
        )
        .await;
        assert!(next_raw(&outbox).await.get("error").is_none());
        send(&tx, 3, "session/new", serde_json::json!({"cwd": "/tmp"})).await;
        let session_id = next_raw(&outbox).await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        for id in [4, 5] {
            send(&tx, id, "session/prompt", serde_json::json!({"sessionId": session_id, "prompt": [{"type":"text", "text":"开始"}]})).await;
            let response = next_raw(&outbox).await;
            assert_eq!(response["id"], id);
            assert!(response.get("error").is_none(), "{response}");
            let running = next_raw(&outbox).await;
            assert_eq!(running["params"]["update"]["state"], "running");
            tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
                .await
                .unwrap()
                .unwrap();
            tx.send(serde_json::json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":session_id}}).to_string()).await.unwrap();
            let idle = next_raw(&outbox).await;
            assert_eq!(idle["params"]["update"]["state"], "idle", "{idle}");
            assert_eq!(idle["params"]["update"]["stopReason"], "cancelled");
        }
        drop(tx);
        let _ = task.await;
        server.abort();
        let _ = server.await;
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
