//! 纯文本/字符串助手（共享，无 GUI/AmuxApp 依赖）。

use gpui::Pixels;

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

/// CJK 等全角字符判定（近似）：这些字符在等宽估算中占一个 em。
fn is_cjk_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// 聊天气泡宽度：按显示文本最长行的字符宽度估算（CJK 全角计 1em、其余计
/// 0.6em），夹在 [min, max]（像素）后返回。逐帧做真实文本整形代价高，此
/// 近似足以让短消息收拢贴内容、长消息触顶换行——markdown 块级内容本身会
/// 撑满可用宽度，不估算则气泡恒为上限宽。
pub fn estimate_bubble_width(text: &str, font_px: f32, min: f32, max: f32) -> Pixels {
    // 气泡 p_3 左右内边距 + 圆角内呼吸空间
    const PAD: f32 = 28.0;
    let widest_em = text
        .lines()
        .map(|line| {
            line.chars()
                .map(|c| if is_cjk_wide(c) { 1.0 } else { 0.6 })
                .sum::<f32>()
        })
        .fold(0.0_f32, f32::max);
    gpui::px((widest_em * font_px + PAD).clamp(min, max))
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

    #[test]
    fn estimate_bubble_width_honors_min_max_and_cjk() {
        let (min, max) = (120.0, 720.0);
        // 短消息：夹到下限
        assert_eq!(estimate_bubble_width("hi", 14.0, min, max), gpui::px(min));
        // 长消息：触到上限
        let long = "x".repeat(10_000);
        assert_eq!(estimate_bubble_width(&long, 14.0, min, max), gpui::px(max));
        // 多行取最长行；CJK 计全宽（4 字 ≈ 56px + 28 padding < 下限）
        assert_eq!(
            estimate_bubble_width("你好\n世界", 14.0, min, max),
            gpui::px(min)
        );
        // ASCII 0.6em：20 字符 ≈ 168 + 28 = 196（f32 累加有微差，容差比较）
        let ascii = "a".repeat(20);
        let width = estimate_bubble_width(&ascii, 14.0, min, max);
        assert!((width.as_f32() - 196.0).abs() < 0.01);
    }
}
