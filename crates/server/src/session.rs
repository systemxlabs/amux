//! 会话管理（docs/DESIGN.md「普通会话存储」「session.*」）：会话列表与历史权威 = server。
//! - 会话元数据持久化于 SQLite（`session.sqlite`），列表由 server 维护
//! - busy/idle 状态在 server 维护并存注册表；每次 Busy<->Idle 变更广播 `session.state_change`
//! - 惰性会话：`session.new` 只写注册表，agent 侧会话延后到首条指令（`session.prompt`）
//!   懒创建（ACP `session/new`）并 `session/resume`
//! - 删除会话与长时间无活动（>1h）时经 ACP `session/close` 关闭 agent 侧会话
//! - 对话历史与活动历史落 `data_dir/sessions/<id>_history.jsonl` / `<id>_activities.jsonl`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use protocol::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionMeta, SessionState,
    SessionStateChange,
};

use crate::agent::{AgentEvent, AgentRegistry};
use crate::history::{SessionLog, TurnMerger};
use crate::registry::{RegistryEntry, SessionRegistry};

/// server → GUI 通知（docs/DESIGN.md 唯一主动推送：`session.state_change`）。
#[derive(Debug, Clone)]
pub enum ServerNotification {
    StateChange(SessionStateChange),
}

pub struct SessionManager {
    agents: Arc<AgentRegistry>,
    registry: Arc<SessionRegistry>,
    data_dir: PathBuf,
    tx: broadcast::Sender<ServerNotification>,
    /// 进行中的活动（`session.ongoing_activity`；按会话 id 独立存储）
    ongoing: Mutex<HashMap<String, Activity>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionManager {
    /// `data_dir`：server 数据目录（会话历史/活动日志目录在其 `sessions/` 下）。
    pub fn new(
        agents: Arc<AgentRegistry>,
        registry: Arc<SessionRegistry>,
        data_dir: PathBuf,
    ) -> (Self, broadcast::Receiver<ServerNotification>) {
        let (tx, rx) = broadcast::channel(256);
        // server 重启后，上次异常退出残留的 Busy 会话没有对应运行中 agent，统一重置为 Idle
        let _ = registry.reset_busy_to_idle();
        let manager = SessionManager {
            agents,
            registry,
            data_dir,
            tx,
            ongoing: Mutex::new(HashMap::new()),
        };
        (manager, rx)
    }

    /// 会话列表惰性分页（纯函数，可单测；docs/DESIGN.md「session.list」）：
    /// `all` 已按最近活跃（`last_active_at` 降序）；`before` 为独占上界游标
    /// （只取 `last_active_at < before` 的更早会话）。返回（窗口, 是否还有更早, 下次游标）。
    pub fn session_page(
        all: &[RegistryEntry],
        limit: usize,
        before: Option<u64>,
    ) -> (Vec<RegistryEntry>, bool, Option<u64>) {
        let filtered: Vec<&RegistryEntry> = all
            .iter()
            .filter(|(m, _)| before.map(|b| m.last_active_at < b).unwrap_or(true))
            .collect();
        let window: Vec<RegistryEntry> =
            filtered.iter().take(limit).map(|e| (*e).clone()).collect();
        let has_more = filtered.len() > limit;
        let next_before = if has_more {
            window.last().map(|(m, _)| m.last_active_at)
        } else {
            None
        };
        (window, has_more, next_before)
    }

    /// 惰性加载切窗（纯函数）：按 limit 与 before 游标计算 [start, end) 与是否还有更早。
    pub fn window_items(len: usize, limit: usize, before: Option<usize>) -> (usize, usize, bool) {
        let end = before.unwrap_or(len).min(len);
        let start = end.saturating_sub(limit);
        (start, end, start > 0)
    }

    // ---- agent ----

    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    // ---- 生命周期 ----

    /// 新建普通会话（**惰性**：只写注册表立即返回，不触发 ACP；agent 侧会话延后到
    /// 首条指令时经 `session/new` 懒创建，docs/DESIGN.md「惰性创建新会话」）。
    pub async fn create(&self, agent: &str, cwd: &str) -> Result<SessionMeta, String> {
        let id = format!("s_{}", uuid::Uuid::new_v4());
        let ts = now();
        protocol::log::info(
            "server.session",
            format!("新建会话 {id}（agent={agent} cwd={cwd}，agent 侧会话延后创建）"),
        );
        let meta = SessionMeta {
            id,
            agent: agent.to_string(),
            cwd: cwd.to_string(),
            state: SessionState::Idle,
            title: String::new(),
            created_at: ts,
            last_active_at: ts,
        };
        self.registry
            .upsert(&meta, "")
            .map_err(|e| format!("注册表写入失败: {e}"))?;
        Ok(meta)
    }

    /// 配置会话标题（用户可随时修改，PRD §3.1）。
    pub async fn configure(&self, session_id: &str, title: &str) -> Result<(), String> {
        self.registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        self.registry
            .set_title(session_id, title.trim(), now())
            .map_err(|e| format!("注册表更新失败: {e}"))?;
        Ok(())
    }

    /// 删除会话：若已有 agent 侧会话，先经 ACP `session/close` 关闭（释放 agent 侧资源，
    /// docs/DESIGN.md「删除会话」）；ACP close 失败不阻断本地删除（agent 侧会话可能已不存在）。
    /// 联动清除注册表条目 + 历史日志 + 活动日志。
    pub async fn delete(&self, session_id: &str) -> Result<(), String> {
        let entry = self
            .registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let (meta, agent_session_id) = entry;
        if !agent_session_id.is_empty() {
            match self.agents.driver_for(&meta.agent) {
                Ok(driver) => {
                    if let Err(e) = driver.close(&agent_session_id) {
                        protocol::log::error(
                            "server.session",
                            format!("关闭 ACP 会话失败（继续本地删除）{session_id}: {e}"),
                        );
                    }
                }
                Err(e) => {
                    protocol::log::error(
                        "server.session",
                        format!("解析 agent 驱动失败（继续本地删除）{session_id}: {e}"),
                    );
                }
            }
        }
        self.registry
            .delete(session_id)
            .map_err(|e| format!("注册表删除失败: {e}"))?;
        SessionLog::open(&self.data_dir, session_id).remove();
        protocol::log::info("server.session", format!("删除会话 {session_id}"));
        Ok(())
    }

    /// 惰性分页会话列表（docs/DESIGN.md「session.list」）：按最近活跃降序切窗。
    pub async fn list(
        &self,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<SessionMeta>, bool, Option<u64>), String> {
        let all = self
            .registry
            .list()
            .map_err(|e| format!("注册表读取失败: {e}"))?;
        let (window, has_more, next_before) = Self::session_page(&all, limit.unwrap_or(50), before);
        let metas = window.into_iter().map(|(m, _)| m).collect();
        Ok((metas, has_more, next_before))
    }

    /// 批量查询指定会话（docs/DESIGN.md「session.info」）。
    pub async fn info(&self, session_ids: &[String]) -> Result<Vec<SessionMeta>, String> {
        let mut metas = Vec::new();
        for id in session_ids {
            if let Some((meta, _)) = self
                .registry
                .get(id)
                .map_err(|e| format!("注册表读取失败: {e}"))?
            {
                metas.push(meta);
            }
        }
        Ok(metas)
    }

    /// 关闭长时间无活动的 agent 侧会话（>timeout_ms，docs/DESIGN.md「主动关闭长时间
    /// 无活动会话」）：对注册表中超过阈值的候选会话经 ACP `session/close` 关闭并清空
    /// agent 侧会话 id（元数据与历史保留）。
    pub async fn close_idle(&self, now_ms: u64, timeout_ms: u64) -> usize {
        let candidates = self
            .registry
            .idle_candidates(now_ms, timeout_ms)
            .unwrap_or_default();
        let mut closed = 0;
        for (sid, _) in candidates {
            let Ok(Some((meta, aid))) = self.registry.get(&sid) else {
                continue;
            };
            if aid.is_empty() {
                continue;
            }
            if let Ok(driver) = self.agents.driver_for(&meta.agent) {
                if driver.close(&aid).is_ok() {
                    if let Ok(()) = self.registry.set_agent_session_id(&sid, "") {
                        closed += 1;
                    }
                }
            }
        }
        closed
    }

    // ---- 会话数据 ----

    /// 分页读对话历史（docs/DESIGN.md「session.history」）：`before` 为独占上界游标。
    pub async fn history(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<usize>,
    ) -> Result<(Vec<HistoryItem>, bool, usize), String> {
        self.registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let items = SessionLog::open(&self.data_dir, session_id).read_history();
        let limit = limit.unwrap_or(200);
        let (start, end, has_more) = Self::window_items(items.len(), limit, before);
        Ok((items[start..end].to_vec(), has_more, start))
    }

    /// 分页读活动历史（docs/DESIGN.md「session.activities」）：`before` 为独占上界游标。
    pub async fn activities(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<usize>,
    ) -> Result<(Vec<Activity>, bool, usize), String> {
        self.registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        let items = SessionLog::open(&self.data_dir, session_id).read_activities();
        let limit = limit.unwrap_or(200);
        let (start, end, has_more) = Self::window_items(items.len(), limit, before);
        Ok((items[start..end].to_vec(), has_more, start))
    }

    /// 查询正在进行中的活动（docs/DESIGN.md「session.ongoing_activity」；无则 None）。
    pub async fn ongoing_activity(&self, session_id: &str) -> Result<Option<Activity>, String> {
        self.registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        Ok(self.ongoing.lock().unwrap().get(session_id).cloned())
    }

    // ---- 交互 ----

    /// 发送指令（docs/DESIGN.md「session.prompt」）：busy 检查 → 首条生成标题 → 惰性创建
    /// agent 会话（session/new）→ resume → 跑 turn（事件喂 TurnMerger，记录 ongoing）→
    /// 写历史/活动 → 置空闲；必要时广播 `session.state_change`（Busy<->Idle）。
    pub async fn prompt(&self, session_id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        if input.is_empty() {
            return Err("prompt 输入必须非空".into());
        }
        // 忙检查 + 标题生成 + Busy 状态 + 懒创建一次完成（注册表为同步写，天然原子）
        let (driver, agent_session_id, cwd, old_state) = {
            let (mut meta, agent_session_id) = self
                .registry
                .get(session_id)
                .map_err(|e| format!("注册表读取失败: {e}"))?
                .ok_or_else(|| format!("会话不存在: {session_id}"))?;
            let old_state = meta.state;
            if meta.state == SessionState::Busy {
                return Err("会话忙：agent 不支持进行中注入（steer），请等待当前工作结束".into());
            }
            if meta.title.is_empty() {
                meta.title = generate_title(&first_text(&input));
            }
            meta.state = SessionState::Busy;
            meta.last_active_at = now();
            let driver = self.agents.driver_for(&meta.agent)?;
            // 惰性创建 agent 侧会话（session/new）并回填；已创建则沿用
            let agent_session_id = if agent_session_id.is_empty() {
                let sid2 = driver.create_session(&meta.cwd)?;
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
            (driver, agent_session_id, cwd, old_state)
        };

        // Busy 广播
        if old_state != SessionState::Busy {
            self.broadcast_state_change(session_id, old_state, SessionState::Busy);
        }

        // 继续既有会话：先经 ACP `session/resume` 恢复 agent 自身上下文（幂等）。
        if let Err(e) = driver.resume_session(&agent_session_id, &cwd) {
            protocol::log::error("server.session", format!("resume 失败 {session_id}: {e}"));
            let err = Activity::Error {
                timestamp: now(),
                detail: format!("恢复 agent 上下文失败: {e}"),
            };
            let _ = SessionLog::open(&self.data_dir, session_id).append_activities(&[err]);
            self.broadcast_state_change(session_id, SessionState::Busy, SessionState::Idle);
            let _ = self
                .registry
                .update_state(session_id, SessionState::Idle, now());
            self.ongoing.lock().unwrap().remove(session_id);
            return Err(format!("恢复 agent 上下文失败: {e}"));
        }

        let ts = now();
        let mut merger = TurnMerger::new();
        merger.push_user(input.clone(), ts);

        // 跑 turn：把指令发给 agent，事件喂合并器，thinking/tool_call 记为 ongoing 活动
        let mut turn_completed = false;
        let prompt_input = input;
        let started = std::time::Instant::now();
        let mut rx = driver.prompt(&agent_session_id, prompt_input);
        // 合并器已 push_user 一次，用户回显事件（UserMessage）忽略；驱动输出经事件驱动
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::UserMessage(_) => {
                    // 回显：合并器已 push_user，此处忽略
                }
                AgentEvent::TurnEnded => {
                    turn_completed = true;
                    break;
                }
                AgentEvent::OutputChunk(text) => merger.push_output(text, now()),
                AgentEvent::Thinking(text) => {
                    merger.push_thinking(text.clone(), now());
                    self.ongoing.lock().unwrap().insert(
                        session_id.to_string(),
                        Activity::Thinking {
                            timestamp: now(),
                            content: text,
                        },
                    );
                }
                AgentEvent::ToolCall {
                    name,
                    title,
                    content,
                } => {
                    merger.push_tool_call(name.clone(), title.clone(), content.clone(), now());
                    self.ongoing.lock().unwrap().insert(
                        session_id.to_string(),
                        Activity::ToolCall {
                            timestamp: now(),
                            name,
                            title,
                            content,
                        },
                    );
                }
                AgentEvent::Compaction(detail) => {
                    self.ongoing.lock().unwrap().insert(
                        session_id.to_string(),
                        Activity::Compaction {
                            timestamp: now(),
                            detail,
                        },
                    );
                }
                AgentEvent::SessionInfo { .. } => {}
            }
        }

        // 若 turn 未正常结束，记录错误活动
        let log = SessionLog::open(&self.data_dir, session_id);
        if !turn_completed {
            let err = Activity::Error {
                timestamp: now(),
                detail: "agent turn 未正常结束（连接中断或 turn 被异常终止）".into(),
            };
            merger.push_error(err);
        }

        // 落库历史 + 活动
        let (history, activities) = merger.finish();
        if !history.is_empty() {
            if let Err(e) = log.append_history(&history) {
                protocol::log::error("server.session", format!("历史落盘失败 {session_id}: {e}"));
            }
        }
        if !activities.is_empty() {
            if let Err(e) = log.append_activities(&activities) {
                protocol::log::error("server.session", format!("活动落盘失败 {session_id}: {e}"));
            }
        }

        // 置空闲 + 清空 ongoing + 广播
        self.ongoing.lock().unwrap().remove(session_id);
        let _ = self
            .registry
            .update_state(session_id, SessionState::Idle, now());
        self.broadcast_state_change(session_id, SessionState::Busy, SessionState::Idle);
        protocol::log::info(
            "server.session",
            format!(
                "prompt 完成 {session_id}（{}ms，turn_completed={turn_completed}）",
                started.elapsed().as_millis()
            ),
        );
        Ok(())
    }

    /// 取消指定普通会话正在进行的工作（docs/DESIGN.md「session.cancel」）。
    pub async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let (meta, agent_session_id) = self
            .registry
            .get(session_id)
            .map_err(|e| format!("注册表读取失败: {e}"))?
            .ok_or_else(|| format!("会话不存在: {session_id}"))?;
        if agent_session_id.is_empty() {
            return Ok(());
        }
        let driver = self.agents.driver_for(&meta.agent)?;
        driver.cancel(&agent_session_id)?;
        // 取消后该会话视为回到空闲，广播状态变更
        if meta.state == SessionState::Busy {
            let _ = self
                .registry
                .update_state(session_id, SessionState::Idle, now());
            self.broadcast_state_change(session_id, SessionState::Busy, SessionState::Idle);
        }
        Ok(())
    }

    fn broadcast_state_change(&self, session_id: &str, old: SessionState, new: SessionState) {
        let payload = SessionStateChange {
            session_id: session_id.to_string(),
            old_state: old,
            new_state: new,
        };
        let _ = self.tx.send(ServerNotification::StateChange(payload));
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
    use crate::agent::{AgentDriver, AgentEvent, AgentRegistry};
    use crate::history::SessionLog;
    use crate::registry::SessionRegistry;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    /// 测试直接构造 manager（独立临时数据目录）。
    fn stub_manager(agent: &str) -> (Arc<SessionManager>, broadcast::Receiver<ServerNotification>) {
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            agent,
            Arc::new(crate::agent::StubAgentDriver::new()),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-sess-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, rx) = SessionManager::new(agents, registry, dir);
        (Arc::new(mgr), rx)
    }

    /// 惰性会话：`session.new` 只写注册表，不触发 ACP；首条 prompt 懒创建 agent 侧会话。
    #[tokio::test]
    async fn create_is_lazy_until_first_prompt() {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        let dir = std::env::temp_dir().join(format!(
            "amux-lazy-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        let meta = mgr.create("codex", "/tmp/lazy").await.unwrap();
        // 创建后：注册表 agent_session_id 为空（未与 ACP 交互）
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(aid.is_empty(), "创建会话不应触发 ACP session/new");

        // 惰性创建会话后即为 Idle
        assert_eq!(meta.state, SessionState::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话列表惰性分页纯函数（docs/DESIGN.md「session.list」）。
    #[test]
    fn session_page_lazy_windows() {
        fn entry(id: &str, last: u64) -> RegistryEntry {
            (
                SessionMeta {
                    id: id.into(),
                    agent: "codex".into(),
                    cwd: "/tmp".into(),
                    state: SessionState::Idle,
                    title: String::new(),
                    created_at: 1,
                    last_active_at: last,
                },
                format!("agent_{id}"),
            )
        }
        let all = vec![
            entry("s9", 900),
            entry("s8", 800),
            entry("s7", 700),
            entry("s6", 600),
            entry("s5", 500),
            entry("s4", 400),
        ];

        let (w, more, nb) = SessionManager::session_page(&all, 2, None);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0.id, "s9");
        assert_eq!(w[1].0.id, "s8");
        assert!(more);
        assert_eq!(nb, Some(800));

        let (w, more, nb) = SessionManager::session_page(&all, 2, Some(800));
        assert_eq!(
            w.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(),
            ["s7", "s6"]
        );
        assert!(more);
        assert_eq!(nb, Some(600));

        let (w, more, nb) = SessionManager::session_page(&all, 2, Some(600));
        assert_eq!(
            w.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(),
            ["s5", "s4"]
        );
        assert!(!more);
        assert_eq!(nb, None);

        let (w, more, nb) = SessionManager::session_page(&[], 2, None);
        assert!(w.is_empty());
        assert!(!more);
        assert_eq!(nb, None);
    }

    /// 历史惰性加载切窗（纯函数）。
    #[test]
    fn window_items_lazy_loading_slices() {
        let (start, end, has_more) = SessionManager::window_items(1000, 200, None);
        assert_eq!((start, end, has_more), (800, 1000, true));
        let (start, end, has_more) = SessionManager::window_items(1000, 200, Some(800));
        assert_eq!((start, end, has_more), (600, 800, true));
        let (start, end, has_more) = SessionManager::window_items(1000, 200, Some(200));
        assert_eq!((start, end, has_more), (0, 200, false));
        let (start, end, has_more) = SessionManager::window_items(50, 200, None);
        assert_eq!((start, end, has_more), (0, 50, false));
    }

    /// prompt 后写历史与活动（合并粒度），分页读取，标题生成。
    #[tokio::test]
    async fn prompt_writes_history_and_activities_with_title() {
        let (mgr, mut rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/work").await.unwrap();

        mgr.prompt(&meta.id, text("实现登录功能")).await.unwrap();

        // 标题按首条指令生成
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].title, "实现登录功能");

        // 历史与活动已落盘
        let log = SessionLog::open(&mgr.data_dir, &meta.id);
        assert!(log.history_exists(), "prompt 后应写历史");
        assert!(log.activities_exists(), "prompt 后应写活动");

        // 分页读历史：用户消息 + 一条合并输出
        let (items, has_more, next_before) = mgr.history(&meta.id, None, None).await.unwrap();
        assert!(matches!(&items[0], HistoryItem::UserMessage { content, .. }
            if content.contains(&ContentBlock::Text { text: "实现登录功能".into() })));
        assert!(items.iter().any(|i| matches!(i, HistoryItem::AgentMessage { content, .. }
            if content.iter().any(|c| matches!(c, ContentBlock::Text { text } if text.contains("完成"))))));
        assert!(!has_more);
        assert_eq!(next_before, 0);

        // 分页读活动：thinking + tool_call
        let (acts, _, _) = mgr.activities(&meta.id, None, None).await.unwrap();
        assert!(acts.iter().any(|a| matches!(a, Activity::Thinking { .. })));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "read_file")));

        // 列表状态回到空闲
        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].state, SessionState::Idle);
        // ongoing 清空
        assert!(mgr.ongoing_activity(&meta.id).await.unwrap().is_none());

        // state_change：prompt 过程中应有 busy→idle
        let mut saw = false;
        while let Ok(n) = rx.recv().await {
            let ServerNotification::StateChange(c) = n;
            if c.session_id == meta.id
                && c.old_state == SessionState::Busy
                && c.new_state == SessionState::Idle
            {
                saw = true;
                break;
            }
        }
        assert!(saw, "prompt 结束应广播 busy→idle");
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }

    /// 删除触发 ACP session/close（driver.close 被调用），并清除注册表与日志。
    #[tokio::test]
    async fn delete_triggers_driver_close() {
        // 用计数关闭驱动验证 close 被调用
        struct Tracking {
            closed: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl AgentDriver for Tracking {
            fn create_session(&self, cwd: &str) -> Result<String, String> {
                Ok(format!("agent_{}", cwd.replace('/', "_")))
            }
            fn resume_session(&self, _a: &str, _c: &str) -> Result<(), String> {
                Ok(())
            }
            fn prompt(
                &self,
                _a: &str,
                _i: Vec<ContentBlock>,
            ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                tokio::spawn(async move {
                    let _ = tx.send(AgentEvent::OutputChunk("输出".into())).await;
                    let _ = tx.send(AgentEvent::TurnEnded).await;
                });
                rx
            }
            fn cancel(&self, _a: &str) -> Result<(), String> {
                Ok(())
            }
            fn close(&self, _a: &str) -> Result<(), String> {
                self.closed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn list_skills(&self) -> Vec<String> {
                Vec::new()
            }
            fn shutdown(&self) {}
        }

        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "track",
            Arc::new(Tracking {
                closed: closed.clone(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-del-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());
        let meta = mgr.create("track", "/tmp/work").await.unwrap();
        mgr.prompt(&meta.id, text("hi")).await.unwrap();
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 0);

        mgr.delete(&meta.id).await.unwrap();
        assert_eq!(
            closed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "删除应触发一次 driver.close（ACP session/close）"
        );
        assert!(registry.get(&meta.id).unwrap().is_none(), "注册表应已删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未 prompt 的会话可删除（无 agent 侧会话则无需 close）。
    #[tokio::test]
    async fn delete_unprompted_needs_no_close() {
        let (mgr, _rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/noop").await.unwrap();
        mgr.delete(&meta.id).await.unwrap();
        assert!(mgr.registry.get(&meta.id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }

    /// 会话不存在错误。
    #[tokio::test]
    async fn missing_session_errors() {
        let (mgr, _rx) = stub_manager("codex");
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.history("nope", None, None).await.is_err());
        assert!(mgr.delete("nope").await.is_err());
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }
}
