//! 时间戳：毫秒 Unix 时间（会话排序与消息/活动时间）。避免引入日期库。

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_monotonic_enough() {
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first);
        assert!(first > 1_600_000_000_000, "应为毫秒级时间戳: {first}");
    }
}
