//! 会话注册表：会话列表由 server 权威维护，
//! 元数据持久化于 SQLite（`~/.amux/server/session.sqlite`）。server 单写者场景，
//! 使用 rusqlite 同步 API（连接置于互斥锁内，短临界区）。
//!
//! 会话仅能经 server 创建（session.new），注册表由构造完整；
//! agent 侧存在但注册表未知的旧会话不出现（不列出、不打开、不回填）。

use parking_lot::{Mutex, MutexGuard};
use std::path::Path;

use protocol::{SessionMeta, SessionState};
use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};

/// SQLite 会话注册表（server 单写者：内部 Connection 用互斥锁串行化）。
pub struct SessionRegistry {
    conn: Mutex<Connection>,
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

const SESSION_SELECT_COLUMNS: &str = "id, agent, cwd, state, title, agent_session_id, created_at, last_active_at, worktree_dir, context_size, context_window_size";

fn session_select(suffix: &str) -> String {
    format!("SELECT {SESSION_SELECT_COLUMNS} FROM sessions {suffix}")
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
        cwd: row.get("cwd")?,
        state: state_from_str(&row.get::<_, String>("state")?)?,
        title: row.get("title")?,
        created_at: row.get::<_, i64>("created_at")? as u64,
        last_active_at: row.get::<_, i64>("last_active_at")? as u64,
        worktree_dir: row.get("worktree_dir")?,
        context_size: row.get::<_, i64>("context_size")? as u64,
        context_window_size: row.get::<_, i64>("context_window_size")? as u64,
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
                agent TEXT NOT NULL,
                cwd TEXT NOT NULL,
                state TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                agent_session_id TEXT,
                created_at INTEGER NOT NULL,
                last_active_at INTEGER NOT NULL,
                worktree_dir TEXT NOT NULL DEFAULT '',
                context_size INTEGER NOT NULL DEFAULT 0,
                context_window_size INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        // server 重启后恢复的会话一律回到空闲：busy 状态由上一进程持有，
        // 其 agent 侧 turn 已随进程终止，残留 busy 会令后续 prompt 永远被拒。
        // agent_session_id 保留，下一次交互按设计走惰性 session/resume。
        conn.execute(
            "UPDATE sessions SET state = ?1 WHERE state = ?2",
            params![SessionState::Idle.as_str(), SessionState::Busy.as_str()],
        )?;
        Ok(SessionRegistry {
            conn: Mutex::new(conn),
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
                (id, agent, cwd, state, title, agent_session_id, created_at, last_active_at, worktree_dir, context_size, context_window_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                agent=excluded.agent, cwd=excluded.cwd, state=excluded.state,
                title=excluded.title, agent_session_id=excluded.agent_session_id,
                created_at=excluded.created_at, last_active_at=excluded.last_active_at,
                worktree_dir=excluded.worktree_dir,
                context_size=excluded.context_size, context_window_size=excluded.context_window_size",
            params![
                meta.id,
                meta.agent,
                meta.cwd,
                meta.state.as_str(),
                meta.title,
                agent_session_id,
                meta.created_at as i64,
                meta.last_active_at as i64,
                meta.worktree_dir,
                meta.context_size as i64,
                meta.context_window_size as i64,
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

    /// 记录会话上下文大小（接收 ACP `usage_update`
    /// 通知后写入当前上下文大小与窗口总大小，单位 token）。
    pub fn set_context_size(
        &self,
        id: &str,
        context_size: u64,
        context_window_size: u64,
    ) -> rusqlite::Result<()> {
        let conn = self.connection();
        conn.execute(
            "UPDATE sessions SET context_size = ?1, context_window_size = ?2 WHERE id = ?3",
            params![context_size as i64, context_window_size as i64, id],
        )?;
        Ok(())
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
            "SELECT id, cwd, worktree_dir, last_active_at FROM sessions
             WHERE state = ?1 AND worktree_dir != ''",
        )?;
        let rows = stmt.query_map([SessionState::Idle.as_str()], |row| {
            Ok((
                WorktreeCandidate {
                    session_id: row.get::<_, String>("id")?,
                    cwd: row.get::<_, String>("cwd")?,
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
    use std::sync::Arc;

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
                context_size: 0,
                context_window_size: 0,
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
        // 回归：server 重启后残留 busy 会让 prompt 永远被拒（agent 侧 turn 已随
        // 进程终止），重新打开注册表时必须复位为空闲。
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
    fn context_size_persists_and_roundtrips() {
        let db = tmp_db("ctx");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, Some(&aid)).unwrap();

        reg.set_context_size("s1", 53_000, 200_000).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.meta.context_size, 53_000);
        assert_eq!(got.meta.context_window_size, 200_000);

        // upsert（标题/状态更新）不应覆盖已记录的上下文大小
        let (mut m2, _) = meta("s1", 200);
        m2.context_size = 60_000;
        m2.context_window_size = 200_000;
        reg.upsert(&m2, Some(&aid)).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.meta.context_size, 60_000);
        assert_eq!(got.meta.context_window_size, 200_000);
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
