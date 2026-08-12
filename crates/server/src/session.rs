//! 会话管理（docs/DESIGN.md §3.2/§4/§5）：会话列表与历史权威 = server。
//! - 会话注册表持久化于 SQLite（`amux.db`），列表由 server 维护（不依赖 ACP `session/list`）
//! - 事件透传（§5.1）与历史合并落库（§5.2）：透传逐条给 GUI，落库按 turn 合并
//! - `open_session` 从本地历史日志按窗口/游标读取，不触发 ACP 重放

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use protocol::{
    generate_title, ContentBlock, HarnessInfo, PassthroughEvent, SessionMeta, SessionState,
};

use crate::agent::{passthrough_event, AgentEvent, AgentRegistry};
use crate::history::SessionLog;
use crate::registry::{RegistryEntry, SessionRegistry};

/// server → GUI 通知（docs/DESIGN.md §4/§5）。
#[derive(Debug, Clone)]
pub enum ServerNotification {
    SessionCreated(SessionMeta),
    /// 崩溃恢复标记（保留枚举位；当前无恢复语义，恒不触发）
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

pub struct SessionManager {
    agents: Arc<AgentRegistry>,
    registry: Arc<SessionRegistry>,
    history_dir: PathBuf,
    tx: broadcast::Sender<ServerNotification>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionManager {
    /// `history_dir`：历史日志目录（server 数据目录，docs/DESIGN.md §4.3）。
    pub fn new(
        agents: Arc<AgentRegistry>,
        registry: Arc<SessionRegistry>,
        history_dir: PathBuf,
    ) -> (Self, broadcast::Receiver<ServerNotification>) {
        let (tx, rx) = broadcast::channel(256);
        let manager = SessionManager {
            agents,
            registry,
            history_dir,
            tx,
        };
        (manager, rx)
    }

    /// 会话列表惰性分页（纯函数，可单测；docs/DESIGN.md §3.2 / PRD §4.1.1）：
    /// `all` 已按最近活跃（`last_event_at` 降序）；`before` 为独占上界游标
    /// （只取 `last_event_at < before` 的更早会话）。返回（窗口, 是否还有更早, 下次游标）。
    pub fn session_page(
        all: &[RegistryEntry],
        limit: usize,
        before: Option<u64>,
    ) -> (Vec<RegistryEntry>, bool, Option<u64>) {
        let filtered: Vec<&RegistryEntry> = all
            .iter()
            .filter(|(m, _)| before.map(|b| m.last_event_at < b).unwrap_or(true))
            .collect();
        let window: Vec<RegistryEntry> = filtered.iter().take(limit).map(|e| (*e).clone()).collect();
        let has_more = filtered.len() > limit;
        let next_before = if has_more {
            window.last().map(|(m, _)| m.last_event_at)
        } else {
            None
        };
        (window, has_more, next_before)
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
        // 创建会话只与 server 交互（docs/DESIGN.md §4.1）：写入注册表立即返回，
        // 不触发 ACP——agent 会话延后到首次 prompt 时懒创建（session/new）。
        let id = format!("s_{}", uuid::Uuid::new_v4());
        protocol::log::info(
            "server.session",
            format!("创建会话 {id}（harness={harness} cwd={cwd}，agent 会话延后创建）"),
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
        self.registry
            .upsert(&meta, "")
            .map_err(|e| format!("注册表写入失败: {e}"))?;
        let _ = self
            .tx
            .send(ServerNotification::SessionCreated(meta.clone()));
        Ok(meta)
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), String> {
        let entry = self
            .registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let (meta, agent_session_id) = entry;
        // 从未 prompt 过（agent 会话尚未创建）则无需 ACP 删除
        if !agent_session_id.is_empty() {
            let driver = self.agents.driver_for(&meta.harness)?;
            driver.delete(&agent_session_id)?;
        }
        // 联动：注册表条目 + 历史日志 +（如有）agent 侧 ACP 会话（docs/DESIGN.md §5.2）
        self.registry
            .delete(session_id)
            .map_err(|e| format!("注册表删除失败: {e}"))?;
        SessionLog::open(&self.history_dir, session_id).remove();
        protocol::log::info("server.session", format!("删除会话 {session_id}"));
        let _ = self.tx.send(ServerNotification::SessionDeleted(meta));
        Ok(())
    }

    /// 修改会话标题（用户可随时修改，PRD §3.1）；广播 session_updated 同步各 GUI。
    pub async fn set_session_title(&self, session_id: &str, title: &str) -> Result<(), String> {
        let ts = now();
        self.registry
            .set_title(session_id, title.trim(), ts)
            .map_err(|e| format!("注册表更新失败: {e}"))?;
        let meta = self
            .registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?
            .0;
        let _ = self.tx.send(ServerNotification::SessionUpdated(meta));
        Ok(())
    }

    /// 惰性分页列表（docs/DESIGN.md §3.2 / PRD §4.1.1）：首次只取最近活跃一窗，
    /// `before` 游标滚动加载更早。
    pub async fn list(
        &self,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<SessionMeta>, bool, Option<u64>), String> {
        let all = self
            .registry
            .list()
            .map_err(|e| format!("注册表读取失败: {e}"))?;
        let (window, has_more, next_before) =
            Self::session_page(&all, limit.unwrap_or(50), before);
        let metas = window.into_iter().map(|(m, _)| m).collect();
        Ok((metas, has_more, next_before))
    }

    // ---- 会话数据（docs/DESIGN.md §5）----

    /// 打开会话：从本地历史日志读取（合并条目，按窗口/游标惰性分页），
    /// **不触发 ACP 重放**（docs/DESIGN.md §5.2）。`before` 为独占上界游标。
    pub async fn open(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<usize>,
    ) -> Result<(Vec<PassthroughEvent>, bool, usize), String> {
        self.registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let events = SessionLog::open(&self.history_dir, session_id).read();
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

    /// prompt：经 driver 触发 turn；透传事件逐条给 GUI（§5.1），同时按 turn 缓冲，
    /// 收到 result（turn 结束，含取消）时按 §5.3 语义合并落库（§5.2）。
    /// 忙时 prompt（steer）依赖 agent 实现；不支持进行中注入时直接报错（§7.1）。
    /// 首条 prompt 时为会话生成默认标题（PRD §3.1）。
    pub async fn prompt(&self, session_id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        // 忙检查 + 标题生成 + Busy 状态一次完成（注册表为同步写，天然原子）
        let (driver, agent_session_id, cwd, title_changed) = {
            let (mut meta, agent_session_id) = self
                .registry
                .get(session_id)
                .map_err(|e| format!("注册表读取失败: {e}"))?
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            if meta.state == SessionState::Busy {
                return Err("会话忙：agent 不支持进行中注入（steer），请等待当前工作结束".into());
            }
            let title_changed = if meta.title.is_empty() {
                meta.title = generate_title(&first_text(&input));
                true
            } else {
                false
            };
            meta.state = SessionState::Busy;
            meta.last_event_at = now();
            let driver = self.agents.driver_for(&meta.harness)?;
            // 创建会话时未与 ACP 交互（docs/DESIGN.md §4.1）：首条 prompt 才懒创建
            // agent 会话（session/new）并回填注册表；已创建则沿用
            let agent_session_id = if agent_session_id.is_empty() {
                let sid2 = driver.create_session(&meta.cwd, meta.model.as_deref())?;
                self.registry
                    .set_agent_session_id(session_id, &sid2)
                    .map_err(|e| format!("注册表写入失败: {e}"))?;
                sid2
            } else {
                agent_session_id
            };
            self.registry
                .upsert(&meta, &agent_session_id)
                .map_err(|e| format!("注册表写入失败: {e}"))?;
            let cwd = meta.cwd.clone();
            (driver, agent_session_id, cwd, title_changed)
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
        if title_changed {
            let meta = self
                .registry
                .get(session_id)
                .ok()
                .flatten()
                .map(|(m, _)| m);
            if let Some(m) = meta {
                let _ = self.tx.send(ServerNotification::SessionUpdated(m));
            }
        }

        // turn 开始（透传边界）+ 用户消息回显（透传；GUI 本地渲染，§7.1）
        let ts = now();
        let mut merger = crate::history::TurnMerger::new();
        let mut forward = |tx: &broadcast::Sender<ServerNotification>,
                           session_id: &str,
                           event: PassthroughEvent|
         -> PassthroughEvent {
            merger.push(&event);
            let _ = tx.send(ServerNotification::Passthrough {
                session_id: session_id.to_string(),
                event: event.clone(),
            });
            event
        };
        let turn_started = PassthroughEvent::TurnStarted { timestamp: ts };
        forward(&self.tx, session_id, turn_started);
        let echo = PassthroughEvent::UserMessage {
            content: input.clone(),
            timestamp: ts,
        };
        forward(&self.tx, session_id, echo);
        self.update_state(session_id, SessionState::Busy).await;

        // 继续既有会话：先经 ACP `session/resume` 恢复 agent 自身上下文
        // （docs/DESIGN.md §7.2；同一进程内幂等）
        if let Err(e) = driver.resume_session(&agent_session_id, &cwd) {
            protocol::log::error("server.session", format!("resume 失败 {session_id}: {e}"));
            self.update_state(session_id, SessionState::Idle).await;
            return Err(format!("恢复 agent 上下文失败: {e}"));
        }

        // 事件逐条透传（GUI 实时渲染），同时喂合并器（§5.2 按 turn 合并落库）
        let mut turn_completed = false;
        let mut rx = driver.prompt(&agent_session_id, input);
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::UserMessage(_) => {
                    // 回显：server 已发用户消息透传事件，此处忽略
                }
                AgentEvent::TurnEnded => {
                    turn_completed = true;
                    break;
                }
                _ => {
                    forward(&self.tx, session_id, passthrough_event(ev, now()));
                }
            }
        }

        // turn 结束（透传边界；收到 result 时合并落库，docs/DESIGN.md §5.2）
        let turn_ended = PassthroughEvent::TurnEnded { timestamp: now() };
        merger.push(&turn_ended);
        let _ = self.tx.send(ServerNotification::Passthrough {
            session_id: session_id.to_string(),
            event: turn_ended,
        });
        if turn_completed {
            let merged = merger.finalize();
            let log = SessionLog::open(&self.history_dir, session_id);
            if let Err(e) = log.append(&merged) {
                protocol::log::error("server.session", format!("历史落盘失败 {session_id}: {e}"));
            }
        }
        self.update_state(session_id, SessionState::Idle).await;
        protocol::log::info(
            "server.session",
            format!(
                "prompt 完成 {session_id}（{}ms，turn_completed={turn_completed}）",
                started.elapsed().as_millis()
            ),
        );
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let (meta, agent_session_id) = self
            .registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        // 从未 prompt 过（agent 会话尚未创建）：无进行中的工作
        if agent_session_id.is_empty() {
            return Ok(());
        }
        let driver = self.agents.driver_for(&meta.harness)?;
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

    /// 更新会话列表的状态字段（busy/idle 由 GUI 应用从透传事件派生，重连经
    /// 列表 meta.state 补齐，docs/DESIGN.md §5.1）。
    async fn update_state(&self, session_id: &str, state: SessionState) {
        let _ = self
            .registry
            .update_state(session_id, state, now());
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
    use crate::history::SessionLog;
    use crate::registry::SessionRegistry;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    /// 测试管理器（stub 驱动接受任意 harness 名；独立临时数据目录）。
    fn stub_manager() -> (SessionManager, broadcast::Receiver<ServerNotification>) {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        let dir = std::env::temp_dir().join(format!(
            "amux-sess-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let registry = Arc::new(SessionRegistry::open(&dir.join("amux.db")).unwrap());
        SessionManager::new(agents, registry, dir)
    }

    /// 创建会话只与 server 交互（docs/DESIGN.md §4.1）：注册表立即写入、
    /// agent 会话延后到首条 prompt 懒创建并回填。
    #[tokio::test]
    async fn create_defers_agent_session_until_prompt() {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        let dir = std::env::temp_dir().join(format!(
            "amux-defer-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let registry = Arc::new(SessionRegistry::open(&dir.join("amux.db")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        let meta = mgr.create("codex", "/tmp/defer", None).await.unwrap();

        // 创建后：注册表 agent_session_id 为空（未与 ACP 交互）
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(aid.is_empty(), "创建会话不应触发 ACP session/new");

        // 首条 prompt：懒创建 agent 会话并回填
        mgr.prompt(&meta.id, text("你好")).await.unwrap();
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(
            aid.starts_with("agent_"),
            "首条 prompt 应懒创建 agent 会话并回填: {aid:?}"
        );

        // 未 prompt 的会话可直接删除（无 ACP 会话，删除仅清注册表与日志）
        let meta2 = mgr.create("codex", "/tmp/defer2", None).await.unwrap();
        mgr.delete(&meta2.id).await.unwrap();
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list.len(), 1, "未 prompt 会话删除后只剩已 prompt 的那个");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话列表惰性分页纯函数（docs/DESIGN.md §3.2 / PRD §4.1.1）：
    /// 按最近活跃降序切窗、before 游标取更早、has_more/next_before 边界。
    #[test]
    fn session_page_lazy_windows() {
        fn entry(id: &str, last: u64) -> RegistryEntry {
            (
                SessionMeta {
                    id: id.into(),
                    harness: "codex".into(),
                    cwd: "/tmp".into(),
                    model: None,
                    state: SessionState::Idle,
                    interrupted: false,
                    title: String::new(),
                    created_at: 1,
                    last_event_at: last,
                },
                format!("agent_{id}"),
            )
        }
        // 已按最近活跃降序
        let all = vec![
            entry("s9", 900),
            entry("s8", 800),
            entry("s7", 700),
            entry("s6", 600),
            entry("s5", 500),
            entry("s4", 400),
        ];

        // 首次一窗（limit=2）：最近活跃在前，has_more，next_before=800（窗口最后一条）
        let (w, more, nb) = SessionManager::session_page(&all, 2, None);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0.id, "s9");
        assert_eq!(w[1].0.id, "s8");
        assert!(more);
        assert_eq!(nb, Some(800));

        // 更早一窗：before=800 → s7/s6，has_more，next_before=600
        let (w, more, nb) = SessionManager::session_page(&all, 2, Some(800));
        assert_eq!(w.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(), ["s7", "s6"]);
        assert!(more);
        assert_eq!(nb, Some(600));

        // 取到最旧一窗：has_more=false，next_before=None
        let (w, more, nb) = SessionManager::session_page(&all, 2, Some(600));
        assert_eq!(w.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(), ["s5", "s4"]);
        assert!(!more);
        assert_eq!(nb, None);

        // 空库
        let (w, more, nb) = SessionManager::session_page(&[], 2, None);
        assert!(w.is_empty());
        assert!(!more);
        assert_eq!(nb, None);

        // 不足一窗
        let (w, more, nb) = SessionManager::session_page(&all, 10, None);
        assert_eq!(w.len(), 6);
        assert!(!more);
        assert_eq!(nb, None);

        // 游标越界（比最旧还旧）：空窗
        let (w, more, nb) = SessionManager::session_page(&all, 2, Some(100));
        assert!(w.is_empty());
        assert!(!more);
        assert_eq!(nb, None);

        // 游标取不到更早但仍有余量：过滤后不足一窗（before 为独占上界，s7(700) 被排除）
        let (w, more, nb) = SessionManager::session_page(&all, 10, Some(700));
        assert_eq!(w.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(), ["s6", "s5", "s4"]);
        assert!(!more);
        assert_eq!(nb, None);
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
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].state, SessionState::Idle);
    }

    /// 历史按 turn 合并落库（docs/DESIGN.md §5.2）：prompt 后日志存在且为合并粒度，
    /// open_session 返回合并条目（不触发 ACP 重放）。
    #[tokio::test]
    async fn prompt_writes_merged_history_and_open_reads_local() {
        let (mgr, _rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();

        mgr.prompt(&meta.id, text("hi")).await.unwrap();

        // 日志文件存在（历史权威在 server）
        let log = SessionLog::open(&mgr.history_dir, &meta.id);
        assert!(log.exists(), "prompt 完成（result）后应落库");

        // open_session 从本地日志读合并条目：一条完整输出（非逐 chunk）
        let (events, _, _) = mgr.open(&meta.id, None, None).await.unwrap();
        assert!(events.iter().any(|e| matches!(e, PassthroughEvent::TurnStarted { .. })));
        assert!(events.iter().any(|e| matches!(e, PassthroughEvent::TurnEnded { .. })));
        let outputs: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                PassthroughEvent::OutputChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // stub 输出："模拟输出：完成"（chunk 已收敛为一条）
        assert!(outputs.len() == 1 && outputs[0].contains("完成"), "合并粒度应一条完整输出: {outputs:?}");
    }

    #[tokio::test]
    async fn create_and_delete_lifecycle() {
        let (mgr, mut rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list.len(), 1);

        mgr.delete(&meta.id).await.unwrap();
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert!(list.is_empty());
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

    /// 删除联动：历史日志一并清除（docs/DESIGN.md §5.2）。
    #[tokio::test]
    async fn delete_removes_history_log() {
        let (mgr, _rx) = stub_manager();
        let meta = mgr.create("codex", "/tmp/work", None).await.unwrap();
        mgr.prompt(&meta.id, text("hi")).await.unwrap();
        let log = SessionLog::open(&mgr.history_dir, &meta.id);
        assert!(log.exists());

        mgr.delete(&meta.id).await.unwrap();
        assert!(!log.exists(), "删除会话应联动清除历史日志");
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

        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].title, "实现登录功能");

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
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].title, "我的标题");
    }

    /// server 重启恢复（docs/DESIGN.md §4.1）：列表从 SQLite 注册表恢复，
    /// 不依赖 ACP `session/list`；历史日志在盘直接可用。
    #[tokio::test]
    async fn restart_recovers_list_and_history_from_local_store() {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        let dir = std::env::temp_dir().join(format!(
            "amux-restart-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let db = dir.join("amux.db");

        // 实例 A：创建 + prompt（注册表与历史落盘）
        let sid;
        {
            let registry = Arc::new(SessionRegistry::open(&db).unwrap());
            let (mgr, _rx) = SessionManager::new(agents.clone(), registry, dir.clone());
            let meta = mgr.create("stub", "/tmp/work", None).await.unwrap();
            sid = meta.id.clone();
            mgr.prompt(&meta.id, text("你好")).await.unwrap();
        }

        // 实例 B（同一数据目录，模拟 server 重启）：列表与历史从本地恢复
        let registry = Arc::new(SessionRegistry::open(&db).unwrap());
        let (mgr2, _rx2) = SessionManager::new(agents, registry, dir.clone());
        let (list, _, _) = mgr2.list(None, None).await.unwrap();
        assert_eq!(list.len(), 1, "重启后列表应从注册表恢复");
        assert_eq!(list[0].id, sid);
        assert_eq!(list[0].cwd, "/tmp/work");

        // 历史日志在盘直接可用（open 本地读，合并条目）
        let (events, _, _) = mgr2.open(&sid, None, None).await.unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, PassthroughEvent::OutputChunk { text, .. } if text.contains("完成"))),
            "重启后历史应可从本地日志读取: {events:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
