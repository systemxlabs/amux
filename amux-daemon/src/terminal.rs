//! 终端：PTY 子进程与输出泵。
//!
//! 终端归属普通会话（由 Server 决定生命周期），Daemon 只负责 PTY 与字节流转发：
//! - 输入：Server 下行的 base64 字节写入 PTY master
//! - 输出：PTY master 读线程 → 异步输出泵 → `terminal.output` 通知（base64）
//! - 进程退出：`terminal.exit` 通知，并摘除条目
//!
//! 输出帧进入 [`crate::outbox::Outbox`]：与 Server 断线期间输出仍在缓存中累积，
//! 重连后补发（docs/DESIGN.md「断线重连」）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;

use amux_common::daemon::notify;
use amux_common::domain::{
    TerminalExitNotification, TerminalIdParams, TerminalInputParams, TerminalOpenParams,
    TerminalOutputNotification, TerminalResizeParams,
};
use base64::Engine as _;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::frames;
use crate::outbox::Outbox;
use crate::rpc::{RpcError, RpcResult};

struct TerminalHandle {
    /// 输入字节流送专职写线程：PTY master write 可能阻塞，不能占用分发任务
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    wait_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

pub struct TerminalRegistry {
    terminals: Mutex<HashMap<String, TerminalHandle>>,
}

/// 启动后尚未交给输出泵的 PTY 子进程失败时必须显式终止并 reap；
/// portable-pty 的 `Child` drop 本身不会替我们回收外部进程。
fn reap_child(mut child: Box<dyn Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

fn stop_terminal(
    killer: &Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    wait_thread: &Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) {
    let _ = killer.lock().kill();
    if let Some(handle) = wait_thread.lock().take() {
        let _ = handle.join();
    }
}

impl TerminalRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            terminals: Mutex::new(HashMap::new()),
        })
    }

    /// 打开终端：spawn PTY shell 并注册输出泵。
    pub fn open(
        self: &Arc<Self>,
        params: TerminalOpenParams,
        outbox: Arc<Outbox>,
    ) -> Result<String, RpcError> {
        let TerminalOpenParams { cwd, cols, rows } = params;
        if cols == 0 || rows == 0 {
            return Err(RpcError::invalid_params("终端行列必须为正"));
        }
        if !std::path::Path::new(&cwd).is_dir() {
            return Err(RpcError::invalid_input(format!(
                "终端 cwd 不存在或不是目录: {cwd}"
            )));
        }
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty = native_pty_system()
            .openpty(pty_size)
            .map_err(|e| RpcError::internal(format!("PTY 分配失败: {e}")))?;
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(&cwd);
        let child = pty
            .slave
            .spawn_command(cmd)
            .map_err(|e| RpcError::internal(format!("shell 启动失败: {e}")))?;
        // slave 描述符在 spawn 后立即释放：任何一端持有 slave 都会阻止 EOF
        drop(pty.slave);
        let writer = match pty.master.take_writer() {
            Ok(writer) => writer,
            Err(e) => {
                reap_child(child);
                return Err(RpcError::internal(format!("PTY writer 获取失败: {e}")));
            }
        };
        let reader = match pty.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => {
                reap_child(child);
                return Err(RpcError::internal(format!("PTY reader 获取失败: {e}")));
            }
        };

        let killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>> =
            Arc::new(Mutex::new(child.clone_killer()));
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let wait_slot = Arc::clone(&child_slot);
        let wait_thread = match std::thread::Builder::new()
            .name("terminal-wait".into())
            .spawn(move || {
                if let Some(mut child) = wait_slot.lock().take() {
                    if let Err(e) = child.wait() {
                        log::warn!("PTY 子进程回收失败: {e}");
                    }
                }
            }) {
            Ok(handle) => Arc::new(Mutex::new(Some(handle))),
            Err(e) => {
                if let Some(child) = child_slot.lock().take() {
                    reap_child(child);
                }
                return Err(RpcError::internal(format!("等待线程启动失败: {e}")));
            }
        };

        let terminal_id = uuid::Uuid::new_v4().to_string();

        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("terminal-input-{terminal_id}"))
            .spawn(move || {
                let mut writer = writer;
                for bytes in input_rx {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            })
        {
            stop_terminal(&killer, &wait_thread);
            return Err(RpcError::internal(format!("输入线程启动失败: {e}")));
        }

        // 读线程：阻塞读 PTY → 通道转发给异步输出泵。EOF 以空 Vec 标记。
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let read_tx = out_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("terminal-read-{terminal_id}"))
            .spawn(move || {
                let mut reader = reader;
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = read_tx.blocking_send(Vec::new());
                            break;
                        }
                        Ok(n) => {
                            if read_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            log::warn!("PTY 读取失败: {e}");
                            let _ = read_tx.blocking_send(Vec::new());
                            break;
                        }
                    }
                }
            })
        {
            stop_terminal(&killer, &wait_thread);
            return Err(RpcError::internal(format!("输出线程启动失败: {e}")));
        }

        self.terminals.lock().insert(
            terminal_id.clone(),
            TerminalHandle {
                input_tx,
                master: Arc::new(Mutex::new(pty.master)),
                killer,
                wait_thread,
            },
        );
        log::info!("终端 {terminal_id} 已打开（{cols}x{rows}，cwd {cwd}）");

        // 输出泵：PTY 字节 → base64 → 出站缓存。进程退出后发 exit 通知并回收条目。
        let this = Arc::clone(self);
        let pump_id = terminal_id.clone();
        tokio::spawn(async move {
            let mut eof = false;
            while let Some(chunk) = out_rx.recv().await {
                if chunk.is_empty() {
                    eof = true;
                    break;
                }
                let payload = TerminalOutputNotification {
                    terminal_id: pump_id.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(&chunk),
                };
                outbox.push(frames::notification(notify::TERMINAL_OUTPUT, &payload));
            }
            if eof {
                // 先摘除条目再发退出通知：收到 terminal.exit 后的操作不会短暂成功
                this.terminals.lock().remove(&pump_id);
                outbox.push(frames::notification(
                    notify::TERMINAL_EXIT,
                    &TerminalExitNotification {
                        terminal_id: pump_id.clone(),
                    },
                ));
                log::info!("终端 {pump_id} 进程已退出");
            }
        });

        Ok(terminal_id)
    }

    /// 向终端写入输入字节流。
    pub fn input(&self, params: TerminalInputParams) -> RpcResult<()> {
        let handle = self.entry(&params.terminal_id)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.data)
            .map_err(|e| RpcError::invalid_params(format!("输入不是合法 base64: {e}")))?;
        handle
            .input_tx
            .send(bytes)
            .map_err(|_| RpcError::internal("终端输入通道已关闭"))?;
        Ok(())
    }

    /// 调整终端行列。
    pub fn resize(&self, params: TerminalResizeParams) -> RpcResult<()> {
        if params.cols == 0 || params.rows == 0 {
            return Err(RpcError::invalid_params("终端行列必须为正"));
        }
        let handle = self.entry(&params.terminal_id)?;
        let master = handle.master.clone();
        let master = master.lock();
        master
            .resize(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RpcError::internal(format!("resize 失败: {e}")))
    }

    /// 关闭终端并回收 PTY。
    pub fn close(&self, params: TerminalIdParams) -> RpcResult<()> {
        let handle = self
            .terminals
            .lock()
            .remove(&params.terminal_id)
            .ok_or_else(not_found)?;
        stop_terminal(&handle.killer, &handle.wait_thread);
        log::info!("终端 {} 已关闭", params.terminal_id);
        Ok(())
    }

    /// Daemon 退出时释放所有终端。
    pub fn shutdown_all(&self) {
        let terminals: Vec<_> = self.terminals.lock().drain().map(|(_, h)| h).collect();
        for terminal in terminals {
            stop_terminal(&terminal.killer, &terminal.wait_thread);
        }
    }

    fn entry(&self, terminal_id: &str) -> RpcResult<TerminalHandle> {
        let handle = self.terminals.lock().get(terminal_id).cloned();
        handle.ok_or_else(not_found)
    }
}

fn not_found() -> RpcError {
    RpcError {
        code: amux_common::jsonrpc::server_error::TERMINAL_NOT_FOUND,
        message: "终端不存在或已关闭".into(),
    }
}

impl Clone for TerminalHandle {
    fn clone(&self) -> Self {
        Self {
            input_tx: self.input_tx.clone(),
            master: Arc::clone(&self.master),
            killer: Arc::clone(&self.killer),
            wait_thread: Arc::clone(&self.wait_thread),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// base64 往返（协议契约）：PTY 输出是任意字节，非 UTF-8 安全。
    #[test]
    fn base64_roundtrip() {
        let raw: Vec<u8> = (0u8..=255).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .unwrap(),
            raw
        );
    }

    #[tokio::test]
    async fn missing_terminal_reports_not_found() {
        let registry = TerminalRegistry::new();
        let error = registry
            .input(TerminalInputParams {
                terminal_id: "nope".into(),
                data: String::new(),
            })
            .unwrap_err();
        assert_eq!(
            error.code,
            amux_common::jsonrpc::server_error::TERMINAL_NOT_FOUND
        );
    }
}
