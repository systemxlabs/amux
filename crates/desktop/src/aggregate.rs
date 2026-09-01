//! 会话视图数据聚合：普通会话从
//! `session.history` / `session.activities` / `session.ongoing_activity` 拉取的数据
//! 聚合为对话气泡与活动列表。纯函数，与 GPUI/WS 分离，便于单测。

use protocol::{Activity, HistoryItem, SessionPlanEntry};

use crate::logic::{history_to_dialog, DialogMsg};

/// 普通会话视图：对话气泡 + 活动历史 + 实时活动 + agent 计划。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionView {
    pub dialog: Vec<DialogMsg>,
    pub activities: Vec<Activity>,
    pub history_has_more: bool,
    pub history_next_before: Option<usize>,
    pub activities_has_more: bool,
    pub activities_next_before: Option<usize>,
    /// 当前实时活动（进行中；空闲 None）。忙碌即 live.is_some()。
    pub live: Option<Activity>,
    /// agent 计划（`session.plan` 查询结果，全量替换）。
    pub plan: Vec<SessionPlanEntry>,
}

impl SessionView {
    /// 增量合并最新一窗：
    /// 新窗与已渲染内容的尾部按（类别，时间戳）序列对齐，只追加真正新增的条目——
    /// 原实现整页替换，既破坏增量语义，还会在下次 10s 轮询时冲掉用户
    /// 「加载更早」载入的旧消息。保留旧前缀时沿用旧分页游标。
    pub fn set_history_page(
        &mut self,
        items: &[HistoryItem],
        has_more: bool,
        next_before: Option<usize>,
    ) {
        let fresh = history_to_dialog(items);
        if self.dialog.is_empty() || fresh.is_empty() {
            self.dialog = fresh;
            self.history_has_more = has_more;
            self.history_next_before = next_before;
            return;
        }
        if let Some(kept_older) = merge_tail(&mut self.dialog, fresh, dialog_key) {
            if kept_older {
                return;
            }
        }
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
        head.append(&mut self.dialog);
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

    /// 增量合并最新一窗活动（同 set_history_page 的对齐策略）。
    pub fn set_activities_page(
        &mut self,
        activities: Vec<Activity>,
        has_more: bool,
        next_before: Option<usize>,
    ) {
        use std::mem;
        if self.activities.is_empty() || activities.is_empty() {
            self.activities = activities;
            self.activities_has_more = has_more;
            self.activities_next_before = next_before;
            return;
        }
        let mut fresh = activities;
        if let Some(kept_older) =
            merge_tail(&mut self.activities, mem::take(&mut fresh), activity_key)
        {
            if kept_older {
                return;
            }
        }
        self.activities_has_more = has_more;
        self.activities_next_before = next_before;
    }

    /// 前插更早一窗活动。
    pub fn prepend_activities(&mut self, earlier: Vec<Activity>) {
        if earlier.is_empty() {
            return;
        }
        let mut head = earlier;
        head.append(&mut self.activities);
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

    /// 设置实时活动；无（None）即回到空闲。
    pub fn set_live(&mut self, live: Option<Activity>) {
        self.live = live;
    }

    /// 设置 agent 计划（docs/DESIGN.md「普通会话计划」：Agent 侧数据为权威，全量覆盖）。
    pub fn set_plan(&mut self, entries: Vec<SessionPlanEntry>) {
        self.plan = entries;
    }
}

/// 对话条目的对齐键（类别 + 时间戳）。
fn dialog_key(m: &DialogMsg) -> (&'static str, u64) {
    match m {
        DialogMsg::UserMessage { timestamp, .. } => ("user", *timestamp),
        DialogMsg::AgentMessage { timestamp, .. } => ("agent", *timestamp),
    }
}

pub(crate) fn activity_key(a: &Activity) -> (&'static str, u64) {
    match a {
        Activity::Thinking { timestamp, .. } => ("thinking", *timestamp),
        Activity::ToolCall { timestamp, .. } => ("tool", *timestamp),
        Activity::Compaction { timestamp, .. } => ("compaction", *timestamp),
        Activity::Error { timestamp, .. } => ("error", *timestamp),
    }
}

/// 把 `fresh` 作为尾部合并进 `current`：
/// 在 fresh 中找到与 current 尾部键序列匹配的最长对齐点，
/// 其后的条目追加到 current。返回 Some(true) 表示 current 保留了
/// 更早的前缀（调用方应保留旧分页游标）；Some(false)/None 表示
/// current 未含更早内容（调用方采用新窗游标）。
fn merge_tail<T: Clone>(
    current: &mut Vec<T>,
    fresh: Vec<T>,
    key: impl Fn(&T) -> (&'static str, u64),
) -> Option<bool> {
    let cur_keys: Vec<_> = current.iter().map(&key).collect();
    let fresh_keys: Vec<_> = fresh.iter().map(&key).collect();
    let last = *cur_keys.last()?;
    // current 尾部键在 fresh 中最晚的出现位置（从后往前找第一处）
    let mut anchor = None;
    for (i, k) in fresh_keys.iter().enumerate().rev() {
        if *k == last {
            anchor = Some(i);
            break;
        }
    }
    let Some(anchor) = anchor else {
        // 完全无交集（异常情况）：保守整页替换
        *current = fresh;
        return Some(false);
    };
    // 从 anchor 向前验证对齐长度
    let mut matched = 0usize;
    while matched < cur_keys.len()
        && matched <= anchor
        && cur_keys[cur_keys.len() - 1 - matched] == fresh_keys[anchor - matched]
    {
        matched += 1;
    }
    let kept_older = matched < cur_keys.len();
    current.extend_from_slice(&fresh[anchor + 1..]);
    Some(kept_older)
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
    fn set_history_page_merges_incrementally() {
        let mut view = SessionView::default();
        view.set_history_page(&[user("问", 1), agent("答", 2)], false, None);

        view.set_history_page(
            &[user("问", 1), agent("答", 2), agent("补充", 3)],
            false,
            None,
        );
        assert_eq!(view.dialog.len(), 3, "增量追加而非整页替换");
        assert!(matches!(&view.dialog[0], DialogMsg::UserMessage { .. }));
    }

    #[test]
    fn set_history_page_preserves_older_window() {
        let mut view = SessionView::default();
        view.set_history_page(&[user("旧", 0)], true, Some(1));
        view.prepend_history(&[user("更早", 0)]);

        view.set_history_page(&[user("旧", 0), agent("新", 5)], true, Some(6));
        let d = &view.dialog;
        assert_eq!(d.len(), 3, "加载更早的内容应在轮询后保留");
        assert_eq!(
            (view.history_has_more, view.history_next_before),
            (true, Some(1)),
            "保留了更早前缀时应沿用旧游标"
        );
    }

    #[test]
    fn live_activity_marks_busy() {
        let mut view = SessionView::default();
        view.set_live(Some(Activity::Thinking {
            timestamp: 1,
            content: "x".into(),
        }));
        assert!(view.live.is_some());
    }
}
