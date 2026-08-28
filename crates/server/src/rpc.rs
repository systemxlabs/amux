//! JSON-RPC 方法处理器，把协议方法面接到会话管理与 git。

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use protocol::{
    method, rpc_error, server_error, ActivitiesResult, AgentListResult, AgentParams, HistoryResult,
    OngoingActivityResult, OpResult, SessionConfigOptionsResult, SessionConfigureParams,
    SessionIdParams, SessionInfoParams, SessionInfoResult, SessionListParams, SessionListResult,
    SessionNewParams, SessionPageParams, SessionPromptParams, SessionResult, WorkspaceDiffParams,
    WorkspaceDiffResult, WorkspaceListParams, WorkspaceReadParams, WorkspaceRestoreParams,
};

use crate::error::SessionError;
use crate::git::GitRunner;
use crate::session::SessionManager;

#[derive(Debug)]
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

fn parse<T: DeserializeOwned>(params: &Option<Value>) -> Result<T, RpcError> {
    let v = params.clone().unwrap_or(Value::Null);
    // JSON-RPC 客户端可发送 null/缺省 params：全默认字段的结构体按空对象解析
    let v = if v.is_null() {
        Value::Object(Default::default())
    } else {
        v
    };
    serde_json::from_value(v).map_err(|e| RpcError::invalid_params(format!("参数非法: {e}")))
}

/// 把会话操作错误映射到协议错误码。
/// 强类型一次映射，替代按中文文案前缀反推。
fn map_session_err(e: SessionError) -> RpcError {
    let message = e.to_string();
    let code = match &e {
        SessionError::NotFound(_) => server_error::SESSION_NOT_FOUND,
        SessionError::Busy => server_error::SESSION_BUSY,
        SessionError::AgentUnavailable(_) => server_error::HARNESS_UNAVAILABLE,
        SessionError::EmptyInput => server_error::INVALID_INPUT,
        SessionError::Storage(_) => rpc_error::INTERNAL_ERROR,
    };
    RpcError { code, message }
}

fn ok_op() -> Result<Value, RpcError> {
    serde_json::to_value(OpResult {
        ok: true,
        message: None,
    })
    .map_err(|e| RpcError::internal(e.to_string()))
}

pub struct Handlers {
    pub manager: Arc<SessionManager>,
    pub git: GitRunner,
}

impl Handlers {
    pub async fn handle(&self, method: &str, params: &Option<Value>) -> Result<Value, RpcError> {
        match method {
            method::AGENT_LIST => {
                let agents = self.manager.agents().list_agents();
                serde_json::to_value(AgentListResult { agents })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::AGENT_RESTART => {
                let p: AgentParams = parse(params)?;
                self.manager
                    .agents()
                    .restart_agent(&p.agent)
                    .map_err(|e| RpcError {
                        code: server_error::HARNESS_UNAVAILABLE,
                        message: e,
                    })?;
                ok_op()
            }

            method::SESSION_NEW => {
                let p: SessionNewParams = parse(params)?;
                let session = self
                    .manager
                    .create(&p.agent, &p.cwd, p.use_worktree)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(SessionResult { session })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_PROMPT => {
                let p: SessionPromptParams = parse(params)?;
                if p.input.is_empty() {
                    return Err(RpcError {
                        code: server_error::INVALID_INPUT,
                        message: "prompt 输入必须非空".into(),
                    });
                }
                self.manager
                    .prompt(&p.session_id, p.input)
                    .await
                    .map_err(map_session_err)?;
                ok_op()
            }

            method::SESSION_CANCEL => {
                let p: SessionIdParams = parse(params)?;
                self.manager
                    .cancel(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                ok_op()
            }

            method::SESSION_DELETE => {
                let p: SessionIdParams = parse(params)?;
                self.manager
                    .delete(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                ok_op()
            }

            method::SESSION_CONFIGURE => {
                let p: SessionConfigureParams = parse(params)?;
                if p.title.is_none() && p.config.is_none() {
                    return Err(RpcError {
                        code: server_error::INVALID_INPUT,
                        message: "session.configure 至少设置标题或会话选项之一".into(),
                    });
                }
                if let Some(title) = &p.title {
                    self.manager
                        .configure(&p.session_id, Some(title))
                        .await
                        .map_err(map_session_err)?;
                }
                if let Some(config) = &p.config {
                    self.manager
                        .set_config_option(&p.session_id, &config.config_id, config.value.clone())
                        .await
                        .map_err(map_session_err)?;
                }
                ok_op()
            }

            method::SESSION_CONFIG_OPTIONS => {
                let p: SessionIdParams = parse(params)?;
                let options = self
                    .manager
                    .config_options(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(SessionConfigOptionsResult { options })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_HISTORY => {
                let p: SessionPageParams = parse(params)?;
                let (items, has_more, next_before) = self
                    .manager
                    .history(&p.session_id, p.limit, p.before)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(HistoryResult {
                    items,
                    has_more,
                    next_before,
                })
                .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_ACTIVITIES => {
                let p: SessionPageParams = parse(params)?;
                let (activities, has_more, next_before) = self
                    .manager
                    .activities(&p.session_id, p.limit, p.before)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(ActivitiesResult {
                    activities,
                    has_more,
                    next_before,
                })
                .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_ONGOING_ACTIVITY => {
                let p: SessionIdParams = parse(params)?;
                let activity = self
                    .manager
                    .ongoing_activity(&p.session_id)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(OngoingActivityResult { activity })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_LIST => {
                let p: SessionListParams = parse(params)?;
                let (sessions, has_more) =
                    self.manager.list(p.limit).await.map_err(map_session_err)?;
                serde_json::to_value(SessionListResult { sessions, has_more })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_INFO => {
                let p: SessionInfoParams = parse(params)?;
                let sessions = self
                    .manager
                    .info(&p.session_ids)
                    .await
                    .map_err(map_session_err)?;
                serde_json::to_value(SessionInfoResult { sessions })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_DIFF => {
                let p: WorkspaceDiffParams = parse(params)?;
                let cwd = self
                    .manager
                    .workspace_cwd(&p.session_id)
                    .map_err(map_session_err)?;
                let r: WorkspaceDiffResult = self.git.diff(&cwd, p.path.as_deref());
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_RESTORE => {
                let p: WorkspaceRestoreParams = parse(params)?;
                let cwd = self
                    .manager
                    .workspace_cwd(&p.session_id)
                    .map_err(map_session_err)?;
                let r = self
                    .git
                    .restore(&cwd, p.path.as_deref(), p.patch.as_deref());
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_LIST => {
                let p: WorkspaceListParams = parse(params)?;
                let cwd = self
                    .manager
                    .workspace_cwd(&p.session_id)
                    .map_err(map_session_err)?;
                let r = self
                    .git
                    .list_workspace(&cwd, p.path.as_deref(), p.limit, p.offset)
                    .map_err(RpcError::internal)?;
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_READ => {
                let p: WorkspaceReadParams = parse(params)?;
                let cwd = self
                    .manager
                    .workspace_cwd(&p.session_id)
                    .map_err(map_session_err)?;
                let r = self
                    .git
                    .read_workspace(&cwd, &p.path, p.offset, p.limit)
                    .map_err(RpcError::internal)?;
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }

            _ => Err(RpcError {
                code: rpc_error::METHOD_NOT_FOUND,
                message: format!("方法不存在: {method}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ContentBlock, SessionListParams};

    /// `parse` 对 null/缺省 params 按空对象解析：全默认字段的结构体得到默认值。
    #[test]
    fn parse_null_params_yields_defaults() {
        let p: SessionListParams = parse(&None).expect("null params 应解析为默认值");
        assert_eq!(p.limit, None);
        let p: SessionListParams =
            parse(&Some(serde_json::Value::Null)).expect("显式 null 同样默认");
        assert_eq!(p.limit, None);
        // 必需字段缺失仍报参数非法
        assert!(parse::<SessionNewParams>(&Some(serde_json::json!({}))).is_err());
    }

    /// 会话操作错误映射（强类型一次映射）。
    #[test]
    fn map_session_err_codes() {
        let e = map_session_err(SessionError::NotFound("nope".into()));
        assert_eq!(e.code, server_error::SESSION_NOT_FOUND);
        let e = map_session_err(SessionError::Busy);
        assert_eq!(e.code, server_error::SESSION_BUSY);
        let e = map_session_err(SessionError::AgentUnavailable("x".into()));
        assert_eq!(e.code, server_error::HARNESS_UNAVAILABLE);
        let e = map_session_err(SessionError::EmptyInput);
        assert_eq!(e.code, server_error::INVALID_INPUT);
        let e = map_session_err(SessionError::Storage("db".into()));
        assert_eq!(e.code, rpc_error::INTERNAL_ERROR);
    }

    /// ContentBlock 往返（协议契约）。
    #[test]
    fn content_block_roundtrip() {
        let b = ContentBlock::Text { text: "hi".into() };
        let s = serde_json::to_string(&b).unwrap();
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ContentBlock::Text { text: "hi".into() });
    }
}
