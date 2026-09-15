//! 文本工具。

/// 按字符截断，超出部分以省略号结尾（日志与摘要用）。
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_marks_cut() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("中文测试", 2), "中文…");
    }
}
