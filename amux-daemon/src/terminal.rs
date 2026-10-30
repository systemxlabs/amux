//! 终端：PTY 子进程、输出环形缓冲与订阅。
//!
//! Daemon 是终端元信息与输出缓冲的权威（docs/DESIGN.md「终端存储」）：
//! - 终端归属普通会话，输出按终端维护从零自增的序号
//! - 每个订阅独立推送，订阅者从环形缓冲起点开始接收
//! - 进程退出以 `terminal.output` 的 exit 事件通知每个订阅
//! - 超过 1 天没有输入输出的终端自动删除

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;

use amux_common::api::TerminalState;
use amux_common::daemon::notify;
use amux_common::domain::{
    TerminalAttachParams, TerminalAttachResult, TerminalDetachParams, TerminalIdParams,
    TerminalInfo, TerminalInputParams, TerminalListParams, TerminalListResult, TerminalOpenParams,
    TerminalOutputNotification, TerminalResizeParams,
};
use base64::Engine as _;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use crate::frames;
use crate::outbox::Outbox;
use crate::rpc::{RpcError, RpcResult};

/// 每个终端的输出环形缓冲上限。
const BUFFER_LIMIT: usize = 256 * 1024;
/// 终端空闲多久后自动删除。
const IDLE_EXPIRE_MS: u64 = 24 * 60 * 60 * 1000;

struct Block {
    sequence: u64,
    data: Arc<Vec<u8>>,
}

struct TerminalEvent {
    sequence: u64,
    data: Option<Arc<Vec<u8>>>,
    exit: bool,
    truncated: bool,
}

struct TerminalHandle {
    session_id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    state: TerminalState,
    last_active: u64,
    /// 输入字节流送专职写线程：PTY master write 可能阻塞，不能占用分发任务
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    wait_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    buffer: VecDeque<Block>,
    buffered_bytes: usize,
    /// 环形缓冲首块的序号；大于 0 表示更早的输出已被丢弃
    start_seq: u64,
    /// 下一个输出块的序号
    next_seq: u64,
    exit_seq: Option<u64>,
    subscriptions: HashMap<String, mpsc::UnboundedSender<TerminalEvent>>,
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl TerminalRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            terminals: Mutex::new(HashMap::new()),
        })
    }

    /// 打开终端：spawn PTY shell 并注册输出泵。
    pub fn open(self: &Arc<Self>, params: TerminalOpenParams) -> Result<String, RpcError> {
        let TerminalOpenParams {
            session_id,
            cwd,
            cols,
            rows,
        } = params;
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

        let now = now_ms();
        self.terminals.lock().insert(
            terminal_id.clone(),
            TerminalHandle {
                session_id,
                cwd: cwd.clone(),
                cols,
                rows,
                state: TerminalState::Running,
                last_active: now,
                input_tx,
                master: Arc::new(Mutex::new(pty.master)),
                killer,
                wait_thread,
                buffer: VecDeque::new(),
                buffered_bytes: 0,
                start_seq: 0,
                next_seq: 0,
                exit_seq: None,
                subscriptions: HashMap::new(),
            },
        );
        log::info!("终端已打开: terminal={terminal_id} cwd={cwd} cols={cols} rows={rows}");

        // 输出泵：PTY 字节 → 环形缓冲 → 每个订阅独立推送。
        let this = Arc::clone(self);
        let pump_id = terminal_id.clone();
        tokio::spawn(async move {
            let mut eof = false;
            while let Some(chunk) = out_rx.recv().await {
                if chunk.is_empty() {
                    eof = true;
                    break;
                }
                this.publish_output(&pump_id, chunk);
            }
            if eof {
                this.publish_exit(&pump_id);
            }
        });

        Ok(terminal_id)
    }
    /// 追加输出并推送给该终端的所有订阅。
    fn publish_output(&self, terminal_id: &str, chunk: Vec<u8>) {
        let data = Arc::new(chunk);
        let mut terminals = self.terminals.lock();
        let Some(handle) = terminals.get_mut(terminal_id) else {
            return;
        };
        let sequence = handle.next_seq;
        handle.next_seq += 1;
        handle.last_active = now_ms();
        handle.buffered_bytes += data.len();
        handle.buffer.push_back(Block {
            sequence,
            data: Arc::clone(&data),
        });
        while handle.buffered_bytes > BUFFER_LIMIT {
            let Some(block) = handle.buffer.pop_front() else {
                break;
            };
            handle.buffered_bytes -= block.data.len();
            handle.start_seq = block.sequence + 1;
        }
        for sender in handle.subscriptions.values() {
            let _ = sender.send(TerminalEvent {
                sequence,
                data: Some(Arc::clone(&data)),
                exit: false,
                truncated: false,
            });
        }
    }

    /// 标记退出并向所有订阅推送 exit 事件；终端元信息与缓冲保留到清理。
    fn publish_exit(&self, terminal_id: &str) {
        let mut terminals = self.terminals.lock();
        let Some(handle) = terminals.get_mut(terminal_id) else {
            return;
        };
        if handle.state == TerminalState::Exited {
            return;
        }
        let sequence = handle.next_seq;
        handle.next_seq += 1;
        handle.exit_seq = Some(sequence);
        handle.state = TerminalState::Exited;
        handle.last_active = now_ms();
        for sender in handle.subscriptions.values() {
            let _ = sender.send(TerminalEvent {
                sequence,
                data: None,
                exit: true,
                truncated: false,
            });
        }
        handle.subscriptions.clear();
        log::info!("终端进程已退出: terminal={terminal_id}");
    }

    /// 订阅终端：先重放环形缓冲中游标之后的部分，再接收实时输出。
    pub fn attach(
        self: &Arc<Self>,
        params: TerminalAttachParams,
        outbox: Arc<Outbox>,
    ) -> RpcResult<TerminalAttachResult> {
        let subscription_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel::<TerminalEvent>();
        {
            let mut terminals = self.terminals.lock();
            let handle = terminals
                .get_mut(&params.terminal_id)
                .ok_or_else(not_found)?;
            if handle.session_id != params.session_id {
                return Err(not_found());
            }
            let (replay, truncated) =
                replay_blocks(&handle.buffer, handle.start_seq, params.sequence);
            let mut first = true;
            for block in replay {
                let _ = tx.send(TerminalEvent {
                    sequence: block.sequence,
                    data: Some(Arc::clone(&block.data)),
                    exit: false,
                    truncated: first && truncated,
                });
                first = false;
            }
            if handle.state == TerminalState::Exited {
                let sequence = handle
                    .exit_seq
                    .unwrap_or_else(|| handle.next_seq.saturating_sub(1));
                let _ = tx.send(TerminalEvent {
                    sequence,
                    data: None,
                    exit: true,
                    truncated: false,
                });
            } else {
                handle.subscriptions.insert(subscription_id.clone(), tx);
            }
        }
        let forward_id = subscription_id.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(event) = rx.recv().await {
                let payload = TerminalOutputNotification {
                    subscription_id: forward_id.clone(),
                    sequence: event.sequence,
                    data: event.data.map(|data| {
                        base64::engine::general_purpose::STANDARD.encode(data.as_slice())
                    }),
                    exit: event.exit,
                    truncated: event.truncated,
                };
                outbox.push(frames::notification(notify::TERMINAL_OUTPUT, &payload));
            }
        });
        Ok(TerminalAttachResult { subscription_id })
    }

    /// 取消订阅；订阅已随终端退出或清理消失时视为成功。
    pub fn detach(&self, params: TerminalDetachParams) -> RpcResult<()> {
        let mut terminals = self.terminals.lock();
        for handle in terminals.values_mut() {
            if handle
                .subscriptions
                .remove(&params.subscription_id)
                .is_some()
            {
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn list(&self, params: TerminalListParams) -> TerminalListResult {
        let terminals = self.terminals.lock();
        let mut list: Vec<TerminalInfo> = terminals
            .iter()
            .filter(|(_, handle)| {
                params
                    .session_id
                    .as_deref()
                    .is_none_or(|session_id| handle.session_id == session_id)
            })
            .map(|(id, handle)| TerminalInfo {
                terminal_id: id.clone(),
                session_id: handle.session_id.clone(),
                cwd: handle.cwd.clone(),
                cols: handle.cols,
                rows: handle.rows,
                state: handle.state,
                sequence: handle.next_seq,
            })
            .collect();
        list.sort_by(|a, b| a.terminal_id.cmp(&b.terminal_id));
        TerminalListResult { terminals: list }
    }

    /// 向终端写入输入字节流。
    pub fn input(&self, params: TerminalInputParams) -> RpcResult<()> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.data)
            .map_err(|e| RpcError::invalid_params(format!("输入不是合法 base64: {e}")))?;
        let mut terminals = self.terminals.lock();
        let handle = terminals
            .get_mut(&params.terminal_id)
            .ok_or_else(not_found)?;
        if handle.session_id != params.session_id {
            return Err(not_found());
        }
        handle.last_active = now_ms();
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
        let master = {
            let mut terminals = self.terminals.lock();
            let handle = terminals
                .get_mut(&params.terminal_id)
                .ok_or_else(not_found)?;
            if handle.session_id != params.session_id {
                return Err(not_found());
            }
            handle.cols = params.cols;
            handle.rows = params.rows;
            handle.last_active = now_ms();
            Arc::clone(&handle.master)
        };
        let result = master
            .lock()
            .resize(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RpcError::internal(format!("resize 失败: {e}")));
        result
    }

    /// 关闭终端并回收 PTY；不存在时视为成功。
    pub fn close(&self, params: TerminalIdParams) -> RpcResult<()> {
        let handle = {
            let mut terminals = self.terminals.lock();
            let Some(handle) = terminals.get(&params.terminal_id) else {
                return Ok(());
            };
            if handle.session_id != params.session_id {
                return Err(not_found());
            }
            terminals.remove(&params.terminal_id)
        };
        if let Some(handle) = handle {
            let sequence = handle.next_seq;
            for sender in handle.subscriptions.values() {
                let _ = sender.send(TerminalEvent {
                    sequence,
                    data: None,
                    exit: true,
                    truncated: false,
                });
            }
            stop_terminal(&handle.killer, &handle.wait_thread);
            log::info!("终端已关闭: terminal={}", params.terminal_id);
        }
        Ok(())
    }

    /// 删除超过 1 天没有输入输出的终端。
    pub fn sweep_idle(&self) {
        let now = now_ms();
        let expired: Vec<String> = {
            let terminals = self.terminals.lock();
            terminals
                .iter()
                .filter(|(_, handle)| now.saturating_sub(handle.last_active) > IDLE_EXPIRE_MS)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for terminal_id in expired {
            let handle = self.terminals.lock().remove(&terminal_id);
            if let Some(handle) = handle {
                let sequence = handle.next_seq;
                for sender in handle.subscriptions.values() {
                    let _ = sender.send(TerminalEvent {
                        sequence,
                        data: None,
                        exit: true,
                        truncated: false,
                    });
                }
                stop_terminal(&handle.killer, &handle.wait_thread);
                log::info!("终端长时间无输入输出，已删除: terminal={terminal_id}");
            }
        }
    }

    /// Daemon 退出时释放所有终端。
    pub fn shutdown_all(&self) {
        let terminals: Vec<_> = self
            .terminals
            .lock()
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for terminal in terminals {
            stop_terminal(&terminal.killer, &terminal.wait_thread);
        }
    }
}

/// 选出订阅需要重放的块：
/// - 无游标：从缓冲开头重放；
/// - 游标早于缓冲头：从缓冲头重放并标记缺口（返回的 bool），客户端据此重置画面；
/// - 游标在缓冲范围内或之后：从第一个序号不小于游标的块开始。
fn replay_blocks(
    buffer: &VecDeque<Block>,
    start_seq: u64,
    sequence: Option<u64>,
) -> (impl Iterator<Item = &Block>, bool) {
    let truncated = match sequence {
        Some(sequence) => sequence < start_seq,
        None => start_seq > 0,
    };
    let from = sequence.unwrap_or(start_seq).max(start_seq);
    (
        buffer.iter().filter(move |block| block.sequence >= from),
        truncated,
    )
}

fn not_found() -> RpcError {
    RpcError {
        code: amux_common::jsonrpc::server_error::TERMINAL_NOT_FOUND,
        message: "终端不存在或已关闭".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_registry() -> (Arc<TerminalRegistry>, String) {
        let registry = TerminalRegistry::new();
        let terminal_id = registry
            .open(TerminalOpenParams {
                session_id: "s1".into(),
                cwd: std::env::temp_dir().to_string_lossy().into_owned(),
                cols: 80,
                rows: 24,
            })
            .unwrap();
        (registry, terminal_id)
    }

    #[tokio::test]
    async fn output_is_buffered_with_increasing_sequences() {
        let (registry, terminal_id) = open_registry();
        let before = registry
            .list(TerminalListParams {
                session_id: Some("s1".into()),
            })
            .terminals
            .into_iter()
            .find(|terminal| terminal.terminal_id == terminal_id)
            .unwrap()
            .sequence;
        registry.publish_output(&terminal_id, b"hello ".to_vec());
        registry.publish_output(&terminal_id, b"world".to_vec());

        let terminal = registry
            .list(TerminalListParams {
                session_id: Some("s1".into()),
            })
            .terminals
            .into_iter()
            .find(|terminal| terminal.terminal_id == terminal_id)
            .unwrap();
        assert_eq!(terminal.sequence, before + 2);
        assert_eq!(terminal.state, TerminalState::Running);
        registry
            .close(TerminalIdParams {
                session_id: "s1".into(),
                terminal_id,
            })
            .unwrap();
    }

    /// 订阅从环形缓冲起点开始推送，序号与终端维护的序号一致。
    #[tokio::test]
    async fn attach_streams_buffer_from_start() {
        let (registry, terminal_id) = open_registry();
        let start = registry
            .list(TerminalListParams {
                session_id: Some("s1".into()),
            })
            .terminals
            .into_iter()
            .find(|terminal| terminal.terminal_id == terminal_id)
            .unwrap()
            .sequence;
        registry.publish_output(&terminal_id, b"hello".to_vec());

        let outbox = Arc::new(Outbox::new());
        let attach = registry
            .attach(
                TerminalAttachParams {
                    session_id: "s1".into(),
                    terminal_id: terminal_id.clone(),
                    sequence: None,
                },
                Arc::clone(&outbox),
            )
            .unwrap();
        let mut found = false;
        for _ in 0..64 {
            let (seq, frame) = outbox.peek().await;
            outbox.ack(seq);
            let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let params = &value["params"];
            if params["subscriptionId"].as_str() != Some(&attach.subscription_id) {
                continue;
            }
            if params["data"].as_str() != Some("aGVsbG8=") {
                continue;
            }
            assert!(params["sequence"].as_u64().unwrap() >= start);
            found = true;
            break;
        }
        assert!(found, "未收到订阅快照");
        registry
            .close(TerminalIdParams {
                session_id: "s1".into(),
                terminal_id,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn list_filters_by_session() {
        let (registry, terminal_id) = open_registry();
        assert_eq!(
            registry
                .list(TerminalListParams {
                    session_id: Some("s1".into()),
                })
                .terminals
                .len(),
            1
        );
        assert!(registry
            .list(TerminalListParams {
                session_id: Some("s2".into()),
            })
            .terminals
            .is_empty());
        registry
            .close(TerminalIdParams {
                session_id: "s1".into(),
                terminal_id,
            })
            .unwrap();
    }

    #[test]
    fn missing_terminal_reports_not_found() {
        let registry = TerminalRegistry::new();
        let error = registry
            .input(TerminalInputParams {
                session_id: "s1".into(),
                terminal_id: "nope".into(),
                data: String::new(),
            })
            .unwrap_err();
        assert_eq!(
            error.code,
            amux_common::jsonrpc::server_error::TERMINAL_NOT_FOUND
        );
        assert!(registry
            .attach(
                TerminalAttachParams {
                    session_id: "s1".into(),
                    terminal_id: "nope".into(),
                    sequence: None,
                },
                Arc::new(Outbox::new()),
            )
            .is_err());
    }

    fn blocks(sequences: &[u64]) -> VecDeque<Block> {
        sequences
            .iter()
            .map(|&sequence| Block {
                sequence,
                data: Arc::new(Vec::new()),
            })
            .collect()
    }

    fn replayed_sequences<'a>(replay: impl Iterator<Item = &'a Block>) -> Vec<u64> {
        replay.map(|block| block.sequence).collect()
    }

    #[test]
    fn replay_without_cursor_streams_from_buffer_start() {
        let buffer = blocks(&[5, 6]);
        let (replay, truncated) = replay_blocks(&buffer, 5, None);
        assert_eq!(replayed_sequences(replay), vec![5, 6]);
        assert!(truncated, "缓冲已丢弃更早输出时需提示客户端");
    }

    #[test]
    fn replay_from_cursor_skips_already_received_blocks() {
        let buffer = blocks(&[0, 1, 2]);
        let (replay, truncated) = replay_blocks(&buffer, 0, Some(1));
        assert_eq!(replayed_sequences(replay), vec![1, 2]);
        assert!(!truncated);
    }

    #[test]
    fn replay_marks_gap_when_cursor_is_before_buffer_start() {
        let buffer = blocks(&[5, 6]);
        let (replay, truncated) = replay_blocks(&buffer, 5, Some(3));
        assert_eq!(replayed_sequences(replay), vec![5, 6]);
        assert!(truncated);
    }

    #[test]
    fn replay_after_latest_sequence_is_empty() {
        let buffer = blocks(&[0, 1]);
        let (replay, truncated) = replay_blocks(&buffer, 0, Some(2));
        assert!(replay.count() == 0);
        assert!(!truncated);
    }
}
