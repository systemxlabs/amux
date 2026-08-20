//! 会话数据存储（docs/DESIGN.md「普通会话存储」）：server 本地日志为权威。
//! - 对话历史：`data_dir/sessions/<session_id>_history.jsonl`，每行一个 `HistoryItem`
//!   （仅用户输入 `UserMessage` 与 agent 输出 `AgentMessage`，流式输出合并后写入）
//! - 活动：`data_dir/sessions/<session_id>_activities.jsonl`，每行一个 `Activity`
//!   （thinking / tool_call / compaction / error）

use std::path::{Path, PathBuf};

use protocol::{Activity, ContentBlock, HistoryItem};

fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// 按会话的数据文件（历史 + 活动，docs/DESIGN.md「普通会话存储」）。
#[derive(Debug, Clone)]
pub struct SessionLog {
    history_path: PathBuf,
    activities_path: PathBuf,
}

fn append_lines<T: serde::Serialize>(path: &Path, entries: &[T]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for e in entries {
        let line = serde_json::to_string(e)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::io::Write::write_all(&mut f, line.as_bytes())?;
        std::io::Write::write_all(&mut f, b"\n")?;
    }
    Ok(())
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

impl SessionLog {
    /// 历史文件路径：`<data_dir>/sessions/<session_id>_history.jsonl`
    pub fn history_path(data_dir: &Path, session_id: &str) -> PathBuf {
        sessions_dir(data_dir).join(format!("{session_id}_history.jsonl"))
    }
    /// 活动文件路径：`<data_dir>/sessions/<session_id>_activities.jsonl`
    pub fn activities_path(data_dir: &Path, session_id: &str) -> PathBuf {
        sessions_dir(data_dir).join(format!("{session_id}_activities.jsonl"))
    }

    pub fn open(data_dir: &Path, session_id: &str) -> Self {
        SessionLog {
            history_path: Self::history_path(data_dir, session_id),
            activities_path: Self::activities_path(data_dir, session_id),
        }
    }

    /// 追加合并后的历史条目（turn 结束落盘；每行一个 HistoryItem）。
    pub fn append_history(&self, items: &[HistoryItem]) -> std::io::Result<()> {
        append_lines(&self.history_path, items)
    }

    /// 追加活动条目（turn 结束落盘；每行一个 Activity）。
    pub fn append_activities(&self, items: &[Activity]) -> std::io::Result<()> {
        append_lines(&self.activities_path, items)
    }

    /// 读取全部历史条目；日志缺失视为空。
    pub fn read_history(&self) -> Vec<HistoryItem> {
        read(&self.history_path)
    }

    /// 读取全部活动条目；日志缺失视为空。
    pub fn read_activities(&self) -> Vec<Activity> {
        read(&self.activities_path)
    }

    /// 历史文件是否存在（删除联动 / 测试断言）。
    pub fn history_exists(&self) -> bool {
        self.history_path.exists()
    }

    /// 活动文件是否存在（删除联动 / 测试断言）。
    pub fn activities_exists(&self) -> bool {
        self.activities_path.exists()
    }

    /// 任一数据文件是否存在。
    pub fn exists_any(&self) -> bool {
        self.history_path.exists() || self.activities_path.exists()
    }

    /// 删除数据文件（会话删除联动）。
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.history_path);
        let _ = std::fs::remove_file(&self.activities_path);
    }
}

/// 单 turn 聚合：把 turn 期间的驱动事件转换为历史 + 活动（docs/DESIGN.md「普通会话存储」）：
/// - 用户输入 → `HistoryItem::UserMessage`
/// - agent 输出合并为一条 `HistoryItem::AgentMessage`
/// - thinking 累积为一条 `Activity::Thinking`，工具调用为 `Activity::ToolCall`
#[derive(Default)]
pub struct TurnMerger {
    output: Option<(String, u64)>,
    thinking: Option<(String, u64)>,
    history: Vec<HistoryItem>,
    activities: Vec<Activity>,
}

impl TurnMerger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加用户消息。
    pub fn push_user(&mut self, content: Vec<ContentBlock>, timestamp: u64) {
        self.finish_output();
        self.finish_thinking();
        self.history
            .push(HistoryItem::UserMessage { content, timestamp });
    }

    /// 追加 agent 输出（合并）。
    pub fn push_output(&mut self, text: String, timestamp: u64) {
        if let Some((t, first)) = &mut self.output {
            if *first == 0 {
                *first = timestamp;
            }
            t.push_str(&text);
        } else {
            self.output = Some((text, timestamp));
        }
    }

    fn finish_output(&mut self) {
        if let Some((text, first)) = self.output.take() {
            self.history.push(HistoryItem::AgentMessage {
                content: vec![ContentBlock::Text { text }],
                timestamp: first,
            });
        }
    }

    /// 追加 thinking（合并连续 thinking）。
    pub fn push_thinking(&mut self, content: String, timestamp: u64) {
        if let Some((c, first)) = &mut self.thinking {
            if *first == 0 {
                *first = timestamp;
            }
            c.push_str(&content);
        } else {
            self.thinking = Some((content, timestamp));
        }
    }

    fn finish_thinking(&mut self) {
        if let Some((content, ts)) = self.thinking.take() {
            self.activities.push(Activity::Thinking {
                timestamp: ts,
                content,
            });
        }
    }

    /// 追加工具调用。
    #[allow(clippy::too_many_arguments)]
    pub fn push_tool_call(
        &mut self,
        name: String,
        title: Option<String>,
        content: Option<String>,
        timestamp: u64,
    ) {
        self.activities.push(Activity::ToolCall {
            timestamp,
            name,
            title,
            content,
        });
    }

    /// 追加执行错误。
    pub fn push_error(&mut self, activity: Activity) {
        self.activities.push(activity);
    }

    /// 完成一个 turn：收拢缓冲并返回（历史, 活动）。
    pub fn finish(&mut self) -> (Vec<HistoryItem>, Vec<Activity>) {
        self.finish_output();
        self.finish_thinking();
        (
            std::mem::take(&mut self.history),
            std::mem::take(&mut self.activities),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合并器：输出合并、thinking 合并、工具各自成条。
    #[test]
    fn merger_merges_output_and_thinking() {
        let mut m = TurnMerger::new();
        m.push_user(vec![ContentBlock::Text { text: "你好".into() }], 1);
        m.push_thinking("x".into(), 3);
        m.push_thinking("y".into(), 4);
        m.push_output("a".into(), 5);
        m.push_output("b".into(), 6);
        m.push_tool_call("t".into(), None, None, 7);
        let (hist, acts) = m.finish();
        assert_eq!(hist.len(), 2, "用户 + 一条合并输出");
        match &hist[1] {
            HistoryItem::AgentMessage { content, timestamp } => {
                assert_eq!(content[0], ContentBlock::Text { text: "ab".into() });
                assert_eq!(*timestamp, 5);
            }
            _ => panic!("第二条应为 AgentMessage"),
        }
        assert_eq!(acts.len(), 2, "thinking 合并 + tool");
        assert!(acts.iter().any(|a| matches!(a, Activity::Thinking { content, .. } if content == "xy")));
        assert!(acts.iter().any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "t")));
    }

    /// 历史/活动分文件。
    #[test]
    fn history_activities_separate_files() {
        let dir = std::env::temp_dir().join(format!(
            "amux-log-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let log = SessionLog::open(&dir, "s1");
        assert!(!log.exists_any());
        assert!(log.read_history().is_empty());
        assert!(log.read_activities().is_empty());

        log.append_history(&[HistoryItem::UserMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            timestamp: 1,
        }])
        .unwrap();
        assert!(log.history_exists());
        assert!(!log.activities_exists());

        log.append_activities(&[Activity::Thinking {
            timestamp: 1,
            content: "想".into(),
        }])
        .unwrap();
        assert!(log.activities_exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
