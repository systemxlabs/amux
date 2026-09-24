//! 持久化：会话元数据（`session.sqlite`）、会话 transcript（每会话一个
//! `transcript.sqlite`）与工作流（`workflow.sqlite`）。
//!
//! 各连接由 `Mutex` 保护；SQLite 查询同步执行（本地文件，量级小）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use amux_common::api::Session;
use amux_common::domain::{Activity, HistoryItem, SessionState};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::timestamps::now_ms;

const TRANSCRIPT_CACHE_LIMIT: usize = 64;

struct CachedTranscript {
    connection: Arc<Mutex<Connection>>,
    last_used: u64,
}

#[derive(Default)]
struct TranscriptCache {
    entries: HashMap<String, CachedTranscript>,
    clock: u64,
}

impl TranscriptCache {
    fn get(&mut self, session_id: &str) -> Option<Arc<Mutex<Connection>>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(session_id)?;
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.connection))
    }

    fn insert(&mut self, session_id: String, connection: Arc<Mutex<Connection>>) {
        self.clock = self.clock.wrapping_add(1);
        if self.entries.len() >= TRANSCRIPT_CACHE_LIMIT {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(session_id, _)| session_id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            session_id,
            CachedTranscript {
                connection,
                last_used: self.clock,
            },
        );
    }

    fn remove(&mut self, session_id: &str) {
        self.entries.remove(session_id);
    }
}

pub struct Store {
    home: PathBuf,
    sessions: Mutex<Connection>,
    workflows: Mutex<Connection>,
    transcripts: Mutex<TranscriptCache>,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self, String> {
        let home = home.to_path_buf();
        std::fs::create_dir_all(&home).map_err(|e| format!("创建 {} 失败: {e}", home.display()))?;
        let sessions = open_db(&home.join("session.sqlite"), SESSION_SCHEMA)?;
        let workflows = open_db(&home.join("workflow.sqlite"), WORKFLOW_SCHEMA)?;
        ensure_column(&sessions, "sessions", "project", "TEXT")?;
        ensure_column(&workflows, "workflows", "project", "TEXT")?;
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
            home,
            sessions: Mutex::new(sessions),
            workflows: Mutex::new(workflows),
            transcripts: Mutex::new(TranscriptCache::default()),
        })
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.home.join("sessions").join(session_id)
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("transcript.sqlite")
    }

    /// 获取会话 transcript 连接；连接按会话缓存，避免流式写入时重复打开数据库。
    fn transcript(
        &self,
        session_id: &str,
        create: bool,
    ) -> Result<Option<Arc<Mutex<Connection>>>, String> {
        let mut transcripts = self.transcripts.lock();
        if let Some(transcript) = transcripts.get(session_id) {
            return Ok(Some(transcript));
        }

        let path = self.transcript_path(session_id);
        if !create && !path.is_file() {
            return Ok(None);
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("会话 transcript 路径无效: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败: {error}", parent.display()))?;
        let transcript = Arc::new(Mutex::new(open_db(&path, TRANSCRIPT_SCHEMA)?));
        transcripts.insert(session_id.to_string(), Arc::clone(&transcript));
        Ok(Some(transcript))
    }

    // ---------- 普通会话 ----------

    pub fn insert_session(&self, session: &Session) -> Result<(), String> {
        self.sessions
            .lock()
            .execute(
                "INSERT INTO sessions (id, state, title, project, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10)",
                params![
                    session.id,
                    session.state.as_str(),
                    session.title,
                    session.project.as_deref(),
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
                "SELECT id, state, title, project, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
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
            "SELECT id, state, title, project, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
             FROM sessions ORDER BY created_at DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], row_to_session);
        collect(rows)
    }

    /// 会话列表：排除被工作流关联的会话（docs/DESIGN.md：`GET /sessions` 只出非关联会话）。
    /// 会话量级小，关联标记在 Rust 侧过滤；`project` 指定时只返回该项目的会话。
    pub fn sessions_page(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> (Vec<Session>, bool) {
        let linked: std::collections::HashSet<String> =
            self.linked_session_ids().into_iter().collect();
        let all: Vec<Session> = self
            .sessions_all()
            .into_iter()
            .filter(|session| !linked.contains(&session.id))
            .filter(|session| match project {
                None => true,
                Some("") => session.project.is_none(),
                Some(project) => session.project.as_deref() == Some(project),
            })
            .collect();
        let has_more = all.len() > offset + limit;
        let page = all.into_iter().skip(offset).take(limit).collect();
        (page, has_more)
    }

    /// 所有被工作流关联的会话 id。
    fn linked_session_ids(&self) -> Vec<String> {
        let conn = self.workflows.lock();
        let mut stmt =
            match conn.prepare("SELECT DISTINCT session_id FROM workflow_linked_sessions") {
                Ok(stmt) => stmt,
                Err(_) => return Vec::new(),
            };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        collect(rows)
    }

    pub fn sessions_of_agent(&self, machine: &str, agent: &str) -> Vec<Session> {
        let conn = self.sessions.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, state, title, project, workspace, worktree_dir, machine, agent, agent_session_id, created_at, updated_at
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
        drop(conn);
        let _ = self.workflows.lock().execute(
            "DELETE FROM workflow_linked_sessions WHERE session_id = ?1",
            params![id],
        );
        self.transcripts.lock().remove(id);
        let dir = self.session_dir(id);
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("删除会话数据目录 {} 失败: {error}", dir.display());
            }
        }
    }

    /// 标记关联会话（供 `GET /sessions` 排除）。
    pub fn link_session(&self, workflow_id: &str, session_id: &str) {
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
        let transcript = match self.transcript(session_id, true) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return;
            }
            Ok(None) => return,
        };
        let _ = transcript.lock().execute(
            "INSERT INTO messages (message_id, role, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(message_id) DO UPDATE SET content = ?3, updated_at = ?5",
            params![
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
        let transcript = match self.transcript(session_id, false) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return (Vec::new(), false);
            }
            Ok(None) => return (Vec::new(), false),
        };
        let conn = transcript.lock();
        let mut stmt = match conn.prepare(
            "SELECT message_id, role, content, created_at, updated_at FROM messages
             ORDER BY updated_at DESC, message_id DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![(limit + 1) as i64, offset as i64], |row| {
            let message_id: String = row.get(0)?;
            let role: String = row.get(1)?;
            let content: String = row.get(2)?;
            let created_at: i64 = row.get(3)?;
            let updated_at: i64 = row.get(4)?;
            Ok((
                message_id,
                role,
                content,
                created_at as u64,
                updated_at as u64,
            ))
        });
        let mut items: Vec<HistoryItem> = match rows {
            Ok(rows) => rows
                .flatten()
                .map(|(id, role, content, created_at, updated_at)| {
                    let blocks = serde_json::from_str(&content).unwrap_or_default();
                    match role.as_str() {
                        "user" => HistoryItem::UserMessage {
                            id,
                            content: blocks,
                            timestamp: created_at,
                        },
                        _ => HistoryItem::AgentMessage {
                            id,
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
        let transcript = match self.transcript(session_id, true) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return;
            }
            Ok(None) => return,
        };
        let _ = transcript.lock().execute(
            "INSERT INTO activities (activity_id, kind, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(activity_id) DO UPDATE SET content = ?3, updated_at = ?5",
            params![
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
        let transcript = match self.transcript(session_id, false) {
            Ok(Some(transcript)) => transcript,
            Err(error) => {
                log::warn!("打开会话 transcript 失败（{session_id}）: {error}");
                return (Vec::new(), false);
            }
            Ok(None) => return (Vec::new(), false),
        };
        let conn = transcript.lock();
        let mut stmt = match conn.prepare(
            "SELECT activity_id, content FROM activities
             ORDER BY updated_at DESC, activity_id DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(params![(limit + 1) as i64, offset as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        let mut activities: Vec<Activity> = match rows {
            Ok(rows) => rows
                .flatten()
                .filter_map(|(id, content)| {
                    let mut activity: Activity = serde_json::from_str(&content).ok()?;
                    // 标识以 activity_id 列为准（列即身份，内容里的标识仅作冗余）
                    activity.set_id(id);
                    Some(activity)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let has_more = activities.len() > limit;
        activities.truncate(limit);
        activities.reverse();
        (activities, has_more)
    }

    /// 会话最近一次活动时间（`activities` 表最新一条；从未有活动时为 `None`）。
    pub fn last_activity_at(&self, session_id: &str) -> Option<u64> {
        let transcript = self.transcript(session_id, false).ok()??;
        let at: Option<i64> = transcript
            .lock()
            .query_row("SELECT MAX(updated_at) FROM activities", [], |row| {
                row.get(0)
            })
            .ok()
            .flatten();
        at.map(|at| at as u64)
    }

    /// 最近一条活动（进行中活动展示用）。
    pub fn latest_activity(&self, session_id: &str) -> Option<Activity> {
        let transcript = self.transcript(session_id, false).ok()??;
        let content = transcript
            .lock()
            .query_row(
                "SELECT content FROM activities ORDER BY updated_at DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten();
        content.and_then(|content| serde_json::from_str(&content).ok())
    }

    // ---------- 工作流会话 ----------

    pub fn insert_workflow(
        &self,
        id: &str,
        title: &str,
        state: SessionState,
        plan: &str,
        project: Option<&str>,
        created_at: u64,
    ) {
        let _ = self.workflows.lock().execute(
            "INSERT INTO workflows (id, title, state, plan, project, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, title, state.as_str(), plan, project, created_at as i64],
        );
    }

    pub fn workflow(&self, id: &str) -> Option<WorkflowRow> {
        self.workflows
            .lock()
            .query_row(
                "SELECT id, title, state, plan, project, created_at, updated_at FROM workflows WHERE id = ?1",
                params![id],
                row_to_workflow,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn workflows_page(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> (Vec<WorkflowRow>, bool) {
        let conn = self.workflows.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, title, state, plan, project, created_at, updated_at FROM workflows
             WHERE (?3 IS NULL OR (?3 = '' AND project IS NULL) OR project = ?3)
             ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return (Vec::new(), false),
        };
        let rows = stmt.query_map(
            params![(limit + 1) as i64, offset as i64, project],
            row_to_workflow,
        );
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

    pub fn set_workflow_title_if_empty(&self, id: &str, title: &str) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET title = ?2, updated_at = ?3
             WHERE id = ?1 AND (title IS NULL OR title = '')",
            params![id, title, now_ms() as i64],
        );
    }

    pub fn touch_workflow(&self, id: &str) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms() as i64],
        );
    }

    /// 更新普通会话所属项目（None = 未归属）。
    pub fn set_session_project(&self, id: &str, project: Option<&str>) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET project = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, project, now_ms() as i64],
        );
    }

    /// 更新工作流会话所属项目（None = 未归属）。
    pub fn set_workflow_project(&self, id: &str, project: Option<&str>) {
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET project = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, project, now_ms() as i64],
        );
    }

    /// 项目删除后，其下所有会话回到未归属（docs/PRD.md「项目管理」）。
    pub fn unassign_project(&self, project: &str) {
        let _ = self.sessions.lock().execute(
            "UPDATE sessions SET project = NULL WHERE project = ?1",
            params![project],
        );
        let _ = self.workflows.lock().execute(
            "UPDATE workflows SET project = NULL WHERE project = ?1",
            params![project],
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

        // 关联普通会话按创建时间倒序排序（docs/PRD.md「会话列表视图」）。
        let mut sessions: Vec<(String, u64)> = ids
            .into_iter()
            .filter_map(|session_id| {
                let created_at = self
                    .sessions
                    .lock()
                    .query_row(
                        "SELECT created_at FROM sessions WHERE id = ?1",
                        params![session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()?;
                Some((session_id, created_at as u64))
            })
            .collect();
        sessions.sort_by_key(|(_, created_at)| std::cmp::Reverse(*created_at));
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
    pub project: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 中心 `session.sqlite` 表结构。
const SESSION_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        state TEXT NOT NULL,
        title TEXT,
        project TEXT,
        workspace TEXT NOT NULL,
        worktree_dir TEXT,
        machine TEXT NOT NULL,
        agent TEXT NOT NULL,
        agent_session_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );";

/// 每会话 `transcript.sqlite` 表结构。
const TRANSCRIPT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS messages (
        message_id TEXT PRIMARY KEY,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS activities (
        activity_id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        content TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );";

/// `workflow.sqlite` 表结构。
const WORKFLOW_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS workflows (
        id TEXT PRIMARY KEY,
        title TEXT,
        state TEXT NOT NULL,
        plan TEXT NOT NULL,
        project TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS workflow_linked_sessions (
        workflow_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        PRIMARY KEY (workflow_id, session_id)
    );";

/// 为已有数据库补充新增列（老库无 project 列时）。
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("读取 {table} 表结构失败: {e}"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("读取 {table} 表结构失败: {e}"))?
        .flatten()
        .collect();
    if columns.iter().any(|name| name == column) {
        return Ok(());
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )
    .map(|_| ())
    .map_err(|e| format!("为 {table} 补充 {column} 列失败: {e}"))
}

fn open_db(path: &Path, schema: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    conn.execute_batch(&format!("PRAGMA journal_mode = WAL;\n{schema}"))
        .map_err(|e| format!("初始化表失败: {e}"))?;
    Ok(conn)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let state: String = row.get(1)?;
    Ok(Session {
        id: row.get(0)?,
        state: amux_common::domain::parse_session_state(&state).unwrap_or(SessionState::Idle),
        title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        project: row.get(3)?,
        workspace: row.get(4)?,
        worktree_dir: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        machine: row.get(6)?,
        agent: row.get(7)?,
        created_at: row.get::<_, i64>(9)? as u64,
        updated_at: row.get::<_, i64>(10)? as u64,
    })
}

fn row_to_workflow(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRow> {
    let state: String = row.get(2)?;
    Ok(WorkflowRow {
        id: row.get(0)?,
        title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        state: amux_common::domain::parse_session_state(&state).unwrap_or(SessionState::Idle),
        plan: row.get(3)?,
        project: row.get(4)?,
        created_at: row.get::<_, i64>(5)? as u64,
        updated_at: row.get::<_, i64>(6)? as u64,
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
            project: None,
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
        store.insert_workflow("w1", "wf", SessionState::Idle, "plan", None, 1);
        store.link_session("w1", "s2");

        let (page, has_more) = store.sessions_page(10, 0, None);
        assert_eq!(
            page.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s1"]
        );
        assert!(!has_more);
        assert_eq!(store.linked_sessions("w1"), ["s2"]);
    }

    #[test]
    fn sessions_page_filters_unassigned_before_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, project, created_at) in
            [("s1", None, 10u64), ("p1", Some("p"), 9), ("s2", None, 8)]
        {
            let mut row = session(id, "pc", "codex");
            row.project = project.map(str::to_string);
            row.created_at = created_at;
            store.insert_session(&row).unwrap();
        }

        let (first, has_more) = store.sessions_page(1, 0, Some(""));
        assert_eq!(
            first.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["s1"]
        );
        assert!(has_more);

        let (second, has_more) = store.sessions_page(1, 1, Some(""));
        assert_eq!(
            second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["s2"]
        );
        assert!(!has_more);
    }

    #[test]
    fn linked_sessions_sorted_by_session_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for (id, created_at) in [("s1", 100u64), ("s2", 300), ("s3", 200)] {
            let mut s = session(id, "pc", "codex");
            s.created_at = created_at;
            store.insert_session(&s).unwrap();
        }
        store.insert_workflow("w1", "wf", SessionState::Idle, "plan", None, 1);
        // 按任意顺序关联，返回时应按创建时间倒序
        store.link_session("w1", "s3");
        store.link_session("w1", "s1");
        store.link_session("w1", "s2");

        assert_eq!(store.linked_sessions("w1"), ["s2", "s3", "s1"]);
    }

    #[test]
    fn workflows_page_filters_project_before_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_workflow("a1", "a1", SessionState::Idle, "p", Some("p"), 10);
        store.insert_workflow("b1", "b1", SessionState::Idle, "q", Some("q"), 9);
        store.insert_workflow("a2", "a2", SessionState::Idle, "p", Some("p"), 8);

        let (first, has_more) = store.workflows_page(1, 0, Some("p"));
        assert_eq!(
            first.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a1"]
        );
        assert!(has_more);

        let (second, has_more) = store.workflows_page(1, 1, Some("p"));
        assert_eq!(
            second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a2"]
        );
        assert!(!has_more);
    }

    #[test]
    fn workflows_page_filters_unassigned_before_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_workflow("a1", "a1", SessionState::Idle, "p", None, 10);
        store.insert_workflow("b1", "b1", SessionState::Idle, "q", Some("q"), 9);
        store.insert_workflow("a2", "a2", SessionState::Idle, "p", None, 8);

        let (first, has_more) = store.workflows_page(1, 0, Some(""));
        assert_eq!(
            first.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a1"]
        );
        assert!(has_more);

        let (second, has_more) = store.workflows_page(1, 1, Some(""));
        assert_eq!(
            second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["a2"]
        );
        assert!(!has_more);
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

    #[test]
    fn transcript_is_isolated_per_session_and_deleted_with_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.insert_session(&session("s1", "pc", "codex")).unwrap();
        store.insert_session(&session("s2", "pc", "codex")).unwrap();

        assert!(!store.transcript_path("s1").exists());
        assert_eq!(store.messages_page("s1", 10, 0).0.len(), 0);
        assert_eq!(store.activities_page("s1", 10, 0).0.len(), 0);
        assert!(!store.transcript_path("s1").exists());

        for session_id in ["s1", "s2"] {
            store.upsert_message(
                session_id,
                "m1",
                "user",
                r#"[{"type":"text","text":"hi"}]"#,
                1,
            );
            store.upsert_activity(
                session_id,
                "a1",
                "thinking",
                r#"{"kind":"thinking","id":"a1","timestamp":1,"thinking":"x"}"#,
                1,
            );
        }

        assert!(store.transcript_path("s1").is_file());
        assert!(store.transcript_path("s2").is_file());
        assert_eq!(store.messages_page("s1", 10, 0).0.len(), 1);
        assert_eq!(store.activities_page("s1", 10, 0).0.len(), 1);

        store.delete_session("s1");

        assert!(!store.session_dir("s1").exists());
        assert_eq!(store.messages_page("s1", 10, 0).0.len(), 0);
        assert_eq!(store.messages_page("s2", 10, 0).0.len(), 1);
        assert_eq!(store.activities_page("s2", 10, 0).0.len(), 1);
    }
}
