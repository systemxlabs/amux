//! 出站帧缓存（断线重连用）。
//!
//! 与 Server 的连接断开期间，Agent 输出与终端输出仍然产生；这些帧进入本缓存，
//! 重连后按序补发。缓存有上限，超限丢弃最早的帧（docs/DESIGN.md「断线重连」）。

use std::collections::VecDeque;

use parking_lot::Mutex;
use tokio::sync::Notify;

/// 缓存上限（帧数）。按 ACP 单帧数百字节估算，上限量级在数 MB。
const DEFAULT_CAPACITY: usize = 8192;

/// 单帧序号：用于「发送成功后再移除」，避免发送中途断线丢帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seq(u64);

pub struct Outbox {
    inner: Mutex<Inner>,
    notify: Notify,
    capacity: usize,
}

struct Inner {
    frames: VecDeque<(Seq, String)>,
    next_seq: u64,
}

impl Outbox {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                frames: VecDeque::new(),
                next_seq: 0,
            }),
            notify: Notify::new(),
            capacity,
        }
    }

    /// 追加一帧；超出上限时丢弃最早的帧。
    pub fn push(&self, frame: String) {
        {
            let mut inner = self.inner.lock();
            let seq = Seq(inner.next_seq);
            inner.next_seq += 1;
            inner.frames.push_back((seq, frame));
            while inner.frames.len() > self.capacity {
                inner.frames.pop_front();
            }
        }
        self.notify.notify_one();
    }

    /// 取队首帧（不移除）；无帧时等待。
    pub async fn peek(&self) -> (Seq, String) {
        loop {
            if let Some((seq, frame)) = self.inner.lock().frames.front() {
                return (*seq, frame.clone());
            }
            self.notify.notified().await;
        }
    }

    /// 发送成功后移除指定帧。
    pub fn ack(&self, seq: Seq) {
        let mut inner = self.inner.lock();
        if inner.frames.front().map(|(front, _)| *front == seq) == Some(true) {
            inner.frames.pop_front();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().frames.len()
    }
}

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_oldest_frame_when_full() {
        let outbox = Outbox::with_capacity(2);
        outbox.push("a".into());
        outbox.push("b".into());
        outbox.push("c".into());
        assert_eq!(outbox.len(), 2);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(outbox.peek().await.1, "b");
            let (seq, _) = outbox.peek().await;
            outbox.ack(seq);
            assert_eq!(outbox.peek().await.1, "c");
        });
    }

    #[tokio::test]
    async fn ack_mismatch_keeps_frame() {
        let outbox = Outbox::new();
        outbox.push("only".into());
        let (seq, _) = outbox.peek().await;
        outbox.ack(Seq(seq.0 + 1));
        assert_eq!(outbox.len(), 1);
    }
}
