//! 会话管理：会话列表与历史权威在 server。
//! - 会话元数据持久化于 SQLite（`session.sqlite`），列表由 server 维护
//! - busy/idle 状态在 server 维护并存注册表；每次 Busy<->Idle 变更广播 `session.state_change`
//! - 惰性会话：`session.new` 只写注册表，agent 侧会话延后到首条指令（`session.prompt`）
//!   懒创建（ACP `session/new`），已有 agent 会话先经 `session/resume` 恢复
//! - 删除会话先经 ACP `session/close` 释放资源，再尝试 `session/delete`；长时间无活动
//!   会话只经 `session/close` 关闭并保留 server 历史
//! - 对话历史与活动历史落 `data_dir/sessions/<id>_history.jsonl` / `<id>_activities.jsonl`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use protocol::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionMeta, SessionState,
    SessionStateChange,
};

use crate::agent::{AgentEvent, AgentRegistry};
use crate::error::SessionError;
use crate::history::{SessionLog, TurnMerger};
use crate::registry::{RegistryEntry, SessionRegistry};

/// server → GUI 的会话状态通知。
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
    controls: Mutex<HashMap<String, Arc<SessionControl>>>,
    /// 已解析历史缓存（GUI 每 10s 轮询打开的会话；文件未变时免全量 JSONL 重解析）
    history_cache: Mutex<HashMap<String, LogCache<HistoryItem>>>,
    /// 已解析活动缓存（同上）
    activities_cache: Mutex<HashMap<String, LogCache<Activity>>>,
}

/// 日志解析缓存条目：以文件字节长度为新鲜度依据——日志 append-only 不截断，
/// 长度不变即内容不变；任何追加后由写入方失效。
struct LogCache<T> {
    items: Arc<Vec<T>>,
    source_len: u64,
}

struct SessionControl {
    busy: AtomicBool,
    deleted: AtomicBool,
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
        // 异常退出后注册表中的 Busy 会话已没有对应的运行中 agent。
        if let Err(e) = registry.reset_busy_to_idle() {
            amux_common::log::error("server.session", format!("重置残留忙会话失败：{e}"));
        }
        let manager = SessionManager {
            agents,
            registry,
            data_dir,
            tx,
            ongoing: Mutex::new(HashMap::new()),
            controls: Mutex::new(HashMap::new()),
            history_cache: Mutex::new(HashMap::new()),
            activities_cache: Mutex::new(HashMap::new()),
        };
        (manager, rx)
    }

    /// 会话列表惰性分页（纯函数，可单测）：
    /// `all` 已按最近活跃时间和会话 ID 降序排列；`before` 是上一页末尾生成的
    /// 不透明游标。复合游标避免会话列表变化或时间戳相同时重复、跳过条目。
    pub fn session_page(
        all: &[RegistryEntry],
        limit: usize,
        before: Option<&str>,
    ) -> (Vec<RegistryEntry>, bool, Option<String>) {
        let start = before
            .and_then(|cursor| cursor.split_once(':'))
            .and_then(|(timestamp, id)| Some((timestamp.parse::<u64>().ok()?, id)))
            .and_then(|(timestamp, id)| {
                all.iter().position(|(meta, _)| {
                    meta.last_active_at < timestamp
                        || (meta.last_active_at == timestamp && meta.id.as_str() < id)
                })
            })
            .unwrap_or(0);
        let limit = limit.max(1);
        let end = start.saturating_add(limit).min(all.len());
        let window = all[start..end].to_vec();
        let has_more = end < all.len();
        let next_before = has_more
            .then(|| window.last())
            .flatten()
            .map(|(meta, _)| format!("{}:{}", meta.last_active_at, meta.id));
        (window, has_more, next_before)
    }

    /// 惰性加载切窗（纯函数）：按 limit 与 before 游标计算 [start, end) 与是否还有更早。
    pub fn window_items(len: usize, limit: usize, before: Option<usize>) -> (usize, usize, bool) {
        let end = before.unwrap_or(len).min(len);
        let start = end.saturating_sub(limit);
        (start, end, start > 0)
    }

    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    /// 新建普通会话（**惰性**：只写注册表立即返回，不触发 ACP；agent 侧会话延后到
    /// 首条指令时经 `session/new` 懒创建）。
    pub async fn create(&self, agent: &str, cwd: &str) -> Result<SessionMeta, SessionError> {
        let id = format!("s_{}", uuid::Uuid::new_v4());
        let ts = now();
        amux_common::log::info(
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
        self.registry.upsert(&meta, "")?;
        self.control(&meta.id);
        Ok(meta)
    }

    fn control(&self, session_id: &str) -> Arc<SessionControl> {
        let mut controls = self.controls.lock().unwrap();
        controls
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionControl {
                    busy: AtomicBool::new(false),
                    deleted: AtomicBool::new(false),
                })
            })
            .clone()
    }

    /// 配置会话标题（用户可随时修改）。
    pub async fn configure(&self, session_id: &str, title: &str) -> Result<(), SessionError> {
        self.get_entry(session_id)?;
        self.registry.set_title(session_id, title.trim(), now())?;
        Ok(())
    }

    /// 删除会话：若已有 agent 侧会话，先经 ACP
    /// `session/close` 关闭；若 ACP Server 支持会话删除，再发 `session/delete`
    /// （不支持删除的 agent 报错，按「不支持」忽略）。ACP 失败不阻断本地删除。
    /// 联动清除注册表条目 + 历史日志 + 活动日志 + 进行中控制块。
    /// 幂等：会话已不存在时仅清理残留日志（部分工作流清理失败后可安全重试）。
    pub async fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        let control = self.control(session_id);
        control.deleted.store(true, Ordering::SeqCst);
        let log = SessionLog::open(&self.data_dir, session_id);
        let entry = self.registry.get(session_id)?;
        let Some((meta, agent_session_id)) = entry else {
            return log
                .remove()
                .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")));
        };
        if !agent_session_id.is_empty() {
            match self.agents.driver_for(&meta.agent) {
                Ok(driver) => {
                    if let Err(e) = driver.close(&agent_session_id) {
                        amux_common::log::error(
                            "server.session",
                            format!("关闭 ACP 会话失败（继续本地删除）{session_id}: {e}"),
                        );
                    }
                    if let Err(e) = driver.delete_session(&agent_session_id) {
                        amux_common::log::debug(
                            "server.session",
                            format!("agent 不支持或删除 ACP 会话失败（忽略）{session_id}: {e}"),
                        );
                    }
                }
                Err(e) => {
                    amux_common::log::error(
                        "server.session",
                        format!("解析 agent 驱动失败（继续本地删除）{session_id}: {e}"),
                    );
                }
            }
        }
        self.registry.delete(session_id)?;
        log.remove()
            .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")))?;
        self.invalidate_log_caches(session_id);
        // 控制块出 map：进行中的 prompt 持有 Arc 克隆仍能看到 deleted 标志；
        // 新请求将得到全新（未删除）的控制块——但会话已不在注册表，NotFound 兜底。
        self.controls.lock().unwrap().remove(session_id);
        amux_common::log::info("server.session", format!("删除会话 {session_id}"));
        Ok(())
    }

    /// 惰性分页会话列表：按最近活跃降序切窗。
    pub async fn list(
        &self,
        limit: Option<usize>,
        before: Option<String>,
    ) -> Result<(Vec<SessionMeta>, bool, Option<String>), SessionError> {
        let all = self.registry.list()?;
        let (window, has_more, next_before) =
            Self::session_page(&all, limit.unwrap_or(50), before.as_deref());
        let metas = window.into_iter().map(|(m, _)| m).collect();
        Ok((metas, has_more, next_before))
    }

    /// 批量查询指定会话。不存在的 id 静默跳过。
    pub async fn info(&self, session_ids: &[String]) -> Result<Vec<SessionMeta>, SessionError> {
        let mut metas = Vec::new();
        for id in session_ids {
            if self.registry.get(id)?.is_some() {
                metas.push(self.get_entry(id)?.0);
            }
        }
        Ok(metas)
    }

    /// 注册表单条读取：不存在 → NotFound，存储故障 → Storage。
    fn get_entry(&self, session_id: &str) -> Result<(SessionMeta, String), SessionError> {
        self.registry
            .get(session_id)?
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    /// 返回普通会话绑定的工作目录。workspace RPC 不接受调用方自带 cwd，
    /// 避免借助已知 session id 浏览或修改另一目录。
    pub fn workspace_cwd(&self, session_id: &str) -> Result<String, SessionError> {
        Ok(self.get_entry(session_id)?.0.cwd)
    }

    /// 关闭长时间无活动的 agent 侧会话（>timeout_ms，
    /// 无活动会话」）。候选选出后复核状态：已回到 Busy 的会话跳过本轮
    /// （避免关掉正在进行中的 turn 的 agent 侧会话）。
    pub async fn close_idle(&self, now_ms: u64, timeout_ms: u64) -> Result<usize, SessionError> {
        let candidates = self
            .registry
            .idle_candidates(now_ms, timeout_ms)
            .unwrap_or_default();
        let mut closed = 0;
        for (sid, _) in candidates {
            let Ok(Some((meta, aid))) = self.registry.get(&sid) else {
                continue;
            };
            if aid.is_empty() || meta.state == SessionState::Busy {
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
        Ok(closed)
    }

    /// 分页读对话历史：`before` 为独占上界游标（条目下标，u64 统一协议游标类型）。
    pub async fn history(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<HistoryItem>, bool, Option<u64>), SessionError> {
        self.get_entry(session_id)?;
        let items = self.cached_history(session_id)?;
        let limit = limit.unwrap_or(200);
        let before_usize = before.map(|b| b as usize);
        let (start, end, has_more) = Self::window_items(items.len(), limit, before_usize);
        let next_before = if has_more { Some(start as u64) } else { None };
        Ok((items[start..end].to_vec(), has_more, next_before))
    }

    /// 分页读活动历史：`before` 为独占上界游标（条目下标，u64 统一协议游标类型）。
    pub async fn activities(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<Activity>, bool, Option<u64>), SessionError> {
        self.get_entry(session_id)?;
        let items = self.cached_activities(session_id)?;
        let limit = limit.unwrap_or(200);
        let before_usize = before.map(|b| b as usize);
        let (start, end, has_more) = Self::window_items(items.len(), limit, before_usize);
        let next_before = if has_more { Some(start as u64) } else { None };
        Ok((items[start..end].to_vec(), has_more, next_before))
    }

    /// 带缓存的对话历史读取：文件长度未变时复用上次解析结果
    /// （GUI 每 10s 轮询打开的会话；日志 append-only，长度不变即内容不变）。
    fn cached_history(&self, session_id: &str) -> Result<Arc<Vec<HistoryItem>>, SessionError> {
        let path = SessionLog::history_path(&self.data_dir, session_id);
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut cache = self.history_cache.lock().unwrap();
        if let Some(c) = cache.get(session_id) {
            if c.source_len == file_len {
                return Ok(c.items.clone());
            }
        }
        let items = Arc::new(
            SessionLog::open(&self.data_dir, session_id)
                .read_history()
                .map_err(|e| SessionError::Storage(format!("会话历史读取失败: {e}")))?,
        );
        cache.insert(
            session_id.to_string(),
            LogCache {
                items: items.clone(),
                source_len: file_len,
            },
        );
        Ok(items)
    }

    /// 带缓存的活动读取：文件长度未变时复用上次解析结果。
    fn cached_activities(&self, session_id: &str) -> Result<Arc<Vec<Activity>>, SessionError> {
        let path = SessionLog::activities_path(&self.data_dir, session_id);
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut cache = self.activities_cache.lock().unwrap();
        if let Some(c) = cache.get(session_id) {
            if c.source_len == file_len {
                return Ok(c.items.clone());
            }
        }
        let items = Arc::new(
            SessionLog::open(&self.data_dir, session_id)
                .read_activities()
                .map_err(|e| SessionError::Storage(format!("会话活动读取失败: {e}")))?,
        );
        cache.insert(
            session_id.to_string(),
            LogCache {
                items: items.clone(),
                source_len: file_len,
            },
        );
        Ok(items)
    }

    /// 追加后失效缓存（下次读取重新解析一次，之后恢复命中）。
    fn invalidate_log_caches(&self, session_id: &str) {
        self.history_cache.lock().unwrap().remove(session_id);
        self.activities_cache.lock().unwrap().remove(session_id);
    }

    /// 查询正在进行中的活动；无则 None。
    pub async fn ongoing_activity(
        &self,
        session_id: &str,
    ) -> Result<Option<Activity>, SessionError> {
        self.get_entry(session_id)?;
        Ok(self.ongoing.lock().unwrap().get(session_id).cloned())
    }

    /// 发送指令：busy 检查 → 首条生成标题 → 惰性创建
    /// agent 会话（session/new）→ resume → 用户消息立即落盘 → 跑 turn（事件喂
    /// TurnMerger，记录 ongoing）→ 写 agent 历史/活动 → 置空闲；必要时广播
    /// `session.state_change`（Busy<->Idle）。落盘失败向上传播（GUI 可见）。
    pub async fn prompt(
        &self,
        session_id: &str,
        input: Vec<ContentBlock>,
    ) -> Result<(), SessionError> {
        if input.is_empty() {
            return Err(SessionError::EmptyInput);
        }
        let control = self.control(session_id);
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        if control
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(SessionError::Busy);
        }

        let setup = self.setup_prompt(session_id, &input).await;
        let (driver, agent_session_id, cwd, old_state) = match setup {
            Ok(value) => value,
            Err(error) => {
                // 尚未进入 Busy 广播，只需释放 busy 标志
                control.busy.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };

        if old_state != SessionState::Busy {
            self.broadcast_state_change(
                session_id,
                old_state,
                SessionState::Busy,
                protocol::StateChangeReason::Completed,
            );
        }

        let ts = now();
        let log = SessionLog::open(&self.data_dir, session_id);
        let user_message = HistoryItem::UserMessage {
            content: input.clone(),
            timestamp: ts,
        };
        if let Err(e) = log.append_history(std::slice::from_ref(&user_message)) {
            amux_common::log::error(
                "server.session",
                format!("用户消息落盘失败 {session_id}: {e}"),
            );
            self.finalize_turn(
                session_id,
                &control,
                false,
                protocol::StateChangeReason::Aborted,
            );
            return Err(SessionError::Storage(format!("用户消息落盘失败: {e}")));
        }
        self.invalidate_log_caches(session_id);

        // 继续既有会话：先经 ACP `session/resume` 恢复 agent 自身上下文（幂等）。
        if let Err(e) = driver.resume_session(&agent_session_id, &cwd) {
            amux_common::log::error("server.session", format!("resume 失败 {session_id}: {e}"));
            let err = Activity::Error {
                timestamp: now(),
                detail: format!("恢复 agent 上下文失败: {e}"),
            };
            if let Err(log_error) = log.append_activities(&[err]) {
                amux_common::log::error(
                    "server.session",
                    format!("resume 错误活动落盘失败 {session_id}: {log_error}"),
                );
            }
            self.invalidate_log_caches(session_id);
            self.finalize_turn(
                session_id,
                &control,
                false,
                protocol::StateChangeReason::Aborted,
            );
            return Err(SessionError::AgentUnavailable(format!(
                "恢复 agent 上下文失败: {e}"
            )));
        }

        let started = std::time::Instant::now();
        let (storage_error, turn_reason) = self
            .run_turn(session_id, &driver, &agent_session_id, input, &control)
            .await;

        self.finalize_turn(session_id, &control, false, turn_reason);
        amux_common::log::info(
            "server.session",
            format!(
                "prompt 完成 {session_id}（{}ms）",
                started.elapsed().as_millis()
            ),
        );
        match (control.deleted.load(Ordering::SeqCst), storage_error) {
            (true, _) => Err(SessionError::NotFound(format!("{session_id}（已删除）"))),
            (false, Some(e)) => Err(e),
            (false, None) => Ok(()),
        }
    }

    /// prompt 前置准备：读元数据、生成标题、置 Busy、惰性创建 agent 侧会话。
    async fn setup_prompt(
        &self,
        session_id: &str,
        input: &[ContentBlock],
    ) -> Result<(crate::agent::SharedDriver, String, String, SessionState), SessionError> {
        let (mut meta, agent_session_id) = self.get_entry(session_id)?;
        let old_state = meta.state;
        if meta.state == SessionState::Busy {
            return Err(SessionError::Busy);
        }
        if meta.title.is_empty() {
            meta.title = generate_title(&first_text(input));
        }
        meta.state = SessionState::Busy;
        meta.last_active_at = now();
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        let agent_session_id = if agent_session_id.is_empty() {
            let sid2 = driver
                .create_session(&meta.cwd)
                .map_err(SessionError::AgentUnavailable)?;
            self.registry.set_agent_session_id(session_id, &sid2)?;
            sid2
        } else {
            agent_session_id
        };
        self.registry.upsert(&meta, &agent_session_id)?;
        let cwd = meta.cwd.clone();
        Ok((driver, agent_session_id, cwd, old_state))
    }

    /// 跑单个 turn：指令发给 agent，事件喂合并器，thinking/tool_call 记为 ongoing。
    /// 返回（落盘阶段的存储错误、turn 结束原因）。删除与 prompt 并发时，
    /// 旧 turn 不得在删除后重新创建历史文件。
    async fn run_turn(
        &self,
        session_id: &str,
        driver: &crate::agent::SharedDriver,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
        control: &SessionControl,
    ) -> (Option<SessionError>, protocol::StateChangeReason) {
        let log = SessionLog::open(&self.data_dir, session_id);
        let mut merger = TurnMerger::new();
        let mut rx = driver.prompt(agent_session_id, input);
        let mut turn_completed = false;
        // 中断时没有 ACP stopReason，保持 aborted。
        let mut turn_reason = protocol::StateChangeReason::Aborted;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::TurnEnded(reason) => {
                    turn_completed = true;
                    turn_reason = reason;
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
                AgentEvent::Error(detail) => {
                    merger.push_error(Activity::Error {
                        timestamp: now(),
                        detail: detail.clone(),
                    });
                    amux_common::log::error(
                        "server.session",
                        format!("agent turn 失败 {session_id}: {detail}"),
                    );
                }
            }
        }
        // 连接中断或异常终止的 turn 也要留下可见错误活动。
        if !turn_completed {
            let err = Activity::Error {
                timestamp: now(),
                detail: "agent turn 未正常结束（连接中断或 turn 被异常终止）".into(),
            };
            merger.push_error(err);
        }

        let deleted = control.deleted.load(Ordering::SeqCst);
        let (history, activities) = merger.finish();
        let mut storage_error: Option<SessionError> = None;
        if !deleted && !history.is_empty() {
            if let Err(e) = log.append_history(&history) {
                amux_common::log::error(
                    "server.session",
                    format!("历史落盘失败 {session_id}: {e}"),
                );
                storage_error = Some(SessionError::Storage(format!("历史落盘失败: {e}")));
            }
        }
        if !deleted && !activities.is_empty() {
            if let Err(e) = log.append_activities(&activities) {
                amux_common::log::error(
                    "server.session",
                    format!("活动落盘失败 {session_id}: {e}"),
                );
                storage_error = Some(SessionError::Storage(format!("活动落盘失败: {e}")));
            }
        }
        if !deleted && (!history.is_empty() || !activities.is_empty()) {
            self.invalidate_log_caches(session_id);
        }
        (storage_error, turn_reason)
    }

    /// turn 统一收尾：清 ongoing、释放 busy、置 Idle 并广播结束原因
    /// （deleted 时跳过状态回写，避免已删除会话在注册表中复活）。
    fn finalize_turn(
        &self,
        session_id: &str,
        control: &SessionControl,
        deleted: bool,
        reason: protocol::StateChangeReason,
    ) {
        self.ongoing.lock().unwrap().remove(session_id);
        control.busy.store(false, Ordering::SeqCst);
        if !deleted {
            if let Err(e) = self
                .registry
                .update_state(session_id, SessionState::Idle, now())
            {
                amux_common::log::error(
                    "server.session",
                    format!("更新空闲状态失败 {session_id}: {e}"),
                );
            }
            self.broadcast_state_change(session_id, SessionState::Busy, SessionState::Idle, reason);
        }
    }

    /// 取消指定普通会话正在进行的工作。
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        if agent_session_id.is_empty() {
            return Ok(());
        }
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        driver
            .cancel(&agent_session_id)
            .map_err(SessionError::AgentUnavailable)?;
        // 只请求 ACP 取消，不提前伪造 Idle；prompt 事件流结束后才释放 busy，
        // 从而避免旧 turn 尚未结束时被新的 prompt 并发启动。
        Ok(())
    }

    fn broadcast_state_change(
        &self,
        session_id: &str,
        old: SessionState,
        new: SessionState,
        reason: protocol::StateChangeReason,
    ) {
        let payload = SessionStateChange {
            session_id: session_id.to_string(),
            old_state: old,
            new_state: new,
            reason,
        };
        let _ = self.tx.send(ServerNotification::StateChange(payload));
    }
}

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
    use tokio::sync::Notify;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

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
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(aid.is_empty(), "创建会话不应触发 ACP session/new");

        assert_eq!(meta.state, SessionState::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        assert_eq!(nb, Some("800:s8".into()));

        let (w, more, nb) = SessionManager::session_page(&all, 2, Some("800:s8"));
        assert_eq!(
            w.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(),
            ["s7", "s6"]
        );
        assert!(more);
        assert_eq!(nb, Some("600:s6".into()));

        let (w, more, nb) = SessionManager::session_page(&all, 2, Some("600:s6"));
        assert_eq!(
            w.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(),
            ["s5", "s4"]
        );
        assert!(!more);
        assert_eq!(nb, None);

        let (w, more, nb) = SessionManager::session_page(&[], 2, None);
        assert!(w.is_empty());
        assert!(!more);
        assert_eq!(nb, None);
    }

    #[test]
    fn session_page_keeps_same_timestamp_entries() {
        let entry = |id: &str| {
            (
                SessionMeta {
                    id: id.into(),
                    agent: "codex".into(),
                    cwd: "/tmp".into(),
                    state: SessionState::Idle,
                    title: String::new(),
                    created_at: 1,
                    last_active_at: 100,
                },
                String::new(),
            )
        };
        let all = vec![entry("s3"), entry("s2"), entry("s1")];
        let (first, more, cursor) = SessionManager::session_page(&all, 2, None);
        assert_eq!(first.len(), 2);
        assert!(more);
        let (second, more, next) = SessionManager::session_page(&all, 2, cursor.as_deref());
        assert_eq!(
            second.iter().map(|e| e.0.id.as_str()).collect::<Vec<_>>(),
            ["s1"]
        );
        assert!(!more);
        assert_eq!(next, None);
    }

    #[test]
    fn session_page_cursor_survives_newer_session() {
        let entry = |id: &str, last_active_at| {
            (
                SessionMeta {
                    id: id.into(),
                    agent: "codex".into(),
                    cwd: "/tmp".into(),
                    state: SessionState::Idle,
                    title: String::new(),
                    created_at: 1,
                    last_active_at,
                },
                String::new(),
            )
        };
        let first_snapshot = vec![entry("s3", 300), entry("s2", 200), entry("s1", 100)];
        let (first, _, cursor) = SessionManager::session_page(&first_snapshot, 2, None);
        assert_eq!(first.last().unwrap().0.id, "s2");

        let changed_snapshot = vec![
            entry("new", 400),
            entry("s3", 300),
            entry("s2", 200),
            entry("s1", 100),
        ];
        let (second, _, _) = SessionManager::session_page(&changed_snapshot, 2, cursor.as_deref());
        assert_eq!(
            second
                .iter()
                .map(|entry| entry.0.id.as_str())
                .collect::<Vec<_>>(),
            ["s1"]
        );
    }

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

    #[tokio::test]
    async fn prompt_writes_history_and_activities_with_title() {
        let (mgr, mut rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/work").await.unwrap();

        mgr.prompt(&meta.id, text("实现登录功能")).await.unwrap();

        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].title, "实现登录功能");

        let log = SessionLog::open(&mgr.data_dir, &meta.id);
        assert!(log.history_exists(), "prompt 后应写历史");
        assert!(log.activities_exists(), "prompt 后应写活动");

        let (items, has_more, next_before) = mgr.history(&meta.id, None, None).await.unwrap();
        assert!(matches!(&items[0], HistoryItem::UserMessage { content, .. }
            if content.contains(&ContentBlock::Text { text: "实现登录功能".into() })));
        assert!(items.iter().any(|i| matches!(i, HistoryItem::AgentMessage { content, .. }
            if content.iter().any(|c| matches!(c, ContentBlock::Text { text } if text.contains("完成"))))));
        assert!(!has_more);
        assert_eq!(next_before, None);

        let (acts, _, _) = mgr.activities(&meta.id, None, None).await.unwrap();
        assert!(acts.iter().any(|a| matches!(a, Activity::Thinking { .. })));
        assert!(acts
            .iter()
            .any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "read_file")));

        let (list, _, _) = mgr.list(None, None).await.unwrap();
        assert_eq!(list[0].state, SessionState::Idle);
        assert!(mgr.ongoing_activity(&meta.id).await.unwrap().is_none());

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
    #[tokio::test]
    async fn prompt_persists_user_message_before_turn_ends() {
        struct BlockingDriver {
            started: Arc<Notify>,
            release: Arc<Notify>,
        }

        impl AgentDriver for BlockingDriver {
            fn create_session(&self, _cwd: &str) -> Result<String, String> {
                Ok("agent_blocking".into())
            }

            fn resume_session(&self, _agent_session_id: &str, _cwd: &str) -> Result<(), String> {
                Ok(())
            }

            fn prompt(
                &self,
                _agent_session_id: &str,
                _input: Vec<ContentBlock>,
            ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                let started = self.started.clone();
                let release = self.release.clone();
                tokio::spawn(async move {
                    started.notify_one();
                    release.notified().await;
                    let _ = tx
                        .send(AgentEvent::TurnEnded(
                            protocol::StateChangeReason::Completed,
                        ))
                        .await;
                });
                rx
            }

            fn cancel(&self, _agent_session_id: &str) -> Result<(), String> {
                Ok(())
            }

            fn close(&self, _agent_session_id: &str) -> Result<(), String> {
                Ok(())
            }

            fn delete_session(&self, _agent_session_id: &str) -> Result<(), String> {
                Ok(())
            }

            fn list_skills(&self) -> Result<Vec<String>, String> {
                Ok(Vec::new())
            }

            fn shutdown(&self) {}
        }

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "blocking",
            Arc::new(BlockingDriver {
                started: started.clone(),
                release: release.clone(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-immediate-history-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (manager, _rx) = SessionManager::new(agents, registry, dir.clone());
        let manager = Arc::new(manager);
        let meta = manager.create("blocking", "/tmp/work").await.unwrap();
        let session_id = meta.id.clone();

        let prompt_manager = manager.clone();
        let prompt_task =
            tokio::spawn(async move { prompt_manager.prompt(&session_id, text("立即保存")).await });
        started.notified().await;

        let log = SessionLog::open(&dir, &meta.id);
        let history = log.read_history().unwrap();
        assert!(matches!(
            history.as_slice(),
            [HistoryItem::UserMessage { content, .. }]
                if content == &text("立即保存")
        ));

        release.notify_one();
        prompt_task.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn delete_triggers_driver_close() {
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
                    let _ = tx
                        .send(AgentEvent::TurnEnded(
                            protocol::StateChangeReason::Completed,
                        ))
                        .await;
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
            fn delete_session(&self, _a: &str) -> Result<(), String> {
                Err("method not found".into())
            }
            fn list_skills(&self) -> Result<Vec<String>, String> {
                Ok(Vec::new())
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
    #[tokio::test]
    async fn delete_unprompted_needs_no_close() {
        let (mgr, _rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/noop").await.unwrap();
        mgr.delete(&meta.id).await.unwrap();
        mgr.delete(&meta.id).await.expect("重复删除应保持幂等");
        assert!(mgr.registry.get(&meta.id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }
    #[tokio::test]
    async fn missing_session_errors() {
        let (mgr, _rx) = stub_manager("codex");
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.history("nope", None, None).await.is_err());
        assert!(mgr.delete("nope").await.is_ok());
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }
}
