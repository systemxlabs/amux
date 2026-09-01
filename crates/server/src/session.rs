//! 会话管理：会话列表与历史权威在 server。
//! - 会话元数据持久化于 SQLite（`session.sqlite`），列表由 server 维护
//! - busy/idle 状态在 server 维护并存注册表；每次 Busy<->Idle 变更广播 `session.state_change`
//! - 惰性会话：`session.new` 只写注册表，agent 侧会话延后到首条指令
//!   （`session.prompt`）或查询/设置会话选项时懒创建（ACP `session/new`），
//!   已有 agent 会话先经 `session/resume` 恢复
//! - 会话选项存储在内存，以 Agent 侧数据为权威（docs/DESIGN.md「普通会话选项」）
//! - 删除会话先经 ACP `session/close` 释放资源，再尝试 `session/delete`；长时间无活动
//!   会话只经 `session/close` 关闭并保留 server 历史
//! - 对话历史与活动历史落 `data_dir/sessions/<id>_history.jsonl` / `<id>_activities.jsonl`

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use protocol::{
    generate_title, Activity, ContentBlock, HistoryItem, SessionMeta, SessionState,
    SessionStateChange,
};

use crate::agent::{AgentEvent, AgentRegistry};
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
    /// 会话选项（docs/DESIGN.md「普通会话选项」：存储在内存，以 Agent 侧数据为
    /// 权威；new/resume 响应、`session/set_config_option` 响应与
    /// `config_option_update` 通知均全量覆盖）。
    config_options: Mutex<HashMap<String, Vec<protocol::SessionConfigOption>>>,
    /// 进行中的活动（`session.ongoing_activity`；按会话 id 独立存储）
    ongoing: Mutex<HashMap<String, Activity>>,
    /// 当前 turn 的累积思考文本（多 chunk 拼接）。
    /// `ongoing` 中的 `Activity::Thinking.content` 写入时引用这里，保证 GUI 看到
    /// 的是「整个思考的前一部分」而不是最新一个流式片段。
    /// 与 `ongoing` 生命周期一致：turn 结束随 `ongoing` 一起清理。
    thinking_buf: Mutex<HashMap<String, (String, u64)>>,
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
    /// Serializes the session's local metadata lifetime with lazy ACP creation.
    /// Deletion must not race the final upsert performed by prompt setup.
    lifecycle: Mutex<()>,
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
            log::error!("重置残留忙会话失败：{e}");
        }
        let manager = SessionManager {
            agents,
            registry,
            data_dir,
            tx,
            config_options: Mutex::new(HashMap::new()),
            ongoing: Mutex::new(HashMap::new()),
            thinking_buf: Mutex::new(HashMap::new()),
            controls: Mutex::new(HashMap::new()),
            history_cache: Mutex::new(HashMap::new()),
            activities_cache: Mutex::new(HashMap::new()),
        };
        (manager, rx)
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

    /// 新建普通会话（**agent 侧会话惰性**，**worktree 立即创建**）：只写注册表
    /// 立即返回，不触发 ACP；agent 侧会话延后到首条指令时经 `session/new`
    /// 懒创建。`use_worktree` 时按 docs/DESIGN.md「工作树存储」在
    /// `~/.amux/worktrees/`（data_dir 同级）下确定路径并立即执行
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
            // docs/DESIGN.md「工作树存储」：会话创建时即落盘，而非首条指令时
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
            context_size: 0,
            context_window_size: 0,
        };
        self.registry.upsert(&meta, "")?;
        self.control(&meta.id);
        Ok(meta)
    }

    fn control(&self, session_id: &str) -> Arc<SessionControl> {
        let mut controls = self.controls.lock();
        controls
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionControl {
                    busy: AtomicBool::new(false),
                    deleted: AtomicBool::new(false),
                    lifecycle: Mutex::new(()),
                })
            })
            .clone()
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

    /// agent 实际工作目录：启用 worktree 时为工作树，否则用户指定目录。
    fn effective_cwd(meta: &SessionMeta) -> String {
        if meta.worktree_dir.is_empty() {
            meta.cwd.clone()
        } else {
            meta.worktree_dir.clone()
        }
    }

    /// 覆盖写入内存中的会话选项（docs/DESIGN.md「普通会话选项」：以 Agent 侧
    /// 数据为权威，new/resume 响应、set_config_option 响应与
    /// config_option_update 通知均全量覆盖）。
    fn store_config_options(&self, session_id: &str, options: Vec<protocol::SessionConfigOption>) {
        self.config_options
            .lock()
            .insert(session_id.to_string(), options);
    }

    /// 惰性创建 agent 侧会话（docs/DESIGN.md「ACP 通信」）：发送指令或查询会话
    /// 选项时才经 `session/new` 创建；new 响应携带的会话选项存入内存。已有
    /// agent 侧会话时原样返回。
    fn ensure_agent_session(
        &self,
        session_id: &str,
        meta: &SessionMeta,
        agent_session_id: &str,
    ) -> Result<(crate::agent::SharedDriver, String), SessionError> {
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        if !agent_session_id.is_empty() {
            return Ok((driver, agent_session_id.to_string()));
        }
        let (sid, options) = driver
            .create_session(&Self::effective_cwd(meta))
            .map_err(SessionError::AgentUnavailable)?;
        self.registry.set_agent_session_id(session_id, &sid)?;
        self.store_config_options(session_id, options);
        Ok((driver, sid))
    }

    /// 查询会话选项（docs/DESIGN.md：查询会话选项同样触发惰性创建/恢复；
    /// 返回内存存储的选项集合，Agent 侧数据为权威）。
    pub async fn config_options(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SessionConfigOption>, SessionError> {
        let control = self.control(session_id);
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        let (driver, agent_session_id) =
            self.ensure_agent_session(session_id, &meta, &agent_session_id)?;
        // 已有 agent 侧会话：先经 ACP `session/resume` 恢复（幂等），响应携带的
        // 最新选项全量覆盖内存存储；幂等 resume 返回空集合时保持既有选项。
        match driver.resume_session(&agent_session_id, &Self::effective_cwd(&meta)) {
            Ok(options) => {
                if !options.is_empty() {
                    self.store_config_options(session_id, options);
                }
            }
            Err(e) => log::error!("查询会话选项时 resume 失败 {session_id}: {e}"),
        }
        Ok(self.current_config_options(session_id))
    }

    /// 查询会话斜杠命令（docs/DESIGN.md「普通会话斜杠命令」：存储在内存，
    /// 以 Agent 侧数据为权威，由 ACP `available_commands_update` 通知驱动）。
    /// 查询不触发惰性创建：尚无 agent 侧会话时返回空（agent 侧会话创建后
    /// agent 才会下发命令集合）。
    pub async fn slash_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SlashCommand>, SessionError> {
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        if agent_session_id.is_empty() {
            return Ok(Vec::new());
        }
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        Ok(driver.available_commands(&agent_session_id))
    }

    /// 查询会话计划（docs/DESIGN.md「普通会话计划」：存储在内存，以 Agent 侧
    /// 数据为权威，由 ACP `plan` 通知驱动）。查询不触发惰性创建：尚无
    /// agent 侧会话时返回空。
    pub async fn plan(
        &self,
        session_id: &str,
    ) -> Result<Vec<protocol::SessionPlanEntry>, SessionError> {
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        if agent_session_id.is_empty() {
            return Ok(Vec::new());
        }
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        Ok(driver.session_plan(&agent_session_id))
    }

    /// 删除会话：若已有 agent 侧会话，先经 ACP
    /// `session/close` 关闭；若 ACP Server 支持会话删除，再发 `session/delete`
    /// （不支持删除的 agent 报错，按「不支持」忽略）。ACP 失败不阻断本地删除。
    /// 联动清除注册表条目 + 历史日志 + 活动日志 + 进行中控制块。
    /// 幂等：会话已不存在时仅清理残留日志（部分工作流清理失败后可安全重试）。
    pub async fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        let control = self.control(session_id);
        control.deleted.store(true, Ordering::SeqCst);
        // Serialize deletion with lazy ACP session creation and the setup upsert.
        // Otherwise delete can remove the row while setup subsequently recreates it.
        let _lifecycle = control.lifecycle.lock();
        let log = SessionLog::open(&self.data_dir, session_id);
        let entry = self.registry.get(session_id)?;
        let Some((meta, agent_session_id)) = entry else {
            return log
                .remove()
                .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")));
        };
        // 元数据先行删除：删除 RPC 立即返回，会话列表随桌面端删除后的主动刷新
        // 即刻生效；agent 往返与 worktree 清理较慢，交由后台任务异步完成。
        self.registry.delete(session_id)?;
        log.remove()
            .map_err(|e| SessionError::Storage(format!("会话日志删除失败: {e}")))?;
        self.invalidate_log_caches(session_id);

        // 资源清理（docs/DESIGN.md「工作树存储」与 ACP 会话生命周期）：
        // ACP close/delete 往返 + worktree 目录清理，均为尽力而为不阻断。
        let agents = self.agents.clone();
        let data_dir = self.data_dir.clone();
        let session_id2 = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            if !agent_session_id.is_empty() {
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
    /// 按数量查询最近活跃的普通会话（docs/DESIGN.md `session.list`）：
    /// 返回按最近活跃排序的前缀（至多 limit 条），has_more 表示是否还有更多。
    pub async fn list(
        &self,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionMeta>, bool), SessionError> {
        let all = self.registry.list()?;
        let limit = limit.unwrap_or(50).max(1);
        let has_more = all.len() > limit;
        let metas = all.into_iter().take(limit).map(|(m, _)| m).collect();
        Ok((metas, has_more))
    }

    /// 批量查询指定会话。不存在的 id 静默跳过。
    pub async fn info(&self, session_ids: &[String]) -> Result<Vec<SessionMeta>, SessionError> {
        let mut metas = Vec::new();
        for id in session_ids {
            // 单次查询同时完成「是否存在」与「取元数据」，避免重复读注册表
            if let Some((meta, _)) = self.registry.get(id)? {
                metas.push(meta);
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
    /// worktree 会话（docs/DESIGN.md「工作树存储」）：agent 实际工作在 worktree，
    /// 改动视图（diff/restore/list/read）应作用于 worktree 目录而非原始目录。
    pub fn workspace_cwd(&self, session_id: &str) -> Result<String, SessionError> {
        let meta = self.get_entry(session_id)?.0;
        Ok(if meta.worktree_dir.is_empty() {
            meta.cwd
        } else {
            meta.worktree_dir
        })
    }

    /// 关闭长时间无活动的 agent 侧会话（>timeout_ms）。候选选出后复核状态：
    /// 已回到 Busy 的会话跳过本轮（避免关掉正在进行中的 turn 的 agent 侧会话）。
    pub async fn close_idle(&self, now_ms: u64, timeout_ms: u64) -> Result<usize, SessionError> {
        let candidates = self.registry.idle_candidates(now_ms, timeout_ms)?;
        let mut closed = 0;
        for (sid, _) in candidates {
            let control = self.control(&sid);
            let _lifecycle = control.lifecycle.lock();
            if control.deleted.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(Some((meta, aid))) = self.registry.get(&sid) else {
                continue;
            };
            if aid.is_empty() || meta.state == SessionState::Busy {
                continue;
            }
            if let Ok(driver) = self.agents.driver_for(&meta.agent) {
                if driver.close(&aid).is_ok() {
                    if let Ok(()) = self.registry.set_agent_session_id(&sid, "") {
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

    /// 清理超过 `timeout_ms` 不活跃会话的 worktree（docs/DESIGN.md「工作树
    /// 存储」）。仅清理关联 worktree，会话本身保留；清理后清空元数据的
    /// worktree_dir，使 agent 工作目录与 workspace RPC 回退到原始 cwd。
    /// 删除失败的 worktree 保留字段，等待下轮清理重试。返回清理数量。
    pub async fn cleanup_idle_worktrees(
        &self,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<usize, SessionError> {
        let candidates = self.registry.idle_worktree_candidates(now_ms, timeout_ms)?;
        let mut cleaned = 0;
        for (sid, cwd, worktree_dir) in candidates {
            let control = self.control(&sid);
            let _lifecycle = control.lifecycle.lock();
            if control.deleted.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(Some((meta, _))) = self.registry.get(&sid) else {
                continue;
            };
            if meta.state != SessionState::Idle || meta.worktree_dir != worktree_dir {
                continue;
            }
            let wt = PathBuf::from(&worktree_dir);
            GitRunner::new().remove_worktree(&cwd, &wt);
            if !wt.exists() {
                if let Err(e) = self.registry.clear_worktree_dir(&sid) {
                    log::error!("清空会话 worktree 目录失败 {sid}: {e}");
                    continue;
                }
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
    ) -> Result<(Vec<HistoryItem>, bool, Option<u64>), SessionError> {
        self.get_entry(session_id)?;
        let items = self.cached_log(
            session_id,
            &self.history_cache,
            SessionLog::history_path(&self.data_dir, session_id),
            |log| log.read_history(),
            "会话历史读取失败",
        )?;
        Ok(Self::page(&items, limit, before))
    }

    /// 分页读活动历史：`before` 为独占上界游标（条目下标，u64 统一协议游标类型）。
    pub async fn activities(
        &self,
        session_id: &str,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<Activity>, bool, Option<u64>), SessionError> {
        self.get_entry(session_id)?;
        let items = self.cached_log(
            session_id,
            &self.activities_cache,
            SessionLog::activities_path(&self.data_dir, session_id),
            |log| log.read_activities(),
            "会话活动读取失败",
        )?;
        Ok(Self::page(&items, limit, before))
    }

    /// 惰性分页切窗并计算下一游标（纯函数，`history`/`activities` 共用）。
    fn page<T: Clone>(
        items: &[T],
        limit: Option<usize>,
        before: Option<u64>,
    ) -> (Vec<T>, bool, Option<u64>) {
        let limit = limit.unwrap_or(200);
        let before_usize = before.map(|b| b as usize);
        let (start, end, has_more) = Self::window_items(items.len(), limit, before_usize);
        let next_before = if has_more { Some(start as u64) } else { None };
        (items[start..end].to_vec(), has_more, next_before)
    }

    /// 带缓存的日志读取：文件长度未变时复用上次解析结果
    /// （GUI 每 10s 轮询打开的会话；日志 append-only，长度不变即内容不变），
    /// 追加后由写入方失效缓存。`history`/`activities` 共用同一逻辑。
    fn cached_log<T>(
        &self,
        session_id: &str,
        cache: &Mutex<HashMap<String, LogCache<T>>>,
        path: PathBuf,
        read: impl FnOnce(&SessionLog) -> std::io::Result<Vec<T>>,
        err_ctx: &str,
    ) -> Result<Arc<Vec<T>>, SessionError> {
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut cache = cache.lock();
        if let Some(c) = cache.get(session_id) {
            if c.source_len == file_len {
                return Ok(c.items.clone());
            }
        }
        let items = Arc::new(
            read(&SessionLog::open(&self.data_dir, session_id))
                .map_err(|e| SessionError::Storage(format!("{err_ctx}: {e}")))?,
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
        self.history_cache.lock().remove(session_id);
        self.activities_cache.lock().remove(session_id);
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
            log::error!("用户消息落盘失败 {session_id}: {e}");
            self.finalize_turn(
                session_id,
                &control,
                control.deleted.load(Ordering::SeqCst),
                protocol::StateChangeReason::Aborted,
            );
            return Err(SessionError::Storage(format!("用户消息落盘失败: {e}")));
        }
        self.invalidate_log_caches(session_id);

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
                    detail: format!("恢复 agent 上下文失败: {e}"),
                };
                if let Err(log_error) = log.append_activities(&[err]) {
                    log::error!("resume 错误活动落盘失败 {session_id}: {log_error}");
                }
                self.invalidate_log_caches(session_id);
                self.finalize_turn(
                    session_id,
                    &control,
                    control.deleted.load(Ordering::SeqCst),
                    protocol::StateChangeReason::Aborted,
                );
                return Err(SessionError::AgentUnavailable(format!(
                    "恢复 agent 上下文失败: {e}"
                )));
            }
        }

        let started = std::time::Instant::now();
        let (storage_error, turn_reason) = self
            .run_turn(session_id, &driver, &agent_session_id, input, &control)
            .await;

        self.finalize_turn(
            session_id,
            &control,
            control.deleted.load(Ordering::SeqCst),
            turn_reason,
        );
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
    async fn setup_prompt(
        &self,
        session_id: &str,
        input: &[ContentBlock],
    ) -> Result<(crate::agent::SharedDriver, String, String, SessionState), SessionError> {
        let control = self.control(session_id);
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
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
        // worktree 在 session.new 已落盘（docs/DESIGN.md「工作树存储」），此处不再创建。
        // agent 实际工作目录：启用 worktree 时为工作树，否则用户指定目录。
        // create/resume 共用此值，GUI 的 workspace/diff RPC 也按它下发。
        let cwd = if meta.worktree_dir.is_empty() {
            meta.cwd.clone()
        } else {
            meta.worktree_dir.clone()
        };
        let driver = self
            .agents
            .driver_for(&meta.agent)
            .map_err(SessionError::AgentUnavailable)?;
        let (driver, agent_session_id) = if agent_session_id.is_empty() {
            // 惰性创建 agent 侧会话，响应中的会话选项存入内存
            //（docs/DESIGN.md「普通会话选项」）
            self.ensure_agent_session(session_id, &meta, "")?
        } else {
            (driver, agent_session_id)
        };
        // 创建与 resume 分支统一 upsert：busy、首条 prompt 生成的标题与活跃时间
        // 立即落盘（docs/DESIGN.md「普通会话状态」：状态以元数据为权威，变更需
        // 立即落盘；否则 turn 进行中列表读到陈旧空闲，且并发 prompt 会放行）。
        // resume 分支若只更新状态，先查过会话选项的会话（agent 侧会话已提前
        // 创建）首条 prompt 生成的标题将永远不落盘。
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        self.registry.upsert(&meta, &agent_session_id)?;
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
                    let ts = now();
                    let (accumulated, first_ts) = {
                        let mut buf = self.thinking_buf.lock();
                        let entry = buf
                            .entry(session_id.to_string())
                            .or_insert_with(|| (String::new(), ts));
                        if entry.1 == 0 {
                            entry.1 = ts;
                        }
                        entry.0.push_str(&text);
                        (entry.0.clone(), entry.1)
                    };
                    self.ongoing.lock().insert(
                        session_id.to_string(),
                        Activity::Thinking {
                            timestamp: first_ts,
                            content: accumulated,
                        },
                    );
                }
                AgentEvent::ToolCall {
                    id,
                    name,
                    title,
                    content,
                } => {
                    merger.push_tool_call(
                        id.clone(),
                        name.clone(),
                        title.clone(),
                        content.clone(),
                        now(),
                    );
                    self.ongoing.lock().insert(
                        session_id.to_string(),
                        Activity::ToolCall {
                            timestamp: now(),
                            name: name.unwrap_or_else(|| "tool_call".into()),
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
                    log::error!("agent turn 失败 {session_id}: {detail}");
                }
                AgentEvent::UsageUpdate { used, size } => {
                    // 记录会话上下文大小（docs/DESIGN.md「ACP 通信」）。
                    if !control.deleted.load(Ordering::SeqCst) {
                        if let Err(e) = self.registry.set_context_size(session_id, used, size) {
                            log::error!("记录会话上下文大小失败 {session_id}: {e}");
                        }
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
            // 重新创建活动文件，故 deleted 时不写。
            if !control.deleted.load(Ordering::SeqCst) {
                if let Err(e) = self.flush_ready_activities(session_id, &log, &mut merger) {
                    storage_error = Some(e);
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
        if !deleted && !history.is_empty() {
            if let Err(e) = log.append_history(&history) {
                log::error!("历史落盘失败 {session_id}: {e}");
                storage_error = Some(SessionError::Storage(format!("历史落盘失败: {e}")));
            }
        }
        if !deleted && !activities.is_empty() {
            if let Err(e) = log.append_activities(&activities) {
                log::error!("活动落盘失败 {session_id}: {e}");
                storage_error = Some(SessionError::Storage(format!("活动落盘失败: {e}")));
            }
        }
        if !deleted && (!history.is_empty() || !activities.is_empty()) {
            self.invalidate_log_caches(session_id);
        }
        (storage_error, turn_reason)
    }

    /// 把已定稿的活动实时追加写盘并失效活动缓存。返回落盘错误（写盘后仍
    /// 会继续跑 turn，仅收集错误供调用方上报）。
    fn flush_ready_activities(
        &self,
        session_id: &str,
        log: &SessionLog,
        merger: &mut TurnMerger,
    ) -> Result<(), SessionError> {
        let ready = merger.take_ready();
        if ready.is_empty() {
            return Ok(());
        }
        if let Err(e) = log.append_activities(&ready) {
            log::error!("活动落盘失败 {session_id}: {e}");
            return Err(SessionError::Storage(format!("活动落盘失败: {e}")));
        }
        self.activities_cache.lock().remove(session_id);
        Ok(())
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
        self.ongoing.lock().remove(session_id);
        self.thinking_buf.lock().remove(session_id);
        control.busy.store(false, Ordering::SeqCst);
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
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        // 空闲会话（或尚无 agent 侧会话）无可取消：ACP agent 对未知 turn
        // 会报错，这里幂等返回成功、不透传——GUI 的取消按钮是常驻的，
        // 调用方无需自行区分忙闲。
        if meta.state != SessionState::Busy || agent_session_id.is_empty() {
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

    /// 设置会话配置选项（docs/DESIGN.md「ACP 通信」：Server 向 ACP Server
    /// 发送 `session/set_config_option` 请求进行设置）。尚无 agent 侧会话时先
    /// 惰性创建；响应中的会话选项全量覆盖内存存储，返回更新后的完整选项集合。
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: protocol::SessionConfigOptionValue,
    ) -> Result<Vec<protocol::SessionConfigOption>, SessionError> {
        let control = self.control(session_id);
        let _lifecycle = control.lifecycle.lock();
        if control.deleted.load(Ordering::SeqCst) {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let (meta, agent_session_id) = self.get_entry(session_id)?;
        let (driver, agent_session_id) =
            self.ensure_agent_session(session_id, &meta, &agent_session_id)?;
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
    use crate::agent::{AgentDriver, AgentEvent, AgentRegistry, StubAgentDriver};
    use crate::history::SessionLog;
    use crate::registry::SessionRegistry;
    use protocol::ContentBlock;
    use std::sync::{Arc, Barrier};
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

    /// 阻塞型驱动：prompt 后等待 release 才结束 turn（用于控制 turn 生命周期，
    /// 在 turn 进行中并发执行删除以验证删除语义）。
    struct BlockingDriver {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl AgentDriver for BlockingDriver {
        fn create_session(
            &self,
            _cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            Ok(("agent_blocking".into(), Vec::new()))
        }

        fn resume_session(
            &self,
            _agent_session_id: &str,
            _cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
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

        fn set_config_option(
            &self,
            _agent_session_id: &str,
            _config_id: &str,
            _value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn shutdown(&self) {}
    }

    /// 测试驱动：prompt 时先发一条上下文大小更新，再正常结束 turn
    /// （验证 usage_update → 注册表记录的链路）。
    struct UsageDriver {
        used: u64,
        size: u64,
    }

    impl AgentDriver for UsageDriver {
        fn create_session(
            &self,
            _cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            Ok(("agent_usage".into(), Vec::new()))
        }

        fn resume_session(
            &self,
            _agent_session_id: &str,
            _cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn prompt(
            &self,
            _agent_session_id: &str,
            _input: Vec<ContentBlock>,
        ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let used = self.used;
            let size = self.size;
            tokio::spawn(async move {
                let _ = tx.send(AgentEvent::UsageUpdate { used, size }).await;
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

        fn set_config_option(
            &self,
            _agent_session_id: &str,
            _config_id: &str,
            _value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn shutdown(&self) {}
    }

    /// 测试驱动：prompt 时按顺序推送多个 thinking chunk 再结束 turn。
    /// 每发完一个 chunk 阻塞等待 `release`，让测试可以串行观察
    /// `ongoing_activity` 在 chunk 累积过程中的中间态。
    struct ThinkingChunksDriver {
        chunks: Vec<&'static str>,
        release: Arc<tokio::sync::Notify>,
    }

    impl AgentDriver for ThinkingChunksDriver {
        fn create_session(
            &self,
            _cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            Ok(("agent_thinking".into(), Vec::new()))
        }

        fn resume_session(
            &self,
            _agent_session_id: &str,
            _cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn prompt(
            &self,
            _agent_session_id: &str,
            _input: Vec<ContentBlock>,
        ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let chunks = self.chunks.clone();
            let release = self.release.clone();
            tokio::spawn(async move {
                for c in chunks {
                    let _ = tx.send(AgentEvent::Thinking(c.into())).await;
                    release.notified().await;
                }
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

        fn set_config_option(
            &self,
            _agent_session_id: &str,
            _config_id: &str,
            _value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(Vec::new())
        }

        fn shutdown(&self) {}
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

    #[tokio::test]
    async fn worktree_eager_create_and_delete_cascade() {
        // data_dir 嵌套一层：worktree 根落在用例沙箱内（data_dir 同级 worktrees/）
        let case = std::env::temp_dir().join(format!(
            "amux-wt-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(case.join("server").join("sessions")).unwrap();

        // 主仓库：需要已有提交（unborn HEAD 无法建 worktree）
        let repo = case.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init", "-q"]);

        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(crate::agent::StubAgentDriver::new()),
        ));
        let registry =
            Arc::new(SessionRegistry::open(&case.join("server").join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry, case.join("server"));

        // session.new 即落盘工作树（docs/DESIGN.md「工作树存储」）
        let meta = mgr
            .create("codex", repo.to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(meta
            .worktree_dir
            .starts_with(case.join("worktrees").to_str().unwrap()));
        let wt = std::path::PathBuf::from(&meta.worktree_dir);
        assert!(wt.is_dir(), "session.new 应立即创建工作树");
        assert!(wt.join(".git").is_file(), ".git 为文件是 worktree 的特征");
        let list = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(list.contains(meta.worktree_dir.trim()));

        // 首次指令：工作树已就位，prompt 正常完成
        mgr.prompt(&meta.id, text("hi")).await.unwrap();
        assert!(wt.is_dir(), "prompt 后工作树仍应存在");

        // 改动视图（workspace RPC）应作用于 worktree 目录，而非原始工作目录
        assert_eq!(
            mgr.workspace_cwd(&meta.id).unwrap(),
            meta.worktree_dir,
            "worktree 会话的 workspace_cwd 应返回 worktree 目录"
        );

        // 删除会话：工作树级联移除（资源清理已异步化，轮询等待后台完成）
        mgr.delete(&meta.id).await.unwrap();
        let mut removed = false;
        for _ in 0..100 {
            if !wt.exists() {
                removed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(removed, "删除会话应级联删除工作树");
        let list = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains(meta.worktree_dir.trim()));
        let _ = std::fs::remove_dir_all(&case);
    }

    #[tokio::test]
    async fn cleanup_idle_worktrees_removes_stale_keeps_recent() {
        // data_dir 嵌套一层：worktree 根落在用例沙箱内（data_dir 同级 worktrees/）
        let case = std::env::temp_dir().join(format!(
            "amux-wt-cleanup-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(case.join("server").join("sessions")).unwrap();

        let repo = case.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init", "-q"]);

        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(crate::agent::StubAgentDriver::new()),
        ));
        let registry =
            Arc::new(SessionRegistry::open(&case.join("server").join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), case.join("server"));

        // 超时会话：worktree 应被清理，字段清空，工作目录回退原始 cwd
        let stale = mgr
            .create("codex", repo.to_str().unwrap(), true)
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
            .create("codex", repo.to_str().unwrap(), true)
            .await
            .unwrap();
        let recent_wt = PathBuf::from(&recent.worktree_dir);
        assert!(recent_wt.is_dir());

        let cleaned = mgr
            .cleanup_idle_worktrees(now_ms, 7 * 24 * 3_600_000)
            .await
            .unwrap();
        assert_eq!(cleaned, 1, "仅超期会话的 worktree 被清理");
        assert!(!stale_wt.exists(), "超期 worktree 应被删除");
        assert!(recent_wt.is_dir(), "近期 worktree 应保留");
        let (stored, _) = registry.get(&stale.id).unwrap().unwrap();
        assert_eq!(stored.worktree_dir, "", "清理后 worktree_dir 应清空");
        assert_eq!(
            mgr.workspace_cwd(&stale.id).unwrap(),
            repo.to_str().unwrap(),
            "清理后工作目录回退原始 cwd"
        );
        let list = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(
            !list.contains(stale.worktree_dir.trim()),
            "主仓库不应再登记已清理 worktree"
        );

        let _ = std::fs::remove_dir_all(&case);
    }

    #[tokio::test]
    async fn worktree_create_rejects_non_git_cwd() {
        let agents = Arc::new(AgentRegistry::new_for_tests());
        let dir = std::env::temp_dir().join(format!(
            "amux-wt-norepo-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        // 非 git 目录启用 worktree 应直接拒绝
        let err = mgr
            .create("codex", "/tmp", true)
            .await
            .expect_err("非 git 仓库应失败");
        assert!(matches!(err, SessionError::Storage(_)), "got: {err:?}");

        // 失败路径不留痕：注册表应为空
        let all = registry.list().unwrap();
        assert!(all.is_empty(), "失败时不应写入注册表: {all:?}");
        let _ = std::fs::remove_dir_all(&dir);
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

        let meta = mgr.create("codex", "/tmp/lazy", false).await.unwrap();
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(aid.is_empty(), "创建会话不应触发 ACP session/new");

        assert_eq!(meta.state, SessionState::Idle);
        assert_eq!(
            mgr.workspace_cwd(&meta.id).unwrap(),
            "/tmp/lazy",
            "非 worktree 会话的 workspace_cwd 返回原始 cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ACP session/new 与删除并发时，删除不能被 setup 的最终 upsert 绕过。
    struct BlockingCreateDriver {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        inner: StubAgentDriver,
    }

    impl AgentDriver for BlockingCreateDriver {
        fn create_session(
            &self,
            cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            self.entered.wait();
            self.release.wait();
            self.inner.create_session(cwd)
        }

        fn resume_session(
            &self,
            agent_session_id: &str,
            cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            self.inner.resume_session(agent_session_id, cwd)
        }

        fn prompt(
            &self,
            agent_session_id: &str,
            input: Vec<ContentBlock>,
        ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
            self.inner.prompt(agent_session_id, input)
        }

        fn cancel(&self, agent_session_id: &str) -> Result<(), String> {
            self.inner.cancel(agent_session_id)
        }

        fn close(&self, agent_session_id: &str) -> Result<(), String> {
            self.inner.close(agent_session_id)
        }

        fn delete_session(&self, agent_session_id: &str) -> Result<(), String> {
            self.inner.delete_session(agent_session_id)
        }

        fn set_config_option(
            &self,
            agent_session_id: &str,
            config_id: &str,
            value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            self.inner
                .set_config_option(agent_session_id, config_id, value)
        }

        fn shutdown(&self) {
            self.inner.shutdown()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_during_lazy_create_does_not_resurrect_session() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "race",
            Arc::new(BlockingCreateDriver {
                entered: entered.clone(),
                release: release.clone(),
                inner: StubAgentDriver::new(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-delete-create-race-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());
        let mgr = Arc::new(mgr);
        let meta = mgr.create("race", "/tmp/race", false).await.unwrap();
        let session_id = meta.id.clone();

        let prompt_mgr = mgr.clone();
        let prompt_id = session_id.clone();
        let prompt_task =
            tokio::spawn(async move { prompt_mgr.prompt(&prompt_id, text("hi")).await });

        // Wait until setup is inside the synchronous ACP create call.
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();

        let delete_mgr = mgr.clone();
        let delete_id = session_id.clone();
        let delete_task = tokio::spawn(async move { delete_mgr.delete(&delete_id).await });
        while !mgr.control(&session_id).deleted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        // Let session/new return. setup must observe deleted and avoid upsert.
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        let prompt_result = prompt_task.await.unwrap();
        assert!(matches!(prompt_result, Err(SessionError::NotFound(_))));
        delete_task.await.unwrap().unwrap();
        assert!(registry.get(&session_id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_records_usage_update_context_size() {
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(UsageDriver {
                used: 53_000,
                size: 200_000,
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-usage-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        let meta = mgr.create("codex", "/tmp/usage", false).await.unwrap();
        mgr.prompt(&meta.id, text("hi")).await.unwrap();

        let (stored, _) = registry.get(&meta.id).unwrap().unwrap();
        assert_eq!(stored.context_size, 53_000);
        assert_eq!(stored.context_window_size, 200_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试驱动：持有会话配置选项，`set_config_option` 按 config_id 更新并返回完整集合
    /// （验证会话选项查询/设置 → 内存记录的链路）。
    struct ConfigDriver {
        options: Mutex<Vec<protocol::SessionConfigOption>>,
    }

    impl AgentDriver for ConfigDriver {
        fn create_session(
            &self,
            _cwd: &str,
        ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
            Ok(("agent_cfg".into(), self.options.lock().clone()))
        }

        fn resume_session(
            &self,
            _agent_session_id: &str,
            _cwd: &str,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            Ok(self.options.lock().clone())
        }

        fn prompt(
            &self,
            _agent_session_id: &str,
            _input: Vec<ContentBlock>,
        ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
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

        fn set_config_option(
            &self,
            _agent_session_id: &str,
            config_id: &str,
            value: protocol::SessionConfigOptionValue,
        ) -> Result<Vec<protocol::SessionConfigOption>, String> {
            let mut options = self.options.lock();
            for opt in options.iter_mut() {
                if opt.id != config_id {
                    continue;
                }
                match (&mut opt.kind, &value) {
                    (
                        protocol::SessionConfigKind::Select { current_value, .. },
                        protocol::SessionConfigOptionValue::ValueId { value: v },
                    ) => *current_value = v.clone(),
                    (
                        protocol::SessionConfigKind::Boolean { current_value },
                        protocol::SessionConfigOptionValue::Boolean { value: v },
                    ) => *current_value = *v,
                    _ => return Err(format!("选项 {config_id} 与值类型不匹配")),
                }
            }
            Ok(options.clone())
        }

        fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn config_options_lazy_query_and_set() {
        // docs/DESIGN.md「普通会话选项」：选项存储在内存，以 Agent 侧数据为权威；
        // 查询会话选项同样触发惰性创建/恢复（docs/DESIGN.md「ACP 通信」）。
        let opts = vec![protocol::SessionConfigOption {
            id: "model".into(),
            name: "模型".into(),
            description: None,
            category: Some("model".into()),
            kind: protocol::SessionConfigKind::Select {
                current_value: "gpt-4o".into(),
                options: vec![
                    protocol::SessionConfigSelectEntry {
                        value: "gpt-4o".into(),
                        name: "GPT-4o".into(),
                    },
                    protocol::SessionConfigSelectEntry {
                        value: "gpt-5".into(),
                        name: "GPT-5".into(),
                    },
                ],
            },
        }];
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(ConfigDriver {
                options: Mutex::new(opts.clone()),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-cfg-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        let meta = mgr.create("codex", "/tmp/cfg", false).await.unwrap();
        // 查询触发惰性创建：agent 侧会话建立，初始选项来自 new 响应
        let stored = mgr.config_options(&meta.id).await.unwrap();
        assert_eq!(stored, opts, "查询应惰性创建并返回 Agent 侧初始选项");
        let (_, aid) = registry.get(&meta.id).unwrap().unwrap();
        assert!(!aid.is_empty(), "查询会话选项应已创建 agent 侧会话");

        // 设置选项：set_config_option 响应全量覆盖内存存储
        let updated = mgr
            .set_config_option(
                &meta.id,
                "model",
                protocol::SessionConfigOptionValue::ValueId {
                    value: "gpt-5".into(),
                },
            )
            .await
            .unwrap();
        match &updated[0].kind {
            protocol::SessionConfigKind::Select { current_value, .. } => {
                assert_eq!(current_value, "gpt-5")
            }
            other => panic!("应为 Select，得到 {other:?}"),
        }
        let again = mgr.config_options(&meta.id).await.unwrap();
        assert_eq!(again, updated, "后续查询应读到 Agent 侧最新的全量选项");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ongoing_thinking_accumulates_across_chunks() {
        // GUI 通过 session.ongoing_activity 看到「思考中」应当是整个 turn 的累积内容，
        // 而不是最新一个流式 chunk（docs/PRD.md 实时活动展示期望）。
        // prompt 由驱动在每个 chunk 后阻塞等待 release，测试用 wait_for_thinking
        // 串行观察三个中间态：单段 → 两段 → 三段。
        let release = Arc::new(tokio::sync::Notify::new());
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(ThinkingChunksDriver {
                chunks: vec!["先读 src/main.rs", "，再分析依赖", "，最后写结论"],
                release: release.clone(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-thinking-accum-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry, dir.clone());
        let mgr = Arc::new(mgr);

        let meta = mgr.create("codex", "/tmp/think", false).await.unwrap();

        // 异步推进 prompt；通过 release 闸门控制驱动节奏
        let mgr_for_task = mgr.clone();
        let id_for_task = meta.id.clone();
        let prompt_task =
            tokio::spawn(async move { mgr_for_task.prompt(&id_for_task, text("hi")).await });

        // 每个 chunk 之后释放 driver 推进下一个 chunk；最后一段也需释放以让 turn 收尾
        for expected in [
            "先读 src/main.rs",
            "先读 src/main.rs，再分析依赖",
            "先读 src/main.rs，再分析依赖，最后写结论",
        ] {
            wait_for_thinking(&mgr, &meta.id, expected).await;
            release.notify_one();
        }
        prompt_task.await.unwrap().unwrap();

        // turn 结束后 ongoing 应已清空
        assert!(mgr.ongoing_activity(&meta.id).await.unwrap().is_none());
        // 落盘的活动历史也只剩一条合并后的 thinking
        let (acts, _, _) = mgr.activities(&meta.id, None, None).await.unwrap();
        let thinking = acts
            .iter()
            .find_map(|a| match a {
                Activity::Thinking { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("应有累积的 thinking 活动");
        assert_eq!(thinking, "先读 src/main.rs，再分析依赖，最后写结论");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮询等待 ongoing 出现 expected 内容。10ms tick × 200 = 2s 上限；
    /// 真实场景下 1 个 tick 即应到位（事件经 mpsc 同步分发）。
    async fn wait_for_thinking(mgr: &SessionManager, sid: &str, expected: &str) {
        for _ in 0..200 {
            if let Some(Activity::Thinking { content, .. }) =
                mgr.ongoing_activity(sid).await.unwrap()
            {
                if content == expected {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("ongoing thinking 内容始终未匹配: {expected}");
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
        let meta = mgr.create("codex", "/tmp/work", false).await.unwrap();

        mgr.prompt(&meta.id, text("实现登录功能")).await.unwrap();

        let (list, _) = mgr.list(None).await.unwrap();
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

        let (list, _) = mgr.list(None).await.unwrap();
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
    async fn prompt_persists_title_when_agent_session_precreated() {
        // GUI 选中会话即查询会话选项，选项查询会惰性创建 agent 侧会话并落盘
        // agent_session_id；首条 prompt 因此走 resume 分支，生成的标题仍须落盘。
        let (mgr, _rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/work", false).await.unwrap();

        mgr.config_options(&meta.id).await.unwrap();
        mgr.prompt(&meta.id, text("实现登录功能")).await.unwrap();

        let (list, _) = mgr.list(None).await.unwrap();
        assert_eq!(list[0].title, "实现登录功能");
        let _ = std::fs::remove_dir_all(&mgr.data_dir);
    }

    #[tokio::test]
    async fn prompt_persists_user_message_before_turn_ends() {
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
        let meta = manager
            .create("blocking", "/tmp/work", false)
            .await
            .unwrap();
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
    async fn deleted_mid_turn_does_not_broadcast_state_change() {
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
            "amux-del-mid-turn-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (manager, mut rx) = SessionManager::new(agents, registry.clone(), dir.clone());
        let manager = Arc::new(manager);
        let meta = manager
            .create("blocking", "/tmp/work", false)
            .await
            .unwrap();
        let session_id = meta.id.clone();

        // turn 进行中删除会话：finalize_turn 不应为已删除会话广播状态变更，
        // 也不应使其在注册表中复活。
        let prompt_manager = manager.clone();
        let prompt_session_id = session_id.clone();
        let prompt_task = tokio::spawn(async move {
            prompt_manager
                .prompt(&prompt_session_id, text("进行中"))
                .await
        });
        started.notified().await;
        manager.delete(&session_id).await.unwrap();

        release.notify_one();
        let prompt_result = prompt_task.await.unwrap();
        assert!(
            matches!(prompt_result, Err(SessionError::NotFound(_))),
            "turn 结束后已删除会话应返回 NotFound: {prompt_result:?}"
        );
        assert!(
            registry.get(&session_id).unwrap().is_none(),
            "删除的会话不应在注册表中复活"
        );

        // 已删除会话不应再广播任何状态变更（删除期间收到的 Busy 广播除外）。
        let mut saw_deleted_broadcast = false;
        while let Ok(n) = rx.try_recv() {
            let ServerNotification::StateChange(c) = n;
            if c.session_id == session_id && c.new_state == SessionState::Idle {
                saw_deleted_broadcast = true;
            }
        }
        assert!(
            !saw_deleted_broadcast,
            "已删除会话不应广播 busy→idle 状态变更"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn activities_flush_during_turn_not_only_at_end() {
        struct StreamingDriver {
            started: Arc<Notify>,
            flushed: Arc<Notify>,
            release: Arc<Notify>,
        }

        impl AgentDriver for StreamingDriver {
            fn create_session(
                &self,
                _cwd: &str,
            ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
                Ok(("agent_stream".into(), Vec::new()))
            }

            fn resume_session(
                &self,
                _agent_session_id: &str,
                _cwd: &str,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
                Ok(Vec::new())
            }

            fn prompt(
                &self,
                _agent_session_id: &str,
                _input: Vec<ContentBlock>,
            ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                let started = self.started.clone();
                let flushed = self.flushed.clone();
                let release = self.release.clone();
                tokio::spawn(async move {
                    started.notify_one();
                    let _ = tx.send(AgentEvent::Thinking("思考中".into())).await;
                    let _ = tx
                        .send(AgentEvent::ToolCall {
                            id: "tc1".into(),
                            name: Some("read_file".into()),
                            title: None,
                            content: None,
                        })
                        .await;
                    // tool_call 合并到同一条活动，遇到 output 后定稿并落盘。
                    let _ = tx.send(AgentEvent::OutputChunk("完成".into())).await;
                    // 事件已发完但 turn 未结束（release 未放行）。
                    flushed.notify_one();
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

            fn set_config_option(
                &self,
                _agent_session_id: &str,
                _config_id: &str,
                _value: protocol::SessionConfigOptionValue,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
                Ok(Vec::new())
            }

            fn shutdown(&self) {}
        }

        let started = Arc::new(Notify::new());
        let flushed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "stream",
            Arc::new(StreamingDriver {
                started: started.clone(),
                flushed: flushed.clone(),
                release: release.clone(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-real-time-activity-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (manager, _rx) = SessionManager::new(agents, registry, dir.clone());
        let manager = Arc::new(manager);
        let meta = manager.create("stream", "/tmp/work", false).await.unwrap();
        let session_id = meta.id.clone();

        let prompt_manager = manager.clone();
        let prompt_task =
            tokio::spawn(async move { prompt_manager.prompt(&session_id, text("立即保存")).await });
        started.notified().await;

        // thinking + tool_call 事件已处理，但 turn 尚未结束（release 未放行）：
        // 已定稿的活动应实时落盘，而非攒到 turn 结束统一写。
        flushed.notified().await;
        let log = SessionLog::open(&dir, &meta.id);
        let acts = log.read_activities().unwrap();
        assert!(
            acts.iter()
                .any(|a| matches!(a, Activity::Thinking { content, .. } if content == "思考中")),
            "thinking 应在 turn 结束前实时落盘"
        );
        assert!(
            acts.iter()
                .any(|a| matches!(a, Activity::ToolCall { name, .. } if name == "read_file")),
            "tool_call 应在 turn 结束前实时落盘"
        );

        release.notify_one();
        prompt_task.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn session_list_count_semantics() {
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "codex",
            Arc::new(crate::agent::StubAgentDriver::new()),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-list-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());
        for (id, ts) in [("s1", 100), ("s2", 200), ("s3", 300)] {
            let (mut m, aid) = (
                SessionMeta {
                    id: id.into(),
                    agent: "codex".into(),
                    cwd: "/tmp".into(),
                    state: SessionState::Idle,
                    title: String::new(),
                    created_at: 1,
                    last_active_at: ts,
                    worktree_dir: String::new(),
                    context_size: 0,
                    context_window_size: 0,
                },
                format!("agent_{id}"),
            );
            registry.upsert(&m, &aid).unwrap();
            let _ = &mut m;
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
    async fn busy_state_persisted_immediately_on_resume_path() {
        // docs/DESIGN.md「普通会话状态」：发送 session/prompt 时置工作中并立即
        // 落盘。回归：resume 分支（已有 agent 侧会话）此前 busy 只改内存，
        // turn 进行中列表读到陈旧空闲，且 Busy 前置检查放行并发 prompt。
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
            "amux-busy-resume-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (manager, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());
        let manager = Arc::new(manager);
        let meta = manager
            .create("blocking", "/tmp/work", false)
            .await
            .unwrap();
        let session_id = meta.id.clone();

        // 第一轮：走惰性创建分支（upsert 已含 busy），正常结束
        let pm = manager.clone();
        let sid1 = session_id.clone();
        let first = tokio::spawn(async move { pm.prompt(&sid1, text("第一轮")).await });
        started.notified().await;
        release.notify_one();
        first.await.unwrap().unwrap();
        let (m, aid) = registry.get(&session_id).unwrap().unwrap();
        assert!(!aid.is_empty(), "首轮后应有 agent 侧会话 id");
        assert_eq!(m.state, SessionState::Idle);

        // 第二轮：走 resume 分支——turn 进行中元数据必须是工作中
        let pm = manager.clone();
        let sid2 = session_id.clone();
        let second = tokio::spawn(async move { pm.prompt(&sid2, text("第二轮")).await });
        started.notified().await;
        let (m, _) = registry.get(&session_id).unwrap().unwrap();
        assert_eq!(m.state, SessionState::Busy, "resume 分支的 busy 应立即落盘");
        // 元数据为权威：并发 prompt 被拒绝
        assert!(matches!(
            manager.prompt(&session_id, text("并发")).await,
            Err(SessionError::Busy)
        ));

        release.notify_one();
        second.await.unwrap().unwrap();
        let (m, _) = registry.get(&session_id).unwrap().unwrap();
        assert_eq!(m.state, SessionState::Idle, "响应接收后回到空闲");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_triggers_driver_close() {
        struct Tracking {
            closed: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl AgentDriver for Tracking {
            fn create_session(
                &self,
                cwd: &str,
            ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
                Ok((format!("agent_{}", cwd.replace('/', "_")), Vec::new()))
            }
            fn resume_session(
                &self,
                _a: &str,
                _c: &str,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
                Ok(Vec::new())
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
            fn set_config_option(
                &self,
                _a: &str,
                _config_id: &str,
                _value: protocol::SessionConfigOptionValue,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
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
        let meta = mgr.create("track", "/tmp/work", false).await.unwrap();
        mgr.prompt(&meta.id, text("hi")).await.unwrap();
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 0);

        mgr.delete(&meta.id).await.unwrap();
        // 清理已异步化：轮询等待后台 driver.close 完成后再断言计数
        let mut closed_seen = false;
        for _ in 0..100 {
            if closed.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                closed_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            closed_seen,
            "删除应触发一次 driver.close（ACP session/close）"
        );
        assert!(registry.get(&meta.id).unwrap().is_none(), "注册表应已删除");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn delete_unprompted_needs_no_close() {
        let (mgr, _rx) = stub_manager("codex");
        let meta = mgr.create("codex", "/tmp/noop", false).await.unwrap();
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

    #[tokio::test]
    async fn cancel_on_idle_session_skips_acp() {
        struct Counting {
            cancels: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl AgentDriver for Counting {
            fn create_session(
                &self,
                cwd: &str,
            ) -> Result<(String, Vec<protocol::SessionConfigOption>), String> {
                Ok((format!("agent_{}", cwd.replace('/', "_")), Vec::new()))
            }
            fn resume_session(
                &self,
                _a: &str,
                _c: &str,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
                Ok(Vec::new())
            }
            fn prompt(
                &self,
                _a: &str,
                _i: Vec<ContentBlock>,
            ) -> tokio::sync::mpsc::Receiver<AgentEvent> {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                tokio::spawn(async move {
                    let _ = tx
                        .send(AgentEvent::TurnEnded(
                            protocol::StateChangeReason::Completed,
                        ))
                        .await;
                });
                rx
            }
            fn cancel(&self, _a: &str) -> Result<(), String> {
                self.cancels
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn close(&self, _a: &str) -> Result<(), String> {
                Ok(())
            }
            fn delete_session(&self, _a: &str) -> Result<(), String> {
                Err("method not found".into())
            }
            fn set_config_option(
                &self,
                _a: &str,
                _config_id: &str,
                _value: protocol::SessionConfigOptionValue,
            ) -> Result<Vec<protocol::SessionConfigOption>, String> {
                Ok(Vec::new())
            }

            fn shutdown(&self) {}
        }

        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let agents = Arc::new(AgentRegistry::new_for_tests_with_driver(
            "track",
            Arc::new(Counting {
                cancels: cancels.clone(),
            }),
        ));
        let dir = std::env::temp_dir().join(format!(
            "amux-cancel-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(SessionRegistry::open(&dir.join("session.sqlite")).unwrap());
        let (mgr, _rx) = SessionManager::new(agents, registry.clone(), dir.clone());

        // 从未 prompt 的会话处于 Idle：cancel 应幂等成功且不触达 ACP
        let meta = mgr.create("track", "/tmp/idle", false).await.unwrap();
        mgr.cancel(&meta.id).await.unwrap();
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "空闲会话的取消不应透传 ACP agent"
        );
        // 状态不被取消操作扰动
        assert_eq!(
            registry.get(&meta.id).unwrap().unwrap().0.state,
            protocol::SessionState::Idle
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
