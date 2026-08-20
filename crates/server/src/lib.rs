//! amux server crate（docs/DESIGN.md §3）。二进制入口在 `main.rs`；模块导出供
//! 集成测试（tests/）复用实现（如 `AcpAgentDriver`）。

pub mod agent;
pub mod config;
pub mod git;
pub mod history;
pub mod registry;
pub mod rpc;
pub mod session;
pub mod transport;

pub use crate::rpc::{Handlers, RpcError};
pub use crate::session::SessionManager;
pub use crate::transport::{Transport, TransportOptions};
