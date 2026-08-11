//! 透传事件 → 会话视图的聚合逻辑（docs/DESIGN.md §5.1：GUI 应用负责收敛输出、
//! 合并活动、派生 busy/idle）。纯函数，与 WS 通知收发分离，便于单测。

use protocol::{Activity, ContentBlock, DialogItem, PassthroughEvent, SessionState};

/// 会话视图状态（由透传事件流聚合/派生）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionView {
    /// 对话内容（用户消息 + agent 完整输出，chunk 已按消息收敛）
    pub dialog: Vec<DialogItem>,
    /// 完整活动历史（thinking 逐块累积为一条、同工具调用合并）
    pub activities: Vec<Activity>,
    /// 当前实时活动（turn 中；空闲时 None）
    pub live_activity: Option<Activity>,
    /// busy / idle（采信 `session_info_update` + turn 边界；重连经会话列表 meta.state 补齐）
    pub busy: bool,
}

impl SessionView {
    /// 新建空白视图。
    pub fn new() -> Self {
        Self::default()
    }
}

/// 把一条透传事件并入会话视图（增量聚合；open_session 重放与实时通知共用）。
pub fn merge_event(view: &mut SessionView, ev: &PassthroughEvent) {
    match ev {
        PassthroughEvent::UserMessage { content, timestamp } => {
            view.dialog.push(DialogItem::UserMessage {
                content: content.clone(),
                timestamp: *timestamp,
            });
        }
        PassthroughEvent::OutputChunk { text, timestamp } => {
            // 消息收敛：输出片段追加到最后一条 AgentOutput；无则新建一条
            match view.dialog.last_mut() {
                Some(DialogItem::AgentOutput { content, .. }) => match content.last_mut() {
                    Some(ContentBlock::Text { text: t }) => t.push_str(text),
                    _ => content.push(ContentBlock::Text { text: text.clone() }),
                },
                _ => view.dialog.push(DialogItem::AgentOutput {
                    content: vec![ContentBlock::Text { text: text.clone() }],
                    timestamp: *timestamp,
                }),
            }
        }
        PassthroughEvent::ThinkingChunk { content, timestamp } => {
            merge_activity(
                view,
                Activity::Thinking {
                    timestamp: *timestamp,
                    content: content.clone(),
                },
            );
        }
        PassthroughEvent::ToolCall {
            name,
            title,
            content,
            timestamp,
        } => {
            merge_activity(
                view,
                Activity::ToolCall {
                    timestamp: *timestamp,
                    name: name.clone(),
                    title: title.clone(),
                    content: content.clone(),
                },
            );
        }
        PassthroughEvent::Compaction { detail, timestamp } => {
            merge_activity(
                view,
                Activity::Compaction {
                    timestamp: *timestamp,
                    detail: detail.clone(),
                },
            );
        }
        PassthroughEvent::SessionInfo { state, .. } => {
            // agent 自报状态（ACP `session_info_update` 携带时采信）
            if let Some(s) = state {
                view.busy = *s == SessionState::Busy;
            }
        }
        PassthroughEvent::TurnStarted { .. } => view.busy = true,
        PassthroughEvent::TurnEnded { .. } => {
            view.busy = false;
            view.live_activity = None;
        }
    }
}

/// 活动合并（docs/DESIGN.md §5.3，与聚合语义一致）：
/// - 连续 thinking 块累积为一条 Thinking（逐块追加内容）
/// - 同工具（相同 name）的 tool_call 合并为一条（title/content 补全）
/// - compaction 独立成条
///
/// 合并后 live_activity 取当前活动。
fn merge_activity(view: &mut SessionView, activity: Activity) {
    match activity {
        Activity::Thinking { content, timestamp } => {
            if let Some(Activity::Thinking { content: c, .. }) = view.activities.last_mut() {
                c.push_str(&content);
            } else {
                view.activities
                    .push(Activity::Thinking { content, timestamp });
            }
        }
        Activity::ToolCall {
            name,
            title,
            content,
            timestamp,
        } => {
            let mergeable = matches!(
                view.activities.last(),
                Some(Activity::ToolCall { name: last, .. }) if *last == name
            );
            if mergeable {
                if let Some(Activity::ToolCall {
                    title: t,
                    content: c,
                    ..
                }) = view.activities.last_mut()
                {
                    if t.is_none() {
                        *t = title;
                    }
                    if let Some(nc) = content {
                        *c = Some(nc);
                    }
                }
            } else {
                view.activities.push(Activity::ToolCall {
                    timestamp,
                    name,
                    title,
                    content,
                });
            }
        }
        Activity::Compaction { detail, timestamp } => {
            view.activities
                .push(Activity::Compaction { detail, timestamp });
        }
    }
    view.live_activity = view.activities.last().cloned();
}

/// 批量聚合：从一组透传事件构建会话视图（open_session 重放窗口）。
pub fn aggregate_events(events: &[PassthroughEvent]) -> SessionView {
    let mut view = SessionView::new();
    for ev in events {
        merge_event(&mut view, ev);
    }
    view
}

/// 把更早一窗的重放事件聚合结果**前插**到现有视图（惰性加载"更早消息"，docs/DESIGN.md §5.2）。
/// 相邻 AgentOutput 片段跨窗分块时合并，避免同一消息被拆成两条气泡。
pub fn prepend_events(view: &mut SessionView, earlier: &[PassthroughEvent]) {
    if earlier.is_empty() {
        return;
    }
    let mut head = aggregate_events(earlier);
    // 跨窗分块：head 末尾与 view 开头的 AgentOutput 同属一条消息 → 合并
    let merge_tail = matches!(
        (head.dialog.last(), view.dialog.first()),
        (
            Some(DialogItem::AgentOutput { .. }),
            Some(DialogItem::AgentOutput { .. })
        )
    );
    if merge_tail {
        if let (
            Some(DialogItem::AgentOutput { content: tail, .. }),
            Some(DialogItem::AgentOutput { content: head0, .. }),
        ) = (head.dialog.last_mut(), view.dialog.first_mut())
        {
            tail.append(head0);
        }
        view.dialog.drain(0..1);
    }
    view.dialog.splice(0..0, head.dialog);
    view.activities.splice(0..0, head.activities);
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
    fn user(text: &str, ts: u64) -> PassthroughEvent {
        PassthroughEvent::UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
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

    /// 输出 chunk 按消息收敛为完整 agent 输出（acceptance (a)）。
    #[test]
    fn output_chunks_converge_into_one_message() {
        let view = aggregate_events(&[
            user("帮我改代码", 1),
            chunk("第一", 2),
            chunk("段输出", 3),
            chunk("，第二段", 4),
        ]);
        assert_eq!(view.dialog.len(), 2);
        match (&view.dialog[0], &view.dialog[1]) {
            (
                DialogItem::UserMessage { content, .. },
                DialogItem::AgentOutput { content: out, .. },
            ) => {
                assert_eq!(block_text(content), "帮我改代码");
                assert_eq!(block_text(out), "第一段输出，第二段");
            }
            other => panic!("应为用户消息 + 一条完整 agent 输出，得到 {other:?}"),
        }
    }

    /// 活动合并：连续 thinking 累积为一条、同工具调用合并、compaction 独立成条（acceptance (b)）。
    #[test]
    fn activities_merge_consecutive_kinds() {
        let mut view = SessionView::new();
        merge_event(&mut view, &think("思考", 1));
        merge_event(&mut view, &think("中…", 2));
        assert_eq!(view.activities.len(), 1, "连续 thinking 应合并为一条");
        match &view.activities[0] {
            Activity::Thinking { content, .. } => assert_eq!(content, "思考中…"),
            other => panic!("应为 Thinking，得到 {other:?}"),
        }
        // 同工具调用合并（title/content 补全）
        merge_event(&mut view, &tool("execute", Some("运行测试"), None, 3));
        merge_event(&mut view, &tool("execute", None, Some("cargo test"), 4));
        assert_eq!(view.activities.len(), 2);
        match &view.activities[1] {
            Activity::ToolCall {
                name,
                title,
                content,
                ..
            } => {
                assert_eq!(name, "execute");
                assert_eq!(title.as_deref(), Some("运行测试"));
                assert_eq!(content.as_deref(), Some("cargo test"));
            }
            other => panic!("应为合并后的 ToolCall，得到 {other:?}"),
        }
        // 不同工具 → 新条目；compaction 独立成条
        merge_event(&mut view, &tool("read", None, None, 5));
        merge_event(
            &mut view,
            &PassthroughEvent::Compaction {
                detail: "压缩".into(),
                timestamp: 6,
            },
        );
        assert_eq!(view.activities.len(), 4);
        assert!(matches!(view.activities[3], Activity::Compaction { .. }));
        // live_activity 取当前活动
        assert!(matches!(
            view.live_activity,
            Some(Activity::Compaction { .. })
        ));
    }

    /// busy/idle 派生：采信 session_info_update + turn 边界（acceptance (c)）。
    #[test]
    fn busy_idle_from_session_info_and_turn_boundaries() {
        let mut view = SessionView::new();
        assert!(!view.busy);
        // turn 开始 → busy
        merge_event(&mut view, &PassthroughEvent::TurnStarted { timestamp: 1 });
        assert!(view.busy);
        // agent 自报空闲（session_info_update 携带状态时采信）
        merge_event(
            &mut view,
            &PassthroughEvent::SessionInfo {
                state: Some(SessionState::Idle),
                timestamp: 2,
            },
        );
        assert!(!view.busy, "session_info_update 自报空闲应覆盖 turn 边界");
        // turn 结束 → idle，实时活动清空
        merge_event(&mut view, &PassthroughEvent::TurnStarted { timestamp: 3 });
        merge_event(&mut view, &think("x", 4));
        assert!(view.busy);
        merge_event(&mut view, &PassthroughEvent::TurnEnded { timestamp: 5 });
        assert!(!view.busy);
        assert!(view.live_activity.is_none(), "turn 结束应清空实时活动");
        // 历史活动保留
        assert_eq!(view.activities.len(), 1);
    }

    /// 前插更早一窗：对话与活动前插，跨窗同消息分块合并（惰性加载）。
    #[test]
    fn prepend_earlier_window_merges_split_message() {
        let mut view = aggregate_events(&[chunk("后半段", 20), think("t", 21)]);
        prepend_events(
            &mut view,
            &[user("更早消息", 1), chunk("前半", 2), think("早期思考", 1)],
        );
        assert_eq!(view.dialog.len(), 2);
        match (&view.dialog[0], &view.dialog[1]) {
            (DialogItem::UserMessage { .. }, DialogItem::AgentOutput { content, .. }) => {
                // 跨窗分块合并为一条完整输出
                assert_eq!(block_text(content), "前半后半段");
            }
            other => panic!("跨窗消息应合并为一条，得到 {other:?}"),
        }
        // 活动也前插
        assert_eq!(view.activities.len(), 2);
        assert_eq!(view.live_activity, view.activities.last().cloned());
    }

    fn block_text(content: &[ContentBlock]) -> String {
        content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}
