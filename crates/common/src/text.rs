//! 纯文本/字符串助手（无协议/IO 依赖）。

/// 截断长文本：超过 `max` 个字符时保留前 `max` 个字符并追加省略号 `…`。
pub fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count > max {
        let prefix: String = s.chars().take(max).collect();
        format!("{prefix}…")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_exact_and_overflow() {
        assert_eq!(truncate("abcd", 4), "abcd", "恰好等于上限不加省略号");
        assert_eq!(truncate("abcd", 2), "ab…", "超限加省略号");
        assert_eq!(truncate("", 2), "");
        // 按字符（而非字节）截断
        assert_eq!(truncate("你好世界", 2), "你好…");
    }
}
