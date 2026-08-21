//! 会话操作错误（强类型）：rpc 边界一次性映射 JSON-RPC 错误码，
//! 替代按中文错误文案前缀反推错误码的脆弱做法。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("会话不存在: {0}")]
    NotFound(String),
    #[error("会话忙：agent 不支持进行中注入（steer），请等待当前工作结束")]
    Busy,
    #[error("agent 不可用: {0}")]
    AgentUnavailable(String),
    #[error("prompt 输入必须非空")]
    EmptyInput,
    /// 注册表/日志等本地存储故障
    #[error("{0}")]
    Storage(String),
}

impl From<rusqlite::Error> for SessionError {
    fn from(e: rusqlite::Error) -> Self {
        SessionError::Storage(format!("注册表读写失败: {e}"))
    }
}
