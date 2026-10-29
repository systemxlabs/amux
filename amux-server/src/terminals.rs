//! 终端输出中转：Daemon 是终端元信息与输出缓冲的权威，Server 只维护
//! 「订阅 ID → SSE 连接」并转发输出（docs/DESIGN.md「终端存储」）。
//!
//! Daemon 对每个订阅独立推送；attach 响应到达前可能已收到该订阅的输出，
//! 先暂存，注册时补发。

use std::collections::HashMap;

use amux_common::api::TerminalOutput;
use amux_common::domain::TerminalOutputNotification;
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// 单个 SSE 连接的中转队列上限；落后时断开，由客户端重连并重新 attach。
const CHANNEL_CAPACITY: usize = 256;
/// attach 响应到达前暂存的事件上限。
const PENDING_LIMIT: usize = 4096;

struct Subscription {
    machine: String,
    sender: mpsc::Sender<TerminalOutput>,
}

#[derive(Default)]
pub struct TerminalRelay {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    subscriptions: HashMap<String, Subscription>,
    pending: HashMap<String, Vec<TerminalOutput>>,
}

impl TerminalRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Daemon 推送终端输出：按订阅转发；订阅尚未注册时暂存。
    pub fn forward(&self, notification: TerminalOutputNotification) {
        let output = TerminalOutput {
            data: notification.data.unwrap_or_default(),
            next_cursor: notification.sequence,
            truncated: notification.truncated,
            exited: notification.exit,
        };
        let mut inner = self.inner.lock();
        if let Some(subscription) = inner.subscriptions.get(&notification.subscription_id) {
            if subscription.sender.try_send(output).is_err() {
                // 接收端落后或已断开：移除订阅，SSE 随之结束，客户端重连后重新 attach。
                inner.subscriptions.remove(&notification.subscription_id);
            }
            return;
        }
        let pending = inner
            .pending
            .entry(notification.subscription_id)
            .or_default();
        if pending.len() < PENDING_LIMIT {
            pending.push(output);
        }
    }

    /// 注册订阅，返回注册前已收到的输出与实时接收端。
    pub fn register(
        &self,
        subscription_id: &str,
        machine: &str,
    ) -> (Vec<TerminalOutput>, mpsc::Receiver<TerminalOutput>) {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let mut inner = self.inner.lock();
        let pending = inner.pending.remove(subscription_id).unwrap_or_default();
        inner.subscriptions.insert(
            subscription_id.to_string(),
            Subscription {
                machine: machine.to_string(),
                sender,
            },
        );
        (pending, receiver)
    }

    pub fn unregister(&self, subscription_id: &str) {
        self.inner.lock().subscriptions.remove(subscription_id);
        self.inner.lock().pending.remove(subscription_id);
    }

    /// Daemon 断线：结束该机器的全部 SSE 订阅。
    pub fn clear_machine(&self, machine: &str) {
        self.inner
            .lock()
            .subscriptions
            .retain(|_, subscription| subscription.machine != machine);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: &str, sequence: u64, data: &str) -> TerminalOutputNotification {
        TerminalOutputNotification {
            subscription_id: id.into(),
            sequence,
            data: Some(data.into()),
            exit: false,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn pending_events_are_delivered_on_register() {
        let relay = TerminalRelay::new();
        relay.forward(notification("s1", 0, "hello"));
        relay.forward(notification("s1", 1, " world"));

        let (pending, mut receiver) = relay.register("s1", "m1");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].next_cursor, 0);
        assert_eq!(pending[1].next_cursor, 1);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn live_events_are_forwarded_after_register() {
        let relay = TerminalRelay::new();
        let (_, mut receiver) = relay.register("s1", "m1");
        relay.forward(notification("s1", 2, "live"));
        let output = receiver.recv().await.unwrap();
        assert_eq!(output.data, "live");
        assert_eq!(output.next_cursor, 2);
    }

    #[test]
    fn clear_machine_drops_its_subscriptions() {
        let relay = TerminalRelay::new();
        let _receiver_1 = relay.register("s1", "m1").1;
        let _receiver_2 = relay.register("s2", "m2").1;
        relay.clear_machine("m1");
        relay.forward(notification("s1", 0, "x"));
        relay.forward(notification("s2", 0, "y"));
        let inner = relay.inner.lock();
        assert!(inner.pending.contains_key("s1"));
        assert!(!inner.pending.contains_key("s2"));
    }
}
