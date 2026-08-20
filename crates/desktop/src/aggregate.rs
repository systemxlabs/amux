//! 会话视图数据聚合（docs/DESIGN.md「对话视图」「活动视图」）：普通会话从
//! `session.history` / `session.activities` / `session.ongoing_activity` 拉取的数据
//! 聚合为对话气泡与活动列表。纯函数，与 GPUI/WS 分离，便于单测。

use protocol::{Activity, HistoryItem};

use crate::logic::{history_to_dialog, DialogMsg};

/// 普通会话视图：对话气泡 + 活动历史 + 实时活动 + busy。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionView {
    pub dialog: Vec<DialogMsg>,
    pub activities: Vec<Activity>,
    pub history_has_more: bool,
    pub history_next_before: Option<usize>,
    pub activities_has_more: bool,
    pub activities_next_before: Option<usize>,
    /// 当前实时活动（进行中；空闲 None）。
    pub live: Option<Activity>,
    pub busy: bool,
}

impl SessionView {
    pub fn set_history_page(
        &mut self,
        items: &[HistoryItem],
        has_more: bool,
        next_before: Option<usize>,
    ) {
        self.dialog = history_to_dialog(items);
        self.history_has_more = has_more;
        self.history_next_before = next_before;
    }

    /// 前插更早一窗历史（惰性加载"更早消息"，保持时间正序）。
    pub fn prepend_history(&mut self, earlier: &[HistoryItem]) {
        if earlier.is_empty() {
            return;
        }
        let mut head = history_to_dialog(earlier);
        // 跨窗分块：head 末尾与当前开头同属一条 AgentMessage → 合并
        let merge_tail = matches!(
            (head.last(), self.dialog.first()),
            (
                Some(DialogMsg::AgentMessage { .. }),
                Some(DialogMsg::AgentMessage { .. })
            )
        );
        if merge_tail {
            if let (
                Some(DialogMsg::AgentMessage { content: tail, .. }),
                Some(DialogMsg::AgentMessage { content: head0, .. }),
            ) = (head.last_mut(), self.dialog.first_mut())
            {
                tail.append(head0);
            }
            self.dialog.remove(0);
        }
        head.extend(self.dialog.drain(..));
        self.dialog = head;
    }

    pub fn prepend_history_page(
        &mut self,
        earlier: &[HistoryItem],
        has_more: bool,
        next_before: Option<usize>,
    ) {
        self.prepend_history(earlier);
        self.history_has_more = has_more;
        self.history_next_before = next_before;
    }

    pub fn set_activities_page(
        &mut self,
        activities: Vec<Activity>,
        has_more: bool,
        next_before: Option<usize>,
    ) {
        self.activities = activities;
        self.activities_has_more = has_more;
        self.activities_next_before = next_before;
    }

    /// 前插更早一窗活动。
    pub fn prepend_activities(&mut self, earlier: Vec<Activity>) {
        if earlier.is_empty() {
            return;
        }
        let mut head = earlier;
        head.extend(self.activities.drain(..));
        self.activities = head;
    }

    pub fn prepend_activities_page(
        &mut self,
        earlier: Vec<Activity>,
        has_more: bool,
        next_before: Option<usize>,
    ) {
        self.prepend_activities(earlier);
        self.activities_has_more = has_more;
        self.activities_next_before = next_before;
    }

    /// 设置实时活动。有进行中活动即视为 busy；无（None）则回到空闲。
    pub fn set_live(&mut self, live: Option<Activity>) {
        self.busy = live.is_some();
        self.live = live;
    }

    /// 从会话元数据状态同步 busy。
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ContentBlock;

    fn user(text: &str, ts: u64) -> HistoryItem {
        HistoryItem::UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: ts,
        }
    }
    fn agent(text: &str, ts: u64) -> HistoryItem {
        HistoryItem::AgentMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: ts,
        }
    }

    #[test]
    fn set_history_replaces_dialog() {
        let mut view = SessionView::default();
        view.set_history_page(&[user("你好", 1), agent("回复", 2)], false, None);
        assert_eq!(view.dialog.len(), 2);
        // 刷新覆盖
        view.set_history_page(&[agent("新回复", 3)], false, None);
        assert_eq!(view.dialog.len(), 1);
        assert!(matches!(&view.dialog[0], DialogMsg::AgentMessage { .. }));
    }

    #[test]
    fn prepend_history_merges_split_agent_message() {
        let mut view = SessionView::default();
        view.set_history_page(&[agent("后半", 20)], false, None);
        view.prepend_history(&[user("更早", 1), agent("前半", 2)]);
        assert_eq!(view.dialog.len(), 2);
        assert!(matches!(&view.dialog[0], DialogMsg::UserMessage { .. }));
        match &view.dialog[1] {
            DialogMsg::AgentMessage { content, .. } => {
                assert_eq!(crate::text::block_text(content), "前半\n后半");
            }
            _ => panic!("跨窗同消息应合并"),
        }
    }

    #[test]
    fn activities_set_and_prepend() {
        let mut view = SessionView::default();
        view.set_activities_page(
            vec![Activity::Compaction {
                timestamp: 2,
                detail: "压缩".into(),
            }],
            false,
            None,
        );
        assert_eq!(view.activities.len(), 1);
        view.prepend_activities(vec![Activity::Error {
            timestamp: 1,
            detail: "早期错误".into(),
        }]);
        assert_eq!(view.activities.len(), 2);
        assert!(matches!(view.activities[0], Activity::Error { .. }));
    }

    #[test]
    fn live_activity_marks_busy() {
        let mut view = SessionView::default();
        view.set_live(Some(Activity::Thinking {
            timestamp: 1,
            content: "x".into(),
        }));
        assert!(view.busy);
        assert!(view.live.is_some());
    }
}
