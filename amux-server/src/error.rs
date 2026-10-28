//! Server 错误类型：service 边界用枚举区分失败语义，HTTP 状态码在 [`ServerError`] 集中映射。
//!
//! 内部仍可能产生 `String`（例如存储层），通过 [`From<String>`] 归为 [`ServerError::Internal`]；
//! 需要对外语义的地方显式构造具体变体。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub type ServerResult<T> = Result<T, ServerError>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ServerError {
    /// 资源不存在
    #[error("{0}")]
    NotFound(String),
    /// 请求参数非法
    #[error("{0}")]
    InvalidInput(String),
    /// 目标机器未接入（或 Daemon 连接中断）
    #[error("{0}")]
    MachineOffline(String),
    /// ACP 请求在超时时间内没有响应
    #[error("{0}")]
    AcpTimeout(String),
    /// ACP 连接已关闭或忙
    #[error("{0}")]
    AcpClosed(String),
    /// Agent 返回错误或连接失败
    #[error("{0}")]
    Agent(String),
    /// Server 内部错误
    #[error("{0}")]
    Internal(String),
}

impl ServerError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub fn machine_offline(message: impl Into<String>) -> Self {
        Self::MachineOffline(message.into())
    }

    pub fn acp_timeout(message: impl Into<String>) -> Self {
        Self::AcpTimeout(message.into())
    }

    pub fn acp_closed(message: impl Into<String>) -> Self {
        Self::AcpClosed(message.into())
    }

    pub fn agent(message: impl Into<String>) -> Self {
        Self::Agent(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// 保留错误语义（进而保留 HTTP 状态码）的前提下补充上下文。
    pub fn prefixed(self, prefix: &str) -> Self {
        match self {
            Self::NotFound(message) => Self::NotFound(format!("{prefix}: {message}")),
            Self::InvalidInput(message) => Self::InvalidInput(format!("{prefix}: {message}")),
            Self::MachineOffline(message) => Self::MachineOffline(format!("{prefix}: {message}")),
            Self::AcpTimeout(message) => Self::AcpTimeout(format!("{prefix}: {message}")),
            Self::AcpClosed(message) => Self::AcpClosed(format!("{prefix}: {message}")),
            Self::Agent(message) => Self::Agent(format!("{prefix}: {message}")),
            Self::Internal(message) => Self::Internal(format!("{prefix}: {message}")),
        }
    }
}

impl From<String> for ServerError {
    fn from(message: String) -> Self {
        Self::Internal(message)
    }
}

impl From<&str> for ServerError {
    fn from(message: &str) -> Self {
        Self::Internal(message.to_string())
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Self::MachineOffline(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::AcpTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::AcpClosed(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Agent(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
