//! amux app↔server 协议面（protocol crate，协议的唯一来源）。
//! 语义依据 docs/DESIGN.md（§4 传输、§5 会话数据、§6 会话、§9 ACP）。
//! GUI 与 server 均从这里导入类型与方法/通知名。

pub mod jsonrpc;
pub mod methods;
pub mod types;

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
