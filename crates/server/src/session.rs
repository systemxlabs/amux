//! 会话管理（docs/DESIGN.md §6）：会话注册表、turn 事件聚合（非流式交付 +
//! activities 有界缓存）、通知广播、prompt 串行化。
//! 历史权威在 agent 侧；server 不保存对话历史，仅维护会话元数据与 activities 缓存。

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, Mutex};

use protocol::{Activity, ContentBlock, DialogItem, SessionMeta, SessionState, TurnCompleted};

use crate::agent::{AgentEvent, DialogRecord, SharedDriver};

/// server → GUI 通知（docs/DESIGN.md §4/§5）。
#[derive(Debug, Clone)]
pub enum ServerNotification {
    SessionCreated(SessionMeta),
    SessionClosed(SessionMeta),
    /// 崩溃恢复标记（重启后忙状态会话标 interrupted，docs/DESIGN.md §3）
    #[allow(dead_code)]
    SessionInterrupted(SessionMeta),
    SessionDeleted(SessionMeta),
    TurnCompleted(TurnCompleted),
    SessionState {
        session_id: String,
        state: SessionState,
    },
    UserMessage {
        session_id: String,
        content: Vec<ContentBlock>,
        timestamp: u64,
    },
}

struct SessionRecord {
    meta: SessionMeta,
    agent_session_id: String,
}

pub struct SessionManager {
    driver: SharedDriver,
    registry: Mutex<HashMap<String, SessionRecord>>,
    /// 按会话的 activities 有界缓存（docs/DESIGN.md §5.3）
    activities: Mutex<HashMap<String, VecDeque<Activity>>>,
    tx: broadcast::Sender<ServerNotification>,
    max_activities: usize,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionManager {
    pub fn new(
        driver: SharedDriver,
        max_activities: usize,
    ) -> (Self, broadcast::Receiver<ServerNotification>) {
        let (tx, rx) = broadcast::channel(256);
        let manager = SessionManager {
            driver,
            registry: Mutex::new(HashMap::new()),
            activities: Mutex::new(HashMap::new()),
            tx,
            max_activities,
        };
        (manager, rx)
    }

    // ---- 生命周期 ----
    pub async fn create(
        &self,
        harness: &str,
        cwd: &str,
        model: Option<&str>,
    ) -> Result<SessionMeta, String> {
        let agent_session_id = self.driver.create_session(cwd, model)?;
        let id = format!("s_{}", uuid_v4());
        let meta = SessionMeta {
            id,
            harness: harness.to_string(),
            cwd: cwd.to_string(),
            model: model.map(str::to_string),
            state: SessionState::Idle,
            interrupted: false,
            closed: false,
            created_at: now(),
            last_event_at: now(),
        };
        self.registry.lock().await.insert(
            meta.id.clone(),
            SessionRecord {
                meta: meta.clone(),
                agent_session_id,
            },
        );
        let _ = self
            .tx
            .send(ServerNotification::SessionCreated(meta.clone()));
        Ok(meta)
    }

    pub async fn resume(&self, session_id: &str) -> Result<SessionMeta, String> {
        let mut reg = self.registry.lock().await;
        let rec = reg
            .get_mut(session_id)
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        self.driver.resume_session(&rec.agent_session_id)?;
        rec.meta.interrupted = false;
        rec.meta.closed = false;
        rec.meta.state = SessionState::Idle;
        let meta = rec.meta.clone();
        let _ = self
            .tx
            .send(ServerNotification::SessionCreated(meta.clone()));
        Ok(meta)
    }

    pub async fn close(&self, session_id: &str) -> Result<(), String> {
        let mut reg = self.registry.lock().await;
        let rec = reg
            .get_mut(session_id)
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        self.driver.close(&rec.agent_session_id)?;
        rec.meta.closed = true;
        let meta = rec.meta.clone();
        let _ = self.tx.send(ServerNotification::SessionClosed(meta));
        Ok(())
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), String> {
        // 先取并移除注册表条目（释放锁），再删 activities——统一锁序避免死锁
        let rec = {
            let mut reg = self.registry.lock().await;
            reg.remove(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?
        };
        self.driver.delete(&rec.agent_session_id)?;
        self.activities.lock().await.remove(session_id);
        let mut meta = rec.meta;
        meta.closed = true;
        let _ = self.tx.send(ServerNotification::SessionDeleted(meta));
        Ok(())
    }

    pub async fn list(&self) -> Vec<SessionMeta> {
        let mut metas: Vec<SessionMeta> = self
            .registry
            .lock()
            .await
            .values()
            .map(|r| r.meta.clone())
            .collect();
        metas.sort_by_key(|m| std::cmp::Reverse(m.created_at));
        metas
    }

    // ---- 会话数据（docs/DESIGN.md §5）----

    /// 打开会话：经 driver 的 `session/load` 全量重放，聚合对话内容返回。
    pub async fn open(&self, session_id: &str) -> Result<Vec<DialogItem>, String> {
        let reg = self.registry.lock().await;
        let rec = reg
            .get(session_id)
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let records = self.driver.load_session(&rec.agent_session_id)?;
        Ok(records
            .into_iter()
            .map(|r| match r {
                DialogRecord::UserMessage(c) => DialogItem::UserMessage {
                    content: c,
                    timestamp: now(),
                },
                DialogRecord::AgentOutput(c) => DialogItem::AgentOutput {
                    content: c,
                    timestamp: now(),
                },
            })
            .collect())
    }

    pub async fn get_activities(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Activity>, String> {
        // 先检查会话存在（释放锁），再取 activities——统一锁序避免死锁
        {
            let reg = self.registry.lock().await;
            if !reg.contains_key(session_id) {
                return Err(format!("会话不存在: {session_id}"));
            }
        }
        let acts = self.activities.lock().await;
        let list = acts.get(session_id).cloned().unwrap_or_default();
        let list: Vec<Activity> = match limit {
            Some(n) if list.len() > n => list.iter().skip(list.len() - n).cloned().collect(),
            _ => list.into_iter().collect(),
        };
        Ok(list)
    }

    // ---- 交互 ----

    /// prompt：经 driver 触发 turn，聚合事件为完整输出 + activities（非流式交付）。
    /// 忙时 prompt（steer）依赖 agent 实现；当前 agent 不支持进行中注入时直接报错
    /// （docs/DESIGN.md §9：不排队、不静默降级）。
    pub async fn prompt(&self, session_id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        let agent_session_id = {
            let reg = self.registry.lock().await;
            let rec = reg
                .get(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            if rec.meta.state == SessionState::Busy {
                return Err("会话忙：agent 不支持进行中注入（steer），请等待当前工作结束".into());
            }
            rec.agent_session_id.clone()
        };

        // 用户消息通知 + 忙状态
        let _ = self.tx.send(ServerNotification::UserMessage {
            session_id: session_id.to_string(),
            content: input.clone(),
            timestamp: now(),
        });
        self.set_state(session_id, SessionState::Busy).await;

        // 聚合 turn 事件
        let mut rx = self.driver.prompt(&agent_session_id, input);
        let mut output: Vec<ContentBlock> = Vec::new();
        let mut acts: Vec<Activity> = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::OutputChunk(s) => output.push(ContentBlock::Text { text: s }),
                AgentEvent::UserMessage(_) => {
                    // 回显：server 已发 user_message 通知，此处忽略
                }
                AgentEvent::Thinking(c) => acts.push(Activity::Thinking {
                    timestamp: now(),
                    content: c,
                }),
                AgentEvent::ToolCall {
                    name,
                    title,
                    content,
                } => {
                    acts.push(Activity::ToolCall {
                        timestamp: now(),
                        name,
                        title,
                        content,
                    });
                }
                AgentEvent::Compaction(d) => acts.push(Activity::Compaction {
                    timestamp: now(),
                    detail: d,
                }),
                AgentEvent::TurnEnded => break,
            }
        }

        // 写入 activities 有界缓存
        if !acts.is_empty() {
            let mut map = self.activities.lock().await;
            let list = map
                .entry(session_id.to_string())
                .or_insert_with(VecDeque::new);
            for a in acts {
                if list.len() >= self.max_activities {
                    list.pop_front();
                }
                list.push_back(a);
            }
        }

        // 非流式交付：完整输出 + 空闲状态
        if !output.is_empty() {
            let _ = self
                .tx
                .send(ServerNotification::TurnCompleted(TurnCompleted {
                    session_id: session_id.to_string(),
                    output,
                    timestamp: now(),
                }));
        }
        self.set_state(session_id, SessionState::Idle).await;
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let agent_session_id = {
            let reg = self.registry.lock().await;
            reg.get(session_id)
                .map(|r| r.agent_session_id.clone())
                .ok_or_else(|| format!("会话不存在: {session_id}"))?
        };
        self.driver.cancel(&agent_session_id)
    }

    async fn set_state(&self, session_id: &str, state: SessionState) {
        {
            let mut reg = self.registry.lock().await;
            if let Some(rec) = reg.get_mut(session_id) {
                rec.meta.state = state;
                rec.meta.last_event_at = now();
            }
        }
        let _ = self.tx.send(ServerNotification::SessionState {
            session_id: session_id.to_string(),
            state,
        });
    }
}

/// 简易唯一 id（s_ 前缀；不引额外依赖）。
fn uuid_v4() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:016x}{:08x}", nanos, std::process::id() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::StubAgentDriver;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    #[tokio::test]
    async fn prompt_aggregates_output_and_activities() {
        let driver: SharedDriver = Arc::new(StubAgentDriver::new());
        let (mgr, mut rx) = SessionManager::new(driver, 100);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();

        mgr.prompt(&meta.id, text("hi")).await.unwrap();

        // 非流式交付：turn_completed 带完整输出（docs/DESIGN.md §5.1）
        let mut saw_output = false;
        while let Ok(n) = rx.recv().await {
            if let ServerNotification::TurnCompleted(t) = n {
                assert!(t.output.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains("模拟输出"))
                ));
                saw_output = true;
                break;
            }
        }
        assert!(saw_output, "应收到 turn_completed 通知");

        // activities 缓存：thinking + tool_call
        let acts = mgr.get_activities(&meta.id, None).await.unwrap();
        assert!(acts.iter().any(|a| matches!(a, Activity::Thinking { .. })));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "read_file")));

        // 会话回到空闲
        let state = mgr.list().await[0].state;
        assert_eq!(state, SessionState::Idle);
    }

    #[tokio::test]
    async fn activities_cache_is_bounded() {
        // 缓存上限 1：stub 每次 turn 产生 thinking + tool_call 两条，只保留最后一条
        let driver: SharedDriver = Arc::new(StubAgentDriver::new());
        let (mgr, _rx) = SessionManager::new(driver, 1);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        mgr.prompt(&meta.id, text("x")).await.unwrap();
        let acts = mgr.get_activities(&meta.id, None).await.unwrap();
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], Activity::ToolCall { .. }));
    }

    #[tokio::test]
    async fn open_session_returns_dialog_content() {
        let driver: SharedDriver = Arc::new(StubAgentDriver::new());
        let (mgr, _rx) = SessionManager::new(driver, 100);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        let items = mgr.open(&meta.id).await.unwrap();
        assert!(items.is_empty()); // stub 无持久化历史（历史权威在 agent，docs/DESIGN.md §5.2）
    }

    #[tokio::test]
    async fn create_and_delete_lifecycle() {
        let driver: SharedDriver = Arc::new(StubAgentDriver::new());
        let (mgr, mut rx) = SessionManager::new(driver, 100);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        assert_eq!(mgr.list().await.len(), 1);

        mgr.delete(&meta.id).await.unwrap();
        assert!(mgr.list().await.is_empty());
        assert!(mgr.get_activities(&meta.id, None).await.is_err());

        // 删除广播 session_deleted
        let mut saw_deleted = false;
        while let Ok(n) = rx.recv().await {
            if let ServerNotification::SessionDeleted(s) = n {
                assert_eq!(s.id, meta.id);
                saw_deleted = true;
                break;
            }
        }
        assert!(saw_deleted);
    }

    #[tokio::test]
    async fn missing_session_errors() {
        let driver: SharedDriver = Arc::new(StubAgentDriver::new());
        let (mgr, _rx) = SessionManager::new(driver, 100);
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.open("nope").await.is_err());
        assert!(mgr.delete("nope").await.is_err());
    }
}
