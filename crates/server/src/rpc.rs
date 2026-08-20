//! JSON-RPC 方法处理器（docs/DESIGN.md「Client-Server 通信」协议表）：
//! 把协议方法面接到会话管理与 git。

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use protocol::{
    method, rpc_error, server_error, ActivitiesResult, AgentListResult, AgentParams,
    AgentSkillsResult, HistoryResult, OngoingActivityResult, OpResult, SessionConfigureParams,
    SessionIdParams, SessionInfoParams, SessionInfoResult, SessionListParams, SessionListResult,
    SessionNewParams, SessionPageParams, SessionPromptParams, SessionResult, WorkspaceDiffResult,
    WorkspaceRestoreParams,
};

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

/// 把会话操作错误映射到协议错误码（docs/DESIGN.md 协议表错误语义）。
fn map_session_err(e: String) -> RpcError {
    if e.starts_with("会话不存在") {
        RpcError::session_not_found(e)
    } else if e.starts_with("会话忙") {
        RpcError {
            code: server_error::SESSION_BUSY,
            message: e,
        }
    } else if e.contains("不可用") || e.contains("未发现 agent") {
        RpcError {
            code: server_error::HARNESS_UNAVAILABLE,
            message: e,
        }
    } else if e.starts_with("prompt 输入必须非空") {
        RpcError {
            code: server_error::INVALID_INPUT,
            message: e,
        }
    } else {
        RpcError::internal(e)
    }
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

            method::AGENT_SKILLS => {
                let p: AgentParams = parse(params)?;
                let skills = match self.manager.agents().driver_for(&p.agent) {
                    Ok(d) => d.list_skills(),
                    Err(_) => Vec::new(),
                };
                serde_json::to_value(AgentSkillsResult { skills })
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
                    .create(&p.agent, &p.cwd)
                    .await
                    .map_err(RpcError::internal)?;
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
                self.manager
                    .configure(&p.session_id, &p.title)
                    .await
                    .map_err(map_session_err)?;
                ok_op()
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
                let (sessions, has_more, next_before) = self
                    .manager
                    .list(p.limit, p.before)
                    .await
                    .map_err(RpcError::internal)?;
                serde_json::to_value(SessionListResult {
                    sessions,
                    has_more,
                    next_before,
                })
                .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::SESSION_INFO => {
                let p: SessionInfoParams = parse(params)?;
                let sessions = self
                    .manager
                    .info(&p.session_ids)
                    .await
                    .map_err(RpcError::internal)?;
                serde_json::to_value(SessionInfoResult { sessions })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_DIFF => {
                let p: WorkspaceRestoreParams = parse(params)?;
                let r: WorkspaceDiffResult = self.git.diff(&p.cwd, p.path.as_deref());
                serde_json::to_value(r).map_err(|e| RpcError::internal(e.to_string()))
            }

            method::WORKSPACE_RESTORE => {
                let p: WorkspaceRestoreParams = parse(params)?;
                let r = self
                    .git
                    .restore(&p.cwd, p.path.as_deref(), p.patch.as_deref());
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
        assert_eq!(p.before, None);
        let p: SessionListParams =
            parse(&Some(serde_json::Value::Null)).expect("显式 null 同样默认");
        assert_eq!(p.limit, None);
        // 必需字段缺失仍报参数非法
        assert!(parse::<SessionNewParams>(&Some(serde_json::json!({}))).is_err());
    }

    /// 会话操作错误映射：会话不存在 → SESSION_NOT_FOUND。
    #[test]
    fn map_session_err_codes() {
        let e = map_session_err("会话不存在: nope".into());
        assert_eq!(e.code, server_error::SESSION_NOT_FOUND);
        let e = map_session_err("会话忙：请等待".into());
        assert_eq!(e.code, server_error::SESSION_BUSY);
        let e = map_session_err("agent 不可用（启动时拉起失败）: x".into());
        assert_eq!(e.code, server_error::HARNESS_UNAVAILABLE);
        let e = map_session_err("prompt 输入必须非空".into());
        assert_eq!(e.code, server_error::INVALID_INPUT);
        let e = map_session_err("其他错误".into());
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
