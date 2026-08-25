//! amux app↔server 协议面；GUI 与 server 共用这里的类型和方法名。

pub mod jsonrpc;
pub mod methods;
pub mod types;

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
