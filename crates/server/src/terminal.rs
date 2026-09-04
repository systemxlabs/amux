//! 用户终端：PTY 子进程与连接绑定生命周期。
//!
//! 终端不归属会话：一条应用连接可开多个，连接断开即全部释放，Server 侧仅存内存。
//! PTY 输出帧直接写入所属连接的专属出站通道（transport 每连接一条 mpsc），
//! 不经过 `session.state_change` 的全局广播——共享 broadcast 通道下高流量
//! 终端输出会把慢连接挤到 Lagged，连带丢弃其他连接的会话事件。
//!
//! 背压链：连接出站 mpsc 满 → 输出泵停 → 读线程阻塞 → PTY 内核缓冲填满 →
//! shell 的 write 阻塞。全程无丢弃，与终端语义一致。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::mpsc;

use protocol::{
    notify, server_error, JsonRpcNotification, TerminalExitNotification, TerminalIdParams,
    TerminalInputParams, TerminalOpenParams, TerminalOutputNotification, TerminalResizeParams,
};

use crate::rpc::RpcError;

/// 连接身份与该连接专属的终端帧出站通道（transport 建立、随连接销毁）。
#[derive(Clone)]
pub struct ConnScope {
    pub conn_id: u64,
    pub frame_tx: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
}

impl ConnScope {
    pub fn new(conn_id: u64, frame_tx: mpsc::Sender<String>) -> Self {
        Self {
            conn_id,
            frame_tx,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct TerminalHandle {
    conn_id: u64,
    /// 输入字节流送专职写线程：PTY master write 可能阻塞，不能占用 dispatcher
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    wait_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

pub struct TerminalService {
    terminals: Mutex<HashMap<String, TerminalHandle>>,
}

/// 启动后尚未交给终端 actor 的 PTY 子进程失败时必须显式终止并 reap；
/// portable-pty 的 `Child` drop 本身不会替我们回收外部进程。
fn reap_child(mut child: Box<dyn Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

fn join_wait_thread(wait_thread: &Arc<Mutex<Option<std::thread::JoinHandle<()>>>>) {
    if let Some(handle) = wait_thread.lock().take() {
        let _ = handle.join();
    }
}

fn stop_terminal(
    killer: &Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    wait_thread: &Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
) {
    killer.lock().kill().ok();
    join_wait_thread(wait_thread);
}

impl Default for TerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalService {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
        }
    }

    /// 打开终端：spawn PTY shell 并注册输出泵。cwd 不存在或行列非法返回参数错误。
    pub fn open(
        self: &Arc<Self>,
        params: TerminalOpenParams,
        conn: &ConnScope,
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
        let wait_slot = child_slot.clone();
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

        let handle = TerminalHandle {
            conn_id: conn.conn_id,
            input_tx,
            master: Arc::new(Mutex::new(pty.master)),
            killer,
            wait_thread,
        };
        let mut terminals = self.terminals.lock();
        if conn.is_closed() {
            stop_terminal(&handle.killer, &handle.wait_thread);
            return Err(RpcError::internal("连接已关闭"));
        }
        terminals.insert(terminal_id.clone(), handle);
        drop(terminals);
        log::info!(
            "终端 {terminal_id} 已打开（连接 {}，{cols}x{rows}，cwd {cwd}）",
            conn.conn_id
        );

        // 输出泵：PTY 字节 → base64 → 所属连接直发。进程退出后发 exit 通知并回收。
        let this = self.clone();
        let frame_tx = conn.frame_tx.clone();
        let exit_conn_id = conn.conn_id;
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
                if frame_tx
                    .send(notification_frame(notify::TERMINAL_OUTPUT, &payload))
                    .await
                    .is_err()
                {
                    // 连接已断：release_conn 会回收终端
                    return;
                }
            }
            if eof {
                // 先摘除条目，再发送退出通知，保证收到 terminal.exit 后的下一次
                // 操作不会因为输出泵尚未完成收尾而短暂成功。
                this.remove(&pump_id);
                let _ = frame_tx
                    .send(notification_frame(
                        notify::TERMINAL_EXIT,
                        &TerminalExitNotification {
                            terminal_id: pump_id.clone(),
                        },
                    ))
                    .await;
                log::info!("终端 {pump_id} 进程已退出（连接 {exit_conn_id}）");
            }
        });

        Ok(terminal_id)
    }

    /// 向终端写入输入字节流（仅允许所属连接操作）。
    pub fn input(&self, params: TerminalInputParams, conn_id: u64) -> Result<(), RpcError> {
        let handle = self.entry_for_connection(&params.terminal_id, conn_id)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.data)
            .map_err(|e| RpcError::invalid_params(format!("输入不是合法 base64: {e}")))?;
        handle
            .input_tx
            .send(bytes)
            .map_err(|_| RpcError::internal("终端输入通道已关闭"))?;
        Ok(())
    }

    /// 调整终端行列（仅允许所属连接操作）。
    pub fn resize(&self, params: TerminalResizeParams, conn_id: u64) -> Result<(), RpcError> {
        let handle = self.entry_for_connection(&params.terminal_id, conn_id)?;
        if params.cols == 0 || params.rows == 0 {
            return Err(RpcError::invalid_params("终端行列必须为正"));
        }
        let resized = handle
            .master
            .lock()
            .resize(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RpcError::internal(format!("resize 失败: {e}")));
        resized
    }

    /// 显式关闭终端（仅允许所属连接操作）。
    /// 先校验归属再摘除：反向顺序会让越权请求把别人的终端从注册表里顺走。
    pub fn close(&self, params: TerminalIdParams, conn_id: u64) -> Result<(), RpcError> {
        self.entry_for_connection(&params.terminal_id, conn_id)?;
        let handle = self
            .terminals
            .lock()
            .remove(&params.terminal_id)
            .ok_or_else(not_found)?;
        stop_terminal(&handle.killer, &handle.wait_thread);
        log::info!("终端 {} 已关闭（连接 {}）", params.terminal_id, conn_id);
        Ok(())
    }

    fn entry_for_connection(
        &self,
        terminal_id: &str,
        conn_id: u64,
    ) -> Result<TerminalHandle, RpcError> {
        let handle = self
            .terminals
            .lock()
            .get(terminal_id)
            .cloned()
            .ok_or_else(not_found)?;
        if handle.conn_id != conn_id {
            return Err(not_found());
        }
        Ok(handle)
    }

    /// 连接断开：先标记连接关闭，再释放该连接的全部终端。
    /// 标记与摘除在同一表锁临界区内完成，防止并发 terminal.open 注册出孤儿 PTY。
    pub fn release_conn(&self, conn: &ConnScope) {
        let victims: Vec<_> = {
            let mut terminals = self.terminals.lock();
            conn.close();
            let ids: Vec<_> = terminals
                .iter()
                .filter(|(_, h)| h.conn_id == conn.conn_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| terminals.remove(&id))
                .collect()
        };
        for terminal in victims {
            stop_terminal(&terminal.killer, &terminal.wait_thread);
            log::info!("终端随连接 {} 断开释放", conn.conn_id);
        }
    }

    /// server 退出时释放所有仍注册的 PTY。每个子进程由 open() 创建的 wait
    /// 线程负责最终 reap；这里摘除句柄并发出终止信号，避免 process::exit 前遗留 shell。
    pub fn shutdown_all(&self) {
        let terminals: Vec<_> = self.terminals.lock().drain().map(|(_, h)| h).collect();
        for terminal in terminals {
            stop_terminal(&terminal.killer, &terminal.wait_thread);
        }
    }

    /// 摘除终端条目（进程已退出路径；无连接校验——exit 通知已发给所属连接）。
    fn remove(&self, terminal_id: &str) {
        self.terminals.lock().remove(terminal_id);
    }
}

fn not_found() -> RpcError {
    RpcError {
        code: server_error::TERMINAL_NOT_FOUND,
        message: "终端不存在或已关闭".into(),
    }
}

/// 通知 → JSON-RPC 帧（terminal 帧与广播帧共用信封）。
pub(crate) fn notification_frame<T: Serialize>(method: &str, payload: &T) -> String {
    serde_json::to_string(&JsonRpcNotification {
        jsonrpc: "2.0".into(),
        method: method.to_string(),
        params: Some(serde_json::to_value(payload).expect("通知负载序列化失败")),
    })
    .expect("通知帧序列化失败")
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
}
