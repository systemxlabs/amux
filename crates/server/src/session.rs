//! 会话管理（docs/DESIGN.md §6）：会话注册表、事件透传（§5.1 server 不聚合，
//! GUI 应用负责收敛/合并/派生）、通知广播、prompt 串行化。
//! 历史权威在 agent 侧；server 不保存对话历史，仅维护会话元数据（含列表状态）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, Mutex};

use protocol::{
    generate_title, ContentBlock, HarnessInfo, PassthroughEvent, SessionMeta, SessionState,
};

use crate::agent::{passthrough_event, AgentEvent, AgentRegistry, SharedDriver};

/// server → GUI 通知（docs/DESIGN.md §4/§5）。
#[derive(Debug, Clone)]
pub enum ServerNotification {
    SessionCreated(SessionMeta),
    /// 崩溃恢复标记（重启后忙状态会话标 interrupted，docs/DESIGN.md §3）
    #[allow(dead_code)]
    SessionInterrupted(SessionMeta),
    SessionDeleted(SessionMeta),
    /// 会话元数据更新（标题修改等，多 GUI 同步）
    SessionUpdated(SessionMeta),
    /// 透传事件（session/update 事件、session_info_update、turn 边界；GUI 聚合，§5.1）
    Passthrough {
        session_id: String,
        event: PassthroughEvent,
    },
}

struct SessionRecord {
    meta: SessionMeta,
    agent_session_id: String,
    driver: SharedDriver,
}

pub struct SessionManager {
    agents: Arc<AgentRegistry>,
    registry: Mutex<HashMap<String, SessionRecord>>,
    tx: broadcast::Sender<ServerNotification>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionManager {
    pub fn new(agents: Arc<AgentRegistry>) -> (Self, broadcast::Receiver<ServerNotification>) {
        let (tx, rx) = broadcast::channel(256);
        let manager = SessionManager {
            agents,
            registry: Mutex::new(HashMap::new()),
            tx,
        };
        (manager, rx)
    }

    // ---- 机器信息 ----
    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        self.agents.harnesses()
    }

    pub fn set_default_model(&self, harness: &str, model: Option<String>) {
        self.agents.set_default_model(harness, model);
    }

    pub fn list_agent_skills(&self, harness: &str) -> Vec<String> {
        match self.agents.driver_for(harness) {
            Ok(d) => d.list_skills(),
            Err(_) => Vec::new(),
        }
    }

    // ---- 生命周期 ----
    pub async fn create(
        &self,
        harness: &str,
        cwd: &str,
        model: Option<&str>,
    ) -> Result<SessionMeta, String> {
        let driver = self.agents.driver_for(harness)?;
        let agent_session_id = driver.create_session(cwd, model)?;
        let id = format!("s_{}", uuid::Uuid::new_v4());
        protocol::log::info(
            "server.session",
            format!("创建会话 {id}（harness={harness} cwd={cwd} agent={agent_session_id}）"),
        );
        let meta = SessionMeta {
            id,
            harness: harness.to_string(),
            cwd: cwd.to_string(),
            model: model.map(str::to_string),
            state: SessionState::Idle,
            interrupted: false,
            title: String::new(),
            created_at: now(),
            last_event_at: now(),
        };
        self.registry.lock().await.insert(
            meta.id.clone(),
            SessionRecord {
                meta: meta.clone(),
                agent_session_id,
                driver,
            },
        );
        let _ = self
            .tx
            .send(ServerNotification::SessionCreated(meta.clone()));
        Ok(meta)
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), String> {
        // 先取并移除注册表条目（释放锁），再删 activities——统一锁序避免死锁
        let rec = {
            let mut reg = self.registry.lock().await;
            reg.remove(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?
        };
        rec.driver.delete(&rec.agent_session_id)?;
        let meta = rec.meta;
        protocol::log::info("server.session", format!("删除会话 {session_id}"));
        let _ = self.tx.send(ServerNotification::SessionDeleted(meta));
        Ok(())
    }

    /// 修改会话标题（用户可随时修改，PRD §3.1）；广播 session_updated 同步各 GUI。
    pub async fn set_session_title(&self, session_id: &str, title: &str) -> Result<(), String> {
        let meta = {
            let mut reg = self.registry.lock().await;
            let rec = reg
                .get_mut(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            rec.meta.title = title.trim().to_string();
            rec.meta.last_event_at = now();
            rec.meta.clone()
        };
        let _ = self.tx.send(ServerNotification::SessionUpdated(meta));
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

    /// 打开会话：经 driver 的 `session/load` 全量重放，返回**透传事件**（GUI 应用聚合，
    /// docs/DESIGN.md §5.1/§5.2）。惰性加载：默认返回最新一窗（`limit` 条），
    /// `before` 为独占上界游标向上取更早历史。
    pub async fn open(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<usize>,
    ) -> Result<(Vec<PassthroughEvent>, bool, usize), String> {
        let (driver, agent_session_id) = {
            let reg = self.registry.lock().await;
            let rec = reg
                .get(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            (rec.driver.clone(), rec.agent_session_id.clone())
        };
        let events = driver.load_session(&agent_session_id)?;
        let limit = limit.unwrap_or(200);
        let (start, end, has_more) = Self::window_items(events.len(), limit, before);
        let slice = events[start..end].to_vec();
        Ok((slice, has_more, start))
    }

    /// 惰性加载切窗（纯函数）：按 limit 与 before 游标计算 [start, end) 与是否还有更早。
    pub fn window_items(len: usize, limit: usize, before: Option<usize>) -> (usize, usize, bool) {
        let end = before.unwrap_or(len).min(len);
        let start = end.saturating_sub(limit);
        (start, end, start > 0)
    }

    // ---- 交互 ----

    /// prompt：经 driver 触发 turn，聚合事件为完整输出 + activities（非流式交付）。
    /// 忙时 prompt（steer）依赖 agent 实现；当前 agent 不支持进行中注入时直接报错
    /// （docs/DESIGN.md §9：不排队、不静默降级）。
    /// 首条 prompt 时为会话生成默认标题（PRD §3.1：由首条指令/目标自动生成简短摘要）。
    pub async fn prompt(&self, session_id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        // 忙检查 + Busy 状态在同一次加锁内完成（原子），避免并发 prompt 竞态
        let (driver, agent_session_id, title_changed) = {
            let mut reg = self.registry.lock().await;
            let rec = reg
                .get_mut(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            if rec.meta.state == SessionState::Busy {
                return Err("会话忙：agent 不支持进行中注入（steer），请等待当前工作结束".into());
            }
            let title_changed = if rec.meta.title.is_empty() {
                let t = first_text(&input);
                rec.meta.title = generate_title(&t);
                true
            } else {
                false
            };
            rec.meta.state = SessionState::Busy;
            rec.meta.last_event_at = now();
            (
                rec.driver.clone(),
                rec.agent_session_id.clone(),
                title_changed,
            )
        };
        let summary: String = first_text(&input).chars().take(60).collect::<String>()
            + if first_text(&input).chars().count() > 60 {
                "…"
            } else {
                ""
            };
        let started = std::time::Instant::now();
        protocol::log::info(
            "server.session",
            format!("prompt 开始 {session_id}（agent={agent_session_id}）：{summary}"),
        );
        let meta = if title_changed {
            self.registry
                .lock()
                .await
                .get(session_id)
                .map(|r| r.meta.clone())
        } else {
            None
        };
        if let Some(m) = meta {
            let _ = self.tx.send(ServerNotification::SessionUpdated(m));
        }

        // turn 开始（透传边界）+ 用户消息回显 + 列表状态标记（不推送状态通知，
        // busy/idle 由 GUI 应用从透传事件派生，docs/DESIGN.md §5.1）
        let ts = now();
        let _ = self.tx.send(ServerNotification::Passthrough {
            session_id: session_id.to_string(),
            event: PassthroughEvent::TurnStarted { timestamp: ts },
        });
        let _ = self.tx.send(ServerNotification::Passthrough {
            session_id: session_id.to_string(),
            event: PassthroughEvent::UserMessage {
                content: input.clone(),
                timestamp: ts,
            },
        });
        self.update_state(session_id, SessionState::Busy).await;

        // 事件逐条透传（不聚合输出、不合并活动、不缓存）：GUI 应用负责收敛/合并/派生
        let mut rx = driver.prompt(&agent_session_id, input);
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::UserMessage(_) => {
                    // 回显：server 已发用户消息透传事件，此处忽略
                }
                AgentEvent::TurnEnded => break,
                _ => {
                    let _ = self.tx.send(ServerNotification::Passthrough {
                        session_id: session_id.to_string(),
                        event: passthrough_event(ev, now()),
                    });
                }
            }
        }
        // turn 结束（透传边界 + 列表状态标记）
        let _ = self.tx.send(ServerNotification::Passthrough {
            session_id: session_id.to_string(),
            event: PassthroughEvent::TurnEnded { timestamp: now() },
        });
        self.update_state(session_id, SessionState::Idle).await;
        protocol::log::info(
            "server.session",
            format!(
                "prompt 完成 {session_id}（{}ms）",
                started.elapsed().as_millis()
            ),
        );
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let (driver, agent_session_id) = {
            let reg = self.registry.lock().await;
            let rec = reg
                .get(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            (rec.driver.clone(), rec.agent_session_id.clone())
        };
        protocol::log::info(
            "server.session",
            format!("取消 {session_id}（agent={agent_session_id}）"),
        );
        let r = driver.cancel(&agent_session_id);
        if let Err(e) = &r {
            protocol::log::error("server.session", format!("取消失败 {session_id}: {e}"));
        }
        r
    }

    /// 更新会话列表的状态字段（不推送状态通知；busy/idle 由 GUI 应用从透传事件派生，
    /// 重连时经会话列表 meta.state 补齐，docs/DESIGN.md §5.1）。
    async fn update_state(&self, session_id: &str, state: SessionState) {
        let mut reg = self.registry.lock().await;
        if let Some(rec) = reg.get_mut(session_id) {
            rec.meta.state = state;
            rec.meta.last_event_at = now();
        }
    }
}

/// 取输入的首个文本块（标题生成用）。
fn first_text(input: &[ContentBlock]) -> String {
    input
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    /// 测试注册表（stub 驱动接受任意 harness 名，忽略本机 PATH 发现）。
    fn stub_manager() -> (SessionManager, broadcast::Receiver<ServerNotification>) {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        SessionManager::new(agents)
    }

    #[tokio::test]
    async fn prompt_passthrough_events() {
        let (mgr, mut rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();

        mgr.prompt(&meta.id, text("hi")).await.unwrap();

        // 透传事件流（docs/DESIGN.md §5.1）：turn 边界 + 输出 chunk + 活动事件，无聚合交付
        let mut events: Vec<PassthroughEvent> = Vec::new();
        let mut saw_turn_started = false;
        let mut saw_turn_ended = false;
        while let Ok(n) = rx.recv().await {
            match n {
                ServerNotification::Passthrough { event, .. } => match event {
                    PassthroughEvent::TurnStarted { .. } => saw_turn_started = true,
                    PassthroughEvent::TurnEnded { .. } => {
                        saw_turn_ended = true;
                        break;
                    }
                    e => events.push(e),
                },
                // 会话生命周期通知（创建/标题更新）与透传流无关，跳过
                ServerNotification::SessionCreated(_)
                | ServerNotification::SessionUpdated(_)
                | ServerNotification::SessionDeleted(_)
                | ServerNotification::SessionInterrupted(_) => {}
            }
        }
        assert!(saw_turn_started, "应有 turn 开始边界");
        assert!(saw_turn_ended, "应有 turn 结束边界");
        // 输出 chunk + thinking + tool_call 逐条透传
        assert!(events.iter().any(
            |e| matches!(e, PassthroughEvent::OutputChunk { text, .. } if text.contains("模拟输出"))
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, PassthroughEvent::ThinkingChunk { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            PassthroughEvent::ToolCall { name, .. } if name == "read_file"
        )));

        // 列表状态回到空闲（重连经 meta.state 补齐，docs/DESIGN.md §5.1）
        assert_eq!(mgr.list().await[0].state, SessionState::Idle);
    }

    #[tokio::test]
    async fn open_session_returns_events() {
        let (mgr, _rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        let (events, _has_more, _) = mgr.open(&meta.id, None, None).await.unwrap();
        assert!(events.is_empty()); // stub 无持久化历史（历史权威在 agent，docs/DESIGN.md §5.2）
    }

    #[tokio::test]
    async fn create_and_delete_lifecycle() {
        let (mgr, mut rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        assert_eq!(mgr.list().await.len(), 1);

        mgr.delete(&meta.id).await.unwrap();
        assert!(mgr.list().await.is_empty());
        assert!(mgr.open(&meta.id, None, None).await.is_err());

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
        let (mgr, _rx) = stub_manager();
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.open("nope", None, None).await.is_err());
        assert!(mgr.delete("nope").await.is_err());
    }

    #[test]
    fn window_items_lazy_loading_slices() {
        // 1000 条历史：默认窗口取最后 200，has_more=true，游标 800
        let (start, end, has_more) = SessionManager::window_items(1000, 200, None);
        assert_eq!((start, end, has_more), (800, 1000, true));
        // 向上取更早一窗：before=800 → [600,800)，还有更早
        let (start, end, has_more) = SessionManager::window_items(1000, 200, Some(800));
        assert_eq!((start, end, has_more), (600, 800, true));
        // 取到最旧一窗：has_more=false
        let (start, end, has_more) = SessionManager::window_items(1000, 200, Some(200));
        assert_eq!((start, end, has_more), (0, 200, false));
        // 历史不足一窗：全部返回，has_more=false
        let (start, end, has_more) = SessionManager::window_items(50, 200, None);
        assert_eq!((start, end, has_more), (0, 50, false));
        // 空历史
        let (start, end, has_more) = SessionManager::window_items(0, 200, None);
        assert_eq!((start, end, has_more), (0, 0, false));
        // before 越界：钳制到末尾
        let (start, end, has_more) = SessionManager::window_items(100, 200, Some(500));
        assert_eq!((start, end, has_more), (0, 100, false));
    }

    #[tokio::test]
    async fn first_prompt_generates_title() {
        let (mgr, mut rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        assert!(meta.title.is_empty());

        mgr.prompt(&meta.id, text("实现登录功能\n然后写测试"))
            .await
            .unwrap();

        let listed = mgr.list().await;
        assert_eq!(listed[0].title, "实现登录功能");

        // session_updated 通知携带新标题（GUI 刷新列表）
        let mut saw_updated = false;
        while let Ok(n) = rx.recv().await {
            if let ServerNotification::SessionUpdated(s) = n {
                assert_eq!(s.title, "实现登录功能");
                saw_updated = true;
                break;
            }
        }
        assert!(saw_updated, "应广播 session_updated");

        // 用户修改标题后，后续 prompt 不再覆盖
        mgr.set_session_title(&meta.id, "我的标题").await.unwrap();
        mgr.prompt(&meta.id, text("第二条指令")).await.unwrap();
        assert_eq!(mgr.list().await[0].title, "我的标题");
    }

    #[tokio::test]
    async fn set_default_model_persists() {
        let (mgr, _rx) = stub_manager();
        mgr.set_default_model("stub", Some("gpt-4o".into()));
        let hs = mgr.harnesses();
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].name, "stub");
        assert_eq!(hs[0].default_model.as_deref(), Some("gpt-4o"));
        // 未知 harness 的模型配置不破坏列表
        mgr.set_default_model("nope", Some("x".into()));
        assert_eq!(mgr.harnesses().len(), 1);
    }
}
