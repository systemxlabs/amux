//! 会话注册表：会话列表由 server 权威维护，
//! 元数据持久化于 SQLite（`~/.amux/server/session.sqlite`）。server 单写者场景，
//! 使用 rusqlite 同步 API（连接置于互斥锁内，短临界区）。
//!
//! 会话仅能经 server 创建（session.new），注册表由构造完整；
//! agent 侧存在但注册表未知的旧会话不出现（不列出、不打开、不回填）。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use protocol::{SessionMeta, SessionState};
use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};

/// SQLite 会话注册表（server 单写者：内部 Connection 用互斥锁串行化）。
pub struct SessionRegistry {
    conn: Mutex<Connection>,
}

/// 注册表条目：会话元数据 + agent 侧会话 id（驱动操作需要）。
pub type RegistryEntry = (SessionMeta, String);

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
    let config_options: String = row.get("config_options")?;
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
        config_options: serde_json::from_str(&config_options).unwrap_or_default(),
    };
    let agent_session_id: String = row.get("agent_session_id")?;
    Ok((meta, agent_session_id))
}

impl SessionRegistry {
    fn connection(&self) -> rusqlite::Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                "session registry mutex poisoned",
            )))
        })
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
                agent_session_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_active_at INTEGER NOT NULL,
                worktree_dir TEXT NOT NULL DEFAULT '',
                context_size INTEGER NOT NULL DEFAULT 0,
                context_window_size INTEGER NOT NULL DEFAULT 0,
                config_options TEXT NOT NULL DEFAULT '[]'
            );",
        )?;
        Ok(SessionRegistry {
            conn: Mutex::new(conn),
        })
    }

    /// 插入或更新会话元数据（create / 标题 / 状态 / 时间戳更新均走这里）。
    pub fn upsert(&self, meta: &SessionMeta, agent_session_id: &str) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO sessions
                (id, agent, cwd, state, title, agent_session_id, created_at, last_active_at, worktree_dir, context_size, context_window_size, config_options)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                agent=excluded.agent, cwd=excluded.cwd, state=excluded.state,
                title=excluded.title, agent_session_id=excluded.agent_session_id,
                created_at=excluded.created_at, last_active_at=excluded.last_active_at,
                worktree_dir=excluded.worktree_dir,
                context_size=excluded.context_size, context_window_size=excluded.context_window_size,
                config_options=excluded.config_options",
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
                serde_json::to_string(&meta.config_options).unwrap_or_else(|_| "[]".into()),
            ],
        )?;
        Ok(())
    }

    /// 按 server 会话 id 取条目（含 agent 侧会话 id）。
    pub fn get(&self, id: &str) -> rusqlite::Result<Option<RegistryEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent, cwd, state, title, agent_session_id, created_at, last_active_at, worktree_dir, context_size, context_window_size, config_options
             FROM sessions WHERE id = ?1",
        )?;
        stmt.query_row(params![id], row_to_entry).optional()
    }

    /// 全部条目，按最近活跃（last_active_at）降序——惰性分页的上游数据。
    pub fn list(&self) -> rusqlite::Result<Vec<RegistryEntry>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent, cwd, state, title, agent_session_id, created_at, last_active_at, worktree_dir, context_size, context_window_size, config_options
             FROM sessions ORDER BY last_active_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_entry)?;
        rows.collect()
    }

    /// 删除条目；返回是否存在。
    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.connection()?;
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
        let conn = self.connection()?;
        conn.execute(
            "UPDATE sessions SET state = ?1, last_active_at = ?2 WHERE id = ?3",
            params![state.as_str(), last_active_at as i64, id],
        )?;
        Ok(())
    }

    /// 更新标题与最近活跃时间。
    pub fn set_title(&self, id: &str, title: &str, last_active_at: u64) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE sessions SET title = ?1, last_active_at = ?2 WHERE id = ?3",
            params![title, last_active_at as i64, id],
        )?;
        Ok(())
    }

    /// 回填 agent 侧会话 id：创建会话时未与 ACP 交互（agent 侧会话延后到首次
    /// prompt 懒创建），首次 prompt 时经 `session/new` 拿到 id 后写入。
    pub fn set_agent_session_id(&self, id: &str, agent_session_id: &str) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE sessions SET agent_session_id = ?1 WHERE id = ?2",
            params![agent_session_id, id],
        )?;
        Ok(())
    }

    /// 记录会话上下文大小（docs/DESIGN.md「ACP 通信」：接收 `usage_update`
    /// 通知后写入当前上下文大小与窗口总大小，单位 token）。
    pub fn set_context_size(
        &self,
        id: &str,
        context_size: u64,
        context_window_size: u64,
    ) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "UPDATE sessions SET context_size = ?1, context_window_size = ?2 WHERE id = ?3",
            params![context_size as i64, context_window_size as i64, id],
        )?;
        Ok(())
    }

    /// 覆盖记录会话配置选项（docs/DESIGN.md「普通会话存储」：元数据含会话选项；
    /// 选项来自 ACP 会话的 new/resume/set_config_option 响应或 config_options_update）。
    pub fn set_config_options(
        &self,
        id: &str,
        config_options: &[protocol::SessionConfigOption],
    ) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        let json = serde_json::to_string(config_options).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "UPDATE sessions SET config_options = ?1 WHERE id = ?2",
            params![json, id],
        )?;
        Ok(())
    }

    /// 清空会话的 worktree 目录（worktree 被自动清理后回退为原始工作目录，
    /// 避免 workspace RPC / prompt 指向已不存在的目录）。
    pub fn clear_worktree_dir(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.connection()?;
        conn.execute("UPDATE sessions SET worktree_dir = '' WHERE id = ?1", [id])?;
        Ok(())
    }

    /// 有 worktree 且超过 `idle_timeout_ms` 不活跃的 idle 会话候选
    /// （docs/DESIGN.md「工作树存储」：不活跃会话自动清理其 worktree）。
    /// 返回 (会话 id, 原始工作目录 cwd, worktree 目录)。
    pub fn idle_worktree_candidates(
        &self,
        now: u64,
        idle_timeout_ms: u64,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, cwd, worktree_dir, last_active_at FROM sessions
             WHERE state = 'idle' AND worktree_dir != ''",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>("id")?,
                row.get::<_, String>("cwd")?,
                row.get::<_, String>("worktree_dir")?,
                row.get::<_, i64>("last_active_at")? as u64,
            ))
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, _, _, t)| now.saturating_sub(*t) > idle_timeout_ms)
            .map(|(id, cwd, worktree_dir, _)| (id, cwd, worktree_dir))
            .collect())
    }

    /// server 启动时把异常退出残留的 Busy 会话重置为 Idle（无对应运行中 agent）。
    pub fn reset_busy_to_idle(&self) -> rusqlite::Result<usize> {
        let conn = self.connection()?;
        let n = conn.execute(
            "UPDATE sessions SET state = 'idle' WHERE state = 'busy'",
            [],
        )?;
        Ok(n)
    }

    /// 全部会话的会话 id 与最近活跃时间，用于无活动回收。
    pub fn idle_candidates(
        &self,
        now: u64,
        idle_timeout_ms: u64,
    ) -> rusqlite::Result<Vec<(String, u64)>> {
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT id, last_active_at FROM sessions WHERE state = 'idle'")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>("id")?,
                row.get::<_, i64>("last_active_at")? as u64,
            ))
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, t)| now.saturating_sub(*t) > idle_timeout_ms)
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
                config_options: Vec::new(),
            },
            format!("agent_{id}"),
        )
    }

    #[test]
    fn registry_crud_roundtrip() {
        let db = tmp_db("crud");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, &aid).unwrap();

        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.id, "s1");
        assert_eq!(got.1, "agent_s1");
        assert_eq!(got.0.last_active_at, 100);

        reg.update_state("s1", SessionState::Busy, 200).unwrap();
        reg.set_title("s1", "我的标题", 300).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.state, SessionState::Busy);
        assert_eq!(got.0.title, "我的标题");
        assert_eq!(got.0.last_active_at, 300);

        let (m2, a2) = meta("s2", 400);
        reg.upsert(&m2, &a2).unwrap();
        let all = reg.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.id, "s2");
        assert_eq!(all[1].0.id, "s1");

        assert!(reg.delete("s1").unwrap());
        assert!(!reg.delete("s1").unwrap());
        assert!(reg.get("s1").unwrap().is_none());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn config_options_roundtrip() {
        use protocol::{SessionConfigKind, SessionConfigOption, SessionConfigSelectEntry};
        let db = tmp_db("cfg");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, &aid).unwrap();
        assert!(
            reg.get("s1").unwrap().unwrap().0.config_options.is_empty(),
            "默认无配置选项"
        );

        let opts = vec![
            SessionConfigOption {
                id: "model".into(),
                name: "模型".into(),
                description: None,
                category: Some("model".into()),
                kind: SessionConfigKind::Select {
                    current_value: "gpt-5".into(),
                    options: vec![SessionConfigSelectEntry {
                        value: "gpt-5".into(),
                        name: "GPT-5".into(),
                    }],
                },
            },
            SessionConfigOption {
                id: "thought".into(),
                name: "推理级别".into(),
                description: None,
                category: None,
                kind: SessionConfigKind::Boolean {
                    current_value: true,
                },
            },
        ];
        reg.set_config_options("s1", &opts).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(
            got.0.config_options, opts,
            "set_config_options 后应读回相同选项"
        );

        // upsert 为全字段覆盖（调用方从 get_entry 取 meta 携带最新选项）：
        // meta 未携带选项时覆盖为空——真实调用方不会这样做
        let (mut m2, _) = meta("s1", 200);
        m2.config_options = opts.clone();
        reg.upsert(&m2, &aid).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.config_options, opts);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn context_size_persists_and_roundtrips() {
        let db = tmp_db("ctx");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, aid) = meta("s1", 100);
        reg.upsert(&m, &aid).unwrap();

        reg.set_context_size("s1", 53_000, 200_000).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.context_size, 53_000);
        assert_eq!(got.0.context_window_size, 200_000);

        // upsert（标题/状态更新）不应覆盖已记录的上下文大小
        let (mut m2, _) = meta("s1", 200);
        m2.context_size = 60_000;
        m2.context_window_size = 200_000;
        reg.upsert(&m2, &aid).unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.context_size, 60_000);
        assert_eq!(got.0.context_window_size, 200_000);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn idle_worktree_candidates_and_clear() {
        let db = tmp_db("wtcand");
        let reg = SessionRegistry::open(&db).unwrap();

        // s1：有 worktree 且超时 → 候选
        let (mut m1, a1) = meta("s1", 100);
        m1.worktree_dir = "/tmp/wt1".into();
        reg.upsert(&m1, &a1).unwrap();
        // s2：无 worktree → 排除
        let (m2, a2) = meta("s2", 100);
        reg.upsert(&m2, &a2).unwrap();
        // s3：有 worktree 但最近活跃 → 排除
        let (mut m3, a3) = meta("s3", 100);
        m3.worktree_dir = "/tmp/wt3".into();
        reg.upsert(&m3, &a3).unwrap();
        reg.update_state("s3", SessionState::Idle, 900).unwrap();

        let now = 1000u64;
        let cands = reg.idle_worktree_candidates(now, 500).unwrap();
        assert_eq!(cands.len(), 1, "仅超时的 worktree 会话入候选: {cands:?}");
        assert_eq!(cands[0], ("s1".into(), "/tmp".into(), "/tmp/wt1".into()));

        // 清空字段后不再入候选
        reg.clear_worktree_dir("s1").unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.0.worktree_dir, "");
        assert!(reg.idle_worktree_candidates(now, 500).unwrap().is_empty());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn deferred_agent_session_id_backfill() {
        let db = tmp_db("defer");
        let reg = SessionRegistry::open(&db).unwrap();
        let (m, _) = meta("s1", 100);
        reg.upsert(&m, "").unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.1, "");
        reg.set_agent_session_id("s1", "mock_s_1").unwrap();
        let got = reg.get("s1").unwrap().unwrap();
        assert_eq!(got.1, "mock_s_1");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn registry_persists_across_reopen() {
        let db = tmp_db("persist");
        {
            let reg = SessionRegistry::open(&db).unwrap();
            let (m, aid) = meta("s1", 100);
            reg.upsert(&m, &aid).unwrap();
        }
        let reg = SessionRegistry::open(&db).unwrap();
        let all = reg.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0.id, "s1");
        assert_eq!(all[0].1, "agent_s1");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn registry_is_send_sync() {
        let db = tmp_db("send");
        let reg = Arc::new(SessionRegistry::open(&db).unwrap());
        let reg2 = reg.clone();
        let h = std::thread::spawn(move || {
            let (m, aid) = meta("s1", 1);
            reg2.upsert(&m, &aid).unwrap();
        });
        h.join().unwrap();
        assert_eq!(reg.list().unwrap().len(), 1);
        let _ = std::fs::remove_file(&db);
    }
}
