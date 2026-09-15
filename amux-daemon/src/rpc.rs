//! JSON-RPC 错误与结果别名（请求分发与各能力模块共用）。

use amux_common::jsonrpc::{rpc_error, server_error};

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

pub type RpcResult<T> = Result<T, RpcError>;

impl RpcError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: rpc_error::INVALID_PARAMS,
            message: message.into(),
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            code: server_error::INVALID_INPUT,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: rpc_error::INTERNAL_ERROR,
            message: message.into(),
        }
    }

    pub fn agent_unavailable(message: impl Into<String>) -> Self {
        Self {
            code: server_error::AGENT_UNAVAILABLE,
            message: message.into(),
        }
    }

    pub fn git(message: impl Into<String>) -> Self {
        Self {
            code: server_error::GIT_FAILED,
            message: message.into(),
        }
    }

    pub fn fs(message: impl Into<String>) -> Self {
        Self {
            code: server_error::FS_FAILED,
            message: message.into(),
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: rpc_error::METHOD_NOT_FOUND,
            message: format!("方法不存在: {method}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_not_found_mentions_method() {
        let error = RpcError::method_not_found("fs.list");
        assert_eq!(error.code, rpc_error::METHOD_NOT_FOUND);
        assert!(error.message.contains("fs.list"));
    }
}
