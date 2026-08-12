//! 会话历史存储（docs/DESIGN.md §5.2）：server 本地事件日志为权威。
//! - 每会话一个日志文件（`~/.amux/server/history/<会话>.log`，JSON Lines）
//! - **按 turn 合并后落盘**：透传事件按 §5.3 语义合并（与 GUI `aggregate.rs` 对齐），
//!   收到 result（turn 结束，含取消）时写入；无 result 的 turn 视为未完成、不落库
//! - `open_session` 从本地日志按窗口/游标读取，**不触发 ACP 重放**

use std::path::Path;
use std::path::PathBuf;

use protocol::PassthroughEvent;

/// 每会话历史日志（JSON Lines：每行一个合并条目）。
#[derive(Debug, Clone)]
pub struct SessionLog {
    path: PathBuf,
}

impl SessionLog {
    /// 日志文件路径：`<data_dir>/history/<session_id>.log`
    pub fn path(data_dir: &Path, session_id: &str) -> PathBuf {
        data_dir.join("history").join(format!("{session_id}.log"))
    }

    pub fn open(data_dir: &Path, session_id: &str) -> Self {
        SessionLog {
            path: Self::path(data_dir, session_id),
        }
    }

    /// 追加合并条目（turn 结束落盘；每行一个 JSON）。
    pub fn append(&self, entries: &[PassthroughEvent]) -> std::io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        for e in entries {
            let line = serde_json::to_string(e)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
            std::io::Write::write_all(&mut f, line.as_bytes())?;
            std::io::Write::write_all(&mut f, b"\n")?;
        }
        Ok(())
    }

    /// 读取全部合并条目；日志缺失（损坏/清空）视为该会话历史为空（docs/DESIGN.md §5.2）。
    pub fn read(&self) -> Vec<PassthroughEvent> {
        std::fs::read_to_string(&self.path)
            .ok()
            .map(|s| {
                s.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 日志文件是否存在（会话删除联动 / 测试断言）。
    #[allow(dead_code)]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// 删除日志（会话删除联动）。
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 按 turn 合并器（docs/DESIGN.md §5.3，语义与 GUI `aggregate.rs` 对齐）：
/// - 输出 chunk 收敛为一条完整 agent 消息（OutputChunk）
/// - thinking 逐块累积为一条（ThinkingChunk）
/// - 同工具调用合并为一条（ToolCall）
/// - compaction / 用户消息 / turn 边界各自成条
///
/// turn 中缓冲，收到 result（TurnEnded）时 `finalize()` 收敛为合并条目；
/// 崩溃（无 TurnEnded）时缓冲丢弃、不落库。
#[derive(Debug, Default)]
pub struct TurnMerger {
    /// 累积的完整输出：(文本, 首 chunk 时间戳)
    output: Option<(String, u64)>,
    /// 累积的 thinking：(文本, 首块时间戳)
    thinking: Option<(String, u64)>,
    /// 合并中的工具调用：(name, title, content, 首次时间戳)
    tool: Option<(String, Option<String>, Option<String>, u64)>,
    /// 已定稿条目
    merged: Vec<PassthroughEvent>,
}

impl TurnMerger {
    /// 新建合并器（turn 开始）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 吸收一条透传事件（保持与 GUI 聚合一致的合并规则）。
    pub fn push(&mut self, ev: &PassthroughEvent) {
        match ev {
            PassthroughEvent::OutputChunk { text, timestamp } => {
                match &mut self.output {
                    Some((t, _)) => t.push_str(text),
                    None => self.output = Some((text.clone(), *timestamp)),
                }
            }
            PassthroughEvent::ThinkingChunk { content, timestamp } => {
                match &mut self.thinking {
                    Some((t, _)) => t.push_str(content),
                    None => self.thinking = Some((content.clone(), *timestamp)),
                }
            }
            PassthroughEvent::ToolCall {
                name,
                title,
                content,
                timestamp,
            } => {
                let mergeable = matches!(
                    &self.tool,
                    Some((n, _, _, _)) if n == name
                );
                if mergeable {
                    if let Some((_, t, c, _)) = &mut self.tool {
                        if t.is_none() {
                            *t = title.clone();
                        }
                        if let Some(nc) = content {
                            *c = Some(nc.clone());
                        }
                    }
                } else {
                    self.flush_tool();
                    self.tool = Some((name.clone(), title.clone(), content.clone(), *timestamp));
                }
            }
            // 打断当前累积并各自成条
            PassthroughEvent::Compaction { .. }
            | PassthroughEvent::UserMessage { .. }
            | PassthroughEvent::TurnStarted { .. }
            | PassthroughEvent::TurnEnded { .. } => {
                self.flush_pending();
                self.merged.push(ev.clone());
            }
            // 状态透传不落历史
            PassthroughEvent::SessionInfo { .. } => {}
        }
    }

    /// 收到 result（turn 结束，含取消）：收敛缓冲并返回合并条目。
    pub fn finalize(mut self) -> Vec<PassthroughEvent> {
        self.flush_pending();
        self.merged
    }
}

impl TurnMerger {
    fn flush_tool(&mut self) {
        if let Some((name, title, content, ts)) = self.tool.take() {
            self.merged.push(PassthroughEvent::ToolCall {
                name,
                title,
                content,
                timestamp: ts,
            });
        }
    }

    fn flush_pending(&mut self) {
        if let Some((text, ts)) = self.output.take() {
            self.merged.push(PassthroughEvent::OutputChunk {
                text,
                timestamp: ts,
            });
        }
        if let Some((content, ts)) = self.thinking.take() {
            self.merged.push(PassthroughEvent::ThinkingChunk {
                content,
                timestamp: ts,
            });
        }
        self.flush_tool();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ContentBlock;

    fn chunk(text: &str, ts: u64) -> PassthroughEvent {
        PassthroughEvent::OutputChunk {
            text: text.into(),
            timestamp: ts,
        }
    }
    fn think(content: &str, ts: u64) -> PassthroughEvent {
        PassthroughEvent::ThinkingChunk {
            content: content.into(),
            timestamp: ts,
        }
    }
    fn tool(name: &str, title: Option<&str>, content: Option<&str>, ts: u64) -> PassthroughEvent {
        PassthroughEvent::ToolCall {
            name: name.into(),
            title: title.map(str::to_string),
            content: content.map(str::to_string),
            timestamp: ts,
        }
    }
    fn user(text: &str, ts: u64) -> PassthroughEvent {
        PassthroughEvent::UserMessage {
            content: vec![ContentBlock::Text {
                text: text.into(),
            }],
            timestamp: ts,
        }
    }

    /// 合并器：输出 chunk 收敛为一条完整消息、thinking 累积一条、同工具合并一条（§5.3）。
    #[test]
    fn merger_merges_streaming_events() {
        let mut m = TurnMerger::new();
        m.push(&PassthroughEvent::TurnStarted { timestamp: 1 });
        m.push(&user("帮我改代码", 2));
        m.push(&think("思考", 3));
        m.push(&think("中…", 4));
        m.push(&tool("execute", Some("运行测试"), None, 5));
        m.push(&tool("execute", None, Some("cargo test"), 6));
        m.push(&chunk("第一", 7));
        m.push(&chunk("段输出", 8));
        m.push(&PassthroughEvent::TurnEnded { timestamp: 9 });

        let out = m.finalize();
        // turn 边界 2 + 用户消息 1 + thinking 1 + tool 1 + 完整输出 1 = 6 条
        assert_eq!(out.len(), 6, "合并粒度应远小于原始 chunk 流: {out:?}");

        assert!(out.iter().any(|e| matches!(e, PassthroughEvent::TurnStarted { .. })));
        assert!(out.iter().any(|e| matches!(e, PassthroughEvent::TurnEnded { .. })));

        let outputs: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                PassthroughEvent::OutputChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["第一段输出"], "chunk 应收敛为一条完整消息");

        let thinks: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                PassthroughEvent::ThinkingChunk { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinks, vec!["思考中…"], "thinking 应累积为一条");

        let tools: Vec<&PassthroughEvent> = out
            .iter()
            .filter(|e| matches!(e, PassthroughEvent::ToolCall { .. }))
            .collect();
        assert_eq!(tools.len(), 1, "同工具调用应合并为一条");
        match &tools[0] {
            PassthroughEvent::ToolCall {
                name,
                title,
                content,
                ..
            } => {
                assert_eq!(name, "execute");
                assert_eq!(title.as_deref(), Some("运行测试"));
                assert_eq!(content.as_deref(), Some("cargo test"));
            }
            _ => unreachable!(),
        }
    }

    /// 合并器：不同工具调用各自成条；compaction 独立成条并打断累积。
    #[test]
    fn merger_splits_distinct_kinds() {
        let mut m = TurnMerger::new();
        m.push(&tool("read", None, None, 1));
        m.push(&tool("execute", None, None, 2));
        m.push(&chunk("a", 3));
        m.push(&PassthroughEvent::Compaction {
            detail: "压缩".into(),
            timestamp: 4,
        });
        m.push(&chunk("b", 5));
        let out = m.finalize();
        let tools = out
            .iter()
            .filter(|e| matches!(e, PassthroughEvent::ToolCall { .. }))
            .count();
        assert_eq!(tools, 2, "不同工具应各自成条");
        let compactions = out
            .iter()
            .filter(|e| matches!(e, PassthroughEvent::Compaction { .. }))
            .count();
        assert_eq!(compactions, 1);
        let outputs: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                PassthroughEvent::OutputChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["a", "b"], "compaction 应打断输出累积");
    }

    /// 日志读写往返 + 缺失视为空 + 追加语义。
    #[test]
    fn session_log_append_read_remove() {
        let dir = std::env::temp_dir().join(format!(
            "amux-hist-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let log = SessionLog::open(&dir, "s1");
        assert!(!log.exists());
        assert!(log.read().is_empty(), "日志缺失视为历史为空");

        let entries = vec![user("hi", 1), chunk("完整输出", 2)];
        log.append(&entries).unwrap();
        assert!(log.exists());
        let read = log.read();
        assert_eq!(read.len(), 2);
        assert!(matches!(&read[1], PassthroughEvent::OutputChunk { text, .. } if text == "完整输出"));

        // 追加第二条 turn（JSON Lines 追加语义）
        log.append(&[chunk("第二段", 3)]).unwrap();
        assert_eq!(log.read().len(), 3);

        log.remove();
        assert!(!log.exists());
        assert!(log.read().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
