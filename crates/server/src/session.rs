//! 会话管理（docs/DESIGN.md §6）：会话注册表、turn 事件聚合（非流式交付 +
//! activities 有界缓存）、通知广播、prompt 串行化。
//! 历史权威在 agent 侧；server 不保存对话历史，仅维护会话元数据与 activities 缓存。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, Mutex};

use protocol::{
    generate_title, Activity, ContentBlock, DialogItem, HarnessInfo, SessionMeta, SessionState,
    TurnCompleted,
};

use crate::agent::{AgentEvent, AgentRegistry, DialogRecord, SharedDriver};

/// server → GUI 通知（docs/DESIGN.md §4/§5）。
#[derive(Debug, Clone)]
pub enum ServerNotification {
    SessionCreated(SessionMeta),
    SessionClosed(SessionMeta),
    /// 崩溃恢复标记（重启后忙状态会话标 interrupted，docs/DESIGN.md §3）
    #[allow(dead_code)]
    SessionInterrupted(SessionMeta),
    SessionDeleted(SessionMeta),
    /// 会话元数据更新（标题修改等，多 GUI 同步）
    SessionUpdated(SessionMeta),
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
    /// turn 中的实时活动（合并后的当前活动，docs/DESIGN.md §5.3 流式）
    Activity {
        session_id: String,
        activity: Activity,
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
    /// 按会话的 activities 有界缓存（docs/DESIGN.md §5.3）
    activities: Mutex<HashMap<String, VecDeque<Activity>>>,
    /// turn 进行中的实时活动（同类事件合并，thinking 流式累积；turn 结束清空）
    live_activities: std::sync::Mutex<HashMap<String, Activity>>,
    tx: broadcast::Sender<ServerNotification>,
    max_activities: usize,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把 turn 事件合并进 activities 列表（docs/DESIGN.md §5.3 事件流合并）：
/// - 连续 thinking 块累积为一条 Thinking（流式，逐块追加内容）
/// - 同工具（相同 kind）的 tool_call / tool_call_update 合并为一条 ToolCall
/// - compaction 独立成条
/// 返回合并/新增后的当前活动（供实时推送；无活动产出时返回 None）。
fn merge_activity(acts: &mut Vec<Activity>, ev: AgentEvent, ts: u64) -> Option<Activity> {
    match ev {
        AgentEvent::Thinking(c) => {
            if let Some(Activity::Thinking { content, .. }) = acts.last_mut() {
                content.push_str(&c);
                acts.last().cloned()
            } else {
                let a = Activity::Thinking {
                    timestamp: ts,
                    content: c,
                };
                acts.push(a.clone());
                Some(a)
            }
        }
        AgentEvent::ToolCall {
            name,
            title,
            content,
        } => {
            let mergeable = matches!(acts.last(), Some(Activity::ToolCall { name: last, .. }) if *last == name);
            if mergeable {
                if let Some(Activity::ToolCall {
                    title: t, content: c, ..
                }) = acts.last_mut()
                {
                    if t.is_none() {
                        *t = title;
                    }
                    if let Some(nc) = content {
                        *c = Some(nc);
                    }
                }
                acts.last().cloned()
            } else {
                let a = Activity::ToolCall {
                    timestamp: ts,
                    name,
                    title,
                    content,
                };
                acts.push(a.clone());
                Some(a)
            }
        }
        AgentEvent::Compaction(d) => {
            let a = Activity::Compaction {
                timestamp: ts,
                detail: d,
            };
            acts.push(a.clone());
            Some(a)
        }
        // OutputChunk / UserMessage / TurnEnded 不产生活动
        _ => None,
    }
}

impl SessionManager {
    pub fn new(
        agents: Arc<AgentRegistry>,
        max_activities: usize,
    ) -> (Self, broadcast::Receiver<ServerNotification>) {
        let (tx, rx) = broadcast::channel(256);
        let manager = SessionManager {
            agents,
            registry: Mutex::new(HashMap::new()),
            activities: Mutex::new(HashMap::new()),
            live_activities: std::sync::Mutex::new(HashMap::new()),
            tx,
            max_activities,
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
        let id = format!("s_{}", uuid_v4());
        let meta = SessionMeta {
            id,
            harness: harness.to_string(),
            cwd: cwd.to_string(),
            model: model.map(str::to_string),
            state: SessionState::Idle,
            interrupted: false,
            closed: false,
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

    pub async fn resume(&self, session_id: &str) -> Result<SessionMeta, String> {
        let mut reg = self.registry.lock().await;
        let rec = reg
            .get_mut(session_id)
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        rec.driver.resume_session(&rec.agent_session_id)?;
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
        rec.driver.close(&rec.agent_session_id)?;
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
        rec.driver.delete(&rec.agent_session_id)?;
        self.activities.lock().await.remove(session_id);
        let mut meta = rec.meta;
        meta.closed = true;
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

    /// 打开会话：经 driver 的 `session/load` 全量重放，聚合对话内容返回。
    pub async fn open(&self, session_id: &str) -> Result<Vec<DialogItem>, String> {
        let (driver, agent_session_id) = {
            let reg = self.registry.lock().await;
            let rec = reg
                .get(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            (rec.driver.clone(), rec.agent_session_id.clone())
        };
        let records = driver.load_session(&agent_session_id)?;
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

    /// 推送合并后的实时活动（turn 进行中；仅在活动有变化时通知，避免刷屏）。
    fn push_live_activity(&self, session_id: &str, activity: Option<Activity>) {
        let Some(activity) = activity else {
            return;
        };
        let mut live = self.live_activities.lock().unwrap();
        let changed = live.get(session_id) != Some(&activity);
        if changed {
            live.insert(session_id.to_string(), activity.clone());
            let _ = self
                .tx
                .send(ServerNotification::Activity {
                    session_id: session_id.to_string(),
                    activity,
                });
        }
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

        // 用户消息通知 + 忙状态
        let _ = self.tx.send(ServerNotification::UserMessage {
            session_id: session_id.to_string(),
            content: input.clone(),
            timestamp: now(),
        });
        self.set_state(session_id, SessionState::Busy).await;

        // 聚合 turn 事件：同类事件合并（thinking 逐块累积、同工具调用合并），
        // 合并后的当前活动经 activity 通知实时推送（docs/DESIGN.md §5.3 流式）
        let mut rx = driver.prompt(&agent_session_id, input);
        let mut output: Vec<ContentBlock> = Vec::new();
        let mut acts: Vec<Activity> = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::OutputChunk(s) => output.push(ContentBlock::Text { text: s }),
                AgentEvent::UserMessage(_) => {
                    // 回显：server 已发 user_message 通知，此处忽略
                }
                AgentEvent::Thinking(c) => {
                    let merged = merge_activity(&mut acts, AgentEvent::Thinking(c), now());
                    self.push_live_activity(session_id, merged);
                }
                AgentEvent::ToolCall {
                    name,
                    title,
                    content,
                } => {
                    let merged =
                        merge_activity(&mut acts, AgentEvent::ToolCall { name, title, content }, now());
                    self.push_live_activity(session_id, merged);
                }
                AgentEvent::Compaction(d) => {
                    let merged = merge_activity(&mut acts, AgentEvent::Compaction(d), now());
                    self.push_live_activity(session_id, merged);
                }
                AgentEvent::TurnEnded => break,
            }
        }
        // turn 结束：清空实时活动，合并后的完整活动写入有界缓存
        self.live_activities.lock().unwrap().remove(session_id);

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
        let (driver, agent_session_id) = {
            let reg = self.registry.lock().await;
            let rec = reg
                .get(session_id)
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            (rec.driver.clone(), rec.agent_session_id.clone())
        };
        driver.cancel(&agent_session_id)
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
    use crate::agent::AgentRegistry;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    /// 测试注册表（stub 驱动接受任意 harness 名，忽略本机 PATH 发现）。
    fn stub_manager(
        max_activities: usize,
    ) -> (SessionManager, broadcast::Receiver<ServerNotification>) {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        SessionManager::new(agents, max_activities)
    }

    /// 事件流合并：连续 thinking 累积为一条，同工具调用合并，compaction 独立成条。
    #[test]
    fn merge_activity_merges_consecutive_kinds() {
        let mut acts: Vec<Activity> = Vec::new();
        // 连续 thinking 块 → 累积为一条
        merge_activity(&mut acts, AgentEvent::Thinking("思考".into()), 1);
        merge_activity(&mut acts, AgentEvent::Thinking("中…".into()), 2);
        assert_eq!(acts.len(), 1);
        match &acts[0] {
            Activity::Thinking { content, .. } => assert_eq!(content, "思考中…"),
            other => panic!("应为合并后的 Thinking，得到 {other:?}"),
        }
        // 同工具 tool_call + tool_call_update → 合并为一条（title/content 补全）
        merge_activity(
            &mut acts,
            AgentEvent::ToolCall {
                name: "execute".into(),
                title: Some("运行测试".into()),
                content: None,
            },
            3,
        );
        merge_activity(
            &mut acts,
            AgentEvent::ToolCall {
                name: "execute".into(),
                title: None,
                content: Some("cargo test".into()),
            },
            4,
        );
        assert_eq!(acts.len(), 2);
        match &acts[1] {
            Activity::ToolCall {
                name, title, content, ..
            } => {
                assert_eq!(name, "execute");
                assert_eq!(title.as_deref(), Some("运行测试"));
                assert_eq!(content.as_deref(), Some("cargo test"));
            }
            other => panic!("应为合并后的 ToolCall，得到 {other:?}"),
        }
        // 不同工具 → 新条目
        merge_activity(
            &mut acts,
            AgentEvent::ToolCall {
                name: "read".into(),
                title: None,
                content: None,
            },
            5,
        );
        assert_eq!(acts.len(), 3);
        // compaction 独立成条
        merge_activity(&mut acts, AgentEvent::Compaction("压缩".into()), 6);
        assert_eq!(acts.len(), 4);
        assert!(matches!(acts[3], Activity::Compaction { .. }));
    }

    #[tokio::test]
    async fn prompt_aggregates_output_and_activities() {
        let (mgr, mut rx) = stub_manager(100);
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
        let (mgr, _rx) = stub_manager(1);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        mgr.prompt(&meta.id, text("x")).await.unwrap();
        let acts = mgr.get_activities(&meta.id, None).await.unwrap();
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], Activity::ToolCall { .. }));
    }

    #[tokio::test]
    async fn open_session_returns_dialog_content() {
        let (mgr, _rx) = stub_manager(100);
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        let items = mgr.open(&meta.id).await.unwrap();
        assert!(items.is_empty()); // stub 无持久化历史（历史权威在 agent，docs/DESIGN.md §5.2）
    }

    #[tokio::test]
    async fn create_and_delete_lifecycle() {
        let (mgr, mut rx) = stub_manager(100);
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
        let (mgr, _rx) = stub_manager(100);
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.open("nope").await.is_err());
        assert!(mgr.delete("nope").await.is_err());
    }

    #[tokio::test]
    async fn first_prompt_generates_title() {
        let (mgr, mut rx) = stub_manager(100);
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
        let (mgr, _rx) = stub_manager(100);
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
