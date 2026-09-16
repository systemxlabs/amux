//! 持久化：会话/对话/活动（`session.sqlite`）与工作流（`workflow.sqlite`）。
//!
//! 单写者：连接由 `Mutex` 保护，所有查询同步执行（SQLite 本地文件，量级小）。

use std::path::Path;

use amux_common::api::Session;
use amux_common::domain::{Activity, HistoryItem, SessionState};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::timestamps::now_ms;

pub struct Store {
    sessions: Mutex<Connection>,
    workflows: Mutex<Connection>,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(home).map_err(|e| format!("创建 {} 失败: {e}", home.display()))?;
        let sessions = open_db(&home.join("session.sqlite"))?;
        let workflows = open_db(&home.join("workflow.sqlite"))?;
        // Server 重启后残留的「工作中」不再有 agent 侧 turn 支撑，统一回到空闲
        sessions
            .execute(
                "UPDATE sessions SET state = 'idle' WHERE state = 'busy'",
                [],
            )
            .map_err(|e| format!("重置会话状态失败: {e}"))?;
        workflows
            .execute(
                "UPDATE workflows SET state = 'idle' WHERE state = 'busy'",
                [],
            )
            .map_err(|e| format!("重置工作流状态失败: {e}"))?;
        Ok(Self {
            sessions: Mutex::new(sessions),
            workflows: Mutex::new(workflows),
        })
    }

    // ---------- 普通会话 ----------

    pub fn insert_session(&self, session: &Session) -> Result<(), String> {
        self.sessions
            .lock()
            .execute(
                "INSERT INTO sessions (id, state, title, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9)",
                params![
                    session.id,
                    session.state.as_str(),
                    session.title,
                    session.workspace,
                    session.worktree_dir,
                    session.machine,
                    session.agent,
                    session.created_at as i64,
                    session.updated_at as i64,
                ],
            )
            .map_err(|e| format!("写入会话失败: {e}"))?;
        Ok(())
    }

    pub fn session(&self, id: &str) -> Option<Session> {
        self.sessions
            .lock()
            .query_row(
                "SELECT id, state, title, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// 所有会话（含关联会话，供内部使用）。
    pub fn sessions_all(&self) -> Vec<Session> {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, state, title, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], row_to_session);
        collect(rows)
    }

    /// 会话列表：排除被工作流关联的会话（docs/DESIGN.md：`GET /sessions` 只出非关联会话）。
    pub fn sessions_page(&self, limit: usize, offset: usize) -> (Vec<Session>, bool) {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, state, title, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
             FROM sessions
             WHERE id NOT IN (SELECT session_id FROM linked_sessions)
             ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![(limit + 1) as i64, offset as i64], row_to_session);
        let mut sessions = collect(rows);
        let has_more = sessions.len() > limit;
        sessions.truncate(limit);
        (sessions, has_more)
    }

    pub fn sessions_of_agent(&self, machine: &str, agent: &str) -> Vec<Session> {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, state, title, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
             FROM sessions WHERE machine = ?1 AND agent = ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![machine, agent], row_to_session);
        collect(rows)
    }

    /// 关联会话的工作流会话 id。
    pub fn workflow_of_session(&self, session_id: &str) -> Option<String> {
        self.workflows
            .lock()
            .query_row(
                "SELECT workflow_id FROM workflow_linked_sessions WHERE session_id = ?1 LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_state(&self, id: &str, state: SessionState) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now_ms() as i64],
        );
    }

    pub fn set_title(&self, id: &str, title: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now_ms() as i64],
        );
    }

    /// 会话活跃：更新最近活跃时间（会话列表按它排序）。
    pub fn touch(&self, id: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms() as i64],
        );
    }

    pub fn agent_session_id(&self, id: &str) -> Option<String> {
        self.sessions
            .lock()
            .query_row(
                "SELECT agent_session_id FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    pub fn set_agent_session_id(&self, id: &str, agent_session_id: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET agent_session_id = ?2 WHERE id = ?1",
            params![id, agent_session_id],
        );
    }

    pub fn delete_session(&self, id: &str) {
        let conn = self.sessions.lock();
        let _ = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id]);
        let _ = conn.execute("DELETE FROM messages WHERE session_id = ?1", params![id]);
        let _ = conn.execute("DELETE FROM activities WHERE session_id = ?1", params![id]);
        let _ = conn.execute(
            "DELETE FROM linked_sessions WHERE session_id = ?1",
            params![id],
        );
        let _ = self.workflows.lock().execute(
            "DELETE FROM workflow_linked_sessions WHERE session_id = ?1",
            params![id],
        );
    }

    /// 标记关联会话（供 `GET /sessions` 排除）。
    pub fn link_session(&self, workflow_id: &str, session_id: &str) {
        let conn = self.sessions.lock();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO linked_sessions (session_id, workflow_id) VALUES (?1, ?2)",
            params![session_id, workflow_id],
        );
        drop(conn);
        let _ = self.workflows.lock().execute(
            "INSERT OR REPLACE INTO workflow_linked_sessions (workflow_id, session_id) VALUES (?1, ?2)",
            params![workflow_id, session_id],
        );
    }

    // ---------- 对话历史 ----------

    pub fn upsert_message(
        &self,
        session_id: &str,
        message_id: &str,
        role: &str,
        content: &str,
        created_at: u64,
    ) {
        let _ = self.sessions.lock().execute(
            "INSERT INTO messages (session_id, message_id, role, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(session_id, message_id) DO UPDATE SET content = ?4, updated_at = ?6",
            params![
                session_id,
                message_id,
                role,
                content,
                created_at as i64,
                now_ms() as i64
            ],
        );
    }

    pub fn messages_page(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> (Vec<HistoryItem>, bool) {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT role, content, created_at, updated_at FROM messages
             WHERE session_id = ?1 ORDER BY updated_at DESC, message_id DESC LIMIT ?2 OFFSET ?3",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(
            params![session_id, (limit + 1) as i64, offset as i64],
            |row| {
                let role: String = row.get(0)?;
                let content: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let updated_at: i64 = row.get(3)?;
                Ok((role, content, created_at as u64, updated_at as u64))
            },
        );
        let mut items: Vec<HistoryItem> = match rows {
            Ok(rows) => rows
                .flatten()
                .map(|(role, content, created_at, updated_at)| {
                    let blocks = serde_json::from_str(&content).unwrap_or_default();
                    match role.as_str() {
                        "user" => HistoryItem::UserMessage {
                            content: blocks,
                            timestamp: created_at,
                        },
                        _ => HistoryItem::AgentMessage {
                            content: blocks,
                            timestamp: updated_at,
                        },
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let has_more = items.len() > limit;
        items.truncate(limit);
        items.reverse();
        (items, has_more)
    }

    // ---------- 活动历史 ----------

    pub fn upsert_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        kind: &str,
        content: &str,
        created_at: u64,
    ) {
        let _ = self.sessions.lock().execute(
            "INSERT INTO activities (session_id, activity_id, kind, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(session_id, activity_id) DO UPDATE SET content = ?4, updated_at = ?6",
            params![
                session_id,
                activity_id,
                kind,
                content,
                created_at as i64,
                now_ms() as i64
            ],
        );
    }

    pub fn activities_page(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> (Vec<Activity>, bool) {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT content FROM activities WHERE session_id = ?1
             ORDER BY updated_at DESC, activity_id DESC LIMIT ?2 OFFSET ?3",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(
            params![session_id, (limit + 1) as i64, offset as i64],
            |row| row.get::<_, String>(0),
        );
        let mut activities: Vec<Activity> = match rows {
            Ok(rows) => rows
                .flatten()
                .filter_map(|content| serde_json::from_str(&content).ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        let has_more = activities.len() > limit;
        activities.truncate(limit);
        activities.reverse();
        (activities, has_more)
    }

    /// 最近一条活动（进行中活动展示用）。
    pub fn latest_activity(&self, session_id: &str) -> Option<Activity> {
        self.sessions
            .lock()
            .query_row(
                "SELECT content FROM activities WHERE session_id = ?1 ORDER BY updated_at DESC LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|content| serde_json::from_str(&content).ok())
    }

    // ---------- 工作流会话 ----------

    pub fn insert_workflow(
        &self,
        id: &str,
        title: &str,
        state: SessionState,
        plan: &str,
        created_at: u64,
    ) {
        let _ = self.workflows.lock().execute(
            "INSERT INTO workflows (id, title, state, plan, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, title, state.as_str(), plan, created_at as i64],
        );
    }

    pub fn workflow(&self, id: &str) -> Option<WorkflowRow> {
        self.workflows
            .lock()
            .query_row(
                "SELECT id, title, state, plan, created_at, updated_at FROM workflows WHERE id = ?1",
                params![id],
                row_to_workflow,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn workflows_page(&self, limit: usize, offset: usize) -> (Vec<WorkflowRow>, bool) {
        let conn = self.workflows.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, title, state, plan, created_at, updated_at FROM workflows
             ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![(limit + 1) as i64, offset as i64], row_to_workflow);
        let mut workflows = collect(rows);
        let has_more = workflows.len() > limit;
        workflows.truncate(limit);
        (workflows, has_more)
    }

    pub fn set_workflow_state(&self, id: &str, state: SessionState) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state.as_str(), now_ms() as i64],
        );
    }

    pub fn set_workflow_title(&self, id: &str, title: &str) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now_ms() as i64],
        );
    }

    pub fn touch_workflow(&self, id: &str) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms() as i64],
        );
    }

    pub fn delete_workflow(&self, id: &str) {
        let conn = self.workflows.lock();
        let _ = conn.execute("DELETE FROM workflows WHERE id = ?1", params![id]);
        let _ = conn.execute(
            "DELETE FROM workflow_linked_sessions WHERE workflow_id = ?1",
            params![id],
        );
    }

    pub fn linked_sessions(&self, workflow_id: &str) -> Vec<String> {
        let ids: Vec<String> = {
            let conn = self.workflows.lock();
            let mut stmt = match conn
                .prepare("SELECT session_id FROM workflow_linked_sessions WHERE workflow_id = ?1")
            {
                Ok(stmt) => stmt,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map(params![workflow_id], |row| row.get::<_, String>(0));
            match rows {
                Ok(rows) => rows.flatten().collect(),
                Err(_) => return Vec::new(),
            }
        };

        // 关联普通会话按其自身最近活跃（updated_at）排序（docs/PRD.md「工作流会话」）。
        let mut sessions: Vec<(String, u64)> = ids
            .into_iter()
            .filter_map(|session_id| {
                let updated_at = self
                    .sessions
                    .lock()
                    .query_row(
                        "SELECT updated_at FROM sessions WHERE id = ?1",
                        params![session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()?;
                Some((session_id, updated_at as u64))
            })
            .collect();
        sessions.sort_by_key(|(_, updated_at)| std::cmp::Reverse(*updated_at));
        sessions.into_iter().map(|(id, _)| id).collect()
    }
}

/// 工作流会话元数据行。
#[derive(Debug, Clone)]
pub struct WorkflowRow {
    pub id: String,
    pub title: String,
    pub state: SessionState,
    pub plan: String,
    pub created_at: u64,
    pub updated_at: u64,
}

fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             state TEXT NOT NULL,
             title TEXT,
             workspace TEXT NOT NULL,
             worktree_dir TEXT,
             machine TEXT NOT NULL,
             agent TEXT NOT NULL,
             agent_session_id TEXT,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS messages (
             session_id TEXT NOT NULL,
             message_id TEXT NOT NULL,
             role TEXT NOT NULL,
             content TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, message_id)
         );
         CREATE TABLE IF NOT EXISTS activities (
             session_id TEXT NOT NULL,
             activity_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             content TEXT,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, activity_id)
         );
         -- 关联会话标记：GET /sessions 据此排除工作流关联会话
         CREATE TABLE IF NOT EXISTS linked_sessions (
             session_id TEXT PRIMARY KEY,
             workflow_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflows (
             id TEXT PRIMARY KEY,
             title TEXT,
             state TEXT NOT NULL,
             plan TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_linked_sessions (
             workflow_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             PRIMARY KEY (workflow_id, session_id)
         );",
    )
    .map_err(|e| format!("初始化表失败: {e}"))?;
    Ok(conn)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let state: String = row.get(1)?;
    Ok(Session {
        id: row.get(0)?,
        state: amux_common::domain::parse_session_state(&state).unwrap_or(SessionState::Idle),
        title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        workspace: row.get(3)?,
        worktree_dir: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        machine: row.get(5)?,
        agent: row.get(6)?,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
    })
}

fn row_to_workflow(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRow> {
    let state: String = row.get(2)?;
    Ok(WorkflowRow {
        id: row.get(0)?,
        title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        state: amux_common::domain::parse_session_state(&state).unwrap_or(SessionState::Idle),
        plan: row.get(3)?,
        created_at: row.get::<_, i64>(4)? as u64,
        updated_at: row.get::<_, i64>(5)? as u64,
    })
}

fn collect<T>(
    rows: rusqlite::Result<
        rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    >,
) -> Vec<T> {
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, machine: &str, agent: &str) -> Session {
        Session {
            id: id.to_string(),
            machine: machine.into(),
            agent: agent.into(),
            title: String::new(),
            state: SessionState::Idle,
            workspace: "/tmp".into(),
            worktree_dir: String::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn sessions_page_excludes_linked_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_session(&session("s2", "pc", "codex")).unwrap();
        store.insert_workflow("w1", "wf", SessionState::Idle, "plan", 1);
        store.link_session("w1", "s2");

        let (page, has_more) = store.sessions_page(10, 0);
        assert_eq!(
            page.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s1"]
        );
        assert!(!has_more);
        assert_eq!(store.linked_sessions("w1"), ["s2"]);
    }

    #[test]
    fn linked_sessions_sorted_by_session_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, updated_at) in [("s1", 100u64), ("s2", 300), ("s3", 200)] {
            let mut s = session(id, "pc", "codex");
            s.updated_at = updated_at;
            store.insert_session(&s).unwrap();
        }
        store.insert_workflow("w1", "wf", SessionState::Idle, "plan", 1);
        // 按任意顺序关联，返回时应按各自最近活跃倒序
        store.link_session("w1", "s3");
        store.link_session("w1", "s1");
        store.link_session("w1", "s2");

        assert_eq!(store.linked_sessions("w1"), ["s2", "s3", "s1"]);
    }

    #[test]
    fn busy_sessions_are_reset_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut s = session("s1", "pc", "codex");
        s.state = SessionState::Busy;
        store.insert_session(&s).unwrap();
        drop(store);

        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(
            reopened.session("s1").unwrap().state,
            SessionState::Idle,
            "Server 重启后不应残留工作中状态"
        );
    }

    #[test]
    fn messages_page_returns_oldest_first_with_paging() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        for index in 0..5 {
            store.upsert_message(
                "s1",
                &format!("m{index}"),
                "user",
                r#"[{"type":"text","text":"hi"}]"#,
                100 + index as u64,
            );
        }
        let (page, has_more) = store.messages_page("s1", 2, 0);
        assert_eq!(page.len(), 2);
        assert!(has_more);
        let (older, has_more) = store.messages_page("s1", 2, 2);
        assert_eq!(older.len(), 2);
        assert!(has_more);
    }
}
