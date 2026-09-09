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

/// 对话气泡中的图片附件：把 Resource 块的 base64 blob 解码为 gpui 可渲染图片。
/// 仅常见位图 mime 可解码（输入区附件只产生这些）；其余块忽略。
pub fn message_images(content: &[ContentBlock]) -> Vec<gpui::Image> {
    use base64::Engine;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Resource {
                mime_type,
                blob: Some(blob),
                ..
            } => {
                let format = match mime_type.as_str() {
                    "image/png" => gpui::ImageFormat::Png,
                    "image/jpeg" => gpui::ImageFormat::Jpeg,
                    "image/gif" => gpui::ImageFormat::Gif,
                    "image/webp" => gpui::ImageFormat::Webp,
                    "image/bmp" => gpui::ImageFormat::Bmp,
                    _ => return None,
                };
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(blob)
                    .ok()?;
                Some(gpui::Image::from_bytes(format, bytes))
            }
            _ => None,
        })
        .collect()
}

/// 折叠空白为单个空格（单行展示用）；超长截断由渲染层按可用宽度处理，
/// 这里不设字符上限，保证拉宽窗口能看到更多内容。
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// 精度档：秒级（带秒数）/ 分级（紧凑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimePrecision {
    /// `HH:MM:SS`（含秒）；跨天附 `MM-DD`。
    Seconds,
    /// `HH:MM`（紧凑，会话列表列宽有限）；跨天只显示日期。
    Compact,
}

/// 本地时区时间戳格式化为展示字符串。
/// `Seconds` 走秒级精度（消息/活动详情），`Compact` 走分级（会话列表行）。
/// 跨天：秒级附 `MM-DD`；紧凑只显示日期。
pub fn format_local_time(timestamp_ms: u64, precision: TimePrecision) -> String {
    let Ok(ts) = jiff::Timestamp::from_millisecond(timestamp_ms as i64) else {
        return String::new();
    };
    let ts = ts.to_zoned(jiff::tz::TimeZone::system());
    let now = jiff::Zoned::now();
    let same_day = ts.date() == now.date();
    match precision {
        TimePrecision::Seconds => {
            let time = ts.strftime("%H:%M:%S").to_string();
            if same_day {
                time
            } else {
                format!("{} {}", ts.strftime("%m-%d"), time)
            }
        }
        TimePrecision::Compact => {
            if same_day {
                ts.strftime("%H:%M").to_string()
            } else {
                ts.strftime("%m-%d").to_string()
            }
        }
    }
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
    fn message_images_decode_image_resources_only() {
        use base64::Engine;
        let png_blob = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        let blocks = vec![
            ContentBlock::Text {
                text: "看这张图".into(),
            },
            ContentBlock::Resource {
                mime_type: "image/png".into(),
                uri: None,
                text: None,
                blob: Some(png_blob.clone()),
            },
            // 非图片 mime 不产生图片
            ContentBlock::Resource {
                mime_type: "application/pdf".into(),
                uri: None,
                text: None,
                blob: Some(png_blob),
            },
            // blob 缺失不产生图片
            ContentBlock::Resource {
                mime_type: "image/jpeg".into(),
                uri: None,
                text: None,
                blob: None,
            },
        ];
        let images = message_images(&blocks);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].bytes, [1u8, 2, 3]);
        assert_eq!(images[0].format, gpui::ImageFormat::Png);
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("a  b\nc\td"), "a b c d");
        assert_eq!(one_line("abcdef"), "abcdef");
        // 不截断：超长内容完整保留，由渲染层按可用宽度截断
        let long = "很长的内容 ".repeat(100);
        assert!(one_line(&long).chars().count() > 500, "不应被字符上限截断");
        assert_eq!(one_line(""), "");
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
        // ASCII 0.6em：20 字符随宽度增长线性变宽，且在 [min, max] 内（不触界）。
        // 用区间断言而非字面像素值，避免与 0.6em 估算常量强耦合。
        let ascii = "a".repeat(20);
        let width = estimate_bubble_width(&ascii, 14.0, min, max);
        let px = width.as_f32();
        assert!(px > min && px < max, "ASCII 中等长度应落在区间内: {px}");
        // 更长文本应更宽（单调递增）
        let longer = "a".repeat(40);
        let w2 = estimate_bubble_width(&longer, 14.0, min, max).as_f32();
        assert!(w2 > px, "更长文本应更宽: {w2} <= {px}");
    }

    #[test]
    fn format_local_time_invalid_timestamp_returns_empty() {
        // 超出 jiff 可表示范围的时间戳不应 panic，返回空串。
        assert_eq!(
            format_local_time(i64::MAX as u64, TimePrecision::Seconds),
            String::new()
        );
        assert_eq!(
            format_local_time(i64::MAX as u64, TimePrecision::Compact),
            String::new()
        );
    }
}
