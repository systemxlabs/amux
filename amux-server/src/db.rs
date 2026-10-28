//! 同步阻塞工作（SQLite、文件 I/O）的执行边界。
//!
//! `blocking` 把独占所有权的任务交给阻塞线程池；`blocking_sync` 用于必须在当前
//! 作用域内借用数据的同步片段，多线程运行时用 `block_in_place` 让出 worker。

use tokio::runtime::RuntimeFlavor;

use crate::error::{ServerError, ServerResult};

pub async fn blocking<T, F>(work: F) -> ServerResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ServerError::internal(format!("阻塞任务失败: {error}")))
}

/// 在当前线程执行同步工作，但让出 async worker。
///
/// 单线程运行时（测试）没有可替换的 worker，直接执行。
pub fn blocking_sync<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}
