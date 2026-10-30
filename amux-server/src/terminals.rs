//! 终端输出中转：Daemon 是终端元信息与输出缓冲的权威，Server 只维护
//! 「订阅 ID → SSE 连接」并转发输出（docs/DESIGN.md「终端存储」）。
//!
//! 订阅 ID 由 Server 生成，在发 `terminal.attach` 之前就注册，因此重放与实时输出
//! 都找得到归属；找不到订阅的输出说明订阅已失效，由调用方通知 Daemon 取消。

use std::collections::HashMap;

use amux_common::api::TerminalOutput;
use amux_common::domain::TerminalOutputNotification;
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// 单个 SSE 连接的中转队列上限：覆盖整段环形缓冲的重放（总数据量受 256 KiB 限制），
/// 落后时断开，由客户端携带游标重连并续传。
const CHANNEL_CAPACITY: usize = 4096;

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
}

impl TerminalRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册订阅并返回接收端。
    pub fn register(&self, subscription_id: &str, machine: &str) -> mpsc::Receiver<TerminalOutput> {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        self.inner.lock().subscriptions.insert(
            subscription_id.to_string(),
            Subscription {
                machine: machine.to_string(),
                sender,
            },
        );
        receiver
    }

    /// 转发终端输出；订阅不存在时返回 false，调用方应通知 Daemon 取消该订阅。
    pub fn forward(&self, notification: TerminalOutputNotification) -> bool {
        let output = TerminalOutput {
            data: notification.data.unwrap_or_default(),
            sequence: notification.sequence,
            truncated: notification.truncated,
            exited: notification.exit,
        };
        let mut inner = self.inner.lock();
        let Some(subscription) = inner.subscriptions.get(&notification.subscription_id) else {
            return false;
        };
        if subscription.sender.try_send(output).is_err() {
            // 接收端落后或已断开：移除订阅，SSE 随之结束，客户端重连后重新 attach。
            inner.subscriptions.remove(&notification.subscription_id);
        }
        true
    }

    pub fn unregister(&self, subscription_id: &str) {
        self.inner.lock().subscriptions.remove(subscription_id);
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
    async fn live_events_are_forwarded_after_register() {
        let relay = TerminalRelay::new();
        let mut receiver = relay.register("s1", "m1");
        assert!(relay.forward(notification("s1", 2, "live")));
        let output = receiver.recv().await.unwrap();
        assert_eq!(output.data, "live");
        assert_eq!(output.sequence, 2);
    }

    #[test]
    fn unknown_subscription_is_reported() {
        let relay = TerminalRelay::new();
        assert!(!relay.forward(notification("missing", 0, "x")));
    }

    #[test]
    fn clear_machine_drops_its_subscriptions() {
        let relay = TerminalRelay::new();
        let _receiver_1 = relay.register("s1", "m1");
        let _receiver_2 = relay.register("s2", "m2");
        relay.clear_machine("m1");
        assert!(!relay.forward(notification("s1", 0, "x")));
        assert!(relay.forward(notification("s2", 0, "y")));
    }
}
