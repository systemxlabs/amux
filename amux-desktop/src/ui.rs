//! 共享渲染辅助：可用状态徽章、时间、内容块文本、活动文案。

use amux_common::domain::{Activity, ContentBlock, HistoryItem};
use gpui::*;
use gpui_component::badge::Badge;
use gpui_component::Theme;

/// agent 可用状态徽章。
pub fn availability_badge(available: bool, theme: &Theme) -> impl IntoElement {
    let (text, color) = if available {
        ("可用", theme.success)
    } else {
        ("不可用", theme.danger)
    };
    Badge::new()
        .color(color)
        .child(div().text_xs().child(text.to_string()))
}

/// 毫秒时间戳 → 本地 `MM-DD HH:MM`。
pub fn timestamp(ms: u64) -> String {
    let seconds = (ms / 1000) as i64;
    match jiff::Timestamp::from_second(seconds) {
        Ok(timestamp) => timestamp
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%m-%d %H:%M")
            .to_string(),
        Err(_) => String::new(),
    }
}

/// 内容块列表 → 纯文本（非文本块以占位表示）。
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Resource { uri, .. } => {
                format!("[资源 {}]", uri.clone().unwrap_or_default())
            }
            ContentBlock::ResourceLink { name, uri, .. } => format!("[链接 {name} {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 对话历史条目的文本。
pub fn history_text(item: &HistoryItem) -> String {
    match item {
        HistoryItem::UserMessage { content, .. } => blocks_text(content),
        HistoryItem::AgentMessage { content, .. } => blocks_text(content),
    }
}

/// 对话历史条目的时间戳。
pub fn history_timestamp(item: &HistoryItem) -> u64 {
    match item {
        HistoryItem::UserMessage { timestamp, .. } => *timestamp,
        HistoryItem::AgentMessage { timestamp, .. } => *timestamp,
    }
}

/// 一条活动的一行文案。
pub fn activity_line(activity: &Activity) -> String {
    match activity {
        Activity::Thinking { thinking, .. } => format!("思考：{thinking}"),
        Activity::ToolCall {
            tool_name, title, ..
        } => {
            let title = title.clone().unwrap_or_default();
            if title.is_empty() {
                format!("工具：{tool_name}")
            } else {
                format!("工具：{tool_name} · {title}")
            }
        }
        Activity::Error { error, .. } => format!("错误：{error}"),
    }
}

/// 活动条目时间戳。
pub fn activity_timestamp(activity: &Activity) -> u64 {
    match activity {
        Activity::Thinking { timestamp, .. } => *timestamp,
        Activity::ToolCall { timestamp, .. } => *timestamp,
        Activity::Error { timestamp, .. } => *timestamp,
    }
}

/// 截断长文本（列表行展示）。
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// 空态占位。
pub fn empty_hint(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .p_3()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(text.to_string())
}
