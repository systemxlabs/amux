//! 会话数据存储：server 本地日志为权威。
//! - 对话历史：`data_dir/sessions/<session_id>_history.jsonl`，每行一个 `HistoryItem`
//!   （仅用户输入 `UserMessage` 与 agent 输出 `AgentMessage`，流式输出合并后写入）
//! - 活动：`data_dir/sessions/<session_id>_activities.jsonl`，每行一个 `Activity`
//!   （thinking / tool_call / error）
//!
//! 文件布局与 JSONL 读写复用 `amux-common::session_log`。

use std::path::{Path, PathBuf};

use amux_common::session_log::{activities_path, append_jsonl, history_path, read_jsonl_page};
use protocol::{Activity, ContentBlock, HistoryItem};

/// 按会话的数据文件（历史 + 活动）。
#[derive(Debug, Clone)]
pub struct SessionLog {
    history_path: PathBuf,
    activities_path: PathBuf,
}

impl SessionLog {
    pub fn open(data_dir: &Path, session_id: &str) -> Self {
        SessionLog {
            history_path: history_path(data_dir, session_id),
            activities_path: activities_path(data_dir, session_id),
        }
    }

    pub fn append_history(&self, items: &[HistoryItem]) -> std::io::Result<()> {
        append_jsonl(&self.history_path, items)
    }

    pub fn append_activities(&self, items: &[Activity]) -> std::io::Result<()> {
        append_jsonl(&self.activities_path, items)
    }

    /// 分页读取历史尾部；窗口外的损坏行不会被本轮反序列化。
    pub fn read_history_page(
        &self,
        limit: usize,
        before: Option<u64>,
    ) -> std::io::Result<(Vec<HistoryItem>, bool, Option<u64>)> {
        read_jsonl_page(&self.history_path, limit, before)
    }

    /// 分页读取活动尾部；窗口外的损坏行不会被本轮反序列化。
    pub fn read_activities_page(
        &self,
        limit: usize,
        before: Option<u64>,
    ) -> std::io::Result<(Vec<Activity>, bool, Option<u64>)> {
        read_jsonl_page(&self.activities_path, limit, before)
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

/// tool_call 事件缺失 kind 字段时的固定名称（session.rs 实时活动共用）。
pub(crate) const DEFAULT_TOOL_NAME: &str = "tool_call";

/// 单 turn 聚合：把 turn 期间的 agent 事件转换为历史 + 活动：
/// - 用户输入 → `HistoryItem::UserMessage`
/// - agent 输出合并为一条 `HistoryItem::AgentMessage`
/// - thinking 累积为一条 `Activity::Thinking`
/// - 同一 `tool_call_id` 的 tool_call / tool_call_update 合并为一条
///   `Activity::ToolCall`
#[derive(Default)]
pub struct TurnMerger {
    output: Option<(String, u64)>,
    thinking: Option<(String, u64)>,
    /// 当前进行中的工具调用（按 ACP `tool_call_id` 合并；遇到新 id 或非工具事件时定稿）。
    current_tool: Option<CurrentTool>,
    history: Vec<HistoryItem>,
    activities: Vec<Activity>,
}

struct CurrentTool {
    id: String,
    name: String,
    title: Option<String>,
    parameters: Option<String>,
    timestamp: u64,
}

impl TurnMerger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_output(&mut self, text: String, timestamp: u64) {
        self.finish_current_tool();
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

    fn finish_current_tool(&mut self) {
        if let Some(tool) = self.current_tool.take() {
            self.activities.push(Activity::ToolCall {
                timestamp: tool.timestamp,
                tool_call_id: tool.id,
                tool_name: tool.name,
                title: tool.title,
                parameters: tool.parameters,
            });
        }
    }

    pub fn push_thinking(&mut self, content: String, timestamp: u64) {
        self.finish_current_tool();
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
                thinking: content,
            });
        }
    }

    pub fn push_tool_call(
        &mut self,
        id: String,
        name: Option<String>,
        title: Option<String>,
        parameters: Option<String>,
        timestamp: u64,
    ) {
        self.finish_thinking();
        if let Some(tool) = &mut self.current_tool {
            if tool.id == id {
                if let Some(name) = name {
                    tool.name = name;
                }
                if title.is_some() {
                    tool.title = title;
                }
                if parameters.is_some() {
                    tool.parameters = parameters;
                }
                return;
            }
        }
        self.finish_current_tool();
        let name = name.unwrap_or_else(|| DEFAULT_TOOL_NAME.into());
        self.current_tool = Some(CurrentTool {
            id,
            name,
            title,
            parameters,
            timestamp,
        });
    }

    pub fn push_error(&mut self, activity: Activity) {
        self.finish_current_tool();
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
        self.finish_current_tool();
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
        m.push_tool_call("tc1".into(), Some("t".into()), None, None, 7);
        m.push_error(Activity::Error {
            timestamp: 8,
            error: "出错".into(),
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
            .any(|a| matches!(a, Activity::Thinking { thinking, .. } if thinking == "xy")));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::ToolCall { tool_name, .. } if tool_name == "t")));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::Error { error, .. } if error == "出错")));
    }

    #[test]
    fn merger_merges_tool_call_updates_by_id() {
        let mut m = TurnMerger::new();
        m.push_tool_call(
            "tc1".into(),
            Some("read".into()),
            Some("读文件".into()),
            Some(r#"{"path":"a"}"#.into()),
            1,
        );
        m.push_tool_call("tc1".into(), None, Some("读文件 v2".into()), None, 2);
        m.push_tool_call("tc1".into(), None, None, Some(r#"{"path":"b"}"#.into()), 3);
        m.push_tool_call(
            "tc2".into(),
            Some("execute".into()),
            Some("运行测试".into()),
            None,
            4,
        );
        m.push_tool_call(
            "tc2".into(),
            None,
            Some("运行测试完成".into()),
            Some(r#"{"cmd":"cargo test"}"#.into()),
            5,
        );
        let (_, acts) = m.finish();
        assert_eq!(acts.len(), 2, "同 id 的多条 update 应合并为一条 ToolCall");
        match &acts[0] {
            Activity::ToolCall {
                tool_call_id,
                tool_name,
                title,
                parameters,
                ..
            } => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(tool_name, "read");
                assert_eq!(title.as_deref(), Some("读文件 v2"));
                assert_eq!(parameters.as_deref(), Some(r#"{"path":"b"}"#));
            }
            _ => panic!("应为 ToolCall"),
        }
        match &acts[1] {
            Activity::ToolCall {
                tool_call_id,
                tool_name,
                title,
                parameters,
                ..
            } => {
                assert_eq!(tool_call_id, "tc2");
                assert_eq!(tool_name, "execute");
                assert_eq!(title.as_deref(), Some("运行测试完成"));
                assert_eq!(parameters.as_deref(), Some(r#"{"cmd":"cargo test"}"#));
            }
            _ => panic!("应为 ToolCall"),
        }
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
        assert!(log.read_history_page(1, None).unwrap().0.is_empty());
        assert!(log.read_activities_page(1, None).unwrap().0.is_empty());

        log.append_history(&[HistoryItem::UserMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            timestamp: 1,
        }])
        .unwrap();
        assert!(log.history_exists());
        assert!(!log.activities_exists());

        log.append_activities(&[Activity::Thinking {
            timestamp: 1,
            thinking: "想".into(),
        }])
        .unwrap();
        assert!(log.activities_exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
