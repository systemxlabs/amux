//! amux server crate。二进制入口在 `main.rs`；模块导出供
//! 集成测试（tests/）复用实现（如 `AcpAgentDriver`）。

pub mod acp;
pub mod acp_terminal;
pub mod agent;
pub mod config;
pub mod discovery;
pub mod error;
pub mod git;
pub mod history;
pub mod registry;
pub mod rpc;
pub mod session;
pub mod terminal;
pub mod transport;
pub mod workspace;

pub use crate::rpc::{Handlers, RpcError};
pub use crate::session::SessionManager;
pub use crate::transport::{Transport, TransportOptions};
