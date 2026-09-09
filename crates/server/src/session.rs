//! 会话管理：会话列表与历史权威在 server。
//! - 会话元数据持久化于 SQLite（`session.sqlite`），列表由 server 维护
//! - busy/idle 状态在 server 维护并存注册表；每次 Busy<->Idle 变更广播 `session.state_change`
//! - 惰性会话：`session.new` 只写注册表，agent 侧会话延后到首条指令
//!   （`session.prompt`）或查询/设置会话选项时懒创建（ACP `session/new`），
//!   已有 agent 会话先经 `session/resume` 恢复
//! - 会话选项存储在内存，以 Agent 侧数据为权威
//! - 删除会话先经 ACP `session/close` 释放资源，再尝试 `session/delete`；长时间无活动
//!   会话只经 `session/close` 关闭并保留 server 历史
//! - 对话历史与活动历史落 `data_dir/sessions/<id>_history.jsonl` / `<id>_activities.jsonl`

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use protocol::{
    generate_title, ActivitiesResult, Activity, ContentBlock, HistoryItem, HistoryResult,
    SessionMeta, SessionState, SessionStateChange,
};

use crate::agent::{AcpAgentDriver, AgentEvent, AgentRegistry};
use crate::error::SessionError;
use crate::git::GitRunner;
use crate::history::{SessionLog, TurnMerger};
use crate::registry::SessionRegistry;

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
    /// 会话选项：内存存储，以 Agent 侧数据为权威；`session/new` /
    /// `session/resume` / `session/set_config_option` 响应以及
    /// `config_option_update` 通知均全量覆盖内存。
    config_options: Mutex<HashMap<String, Vec<protocol::SessionConfigOption>>>,
    /// 进行中的活动（`session.ongoing_activity`；按会话 id 独立存储）
    ongoing: Mutex<HashMap<String, Activity>>,
    /// 当前未定型思考块的累积文本（多 chunk 拼接）。
    /// `ongoing` 中的 `Activity::Thinking.content` 写入时引用这里，保证 GUI 看到
    /// 的是「当前思考块已流式输出的前一部分」而不是最新一个流式片段。
    /// 思考块被 TurnMerger 定稿（tool_call/error 到达）时随之重置，
    /// 与 `ongoing` 生命周期一致：turn 结束随 `ongoing` 一起清理。
    thinking_buf: Mutex<HashMap<String, ThinkingBuffer>>,
    controls: Mutex<HashMap<String, Arc<SessionControl>>>,
}

struct SessionControl {
    /// 进行中的 turn 数：0 = 空闲。并发 prompt 均直接转发给 ACP server
    /// （是否受理由 agent 决定），用计数而非布尔跟踪忙闲。
    turns: AtomicUsize,
    deleted: AtomicBool,
    /// 串行化删除与 agent 侧会话创建/元数据写回，避免删除竞态下会话复活。
    lifecycle: Mutex<()>,
}

#[derive(Debug, Clone)]
struct ThinkingBuffer {
    content: String,
    /// 首个 chunk 的时间戳；None 表示尚未收到 chunk。
    first_timestamp: Option<u64>,
}

/// prompt 前置准备产物：具名字段替代 4 元组返回，
/// 避免相邻 String（agent_session_id / cwd）解构错位。
struct PromptSetup {
    driver: Arc<AcpAgentDriver>,
    agent_session_id: String,
    cwd: String,
    old_state: SessionState,
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
        let manager = SessionManager {
            agents,
            registry,
            data_dir,
            tx,
            config_options: Mutex::new(HashMap::new()),
            ongoing: Mutex::new(HashMap::new()),
            thinking_buf: Mutex::new(HashMap::new()),
            controls: Mutex::new(HashMap::new()),
        };
        (manager, rx)
    }

    pub fn agents(&self) -> &AgentRegistry {
        &self.agents
    }

    /// 新建普通会话（**agent 侧会话惰性**，**worktree 立即创建**）：只写注册表
    /// 立即返回，不触发 ACP；agent 侧会话延后到首条指令时经 `session/new`
    /// 懒创建。`use_worktree` 时在 `~/.amux/worktrees/`（data_dir 同级）下确定路径并立即执行
    /// `git worktree add`——失败时注册表尚未落盘，整体回滚返回错误。
    pub async fn create(
        &self,
        agent: &str,
        cwd: &str,
        use_worktree: bool,
    ) -> Result<SessionMeta, SessionError> {
        let id = format!("s_{}", uuid::Uuid::new_v4());
        let ts = now();
        log::info!("新建会话 {id}（agent={agent} cwd={cwd}，agent 侧会话延后创建）");
        let worktree_dir = if use_worktree {
            // 前置校验：非 git 仓库直接报错，避免落到 raw git 输出
            if !GitRunner::new().is_repo(cwd) {
                return Err(SessionError::Storage(format!(
                    "工作目录不是 git 仓库，无法启用 worktree: {cwd}"
                )));
            }
            let repo_name = std::path::Path::new(cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into());
            // 随机串取 UUID 前 8 位：同仓库多会话并存、路径可读且免碰撞
            let rand: String = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            let target = self
                .data_dir
                .parent()
                .unwrap_or(std::path::Path::new("/tmp"))
                .join("worktrees")
                .join(format!("{repo_name}-{rand}"));
            // Worktree 在创建会话时落盘，而非等到首条指令。
            GitRunner::new()
                .create_worktree(cwd, &target)
                .map_err(SessionError::Storage)?;
            target.to_string_lossy().into_owned()
        } else {
            String::new()
        };
        let meta = SessionMeta {
            id,
            agent: agent.to_string(),
            cwd: cwd.to_string(),
            state: SessionState::Idle,
            title: String::new(),
            created_at: ts,
            last_active_at: ts,
            worktree_dir,
        };
        self.registry.upsert(&meta, None)?;
        self.controls
            .lock()
            .insert(meta.id.clone(), Self::new_control());
        Ok(meta)
    }

    fn new_control() -> Arc<SessionControl> {
        Arc::new(SessionControl {
            turns: AtomicUsize::new(0),
            deleted: AtomicBool::new(false),
            lifecycle: Mutex::new(()),
        })
    }

    fn control(&self, session_id: &str) -> Result<Arc<SessionControl>, SessionError> {
        if let Some(control) = self.controls.lock().get(session_id).cloned() {
            return Ok(control);
        }
        // Do not allocate a control block for arbitrary/nonexistent IDs. This method is
        // reached by RPC paths before the operation-specific lookup; creating entries
        // first would let repeated invalid requests grow the map without bound.
        if self.registry.get(session_id)?.is_none() {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let mut controls = self.controls.lock();
        Ok(controls
            .entry(session_id.to_string())
            .or_insert_with(Self::new_control)
            .clone())
    }

    /// 仅移除仍由调用方持有的控制块。删除并发时，旧请求可能在首轮注册表
    /// 检查后才创建控制块；按 Arc 身份比较可避免它清掉后续请求已经接管的条目。
    fn remove_control_if_current(&self, session_id: &str, control: &Arc<SessionControl>) {
        let mut controls = self.controls.lock();
        if controls
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, control))
        {
            controls.remove(session_id);
        }
    }

    fn remove_control_if_not_found(
        &self,
        session_id: &str,
        control: &Arc<SessionControl>,
        error: &SessionError,
    ) {
        if matches!(error, SessionError::NotFound(_)) {
            self.remove_control_if_current(session_id, control);
        }
    }

    /// 配置会话标题（用户可随时修改）。
    pub async fn configure(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<(), SessionError> {
        if let Some(title) = title {
            self.get_entry(session_id)?;
            self.registry.set_title(session_id, title.trim(), now())?;
        }
        Ok(())
    }

    /// 会话生效工作目录：启用 worktree 时为工作树，否则用户指定目录。
    /// worktree 被过期清理时按记录路径原地重建（清理不删元数据，路径一致），
    /// 重建失败视为存储类错误向上传播。
    fn resolve_cwd(&self, session_id: &str, meta: &SessionMeta) -> Result<String, SessionError> {
        if meta.worktree_dir.is_empty() {
            return Ok(meta.cwd.clone());
        }
        let wt = PathBuf::from(&meta.worktree_dir);
        if !wt.exists() {
            GitRunner::new()
                .rebuild_worktree(&meta.cwd, &wt)
                .map_err(|e| {
                    SessionError::Storage(format!("重建 worktree 失败 {}: {e}", wt.display()))
                })?;
            log::info!("已按原路径重建过期清理的 worktree: {}", wt.display());
            // 重建由用户访问（发指令/查看目录）触发，即视为会话活跃，
            // 否则刚重建的 worktree 会在下一轮清理被立即回收
            self.registry.update_state(session_id, meta.state, now())?;
        }
        Ok(meta.worktree_dir.clone())
    }

    /// 覆盖写入内存中的会话选项：以 Agent 侧数据为权威，new/resume 响应、
    /// set_config_option 响应与 config_option_update 通知均全量覆盖。
    fn store_config_options(&self, session_id: &str, options: Vec<protocol::SessionConfigOption>) {
        self.config_options
            .lock()
            .insert(session_id.to_string(), options);
    }

    /// 惰性创建 agent 侧会话：发送指令或查询会话选项时才经 `session/new` 创建；
    /// new 响应携带的会话选项存入内存。已有 agent 侧会话时原样返回。
    fn ensure_agent_session(
        &self,
        session_id: &str,
        meta: &SessionMeta,
        agent_session_id: Option<&str>,
    ) -> Result<(Arc<AcpAgentDriver>, String), SessionError> {
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        if let Some(existing) = agent_session_id {
            return Ok((driver, existing.to_string()));
        }
        let cwd = self.resolve_cwd(session_id, meta)?;
        let (sid, options) = driver
            .create_session(&cwd)
            .map_err(SessionError::AgentUnavailable)?;
        self.registry.set_agent_session_id(session_id, Some(&sid))?;
        self.store_config_options(session_id, options);
        Ok((driver, sid))
    }

    /// 查询会话选项：触发惰性创建/恢复，并返回内存中以 Agent 侧数据为权威的集合。
    pub async fn config_options(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SessionConfigOption>, SessionError> {
        let control = self.control(session_id)?;
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let entry = match self.get_entry(session_id) {
            Ok(entry) => entry,
            Err(error) => {
                self.remove_control_if_not_found(session_id, &control, &error);
                return Err(error);
            }
        };
        let (driver, agent_session_id) =
            self.ensure_agent_session(session_id, &entry.meta, entry.agent_session_id.as_deref())?;
        let meta = entry.meta;
        // 已有 agent 侧会话：先经 ACP `session/resume` 恢复（幂等），响应携带的
        // 最新选项全量覆盖内存存储；幂等 resume 返回空集合时保持既有选项。
        let cwd = self.resolve_cwd(session_id, &meta)?;
        match driver.resume_session(&agent_session_id, &cwd) {
            Ok(options) => {
                if !options.is_empty() {
                    self.store_config_options(session_id, options);
                }
            }
            Err(e) => log::error!("查询会话选项时 resume 失败 {session_id}: {e}"),
        }
        Ok(self.current_config_options(session_id))
    }

    /// 获取已存在的 agent 侧会话；不会触发惰性创建。
    fn existing_agent_session(
        &self,
        session_id: &str,
    ) -> Result<Option<(Arc<AcpAgentDriver>, String)>, SessionError> {
        let entry = self.get_entry(session_id)?;
        let Some(agent_session_id) = entry.agent_session_id else {
            return Ok(None);
        };
        let driver = self
            .agents
            .driver_for(&entry.meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        Ok(Some((driver, agent_session_id)))
    }

    /// 查询会话斜杠命令：内存缓存以 Agent 侧数据为权威，由 ACP
    /// `available_commands_update` 通知驱动。
    /// 查询不触发惰性创建：尚无 agent 侧会话时返回空（agent 侧会话创建后
    /// agent 才会下发命令集合）。
    pub async fn slash_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SlashCommand>, SessionError> {
        let Some((driver, agent_session_id)) = self.existing_agent_session(session_id)? else {
            return Ok(Vec::new());
        };
        Ok(driver.available_commands(&agent_session_id))
    }

    /// 查询会话计划：内存缓存以 Agent 侧数据为权威，由 ACP `plan` 通知驱动。
    /// 查询不触发惰性创建：尚无
    /// agent 侧会话时返回空。
    pub async fn plan(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SessionPlanEntry>, SessionError> {
        let Some((driver, agent_session_id)) = self.existing_agent_session(session_id)? else {
            return Ok(Vec::new());
        };
        Ok(driver.session_plan(&agent_session_id))
    }

    /// 查询会话上下文信息（内存存储，以 Agent 侧数据为权威；
    /// 查询本身不创建或恢复 agent 侧会话）。
    pub async fn context(
        &self,
        session_id: &str,
    ) -> Result<protocol::SessionContextResult, SessionError> {
        self.get_entry(session_id)?;
        Ok(self.registry.context(session_id))
    }

    /// 删除会话：若已有 agent 侧会话，先经 ACP
    /// `session/close` 关闭；若 ACP Server 支持会话删除，再发 `session/delete`
    /// （不支持删除的 agent 报错，按「不支持」忽略）。ACP 失败不阻断本地删除。
    /// 联动清除注册表条目 + 历史日志 + 活动日志 + 进行中控制块。
    /// 幂等：会话已不存在时仅清理残留日志（部分工作流清理失败后可安全重试）。
    pub async fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        // 不存在的 ID 仍保持删除幂等，但不能为随机 ID 创建控制块。
        let entry = self.registry.get(session_id)?;
        if entry.is_none() {
            return SessionLog::open(&self.data_dir, session_id)
                .remove()
                .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")));
        }
        let control = self.control(session_id)?;
        control.deleted.store(true, Ordering::SeqCst);
        let _lifecycle = control.lifecycle.lock();
        let log = SessionLog::open(&self.data_dir, session_id);
        let Some(entry) = self.registry.get(session_id)? else {
            // 另一条删除请求可能已经完成本地删除；当前控制块仍需移除。
            self.remove_control_if_current(session_id, &control);
            return log
                .remove()
                .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")));
        };
        let meta = entry.meta;
        let agent_session_id = entry.agent_session_id;
        // 即刻生效；agent 往返与 worktree 清理较慢，交由后台任务异步完成。
        self.registry.delete(session_id)?;
        log.remove()
            .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")))?;

        // 资源清理包括 ACP close/delete 往返和 worktree 目录清理，
        // 均为尽力而为，不阻断本地删除。
        let agents = self.agents.clone();
        let data_dir = self.data_dir.clone();
        let session_id2 = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(agent_session_id) = agent_session_id {
                match agents.driver_for(&meta.agent) {
                    Ok(driver) => {
                        if let Err(e) = driver.close(&agent_session_id) {
                            log::error!("关闭 ACP 会话失败（继续本地删除）{session_id2}: {e}");
                        }
                        if let Err(e) = driver.delete_session(&agent_session_id) {
                            log::debug!(
                                "agent 不支持或删除 ACP 会话失败（忽略）{session_id2}: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        log::error!("解析 agent 驱动失败（继续本地删除）{session_id2}: {e}");
                    }
                }
            }
            if !meta.worktree_dir.is_empty() {
                let wt = std::path::PathBuf::from(&meta.worktree_dir);
                if wt.exists() {
                    GitRunner::new().remove_worktree(&meta.cwd, &wt);
                }
            }
            // 会话日志文件可能在删除前一刻仍有流式追加（并发 turn 兜底），再清一次
            let _ = SessionLog::open(&data_dir, &session_id2).remove();
            log::info!("会话资源清理完成 {session_id2}");
        });

        // 控制块出 map：进行中的 prompt 持有 Arc 克隆仍能看到 deleted 标志；
        // 新请求将得到全新（未删除）的控制块——但会话已不在注册表，NotFound 兜底。
        self.controls.lock().remove(session_id);
        self.config_options.lock().remove(session_id);
        log::info!("删除会话 {session_id}");
        Ok(())
    }

    /// 惰性分页会话列表：按最近活跃降序切窗。
    /// 按数量查询最近活跃的普通会话：
    /// 返回按最近活跃排序的前缀（至多 limit 条），has_more 表示是否还有更多。
    pub async fn list(
        &self,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionMeta>, bool), SessionError> {
        let limit = limit.unwrap_or(protocol::SESSION_LIST_DEFAULT_LIMIT).max(1);
        let (entries, has_more) = self.registry.list(limit)?;
        let metas = entries.into_iter().map(|entry| entry.meta).collect();
        Ok((metas, has_more))
    }

    /// 批量查询指定会话。不存在的 id 静默跳过。
    pub async fn info(&self, session_ids: &[String]) -> Result<Vec<SessionMeta>, SessionError> {
        let mut metas = Vec::new();
        for id in session_ids {
            // 单次查询同时完成「是否存在」与「取元数据」，避免重复读注册表
            if let Some(entry) = self.registry.get(id)? {
                metas.push(entry.meta);
            }
        }
        Ok(metas)
    }

    /// 注册表单条读取：不存在 → NotFound，存储故障 → Storage。
    fn get_entry(&self, session_id: &str) -> Result<crate::registry::RegistryEntry, SessionError> {
        self.registry
            .get(session_id)?
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    /// 返回普通会话绑定的工作目录。workspace RPC 不接受调用方自带 cwd，
    /// 避免借助已知 session id 浏览或修改另一目录。
    /// worktree 会话：agent 实际工作在 worktree，
    /// 改动视图（diff/restore/list/read）应作用于 worktree 目录而非原始目录；
    /// worktree 已被过期清理时按原路径重建。
    pub fn workspace_cwd(&self, session_id: &str) -> Result<String, SessionError> {
        let meta = self.get_entry(session_id)?.meta;
        self.resolve_cwd(session_id, &meta)
    }

    /// 关闭长时间无活动的 agent 侧会话（>timeout_ms）。候选选出后复核状态：
    /// 已回到 Busy 的会话跳过本轮（避免关掉正在进行中的 turn 的 agent 侧会话）。
    pub async fn close_idle(
        &self,
        now_ms: u64,
        idle_timeout: std::time::Duration,
    ) -> Result<usize, SessionError> {
        let candidates = self.registry.idle_candidates(now_ms, idle_timeout)?;
        let mut closed = 0;
        for candidate in candidates {
            let sid = candidate.session_id;
            let Ok(control) = self.control(&sid) else {
                continue;
            };
            let _lifecycle = control.lifecycle.lock();
            if control.deleted.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(Some(entry)) = self.registry.get(&sid) else {
                continue;
            };
            let meta = entry.meta;
            let Some(aid) = entry.agent_session_id else {
                continue;
            };
            if meta.state == SessionState::Busy {
                continue;
            }
            if let Ok(driver) = self.agents.driver_for(&meta.agent) {
                if driver.close(&aid).is_ok() {
                    if let Ok(()) = self.registry.set_agent_session_id(&sid, None) {
                        // agent 侧会话已关闭，内存中的会话选项随之失效；
                        // 下次交互惰性重建时以 Agent 侧数据重新覆盖
                        self.config_options.lock().remove(&sid);
                        closed += 1;
                    }
                }
            }
        }
        Ok(closed)
    }

    /// 清理超过 `timeout_ms` 不活跃会话的 worktree。仅清理关联 worktree，会话本身
    /// 保留；元数据中的 worktree_dir 一并保留，agent 工作目录与 workspace RPC
    /// 后续访问时按原路径惰性重建。删除失败的 worktree 等待下轮清理重试。
    /// 返回清理数量。
    pub async fn cleanup_idle_worktrees(
        &self,
        now_ms: u64,
        idle_timeout: std::time::Duration,
    ) -> Result<usize, SessionError> {
        let candidates = self
            .registry
            .idle_worktree_candidates(now_ms, idle_timeout)?;
        let mut cleaned = 0;
        for candidate in candidates {
            let sid = candidate.session_id;
            let Ok(control) = self.control(&sid) else {
                continue;
            };
            let _lifecycle = control.lifecycle.lock();
            if control.deleted.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(Some(entry)) = self.registry.get(&sid) else {
                continue;
            };
            let meta = entry.meta;
            if meta.state != SessionState::Idle || meta.worktree_dir != candidate.worktree_dir {
                continue;
            }
            let wt = PathBuf::from(&candidate.worktree_dir);
            if !wt.exists() {
                // 上轮已清理（元数据保留）：目录不存在即无需再清理
                continue;
            }
            GitRunner::new().remove_worktree(&candidate.cwd, &wt);
            if !wt.exists() {
                cleaned += 1;
            }
        }
        Ok(cleaned)
    }

    /// 分页读对话历史：`before` 为独占上界游标（条目下标，u64 统一协议游标类型）。
    pub async fn history(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<HistoryResult, SessionError> {
        self.get_entry(session_id)?;
        let limit = limit.unwrap_or(protocol::SESSION_PAGE_DEFAULT_LIMIT).max(1);
        let (items, has_more, next_before) = SessionLog::open(&self.data_dir, session_id)
            .read_history_page(limit, before)
            .map_err(|e| SessionError::Storage(format!("会话历史读取失败: {e}")))?;
        Ok(HistoryResult {
            items,
            has_more,
            next_before,
        })
    }

    /// 分页读活动历史：`before` 为独占上界游标（条目下标，u64 统一协议游标类型）。
    pub async fn activities(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<ActivitiesResult, SessionError> {
        self.get_entry(session_id)?;
        let limit = limit.unwrap_or(protocol::SESSION_PAGE_DEFAULT_LIMIT).max(1);
        let (activities, has_more, next_before) = SessionLog::open(&self.data_dir, session_id)
            .read_activities_page(limit, before)
            .map_err(|e| SessionError::Storage(format!("会话活动读取失败: {e}")))?;
        Ok(ActivitiesResult {
            activities,
            has_more,
            next_before,
        })
    }

    /// 查询正在进行中的活动；无则 None。
    pub async fn ongoing_activity(
        &self,
        session_id: &str,
    ) -> Result<Option<Activity>, SessionError> {
        self.get_entry(session_id)?;
        Ok(self.ongoing.lock().get(session_id).cloned())
    }

    /// 发送指令：busy 检查 → 首条生成标题 → 惰性创建
    /// agent 会话（session/new）→ resume → 用户消息立即落盘 → 跑 turn（事件喂
    /// TurnMerger，记录 ongoing）→ 写 agent 历史/活动 → 置空闲；必要时广播
    /// `session.state_change`（Busy<->Idle）。落盘失败向上传播（GUI 可见）。
    ///
    /// 不做本地忙时拒绝：turn 进行中收到的新 prompt 照样转发给 ACP server，
    /// 是否受理（steer/排队/报错）由 agent 决定；agent 以错误响应拒绝时经
    /// `PromptFailed` 上报为请求错误。同会话多个 turn 并发时以 `turns` 计数
    /// 维护忙闲，最后一个 turn 结束才回空闲。
    pub async fn prompt(
        &self,
        session_id: &str,
        input: Vec<ContentBlock>,
    ) -> Result<(), SessionError> {
        if input.is_empty() {
            return Err(SessionError::EmptyInput);
        }
        let control = self.control(session_id)?;
        // 先取得生命周期锁再递增 turns，使 prompt 与 cancel/delete 有明确的
        // 线性化顺序；否则 cancel 可能在 setup_prompt 写回 Busy 前读到旧的 Idle。
        let lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        // 删除会先标记 deleted 并等待这把锁；持有期间完成 agent session 创建、
        // Busy 元数据写回和用户消息首写，避免删除后旧 prompt 再创建日志。
        let setup = self.setup_prompt(session_id, &input, &control);
        let PromptSetup {
            driver,
            agent_session_id,
            cwd,
            old_state,
        } = match setup {
            Ok(value) => value,
            Err(error) => {
                // 若会话已被删除，移除这次竞态中刚创建的孤儿控制块。
                self.remove_control_if_not_found(session_id, &control, &error);
                return Err(error);
            }
        };
        control.turns.fetch_add(1, Ordering::SeqCst);

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
            log::error!("用户消息落盘失败 {session_id}: {e}");
            drop(lifecycle);
            self.finalize_turn(session_id, &control, protocol::StateChangeReason::Aborted);
            return Err(SessionError::Storage(format!("用户消息落盘失败: {e}")));
        }
        // 从这里开始删除可以安全清理日志；后续 turn 只会追加活动/历史，且均受
        // deleted 标记保护，不会在删除后重新创建已删除会话。
        drop(lifecycle);

        // 继续既有会话：先经 ACP `session/resume` 恢复 agent 自身上下文（幂等）。
        // 恢复响应携带的最新配置选项（幂等 resume 返回空）全量覆盖内存存储。
        match driver.resume_session(&agent_session_id, &cwd) {
            Ok(options) => {
                if !options.is_empty() {
                    self.store_config_options(session_id, options);
                }
            }
            Err(e) => {
                log::error!("resume 失败 {session_id}: {e}");
                let err = Activity::Error {
                    timestamp: now(),
                    error: format!("恢复 agent 上下文失败: {e}"),
                };
                let _lifecycle = control.lifecycle.lock();
                if !control.deleted.load(Ordering::SeqCst) {
                    if let Err(log_error) = log.append_activities(&[err]) {
                        log::error!("resume 错误活动落盘失败 {session_id}: {log_error}");
                    }
                }
                drop(_lifecycle);
                self.finalize_turn(session_id, &control, protocol::StateChangeReason::Aborted);
                return Err(SessionError::AgentUnavailable(format!(
                    "恢复 agent 上下文失败: {e}"
                )));
            }
        }

        let started = std::time::Instant::now();
        let (storage_error, turn_reason) = self
            .run_turn(session_id, &driver, &agent_session_id, input, &control)
            .await;

        self.finalize_turn(session_id, &control, turn_reason);
        log::info!(
            "prompt 完成 {session_id}（{}ms）",
            started.elapsed().as_millis()
        );
        match (control.deleted.load(Ordering::SeqCst), storage_error) {
            (true, _) => Err(SessionError::NotFound(format!("{session_id}（已删除）"))),
            (false, Some(e)) => Err(e),
            (false, None) => Ok(()),
        }
    }

    /// prompt 前置准备：读元数据、生成标题、置 Busy、惰性创建 agent 侧会话。
    /// turn 进行中（meta.state == Busy）不拒绝：原样置 Busy 落盘并返回，
    /// 受理与否由 agent 决定。
    fn setup_prompt(
        &self,
        session_id: &str,
        input: &[ContentBlock],
        control: &SessionControl,
    ) -> Result<PromptSetup, SessionError> {
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let entry = self.get_entry(session_id)?;
        let mut meta = entry.meta;
        let agent_session_id = entry.agent_session_id;
        let old_state = meta.state;
        // （agent_session_id 现为 Option：None = agent 侧会话尚未惰性创建）
        if meta.title.is_empty() {
            meta.title = generate_title(&first_text(input));
        }
        meta.state = SessionState::Busy;
        meta.last_active_at = now();
        // worktree 在 session.new 已落盘；被过期清理后此处按原路径惰性重建。
        // agent 实际工作目录：启用 worktree 时为工作树，否则用户指定目录。
        // create/resume 共用此值，GUI 的 workspace/diff RPC 也按它下发。
        let cwd = self.resolve_cwd(session_id, &meta)?;
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        let (driver, agent_session_id) = match agent_session_id {
            Some(id) => (driver, id),
            // 惰性创建 agent 侧会话，响应中的会话选项存入内存
            //（选项以 Agent 侧数据为权威）
            None => self.ensure_agent_session(session_id, &meta, None)?,
        };
        // 创建与 resume 分支统一 upsert：busy、首条 prompt 生成的标题与活跃时间
        // 立即落盘：状态以元数据为权威，避免 turn 进行中列表读到陈旧空闲。
        // resume 分支若只更新状态，先查过会话选项的会话（agent 侧会话已提前
        // 创建）首条 prompt 生成的标题将永远不落盘。
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        self.registry.upsert(&meta, Some(&agent_session_id))?;
        Ok(PromptSetup {
            driver,
            agent_session_id,
            cwd,
            old_state,
        })
    }

    /// 跑单个 turn：指令发给 agent，事件喂合并器，thinking/tool_call 记为 ongoing。
    /// 返回（落盘阶段的存储错误、turn 结束原因）。删除与 prompt 并发时，
    /// 旧 turn 不得在删除后重新创建历史文件。
    async fn run_turn(
        &self,
        session_id: &str,
        driver: &Arc<AcpAgentDriver>,
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
        // ACP server 以错误响应拒绝本次 prompt（turn 未开始）：作为请求错误上报
        let mut prompt_failed: Option<String> = None;
        let mut storage_error: Option<SessionError> = None;
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
                    // 累积写入 thinking_buf，再让 ongoing 引用累积内容——
                    // 否则多个流式 chunk 到达时，GUI 看到的「思考中」只会是
                    // 最新一段，落盘的历史活动（merger 累积）反而更全，行为不一致。
                    // buf 只覆盖当前未定型的思考块：tool_call/error 定稿后已重置，
                    // 与 merger「每个思考块一条活动」的语义保持同步。
                    let ts = now();
                    let (accumulated, first_ts) = {
                        let mut buf = self.thinking_buf.lock();
                        let entry =
                            buf.entry(session_id.to_string())
                                .or_insert_with(|| ThinkingBuffer {
                                    content: String::new(),
                                    first_timestamp: Some(ts),
                                });
                        entry.content.push_str(&text);
                        (entry.content.clone(), entry.first_timestamp.unwrap_or(ts))
                    };
                    self.ongoing.lock().insert(
                        session_id.to_string(),
                        Activity::Thinking {
                            timestamp: first_ts,
                            thinking: accumulated,
                        },
                    );
                }
                AgentEvent::ToolCall {
                    id,
                    name,
                    title,
                    parameters,
                } => {
                    merger.push_tool_call(
                        id.clone(),
                        name.clone(),
                        title.clone(),
                        parameters.clone(),
                        now(),
                    );
                    self.ongoing.lock().insert(
                        session_id.to_string(),
                        Activity::ToolCall {
                            timestamp: now(),
                            tool_call_id: id,
                            tool_name: name
                                .unwrap_or_else(|| crate::history::DEFAULT_TOOL_NAME.into()),
                            title,
                            parameters,
                        },
                    );
                    // merger 已定稿当前思考块；重置 buf 使后续思考开启新块，
                    // 否则 ongoing 会一直携带第一段思考的内容。
                    self.thinking_buf.lock().remove(session_id);
                }
                AgentEvent::Error(detail) => {
                    merger.push_error(Activity::Error {
                        timestamp: now(),
                        error: detail.clone(),
                    });
                    // push_error 同样定稿思考块，同步重置。
                    self.thinking_buf.lock().remove(session_id);
                    log::error!("agent turn 失败 {session_id}: {detail}");
                }
                AgentEvent::PromptFailed(detail) => {
                    merger.push_error(Activity::Error {
                        timestamp: now(),
                        error: detail.clone(),
                    });
                    self.thinking_buf.lock().remove(session_id);
                    log::error!("agent 拒绝 prompt {session_id}: {detail}");
                    prompt_failed = Some(detail);
                }
                AgentEvent::UsageUpdate { used, size } => {
                    // 记录 ACP 提供的会话上下文大小（内存存储，以 Agent 侧为权威）。
                    if !control.deleted.load(Ordering::SeqCst) {
                        self.registry.set_context_size(session_id, used, size);
                    }
                }
                AgentEvent::ConfigOptions(options) => {
                    // 会话配置选项变更（ACP config_options_update）：全量覆盖内存存储
                    if !control.deleted.load(Ordering::SeqCst) {
                        self.store_config_options(session_id, options);
                    }
                }
            }
            // 活动实时逐条落盘：thinking 累积到 tool_call/error 才定稿，
            // 定稿即写，不等 turn 结束。删除与 prompt 并发时旧 turn 不得
            // 重新创建活动文件，故由 flush_ready_activities 与删除共用生命周期锁。
            if let Err(e) = self.flush_ready_activities(session_id, &log, &mut merger, control) {
                storage_error = Some(e);
            }
        }
        // 连接中断或异常终止的 turn 也要留下可见错误活动。
        if !turn_completed {
            let err = Activity::Error {
                timestamp: now(),
                error: "agent turn 未正常结束（连接中断或 turn 被异常终止）".into(),
            };
            merger.push_error(err);
        }

        let (history, activities) = merger.finish();
        let _lifecycle = control.lifecycle.lock();
        if !control.deleted.load(Ordering::SeqCst) {
            if !history.is_empty() {
                if let Err(e) = log.append_history(&history) {
                    log::error!("历史落盘失败 {session_id}: {e}");
                    storage_error = Some(SessionError::Storage(format!("历史落盘失败: {e}")));
                }
            }
            if !activities.is_empty() {
                if let Err(e) = log.append_activities(&activities) {
                    log::error!("活动落盘失败 {session_id}: {e}");
                    storage_error = Some(SessionError::Storage(format!("活动落盘失败: {e}")));
                }
            }
        }
        if let Some(detail) = prompt_failed {
            storage_error = Some(SessionError::PromptFailed(detail));
        }
        (storage_error, turn_reason)
    }

    /// 把已定稿的活动实时追加写盘。返回落盘错误（写盘后仍会继续跑 turn，
    /// 仅收集错误供调用方上报）。
    fn flush_ready_activities(
        &self,
        session_id: &str,
        log: &SessionLog,
        merger: &mut TurnMerger,
        control: &SessionControl,
    ) -> Result<(), SessionError> {
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            merger.take_ready();
            return Ok(());
        }
        let ready = merger.take_ready();
        if ready.is_empty() {
            return Ok(());
        }
        if let Err(e) = log.append_activities(&ready) {
            log::error!("活动落盘失败 {session_id}: {e}");
            return Err(SessionError::Storage(format!("活动落盘失败: {e}")));
        }
        Ok(())
    }

    /// turn 统一收尾：递减进行中计数；归零时清 ongoing、置 Idle 并广播结束
    /// 原因（并发 turn 未全部结束则保持 Busy，ongoing 交由余下 turn 继续）。
    /// deleted 时跳过状态回写，避免已删除会话在注册表中复活。
    fn finalize_turn(
        &self,
        session_id: &str,
        control: &SessionControl,
        reason: protocol::StateChangeReason,
    ) {
        let _lifecycle = control.lifecycle.lock();
        let deleted = control.deleted.load(Ordering::SeqCst);
        if control.turns.fetch_sub(1, Ordering::SeqCst) > 1 {
            return;
        }
        self.ongoing.lock().remove(session_id);
        self.thinking_buf.lock().remove(session_id);
        if !deleted {
            if let Err(e) = self
                .registry
                .update_state(session_id, SessionState::Idle, now())
            {
                log::error!("更新空闲状态失败 {session_id}: {e}");
            }
            self.broadcast_state_change(session_id, SessionState::Busy, SessionState::Idle, reason);
        }
    }

    /// 取消指定普通会话正在进行的工作。
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        let control = self.control(session_id)?;
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let entry = self.get_entry(session_id)?;
        let meta = entry.meta;
        // 空闲会话（或尚无 agent 侧会话）无可取消：ACP agent 对未知 turn
        // 会报错，这里幂等返回成功、不透传——GUI 的取消按钮是常驻的，
        // 调用方无需自行区分忙闲。控制块是并发状态权威，注册表可能仍在
        // prompt 初始化的写回窗口内保持 Idle。
        if control.turns.load(Ordering::SeqCst) == 0 {
            return Ok(());
        }
        let Some(agent_session_id) = entry.agent_session_id else {
            return Ok(());
        };
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        driver
            .cancel(&agent_session_id)
            .map_err(SessionError::AgentUnavailable)?;
        // 只请求 ACP 取消，不提前伪造 Idle；turn 事件流结束后才递减 turns，
        // 会话忙闲始终由真实在途的 turn 数决定。
        Ok(())
    }

    /// 设置会话配置选项：Server 向 ACP Server
    /// 发送 `session/set_config_option` 请求进行设置。尚无 agent 侧会话时先
    /// 惰性创建；响应中的会话选项全量覆盖内存存储，返回更新后的完整选项集合。
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: protocol::SessionConfigOptionValue,
    ) -> Result<Vec<protocol::SessionConfigOption>, SessionError> {
        let control = self.control(session_id)?;
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let entry = match self.get_entry(session_id) {
            Ok(entry) => entry,
            Err(error) => {
                self.remove_control_if_not_found(session_id, &control, &error);
                return Err(error);
            }
        };
        let (driver, agent_session_id) =
            self.ensure_agent_session(session_id, &entry.meta, entry.agent_session_id.as_deref())?;
        log::info!(
            "会话选项设置请求：session={session_id} agent_session={agent_session_id} config_id={config_id} value={value:?}"
        );
        let options = driver
            .set_config_option(&agent_session_id, config_id, value)
            .map_err(SessionError::AgentUnavailable)?;
        if options.is_empty() {
            // agent 未在响应中携带 configOptions（如不支持该回执）：与 resume
            // 幂等返回空集合的语义一致，保留既有选项而非覆盖成空
            log::info!("会话选项设置响应未携带 configOptions，保留既有选项");
            return Ok(self.current_config_options(session_id));
        }
        self.store_config_options(session_id, options.clone());
        Ok(options)
    }

    /// 当前内存中的会话选项（无则空）。
    fn current_config_options(&self, session_id: &str) -> Vec<protocol::SessionConfigOption> {
        self.config_options
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
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
    use crate::registry::SessionRegistry;
    use protocol::ContentBlock;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text {
            text: s.to_string(),
        }]
    }

    /// 测试注册表：禁用自动发现、不配置驱动。本模块用例均不触达 agent；
    /// 触达 agent 的用例在 tests/session_manager.rs，经 mock_acp 子进程走真实路径。
    fn test_agents() -> Arc<AgentRegistry> {
        std::env::set_var("AMUX_NO_DISCOVERY", "1");
        Arc::new(AgentRegistry::new(None))
    }

    fn manager_at(
        dir: &std::path::Path,
    ) -> (
        Arc<SessionManager>,
        Arc<SessionRegistry>,
        broadcast::Receiver<ServerNotification>,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, rx) = SessionManager::new(test_agents(), registry.clone(), dir.to_path_buf());
        (Arc::new(mgr), registry, rx)
    }

    #[tokio::test]
    async fn missing_session_requests_do_not_allocate_controls() {
        let dir = std::env::temp_dir().join(format!(
            "amux-sess-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (manager, _registry, _rx) = manager_at(&dir);
        assert!(manager.controls.lock().is_empty());

        assert!(matches!(
            manager.prompt("missing", text("hello")).await,
            Err(SessionError::NotFound(_))
        ));
        assert!(matches!(
            manager.config_options("missing").await,
            Err(SessionError::NotFound(_))
        ));
        assert!(matches!(
            manager
                .set_config_option(
                    "missing",
                    "model",
                    protocol::SessionConfigOptionValue::ValueId {
                        value: "fast".into(),
                    },
                )
                .await,
            Err(SessionError::NotFound(_))
        ));
        assert!(manager.delete("missing").await.is_ok());
        assert!(manager.controls.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn git(cwd: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} 失败: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn commit_repo(repo: &std::path::Path) {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-b", "main", "-q"]);
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "init", "-q"]);
    }

    #[tokio::test]
    async fn cleanup_idle_worktrees_removes_stale_keeps_recent() {
        // data_dir 嵌套一层：worktree 根落在用例沙箱内（data_dir 同级 worktrees/）
        let case = std::env::temp_dir().join(format!(
            "amux-wt-cleanup-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        commit_repo(&case.join("repo"));
        let (mgr, registry, _rx) = manager_at(&case.join("server"));

        // 超时会话：worktree 应被清理，元数据保留，后续访问按原路径重建
        let stale = mgr
            .create("codex", case.join("repo").to_str().unwrap(), true)
            .await
            .unwrap();
        let stale_wt = PathBuf::from(&stale.worktree_dir);
        assert!(stale_wt.is_dir());
        // 把 last_active_at 拨回 8 天前（>7 天超时阈值）
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let old_ts = now_ms - 8 * 24 * 3_600_000;
        registry
            .update_state(&stale.id, SessionState::Idle, old_ts)
            .unwrap();

        // 近期会话：worktree 保留
        let recent = mgr
            .create("codex", case.join("repo").to_str().unwrap(), true)
            .await
            .unwrap();
        let recent_wt = PathBuf::from(&recent.worktree_dir);
        assert!(recent_wt.is_dir());

        let cleaned = mgr
            .cleanup_idle_worktrees(now_ms, std::time::Duration::from_secs(7 * 24 * 3_600))
            .await
            .unwrap();
        assert_eq!(cleaned, 1, "仅超期会话的 worktree 被清理");
        assert!(!stale_wt.exists(), "超期 worktree 应被删除");
        assert!(recent_wt.is_dir(), "近期 worktree 应保留");
        let stored = registry.get(&stale.id).unwrap().unwrap();
        assert_eq!(
            stored.meta.worktree_dir, stale.worktree_dir,
            "清理保留 worktree 元数据"
        );
        let list = git(&case.join("repo"), &["worktree", "list", "--porcelain"]);
        assert!(
            !list.contains(stale.worktree_dir.trim()),
            "主仓库不应再登记已清理 worktree"
        );

        // 后续访问按原路径重建，并检回原分支（分支名 = 目录 basename）
        assert_eq!(
            mgr.workspace_cwd(&stale.id).unwrap(),
            stale.worktree_dir,
            "访问工作目录触发按原路径重建"
        );
        assert!(stale_wt.is_dir(), "重建的 worktree 落在同一目录");
        let branch = git(&stale_wt, &["branch", "--show-current"]);
        assert_eq!(
            branch.trim(),
            stale_wt.file_name().unwrap().to_str().unwrap(),
            "重建 worktree 应检回原工作分支"
        );
        let list = git(&case.join("repo"), &["worktree", "list", "--porcelain"]);
        assert!(
            list.contains(stale.worktree_dir.trim()),
            "重建后主仓库重新登记 worktree"
        );

        // 再次清理：重建已视为会话活跃（last_active_at 刷新），不再入候选
        let cleaned = mgr
            .cleanup_idle_worktrees(now_ms, std::time::Duration::from_secs(7 * 24 * 3_600))
            .await
            .unwrap();
        assert_eq!(cleaned, 0, "刚重建的 worktree 不应被下一轮清理立即回收");

        let _ = std::fs::remove_dir_all(&case);
    }

    #[tokio::test]
    async fn worktree_create_rejects_non_git_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "amux-wt-norepo-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (mgr, registry, _rx) = manager_at(&dir);

        // 非 git 目录启用 worktree 应直接拒绝
        let err = mgr
            .create("codex", "/tmp", true)
            .await
            .expect_err("非 git 仓库应失败");
        assert!(matches!(err, SessionError::Storage(_)), "got: {err:?}");

        // 失败路径不留痕：注册表应为空
        let (all, has_more) = registry.list(10).unwrap();
        assert!(!has_more);
        assert!(all.is_empty(), "失败时不应写入注册表: {all:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn create_is_lazy_until_first_prompt() {
        let dir = std::env::temp_dir().join(format!(
            "amux-lazy-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (mgr, registry, _rx) = manager_at(&dir);

        let meta = mgr.create("codex", "/tmp/lazy", false).await.unwrap();
        let entry = registry.get(&meta.id).unwrap().unwrap();
        assert!(
            entry.agent_session_id.is_none(),
            "创建会话不应触发 ACP session/new"
        );

        assert_eq!(meta.state, SessionState::Idle);
        assert_eq!(
            mgr.workspace_cwd(&meta.id).unwrap(),
            "/tmp/lazy",
            "非 worktree 会话的 workspace_cwd 返回原始 cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_list_count_semantics() {
        let dir = std::env::temp_dir().join(format!(
            "amux-list-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (mgr, registry, _rx) = manager_at(&dir);
        for (id, ts) in [("s1", 100), ("s2", 200), ("s3", 300)] {
            let m = SessionMeta {
                id: id.into(),
                agent: "codex".into(),
                cwd: "/tmp".into(),
                state: SessionState::Idle,
                title: String::new(),
                created_at: 1,
                last_active_at: ts,
                worktree_dir: String::new(),
            };
            registry.upsert(&m, Some(&format!("agent_{id}"))).unwrap();
        }
        // 按数量查询：前缀 + has_more；数量增大是更长前缀，不重不漏
        let (metas, has_more) = mgr.list(Some(2)).await.unwrap();
        assert_eq!(
            metas.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2"]
        );
        assert!(has_more);
        let (metas, has_more) = mgr.list(Some(3)).await.unwrap();
        assert_eq!(
            metas.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["s3", "s2", "s1"]
        );
        assert!(!has_more);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_unprompted_needs_no_close() {
        let dir = std::env::temp_dir().join(format!(
            "amux-noop-del-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (mgr, registry, _rx) = manager_at(&dir);
        let meta = mgr.create("codex", "/tmp/noop", false).await.unwrap();
        mgr.delete(&meta.id).await.unwrap();
        mgr.delete(&meta.id).await.expect("重复删除应保持幂等");
        assert!(registry.get(&meta.id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_session_errors() {
        let dir = std::env::temp_dir().join(format!(
            "amux-miss-err-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let (mgr, _registry, _rx) = manager_at(&dir);
        assert!(mgr.prompt("nope", text("x")).await.is_err());
        assert!(mgr.history("nope", None, None).await.is_err());
        assert!(mgr.delete("nope").await.is_ok());
        assert!(mgr.controls.lock().is_empty(), "未知会话请求不应创建控制块");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
