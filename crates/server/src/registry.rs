//! 会话注册表：会话列表由 server 权威维护，
//! 元数据持久化于 SQLite（`~/.amux/server/session.sqlite`）。server 单写者场景，
//! 使用 rusqlite 同步 API（连接置于互斥锁内，短临界区）。
//!
//! 会话仅能经 server 创建（session.new），注册表由构造完整；
//! agent 侧存在但注册表未知的旧会话不出现（不列出、不打开、不回填）。

use parking_lot::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::path::Path;

use protocol::{Activity, HistoryItem, SessionContextResult, SessionMeta, SessionState};
use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};

/// SQLite 会话注册表（server 单写者：内部 Connection 用互斥锁串行化）。
/// 会话上下文大小不落盘：存储在内存（`contexts`），以 Agent 侧数据为权威，
/// server 重启后由下一次 ACP `usage_update` 通知重新填充。
pub struct SessionRegistry {
    conn: Mutex<Connection>,
    contexts: Mutex<HashMap<String, SessionContextResult>>,
}

/// 注册表条目：会话元数据 + agent 侧会话 id（驱动操作需要）。
/// `agent_session_id` 为 None 表示 agent 侧会话尚未惰性创建。
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub meta: SessionMeta,
    pub agent_session_id: Option<String>,
}

/// 超时 worktree 清理所需的最小会话信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCandidate {
    pub session_id: String,
    pub cwd: String,
    pub worktree_dir: String,
}

/// 超时 agent 会话回收所需的会话信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleCandidate {
    pub session_id: String,
    pub last_active_at: u64,
}

const SESSION_SELECT_COLUMNS: &str = "id, state, title, workspace, worktree_dir, agent, agent_session_id, created_at, last_active_at";

fn session_select(suffix: &str) -> String {
    format!("SELECT {SESSION_SELECT_COLUMNS} FROM sessions {suffix}")
}

/// 分页查询的单行：`kind` 为 messages.role / activities.kind，
/// `content` 为条目 JSON，`created_at` 为首次写入时间。
struct StoredRow {
    kind: String,
    content: String,
    created_at: u64,
}

/// 组装分页窗口：`has_more` 由是否多取一条决定；`next_offset` 为下一页
/// 的 LIMIT/OFFSET 偏移（= 本页 offset + 已返回条数），窗口按 rowid 升序返回。
fn finish_page<T>(
    rows: &[StoredRow],
    has_more: bool,
    offset: usize,
    items: Vec<T>,
) -> (Vec<T>, bool, Option<usize>) {
    let next_offset = if has_more {
        Some(offset + rows.len())
    } else {
        None
    };
    (items, has_more, next_offset)
}

fn state_from_str(s: &str) -> rusqlite::Result<SessionState> {
    protocol::parse_session_state(s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid session state: {s}"),
            )),
        )
    })
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<RegistryEntry> {
    let meta = SessionMeta {
        id: row.get("id")?,
        agent: row.get("agent")?,
        cwd: row.get("workspace")?,
        state: state_from_str(&row.get::<_, String>("state")?)?,
        title: row.get("title")?,
        created_at: row.get::<_, i64>("created_at")? as u64,
        last_active_at: row.get::<_, i64>("last_active_at")? as u64,
        worktree_dir: row.get("worktree_dir")?,
    };
    Ok(RegistryEntry {
        meta,
        agent_session_id: row.get("agent_session_id")?,
    })
}

impl SessionRegistry {
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// 打开（或创建）注册表数据库；建表幂等。
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                title TEXT,
                workspace TEXT NOT NULL,
                worktree_dir TEXT,
                agent TEXT NOT NULL,
                agent_session_id TEXT,
                created_at INTEGER NOT NULL,
                last_active_at INTEGER NOT NULL
            );
            -- 对话历史：消息内容按 (session_id, message_id) upsert（v2 流式 update
            -- 的 upsert 语义）；LIMIT/OFFSET 分页按 rowid（首次插入顺序）倒序取窗。
            CREATE TABLE IF NOT EXISTS messages (
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, message_id)
            );
            -- 活动历史：activity_id = toolCallId / thought messageId / 本地生成 ID
            CREATE TABLE IF NOT EXISTS activities (
                session_id TEXT NOT NULL,
                activity_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, activity_id)
            );",
        )?;
        // server 重启后恢复的会话一律回到空闲：busy 状态由上一进程持有，
        // 其 agent 侧 turn 已随进程终止，残留 busy 会让列表状态永远不空闲。
        // agent_session_id 保留，下一次交互按设计走惰性 session/resume。
        conn.execute(
            "UPDATE sessions SET state = ?1 WHERE state = ?2",
            params![SessionState::Idle.as_str(), SessionState::Busy.as_str()],
        )?;
        Ok(SessionRegistry {
            conn: Mutex::new(conn),
            contexts: Mutex::new(HashMap::new()),
        })
    }

    /// 插入或更新会话元数据（create / 标题 / 状态 / 时间戳更新均走这里）。
    /// `agent_session_id` 为 NULL 表示尚无 agent 侧会话。
    pub fn upsert(
        &self,
        meta: &SessionMeta,
        agent_session_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "INSERT INTO sessions
                (id, state, title, workspace, worktree_dir, agent, agent_session_id, created_at, last_active_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                state=excluded.state, title=excluded.title, workspace=excluded.workspace,
                worktree_dir=excluded.worktree_dir, agent=excluded.agent,
                agent_session_id=excluded.agent_session_id,
                created_at=excluded.created_at, last_active_at=excluded.last_active_at",
            params![
                meta.id,
                meta.state.as_str(),
                meta.title,
                meta.cwd,
                meta.worktree_dir,
                meta.agent,
                agent_session_id,
                meta.created_at as i64,
                meta.last_active_at as i64,
            ],
        )?;
        Ok(())
    }

    /// 按 server 会话 id 取条目（含 agent 侧会话 id）。
    pub fn get(&self, id: &str) -> rusqlite::Result<Option<RegistryEntry>> {
        let conn = self.connection();
        let mut stmt = conn.prepare(&session_select("WHERE id = ?1"))?;
        stmt.query_row(params![id], row_to_entry).optional()
    }

    /// 最近活跃的前缀，最多 `limit` 条；多读一条用于判断是否还有更多。
    /// SQL 层截断，避免会话规模增长后把全表装载进内存。
    pub fn list(&self, limit: usize) -> rusqlite::Result<(Vec<RegistryEntry>, bool)> {
        let query_limit = limit.max(1).saturating_add(1) as i64;
        let conn = self.connection();
        let mut stmt = conn.prepare(&session_select(
            "ORDER BY last_active_at DESC, id DESC LIMIT ?1",
        ))?;
        let rows = stmt.query_map([query_limit], row_to_entry)?;
        let mut entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok((entries, has_more))
    }

    /// 删除条目；返回是否存在。
    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        self.contexts.lock().remove(id);
        let conn = self.connection();
        let n = conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// 更新会话状态与最近活跃时间。
    pub fn update_state(
        &self,
        id: &str,
        state: SessionState,
        last_active_at: u64,
    ) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sessions SET state = ?1, last_active_at = ?2 WHERE id = ?3",
            params![state.as_str(), last_active_at as i64, id],
        )?;
        Ok(())
    }

    /// 更新标题与最近活跃时间。
    pub fn set_title(&self, id: &str, title: &str, last_active_at: u64) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sessions SET title = ?1, last_active_at = ?2 WHERE id = ?3",
            params![title, last_active_at as i64, id],
        )?;
        Ok(())
    }

    /// 回填或清除 agent 侧会话 id：创建会话时未与 ACP 交互（agent 侧会话延后到
    /// 首次 prompt 懒创建），首次 prompt 时经 `session/new` 拿到 id 后写入。
    pub fn set_agent_session_id(
        &self,
        id: &str,
        agent_session_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sessions SET agent_session_id = ?1 WHERE id = ?2",
            params![agent_session_id, id],
        )?;
        Ok(())
    }

    /// 记录会话上下文大小（接收 ACP `usage_update` 通知后写入，
    /// 单位 token；仅内存存储，以 Agent 侧数据为权威）。
    pub fn set_context_size(&self, id: &str, context_size: u64, context_window_size: u64) {
        self.contexts.lock().insert(
            id.to_string(),
            SessionContextResult {
                context_size,
                context_window_size,
            },
        );
    }

    /// 查询会话上下文大小；尚未收到 `usage_update` 通知时两者均为 0。
    pub fn context(&self, id: &str) -> SessionContextResult {
        self.contexts.lock().get(id).copied().unwrap_or_default()
    }

    /// 消息 upsert（对话历史）：同 (session_id, message_id) 覆盖内容与 updated_at，
    /// 首次插入时记录 created_at。消息 ID 由上游提供（用户消息为 Amux 生成，
    /// agent 消息为 ACP `messageId`）。
    pub fn upsert_message(
        &self,
        session_id: &str,
        message_id: &str,
        role: &str,
        content_json: &str,
        created_at: u64,
        updated_at: u64,
    ) -> rusqlite::Result<()> {
        self.connection().execute(
            "INSERT INTO messages (session_id, message_id, role, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (session_id, message_id) DO UPDATE SET
                content = excluded.content,
                updated_at = excluded.updated_at",
            params![
                session_id,
                message_id,
                role,
                content_json,
                created_at as i64,
                updated_at as i64
            ],
        )?;
        Ok(())
    }

    /// 删除单条消息（v2 `agent_message` 清空内容时不留空条目）。
    pub fn remove_message(&self, session_id: &str, message_id: &str) -> rusqlite::Result<()> {
        self.connection().execute(
            "DELETE FROM messages WHERE session_id = ?1 AND message_id = ?2",
            params![session_id, message_id],
        )?;
        Ok(())
    }

    /// 活动 upsert：activity_id = toolCallId / thought messageId / 本地生成 ID。
    pub fn upsert_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        kind: &str,
        content_json: &str,
        created_at: u64,
        updated_at: u64,
    ) -> rusqlite::Result<()> {
        self.connection().execute(
            "INSERT INTO activities (session_id, activity_id, kind, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (session_id, activity_id) DO UPDATE SET
                kind = excluded.kind,
                content = excluded.content,
                updated_at = excluded.updated_at",
            params![
                session_id,
                activity_id,
                kind,
                content_json,
                created_at as i64,
                updated_at as i64
            ],
        )?;
        Ok(())
    }

    /// LIMIT/OFFSET 分页读取对话历史尾部：`offset` 从最新一条算起跳过条数
    /// （0 = 最新一窗），返回按 rowid 升序的窗口。
    pub fn history_page(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<(Vec<HistoryItem>, bool, Option<usize>)> {
        let (rows, has_more) = self.page_rows("messages", session_id, limit, offset)?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let blocks: Vec<protocol::ContentBlock> = serde_json::from_str(&row.content)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            items.push(match row.kind.as_str() {
                "user" => HistoryItem::UserMessage {
                    content: blocks,
                    timestamp: row.created_at,
                },
                _ => HistoryItem::AgentMessage {
                    content: blocks,
                    timestamp: row.created_at,
                },
            });
        }
        Ok(finish_page(&rows, has_more, offset, items))
    }

    /// LIMIT/OFFSET 分页读取活动历史尾部；语义同 [`Self::history_page`]。
    pub fn activities_page(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<(Vec<Activity>, bool, Option<usize>)> {
        let (rows, has_more) = self.page_rows("activities", session_id, limit, offset)?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let activity: Activity = serde_json::from_str(&row.content)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            items.push(activity);
        }
        Ok(finish_page(&rows, has_more, offset, items))
    }

    /// 会话是否有任何历史/活动数据（删除路径据此决定是否清理）。
    pub fn session_data_exists(&self, session_id: &str) -> rusqlite::Result<bool> {
        let conn = self.connection();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id = ?1)
                 OR EXISTS(SELECT 1 FROM activities WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    /// 删除会话的历史与活动数据。
    pub fn remove_session_data(&self, session_id: &str) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        conn.execute(
            "DELETE FROM activities WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// 按首次插入顺序（rowid）倒序取一窗（多取一条判断 has_more），返回升序窗口。
    /// 每行：`(role/kind, content, created_at)`。
    fn page_rows(
        &self,
        table: &str,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<(Vec<StoredRow>, bool)> {
        let limit = limit.max(1);
        let conn = self.connection();
        let sql = format!(
            "SELECT {kind}, content, created_at FROM {table}
             WHERE session_id = ?1
             ORDER BY rowid DESC LIMIT ?2 OFFSET ?3",
            kind = if table == "messages" { "role" } else { "kind" },
        );
        let mut stmt = conn.prepare(&sql)?;
        let mapped = stmt.query_map(
            params![session_id, limit as i64 + 1, offset as i64],
            |row| {
                Ok(StoredRow {
                    kind: row.get::<_, String>(0)?,
                    content: row.get::<_, String>(1)?,
                    created_at: row.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        let mut rows = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = rows.len() > limit;
        if has_more {
            // 多取的是最旧一条（位于 DESC 窗口末尾）：仅保留最新 limit 条
            rows.truncate(limit);
        }
        rows.reverse();
        Ok((rows, has_more))
    }

    /// 有 worktree 且超过 `idle_timeout_ms` 不活跃的 idle 会话候选。
    /// 返回 (会话 id, 原始工作目录 cwd, worktree 目录)。
    pub fn idle_worktree_candidates(
        &self,
        now: u64,
        idle_timeout: std::time::Duration,
    ) -> rusqlite::Result<Vec<WorktreeCandidate>> {
        let conn = self.connection();
        let mut stmt = conn.prepare(
            "SELECT id, workspace, worktree_dir, last_active_at FROM sessions
             WHERE state = ?1 AND worktree_dir != ''",
        )?;
        let rows = stmt.query_map([SessionState::Idle.as_str()], |row| {
            Ok((
                WorktreeCandidate {
                    session_id: row.get::<_, String>("id")?,
                    cwd: row.get::<_, String>("workspace")?,
                    worktree_dir: row.get::<_, String>("worktree_dir")?,
                },
                row.get::<_, i64>("last_active_at")? as u64,
            ))
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, timestamp)| {
                now.saturating_sub(*timestamp) > idle_timeout.as_millis() as u64
            })
            .map(|(candidate, _)| candidate)
            .collect())
    }

    /// 全部会话的会话 id 与最近活跃时间，用于无活动回收。
    pub fn idle_candidates(
        &self,
        now: u64,
        idle_timeout: std::time::Duration,
    ) -> rusqlite::Result<Vec<IdleCandidate>> {
        let conn = self.connection();
        let mut stmt = conn.prepare("SELECT id, last_active_at FROM sessions WHERE state = ?1")?;
        let rows = stmt.query_map([SessionState::Idle.as_str()], |row| {
            Ok(IdleCandidate {
                session_id: row.get::<_, String>("id")?,
                last_active_at: row.get::<_, i64>("last_active_at")? as u64,
            })
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|candidate| {
                now.saturating_sub(candidate.last_active_at) > idle_timeout.as_millis() as u64
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ContentBlock;
    use std::sync::Arc;

    fn text_blocks(text: &str) -> String {
        serde_json::to_string(&[ContentBlock::Text { text: text.into() }]).unwrap()
    }

    fn text_of(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn item_text(item: &HistoryItem) -> String {
        match item {
            HistoryItem::UserMessage { content, .. }
            | HistoryItem::AgentMessage { content, .. } => text_of(content),
        }
    }

    fn tmp_db(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "amux-registry-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn meta(id: &str, last_active_at: u64) -> (SessionMeta, String) {
        (
            SessionMeta {
                id: id.into(),
                agent: "codex".into(),
                cwd: "/tmp".into(),
                state: SessionState::Idle,
                title: String::new(),
                created_at: 1,
                last_active_at,
                worktree_dir: String::new(),
            },
            format!("agent_{id}"),
        )
    }

    #[test]
    fn registry_crud_roundtrip() {
        let db = tmp_db("crud");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, Some(&aid)).unwrap();

        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.meta.id, "s1");
        assert_eq!(got.agent_session_id.as_deref(), Some("agent_s1"));
        assert_eq!(got.meta.last_active_at, 100);

        reg.update_state("s1", SessionState::Busy, 200).unwrap();
        reg.set_title("s1", "我的标题", 300).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.meta.state, SessionState::Busy);
        assert_eq!(got.meta.title, "我的标题");
        assert_eq!(got.meta.last_active_at, 300);

        let (m2, a2) = meta("s2", 400);
        reg.upsert(&m2, Some(&a2)).unwrap();
        let (all, has_more) = reg.list(10).unwrap();
        assert!(!has_more);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].meta.id, "s2");
        assert_eq!(all[1].meta.id, "s1");
        let (limited, has_more) = reg.list(1).unwrap();
        assert!(has_more);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].meta.id, "s2");

        assert!(reg.delete("s1").unwrap());
        assert!(!reg.delete("s1").unwrap());
        assert!(reg.get("s1").unwrap().is_none());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn reopen_resets_stale_busy_to_idle() {
        // 回归：server 重启后残留 busy 与真实状态不符（agent 侧 turn 已随
        // 进程终止，不会再有 turn 收尾回写空闲），重新打开注册表时必须复位为空闲。
        let db = tmp_db("reset-busy");
        {
            let reg = SessionRegistry::open(&db).unwrap();
            let (m, aid) = meta("s1", 100);
            reg.upsert(&m, Some(&aid)).unwrap();
            reg.update_state("s1", SessionState::Busy, 200).unwrap();
            assert_eq!(
                reg.get("s1").unwrap().unwrap().meta.state,
                SessionState::Busy
            );
        }
        // 原实例 drop 后重新打开：同一份 sqlite，模拟 server 重启
        {
            let reg = SessionRegistry::open(&db).unwrap();
            let got = reg.get("s1").unwrap().unwrap();
            assert_eq!(got.meta.state, SessionState::Idle);
            // agent 侧会话 id 保留：下一次交互按设计走惰性 session/resume
            assert_eq!(got.agent_session_id.as_deref(), Some("agent_s1"));
        }
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn context_size_kept_in_memory_only() {
        let db = tmp_db("ctx");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, Some(&aid)).unwrap();

        assert_eq!(reg.context("s1"), SessionContextResult::default());
        reg.set_context_size("s1", 53_000, 200_000);
        assert_eq!(
            reg.context("s1"),
            SessionContextResult {
                context_size: 53_000,
                context_window_size: 200_000,
            }
        );

        // 上下文不落盘：同一份 sqlite 重新打开（模拟 server 重启）后回到未上报状态
        drop(reg);
        let reg = SessionRegistry::open(&db).unwrap();
        assert_eq!(reg.context("s1"), SessionContextResult::default());

        // 元数据不含上下文字段：upsert 也无法覆盖上下文内存
        let (m2, _) = meta("s1", 200);
        reg.upsert(&m2, Some(&aid)).unwrap();
        assert_eq!(reg.context("s1"), SessionContextResult::default());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn idle_worktree_candidates() {
        let db = tmp_db("wtcand");
        let reg = SessionRegistry::open(&db).unwrap();

        // s1：有 worktree 且超时 → 候选
        let (mut m1, a1) = meta("s1", 100);
        m1.worktree_dir = "/tmp/wt1".into();
        reg.upsert(&m1, Some(&a1)).unwrap();
        // s2：无 worktree → 排除
        let (m2, a2) = meta("s2", 100);
        reg.upsert(&m2, Some(&a2)).unwrap();
        // s3：有 worktree 但最近活跃 → 排除
        let (mut m3, a3) = meta("s3", 100);
        m3.worktree_dir = "/tmp/wt3".into();
        reg.upsert(&m3, Some(&a3)).unwrap();
        reg.update_state("s3", SessionState::Idle, 900).unwrap();

        let now = 1000u64;
        let cands = reg
            .idle_worktree_candidates(now, std::time::Duration::from_millis(500))
            .unwrap();
        assert_eq!(cands.len(), 1, "仅超时的 worktree 会话入候选: {cands:?}");
        assert_eq!(
            cands[0],
            WorktreeCandidate {
                session_id: "s1".into(),
                cwd: "/tmp".into(),
                worktree_dir: "/tmp/wt1".into(),
            }
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn deferred_agent_session_id_backfill() {
        let db = tmp_db("defer");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, _) = meta("s1", 100);
        reg.upsert(&m, None).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.agent_session_id, None);
        reg.set_agent_session_id("s1", Some("mock_s_1")).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.agent_session_id.as_deref(), Some("mock_s_1"));
        reg.set_agent_session_id("s1", None).unwrap();
        assert_eq!(reg.get("s1").unwrap().unwrap().agent_session_id, None);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn registry_persists_across_reopen() {
        let db = tmp_db("persist");
        {
            let reg = SessionRegistry::open(&db).unwrap();
            let (m, aid) = meta("s1", 100);
            reg.upsert(&m, Some(&aid)).unwrap();
        }
        let reg = SessionRegistry::open(&db).unwrap();
        let (all, has_more) = reg.list(10).unwrap();
        assert!(!has_more);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].meta.id, "s1");
        assert_eq!(all[0].agent_session_id.as_deref(), Some("agent_s1"));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn message_page_offset_returns_newest_window_first() {
        let db = tmp_db("page");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, Some(&aid)).unwrap();

        // 连续写入 5 条消息：rowid 即插入顺序，created_at 单调递增
        for i in 0..5 {
            reg.upsert_message(
                "s1",
                &format!("m{i}"),
                "user",
                &text_blocks(&format!("msg-{i}")),
                i as u64,
                i as u64,
            )
            .unwrap();
        }

        // 首页：最新 3 条（msg-2/3/4），has_more=true，next_offset=3
        let (page, has_more, next_offset) = reg.history_page("s1", 3, 0).unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(item_text(&page[0]), "msg-2");
        assert_eq!(item_text(&page[2]), "msg-4");
        assert!(has_more);
        assert_eq!(next_offset, Some(3));

        // 更早一窗：跳过 3 条，只剩 msg-0/1，has_more=false
        let (page, has_more, next_offset) =
            reg.history_page("s1", 3, next_offset.unwrap()).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(item_text(&page[0]), "msg-0");
        assert_eq!(item_text(&page[1]), "msg-1");
        assert!(!has_more);
        assert_eq!(next_offset, None);

        // upsert 不改变 rowid 位置：覆盖 msg-3 后首页 3 条仍为 msg-2/3/4（msg-4 位于末尾）
        reg.upsert_message("s1", "m3", "user", &text_blocks("msg-3-updated"), 3, 99)
            .unwrap();
        let (page, _, _) = reg.history_page("s1", 3, 0).unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(item_text(&page[1]), "msg-3-updated");
        assert_eq!(item_text(&page[2]), "msg-4");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn registry_is_send_sync() {
        let db = tmp_db("send");
        let reg = Arc::new(SessionRegistry::open(&db).unwrap());
        let reg2 = reg.clone();
        let h = std::thread::spawn(move || {
            let (m, aid) = meta("s1", 1);
            reg2.upsert(&m, Some(&aid)).unwrap();
        });
        h.join().unwrap();
        assert_eq!(reg.list(10).unwrap().0.len(), 1);
        let _ = std::fs::remove_file(&db);
    }
}
