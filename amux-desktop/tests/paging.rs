//! 列表滚动分页的合并契约（docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」
//! 「活动列表滚动机制」）：窗口贴着最新一端，刷新把最新一页并入窗口，
//! 随滚动向更早方向按页扩展。测试条目为（标识, 内容），标识即条目身份。

use amux_desktop::poll::{merge_newest, prepend_older};
use amux_desktop::state::Paging;

fn id<'a>(item: &'a (&'a str, &'a str)) -> &'a str {
    item.0
}

fn item(id: &'static str, content: &'static str) -> (&'static str, &'static str) {
    (id, content)
}

/// 空窗口：最新一页直接作为窗口。
#[test]
fn first_page_fills_empty_window() {
    let mut items = Vec::new();
    let mut paging = Paging::default();

    merge_newest(
        &mut items,
        &mut paging,
        vec![item("a", "1"), item("b", "1")],
        true,
        id,
    );

    assert_eq!(items, vec![item("a", "1"), item("b", "1")]);
    assert!(paging.has_older);
}

/// 刷新：页内已有的条目取新内容（流式输出会改内容），新条目追加在最末。
#[test]
fn refresh_updates_existing_entries_and_appends_newer() {
    let mut items = vec![item("a", "1"), item("b", "1")];
    let mut paging = Paging::default();

    merge_newest(
        &mut items,
        &mut paging,
        vec![item("b", "2"), item("c", "1")],
        false,
        id,
    );

    assert_eq!(
        items,
        vec![item("a", "1"), item("b", "2"), item("c", "1")],
        "b 就地更新到原位置，c 追加在末尾"
    );
    assert!(!paging.has_older);
}

/// 条目移到最新端（服务端按更新时间排序）时只保留最新位置的一份，不重复。
#[test]
fn refresh_keeps_moved_entry_once_at_new_position() {
    let mut items = vec![item("a", "1"), item("b", "1")];
    let mut paging = Paging::default();

    merge_newest(
        &mut items,
        &mut paging,
        vec![item("b", "1"), item("a", "2")],
        false,
        id,
    );

    assert_eq!(items, vec![item("b", "1"), item("a", "2")]);
}

/// 页比窗口短时（两次刷新之间没有新增），窗口保持连续、不产生重复条目。
#[test]
fn shorter_page_keeps_window_contiguous() {
    let mut items = vec![item("a", "1"), item("b", "1"), item("c", "1")];
    let mut paging = Paging::default();

    merge_newest(&mut items, &mut paging, vec![item("c", "1")], true, id);

    assert_eq!(items, vec![item("a", "1"), item("b", "1"), item("c", "1")]);
}

/// 更早一页插入后记下位移量，视图据此把原首条目保持在原位置。
#[test]
fn prepending_records_shift() {
    let mut items = vec![item("c", "1"), item("d", "1")];
    let mut paging = Paging::default();

    prepend_older(
        &mut items,
        &mut paging,
        vec![item("a", "1"), item("b", "1")],
    );

    assert_eq!(
        items,
        vec![
            item("a", "1"),
            item("b", "1"),
            item("c", "1"),
            item("d", "1")
        ]
    );
    assert_eq!(paging.shift, Some(2), "位移量为插入的条目数");
}
