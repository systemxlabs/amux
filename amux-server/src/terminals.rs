//! 终端输出缓存（Server 侧）。
//!
//! Daemon 只推 PTY 字节流，Server 负责按会话缓存输出并支持游标增量读取
//! （docs/DESIGN.md「终端存储」「终端视图」）：缓存有上限，超限丢弃最早的输出；
//! 客户端游标早于缓存起点时返回缓存全量并标记 `truncated`。

use std::collections::{HashMap, VecDeque};

use amux_common::api::{Terminal, TerminalOutput, TerminalState};
use base64::Engine as _;
use parking_lot::Mutex;

/// 单终端缓存上限（字节）。
const BUFFER_LIMIT: usize = 256 * 1024;

struct Entry {
    session_id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    state: TerminalState,
    buffer: VecDeque<u8>,
    /// buffer 首字节对应的流偏移
    start: u64,
    /// 已产生的总字节数（= 下次读取游标）
    total: u64,
}

#[derive(Default)]
pub struct TerminalCache {
    terminals: Mutex<HashMap<String, Entry>>,
}

impl TerminalCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, session_id: &str, terminal_id: &str, cwd: &str, cols: u16, rows: u16) {
        self.terminals.lock().insert(
            terminal_id.to_string(),
            Entry {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
                cols,
                rows,
                state: TerminalState::Running,
                buffer: VecDeque::new(),
                start: 0,
                total: 0,
            },
        );
    }

    pub fn output(&self, terminal_id: &str, data: &[u8]) {
        let mut terminals = self.terminals.lock();
        let Some(entry) = terminals.get_mut(terminal_id) else {
            return;
        };
        entry.buffer.extend(data.iter().copied());
        entry.total += data.len() as u64;
        while entry.buffer.len() > BUFFER_LIMIT {
            entry.buffer.pop_front();
            entry.start += 1;
        }
    }

    pub fn exit(&self, terminal_id: &str) {
        if let Some(entry) = self.terminals.lock().get_mut(terminal_id) {
            entry.state = TerminalState::Exited;
        }
    }

    pub fn resize(&self, terminal_id: &str, cols: u16, rows: u16) {
        if let Some(entry) = self.terminals.lock().get_mut(terminal_id) {
            entry.cols = cols;
            entry.rows = rows;
        }
    }

    /// 读取游标之后的增量输出。
    pub fn read(&self, terminal_id: &str, cursor: Option<u64>) -> Option<TerminalOutput> {
        let terminals = self.terminals.lock();
        let entry = terminals.get(terminal_id)?;
        let requested = cursor.unwrap_or(0);
        let truncated = requested < entry.start;
        let from = requested.max(entry.start);
        let skip = (from - entry.start) as usize;
        let bytes: Vec<u8> = entry.buffer.iter().skip(skip).copied().collect();
        Some(TerminalOutput {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            next_cursor: entry.total,
            truncated,
        })
    }

    pub fn list(&self, session_id: &str) -> Vec<Terminal> {
        let terminals = self.terminals.lock();
        let mut list: Vec<Terminal> = terminals
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, entry)| Terminal {
                id: id.clone(),
                cwd: entry.cwd.clone(),
                cols: entry.cols,
                rows: entry.rows,
                state: entry.state,
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn contains(&self, terminal_id: &str) -> bool {
        self.terminals.lock().contains_key(terminal_id)
    }

    pub fn remove(&self, terminal_id: &str) {
        self.terminals.lock().remove(terminal_id);
    }

    /// 会话删除：释放其全部终端缓存，返回需要通知 Daemon 关闭的终端 id。
    pub fn remove_session(&self, session_id: &str) -> Vec<String> {
        let mut terminals = self.terminals.lock();
        let ids: Vec<String> = terminals
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            terminals.remove(id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(output: &TerminalOutput) -> String {
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(&output.data)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn reads_incremental_output_by_cursor() {
        let cache = TerminalCache::new();
        cache.open("s1", "t1", "/w", 80, 24);
        cache.output("t1", b"hello ");
        cache.output("t1", b"world");

        let first = cache.read("t1", None).unwrap();
        assert_eq!(decoded(&first), "hello world");
        assert_eq!(first.next_cursor, 11);
        assert!(!first.truncated);

        // 游标之后的增量
        let second = cache.read("t1", Some(6)).unwrap();
        assert_eq!(decoded(&second), "world");

        let empty = cache.read("t1", Some(11)).unwrap();
        assert_eq!(decoded(&empty), "");
    }

    #[test]
    fn cursor_before_buffer_start_reports_truncated() {
        let cache = TerminalCache::new();
        cache.open("s1", "t1", "/w", 80, 24);
        let chunk = vec![b'x'; 1024];
        for _ in 0..(BUFFER_LIMIT / 1024 + 4) {
            cache.output("t1", &chunk);
        }
        let output = cache.read("t1", Some(0)).unwrap();
        assert!(output.truncated, "旧输出已被丢弃时应标记 truncated");
        assert_eq!(output.next_cursor, (BUFFER_LIMIT + 4096) as u64);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&output.data)
            .unwrap();
        assert_eq!(bytes.len(), BUFFER_LIMIT);
    }

    #[test]
    fn exit_marks_state_and_session_removal_collects_ids() {
        let cache = TerminalCache::new();
        cache.open("s1", "t1", "/w", 80, 24);
        cache.open("s2", "t2", "/w", 80, 24);
        cache.exit("t1");
        assert_eq!(cache.list("s1")[0].state, TerminalState::Exited);
        assert_eq!(cache.remove_session("s1"), ["t1"]);
        assert!(!cache.contains("t1"));
        assert!(cache.contains("t2"));
    }
}
