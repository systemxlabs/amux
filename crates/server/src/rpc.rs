//! JSON-RPC 方法处理器（docs/DESIGN.md §4）：把协议方法面接到会话管理与 git。

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use protocol::{
    method, rpc_error, server_error, CreateSessionParams, GitDiffParams, GitOpResult,
    GitRevertParams, ListAgentSkillsParams, ListAgentSkillsResult, ListSessionsParams, MachineInfo,
    OpenSessionParams, OpenSessionResult, PromptParams, SessionIdParams, SessionResult,
    SessionsResult, SetDefaultModelParams, SetSessionTitleParams,
};

use crate::git::GitRunner;
use crate::session::SessionManager;

pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        RpcError {
            code: rpc_error::INVALID_PARAMS,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        RpcError {
            code: rpc_error::INTERNAL_ERROR,
            message: msg.into(),
        }
    }
    pub fn session_not_found(msg: impl Into<String>) -> Self {
        RpcError {
            code: server_error::SESSION_NOT_FOUND,
            message: msg.into(),
        }
    }
}

pub struct Handlers {
    pub manager: Arc<SessionManager>,
    pub git: GitRunner,
    pub server_version: String,
}

fn parse<T: DeserializeOwned>(params: &Option<Value>) -> Result<T, RpcError> {
    let v = params.clone().unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| RpcError::invalid_params(format!("参数非法: {e}")))
}

/// 统一映射会话操作错误：会话不存在 → SESSION_NOT_FOUND；steer 不支持 → STEER_UNSUPPORTED。
fn map_session_err(e: String) -> RpcError {
    if e.starts_with("会话不存在") {
        RpcError::session_not_found(e)
    } else if e.contains("steer") {
        RpcError {
            code: server_error::STEER_UNSUPPORTED,
            message: e,
        }
    } else {
        RpcError::internal(e)
    }
}

impl Handlers {
    pub async fn handle(&self, method: &str, params: &Option<Value>) -> Result<Value, RpcError> {
        match method {
            method::GET_INFO => {
                let info = MachineInfo {
                    server_version: self.server_version.clone(),
                    harnesses: self.manager.harnesses(),
                };
                Ok(serde_json::to_value(info).map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::LIST_SESSIONS => {
                let p: ListSessionsParams = parse(params)?;
                let (sessions, has_more, next_before) = self
                    .manager
                    .list(p.limit, p.before)
                    .await
                    .map_err(RpcError::internal)?;
                Ok(serde_json::to_value(SessionsResult {
                    sessions,
                    has_more,
                    next_before,
                })
                .map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::CREATE_SESSION => {
                let p: CreateSessionParams = parse(params)?;
                let session = self
                    .manager
                    .create(&p.harness, &p.cwd, p.model.as_deref())
                    .await
                    .map_err(RpcError::internal)?;
                Ok(serde_json::to_value(SessionResult { session })
                    .map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::DELETE_SESSION => {
                let p: SessionIdParams = parse(params)?;
                self.manager
                    .delete(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                Ok(Value::Null)
            }
            method::PROMPT => {
                let p: PromptParams = parse(params)?;
                if p.input.is_empty() {
                    return Err(RpcError::invalid_params("prompt 输入必须非空"));
                }
                self.manager
                    .prompt(&p.session_id, p.input)
                    .await
                    .map_err(map_session_err)?;
                Ok(Value::Null)
            }
            method::CANCEL => {
                let p: SessionIdParams = parse(params)?;
                self.manager
                    .cancel(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                Ok(Value::Null)
            }
            method::OPEN_SESSION => {
                let p: OpenSessionParams = parse(params)?;
                let (events, has_more, next_before) = self
                    .manager
                    .open(&p.session_id, p.limit, p.before)
                    .await
                    .map_err(map_session_err)?;
                Ok(serde_json::to_value(OpenSessionResult {
                    events,
                    has_more,
                    next_before,
                })
                .map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::GIT_STATUS => {
                let cwd = parse::<GitCwdParams>(params)?.cwd;
                match self.git.status(&cwd) {
                    Ok(st) => {
                        Ok(serde_json::to_value(st)
                            .map_err(|e| RpcError::internal(e.to_string()))?)
                    }
                    Err(e) => Err(RpcError::internal(format!("git status 失败: {e}"))),
                }
            }
            method::GIT_DIFF => {
                let p: GitDiffParams = parse(params)?;
                let r = self.git.diff(&p.cwd, p.path.as_deref());
                Ok(serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::GIT_PUSH => {
                let cwd = parse::<GitCwdParams>(params)?.cwd;
                let r = self.git.push(&cwd);
                Ok(serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::GIT_REVERT => {
                let p: GitRevertParams = parse(params)?;
                let r: GitOpResult = self
                    .git
                    .revert(&p.cwd, p.path.as_deref(), p.patch.as_deref());
                Ok(serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))?)
            }
            method::SET_SESSION_TITLE => {
                let p: SetSessionTitleParams = parse(params)?;
                self.manager
                    .set_session_title(&p.session_id, &p.title)
                    .await
                    .map_err(map_session_err)?;
                Ok(Value::Null)
            }
            method::SET_DEFAULT_MODEL => {
                let p: SetDefaultModelParams = parse(params)?;
                self.manager.set_default_model(&p.harness, p.model);
                Ok(Value::Null)
            }
            method::LIST_AGENT_SKILLS => {
                let p: ListAgentSkillsParams = parse(params)?;
                let skills = self.manager.list_agent_skills(&p.harness);
                Ok(serde_json::to_value(ListAgentSkillsResult { skills })
                    .map_err(|e| RpcError::internal(e.to_string()))?)
            }
            _ => Err(RpcError {
                code: rpc_error::METHOD_NOT_FOUND,
                message: format!("方法不存在: {method}"),
            }),
        }
    }
}

#[derive(serde::Deserialize)]
struct GitCwdParams {
    cwd: String,
}
