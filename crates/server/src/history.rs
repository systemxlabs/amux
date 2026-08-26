//! 会话数据存储：server 本地日志为权威。
//! - 对话历史：`data_dir/sessions/<session_id>_history.jsonl`，每行一个 `HistoryItem`
//!   （仅用户输入 `UserMessage` 与 agent 输出 `AgentMessage`，流式输出合并后写入）
//! - 活动：`data_dir/sessions/<session_id>_activities.jsonl`，每行一个 `Activity`
//!   （thinking / tool_call / compaction / error）
//!
//! 文件布局与 JSONL 读写复用 `amux-common::session_log`。

use std::path::{Path, PathBuf};

use amux_common::session_log::{activities_path, append_jsonl, history_path, read_jsonl};
use protocol::{Activity, ContentBlock, HistoryItem};

/// 按会话的数据文件（历史 + 活动）。
#[derive(Debug, Clone)]
pub struct SessionLog {
    history_path: PathBuf,
    activities_path: PathBuf,
}

impl SessionLog {
    pub fn history_path(data_dir: &Path, session_id: &str) -> PathBuf {
        history_path(data_dir, session_id)
    }
    pub fn activities_path(data_dir: &Path, session_id: &str) -> PathBuf {
        activities_path(data_dir, session_id)
    }

    pub fn open(data_dir: &Path, session_id: &str) -> Self {
        SessionLog {
            history_path: Self::history_path(data_dir, session_id),
            activities_path: Self::activities_path(data_dir, session_id),
        }
    }

    pub fn append_history(&self, items: &[HistoryItem]) -> std::io::Result<()> {
        append_jsonl(&self.history_path, items)
    }

    pub fn append_activities(&self, items: &[Activity]) -> std::io::Result<()> {
        append_jsonl(&self.activities_path, items)
    }

    /// 读取全部历史条目；日志缺失视为空，损坏内容返回错误。
    pub fn read_history(&self) -> std::io::Result<Vec<HistoryItem>> {
        read_jsonl(&self.history_path)
    }

    /// 读取全部活动条目；日志缺失视为空，损坏内容返回错误。
    pub fn read_activities(&self) -> std::io::Result<Vec<Activity>> {
        read_jsonl(&self.activities_path)
    }

    pub fn history_exists(&self) -> bool {
        self.history_path.exists()
    }

    pub fn activities_exists(&self) -> bool {
        self.activities_path.exists()
    }

    pub fn exists_any(&self) -> bool {
        self.history_path.exists() || self.activities_path.exists()
    }

    pub fn remove(&self) -> std::io::Result<()> {
        for path in [&self.history_path, &self.activities_path] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// 单 turn 聚合：把 turn 期间的驱动事件转换为历史 + 活动：
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

    #[allow(clippy::too_many_arguments)]
    pub fn push_tool_call(
        &mut self,
        name: String,
        title: Option<String>,
        content: Option<String>,
        timestamp: u64,
    ) {
        self.finish_thinking();
        self.activities.push(Activity::ToolCall {
            timestamp,
            name,
            title,
            content,
        });
    }

    pub fn push_error(&mut self, activity: Activity) {
        self.finish_thinking();
        self.activities.push(activity);
    }

    /// 取走已定稿待写盘的活动（thinking 累积到 tool_call/error/turn 结束才定稿）。
    /// 供调用方在事件循环中实时逐条落盘，而非攒到 turn 结束统一写。
    pub fn take_ready(&mut self) -> Vec<Activity> {
        std::mem::take(&mut self.activities)
    }

    pub fn finish(&mut self) -> (Vec<HistoryItem>, Vec<Activity>) {
        self.finish_output();
        self.finish_thinking();
        (std::mem::take(&mut self.history), self.take_ready())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merger_merges_output_and_thinking() {
        let mut m = TurnMerger::new();
        m.push_thinking("x".into(), 3);
        m.push_thinking("y".into(), 4);
        m.push_output("a".into(), 5);
        m.push_output("b".into(), 6);
        m.push_tool_call("t".into(), None, None, 7);
        m.push_error(Activity::Error {
            timestamp: 8,
            detail: "出错".into(),
        });
        let (hist, acts) = m.finish();
        assert_eq!(hist.len(), 1, "仅输出合并为一条");
        match &hist[0] {
            HistoryItem::AgentMessage { content, timestamp } => {
                assert_eq!(content[0], ContentBlock::Text { text: "ab".into() });
                assert_eq!(*timestamp, 5);
            }
            _ => panic!("应为 AgentMessage"),
        }
        assert_eq!(acts.len(), 3, "thinking 合并 + tool + error");
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::Thinking { content, .. } if content == "xy")));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "t")));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::Error { detail, .. } if detail == "出错")));
    }

    #[test]
    fn history_activities_separate_files() {
        let dir = std::env::temp_dir().join(format!(
            "amux-log-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let log = SessionLog::open(&dir, "s1");
        assert!(!log.exists_any());
        assert!(log.read_history().unwrap().is_empty());
        assert!(log.read_activities().unwrap().is_empty());

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
