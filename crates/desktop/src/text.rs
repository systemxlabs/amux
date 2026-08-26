//! 纯文本/字符串助手（共享，无 GUI/AmuxApp 依赖）。

use protocol::ContentBlock;

/// 把 `ContentBlock` 列表中的文本块拼接为字符串；非文本块忽略。
/// 多个文本块用换行分隔（对话气泡渲染场景）。
pub fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 折叠空白为单个空格后再截断（单行展示用）。
pub fn one_line(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    amux_common::text::truncate(&collapsed, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_text_joins_text_blocks_and_ignores_others() {
        let blocks = vec![
            ContentBlock::Text { text: "a".into() },
            ContentBlock::Text { text: "b".into() },
        ];
        assert_eq!(block_text(&blocks), "a\nb");

        let mixed = vec![
            ContentBlock::Text { text: "x".into() },
            ContentBlock::Resource {
                mime_type: "image/png".into(),
                uri: None,
                text: None,
                blob: None,
            },
        ];
        assert_eq!(block_text(&mixed), "x");
    }

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("a  b\nc\td", 100), "a b c d");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("", 3), "");
    }
}
