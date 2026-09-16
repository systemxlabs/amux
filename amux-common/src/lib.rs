//! amux 共享代码。
//!
//! - [`jsonrpc`]：Server-Daemon WebSocket 用的 JSON-RPC 2.0 信封与错误码
//! - [`daemon`]：Daemon 协议（方法名、参数与结果、通知）
//! - [`api`]：Client API（HTTPS）的请求与响应类型
//! - [`domain`]：两侧共享的领域类型（会话、对话内容、活动、终端、diff 等）
//! - [`paths`]：运行目录（默认 `~/.amux`）
//! - [`log`]：统一日志初始化
//! - [`text`]：文本工具（截断等）

pub mod api;
pub mod daemon;
pub mod domain;
pub mod jsonrpc;
pub mod log;
pub mod paths;
pub mod text;
