//! 用户终端：PTY 子进程 + 连接绑定生命周期（docs/DESIGN.md「终端」）。
//!
//! 终端不归属会话：一条应用连接可开多个，连接断开即全部释放，Server 侧仅存内存。
//! PTY 输出帧直接写入所属连接的专属出站通道（transport 每连接一条 mpsc），
//! 不经过 `session.state_change` 的全局广播——共享 broadcast 通道下高流量
//! 终端输出会把慢连接挤到 Lagged，连带丢弃其他连接的会话事件。
//!
//! 背压链：连接出站 mpsc 满 → 输出泵停 → 读线程阻塞 → PTY 内核缓冲填满 →
//! shell 的 write 阻塞。全程无丢弃，与终端语义一致。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
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
}

#[derive(Clone)]
struct TerminalHandle {
    conn_id: u64,
    /// 输入字节流送专职写线程：PTY master write 可能阻塞，不能占用 dispatcher
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send>>>,
}

pub struct TerminalService {
    terminals: Mutex<HashMap<String, TerminalHandle>>,
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
            return Err(RpcError {
                code: server_error::INVALID_INPUT,
                message: format!("终端 cwd 不存在或不是目录: {cwd}"),
            });
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
        let writer = pty
            .master
            .take_writer()
            .map_err(|e| RpcError::internal(format!("PTY writer 获取失败: {e}")))?;
        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|e| RpcError::internal(format!("PTY reader 获取失败: {e}")))?;
        let killer = child.clone_killer();
        let terminal_id = uuid::Uuid::new_v4().to_string();

        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
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
            .map_err(|e| RpcError::internal(format!("输入线程启动失败: {e}")))?;

        // 读线程：阻塞读 PTY → 通道转发给异步输出泵。EOF 以空 Vec 标记。
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let read_tx = out_tx.clone();
        std::thread::Builder::new()
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
            .map_err(|e| RpcError::internal(format!("输出线程启动失败: {e}")))?;

        self.terminals.lock().unwrap().insert(
            terminal_id.clone(),
            TerminalHandle {
                conn_id: conn.conn_id,
                input_tx,
                master: Arc::new(Mutex::new(pty.master)),
                killer: Arc::new(Mutex::new(killer)),
            },
        );
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
                let _ = frame_tx
                    .send(notification_frame(
                        notify::TERMINAL_EXIT,
                        &TerminalExitNotification {
                            terminal_id: pump_id.clone(),
                        },
                    ))
                    .await;
                log::info!("终端 {pump_id} 进程已退出（连接 {exit_conn_id}）");
                this.remove(&pump_id);
            }
        });

        Ok(terminal_id)
    }

    /// 向终端写入输入字节流。
    pub fn input(&self, params: TerminalInputParams) -> Result<(), RpcError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.data)
            .map_err(|e| RpcError::invalid_params(format!("输入不是合法 base64: {e}")))?;
        let handle = self
            .terminals
            .lock()
            .unwrap()
            .get(&params.terminal_id)
            .cloned()
            .ok_or_else(not_found)?;
        let _ = handle.input_tx.send(bytes);
        Ok(())
    }

    /// 调整终端行列。
    pub fn resize(&self, params: TerminalResizeParams) -> Result<(), RpcError> {
        let handle = self
            .terminals
            .lock()
            .unwrap()
            .get(&params.terminal_id)
            .cloned()
            .ok_or_else(not_found)?;
        if params.cols == 0 || params.rows == 0 {
            return Err(RpcError::invalid_params("终端行列必须为正"));
        }
        let resized = handle
            .master
            .lock()
            .unwrap()
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
        {
            let map = self.terminals.lock().unwrap();
            let handle = map.get(&params.terminal_id).ok_or_else(not_found)?;
            if handle.conn_id != conn_id {
                return Err(not_found());
            }
        }
        let handle = self
            .terminals
            .lock()
            .unwrap()
            .remove(&params.terminal_id)
            .ok_or_else(not_found)?;
        handle.killer.lock().unwrap().kill().ok();
        log::info!("终端 {} 已关闭（连接 {}）", params.terminal_id, conn_id);
        Ok(())
    }

    /// 连接断开：释放该连接的全部终端（杀进程；输出泵随后经 EOF 自行摘除条目）。
    pub fn release_conn(&self, conn_id: u64) {
        let victims: Vec<_> = self
            .terminals
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.conn_id == conn_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in victims {
            if let Some(h) = self.terminals.lock().unwrap().remove(&id) {
                h.killer.lock().unwrap().kill().ok();
                log::info!("终端 {id} 随连接 {conn_id} 断开释放");
            }
        }
    }

    /// 摘除终端条目（进程已退出路径；无连接校验——exit 通知已发给所属连接）。
    fn remove(&self, terminal_id: &str) {
        self.terminals.lock().unwrap().remove(terminal_id);
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
